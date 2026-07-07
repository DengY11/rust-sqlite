use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rustsql::common::types::{ColumnDef, ColumnType, IndexMeta, RowId, Schema, Value};
use rustsql::db::Database;
use rustsql::engine::{
    CatalogStore, IndexStore, PlanningStorageEngine, TableStore, TransactionManager,
};
use rustsql::sql::ast::{CompareOp, IsolationLevel};
use rustsql::storage::v2::FileStorage;
use rustsql::storage::v2::btree::BTree;
use rustsql::storage::v2::catalog::{CatalogState, load_catalog, store_catalog};
use rustsql::storage::v2::page::PageId;
use rustsql::storage::v2::pager::Pager;
use tempfile::tempdir;

fn users_schema() -> Schema {
    Schema::new(
        "users",
        vec![
            ColumnDef::primary_key("id", ColumnType::Integer),
            ColumnDef::new("name", ColumnType::Text),
            ColumnDef::new("email", ColumnType::Text),
            ColumnDef::new("active", ColumnType::Boolean),
        ],
    )
}

fn user_row(id: i64, name: &str, email: &str, active: bool) -> Vec<Value> {
    vec![
        Value::Integer(id),
        Value::from(name),
        Value::from(email),
        Value::Boolean(active),
    ]
}

fn unique_email_index(name: &str) -> IndexMeta {
    IndexMeta {
        name: name.to_string(),
        columns: vec!["email".to_string()],
        decorated_columns: None,
        unique: true,
        predicate: None,
    }
}

#[test]
fn storage_v2_persists_schema_and_rows_across_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let storage = FileStorage::open(&path).unwrap();
        let txn = storage.begin().unwrap();
        storage
            .create_schema(
                txn,
                Schema::new(
                    "users",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("name", ColumnType::Text).nullable(false),
                    ],
                ),
            )
            .unwrap();
        storage
            .insert_row(txn, "users", vec![Value::Integer(1), Value::from("alice")])
            .unwrap();
        storage.commit(txn).unwrap();
    }

    let reopened = FileStorage::open(&path).unwrap();
    let txn = reopened.begin().unwrap();
    assert!(reopened.get_schema(txn, "users").unwrap().is_some());
    assert_eq!(reopened.scan_rows(txn, "users").unwrap().len(), 1);
    reopened.rollback(txn).unwrap();
}

#[test]
fn storage_v2_renames_table_and_column_and_rewrites_added_column() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage
        .create_index(
            txn,
            "users",
            IndexMeta {
                name: "idx_users_name".to_string(),
                columns: vec!["name".to_string()],
                decorated_columns: None,
                unique: false,
                predicate: None,
            },
        )
        .unwrap();
    storage
        .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
        .unwrap();

    storage
        .add_column(
            txn,
            "users",
            ColumnDef::new("age", ColumnType::Integer).default_value(
                rustsql::common::types::ColumnDefault::Literal(Value::Integer(0)),
            ),
        )
        .unwrap();
    storage
        .rename_column(txn, "users", "name", "full_name")
        .unwrap();
    storage.rename_schema(txn, "users", "customers").unwrap();

    assert!(storage.get_schema(txn, "users").unwrap().is_none());
    let schema = storage.get_schema(txn, "customers").unwrap().unwrap();
    assert_eq!(schema.name, "customers");
    assert_eq!(schema.columns[1].name, "full_name");
    assert_eq!(schema.columns[4].name, "age");
    assert_eq!(
        storage.scan_rows(txn, "customers").unwrap()[0].1[4],
        Value::Integer(0)
    );
    assert_eq!(
        storage.list_indexes(txn, "customers").unwrap()[0].columns,
        vec!["full_name".to_string()]
    );
    storage.rollback(txn).unwrap();
}

#[test]
fn storage_v2_rollback_discards_uncommitted_rows_after_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let storage = FileStorage::open(&path).unwrap();
        let txn = storage.begin().unwrap();
        storage
            .create_schema(
                txn,
                Schema::new(
                    "users",
                    vec![ColumnDef::primary_key("id", ColumnType::Integer)],
                ),
            )
            .unwrap();
        storage
            .insert_row(txn, "users", vec![Value::Integer(1)])
            .unwrap();
        storage.rollback(txn).unwrap();
    }

    let reopened = FileStorage::open(&path).unwrap();
    let txn = reopened.begin().unwrap();
    assert!(reopened.get_schema(txn, "users").unwrap().is_none());
    assert!(reopened.scan_rows(txn, "users").unwrap().is_empty());
    reopened.rollback(txn).unwrap();
}

#[test]
fn storage_v2_get_row_by_rowid_after_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let storage = FileStorage::open(&path).unwrap();
        let txn = storage.begin().unwrap();
        storage
            .create_schema(
                txn,
                Schema::new(
                    "users",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("name", ColumnType::Text),
                    ],
                ),
            )
            .unwrap();
        let row_id = storage
            .insert_row(txn, "users", vec![Value::Integer(1), Value::from("alice")])
            .unwrap();
        assert_eq!(row_id.0, 1);
        storage.commit(txn).unwrap();
    }

    let reopened = FileStorage::open(&path).unwrap();
    let txn = reopened.begin().unwrap();
    assert_eq!(
        reopened
            .get_row(txn, "users", rustsql::common::types::RowId(1))
            .unwrap(),
        Some(vec![Value::Integer(1), Value::from("alice")])
    );
    reopened.rollback(txn).unwrap();
}

#[test]
fn storage_v2_planning_context_exposes_schema_without_indexes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage
        .create_schema(
            txn,
            Schema::new(
                "users",
                vec![ColumnDef::primary_key("id", ColumnType::Integer)],
            ),
        )
        .unwrap();

    let context = storage.planning_context_snapshot(Some(txn)).unwrap();
    assert!(context.schema("users").is_some());
    assert!(context.indexes_for("users").is_empty());

    storage.rollback(txn).unwrap();
}

#[test]
fn storage_v2_planning_context_exposes_persisted_indexes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();
        let mut catalog = CatalogState::default();
        catalog.schemas.insert(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                ],
            ),
        );
        catalog.table_roots.insert("users".to_string(), PageId(2));
        catalog
            .indexes
            .entry("users".to_string())
            .or_default()
            .insert(
                "idx_users_name".to_string(),
                IndexMeta {
                    name: "idx_users_name".to_string(),
                    columns: vec!["name".to_string()],
                    decorated_columns: None,
                    unique: false,
                    predicate: None,
                },
            );
        catalog
            .index_roots
            .entry("users".to_string())
            .or_default()
            .insert("idx_users_name".to_string(), PageId(3));
        store_catalog(&mut pager, txn, &catalog).unwrap();
        pager.commit(txn).unwrap();
    }

    let reopened = FileStorage::open(&path).unwrap();
    let context = reopened.planning_context_snapshot(None).unwrap();
    assert_eq!(context.indexes_for("users").len(), 1);
    assert_eq!(context.indexes_for("users")[0].name, "idx_users_name");
}

#[test]
fn storage_v2_begin_with_isolation_keeps_planning_context_working() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage
        .begin_with_isolation(IsolationLevel::Serializable)
        .unwrap();

    storage.create_schema(txn, users_schema()).unwrap();

    let context = storage.planning_context_snapshot(Some(txn)).unwrap();
    assert!(context.schema("users").is_some());

    storage.rollback(txn).unwrap();
}

#[test]
fn storage_v2_repeatable_read_keeps_row_snapshot_stable_across_concurrent_commit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();

    let setup_txn = storage.begin().unwrap();
    storage.create_schema(setup_txn, users_schema()).unwrap();
    storage.commit(setup_txn).unwrap();

    let reader = storage
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    assert!(storage.scan_rows(reader, "users").unwrap().is_empty());

    let writer = storage
        .begin_with_isolation(IsolationLevel::ReadCommitted)
        .unwrap();
    storage
        .insert_row(writer, "users", user_row(1, "alice", "a@example.com", true))
        .unwrap();
    storage.commit(writer).unwrap();

    assert!(storage.scan_rows(reader, "users").unwrap().is_empty());

    storage.rollback(reader).unwrap();
}

#[test]
fn storage_v2_read_committed_refreshes_row_snapshot_between_statements() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();

    let setup_txn = storage.begin().unwrap();
    storage.create_schema(setup_txn, users_schema()).unwrap();
    storage.commit(setup_txn).unwrap();

    let reader = storage
        .begin_with_isolation(IsolationLevel::ReadCommitted)
        .unwrap();
    assert!(storage.scan_rows(reader, "users").unwrap().is_empty());

    let writer = storage
        .begin_with_isolation(IsolationLevel::Serializable)
        .unwrap();
    storage
        .insert_row(writer, "users", user_row(1, "alice", "a@example.com", true))
        .unwrap();
    storage.commit(writer).unwrap();

    assert_eq!(storage.scan_rows(reader, "users").unwrap().len(), 1);

    storage.rollback(reader).unwrap();
}

#[test]
fn storage_v2_read_committed_refreshes_catalog_snapshot_between_statements() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    let reader_storage = FileStorage::open(&path).unwrap();
    let writer_storage = FileStorage::open(&path).unwrap();

    let reader = reader_storage
        .begin_with_isolation(IsolationLevel::ReadCommitted)
        .unwrap();
    assert!(
        reader_storage
            .planning_context_snapshot(Some(reader))
            .unwrap()
            .schema("users")
            .is_none()
    );

    let writer = writer_storage.begin().unwrap();
    writer_storage
        .create_schema(writer, users_schema())
        .unwrap();
    writer_storage.commit(writer).unwrap();

    assert!(
        reader_storage
            .planning_context_snapshot(Some(reader))
            .unwrap()
            .schema("users")
            .is_some()
    );

    reader_storage.rollback(reader).unwrap();
}

#[test]
fn storage_v2_deadlock_selected_victim_wakes_and_cleanup_leaves_no_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let setup_storage = FileStorage::open(&path).unwrap();

    let setup_txn = setup_storage.begin().unwrap();
    setup_storage
        .create_schema(setup_txn, users_schema())
        .unwrap();
    setup_storage.commit(setup_txn).unwrap();

    let (writer_inserted_tx, writer_inserted_rx) = mpsc::channel();
    let (reader_ready_tx, reader_ready_rx) = mpsc::channel();
    let (reader_inserting_tx, reader_inserting_rx) = mpsc::channel();
    let (start_writer_deadlock_tx, start_writer_deadlock_rx) = mpsc::channel();
    let (writer_done_tx, writer_done_rx) = mpsc::channel();
    let (reader_deadlock_tx, reader_deadlock_rx) = mpsc::channel();

    let writer_path = path.clone();
    let writer = thread::spawn(move || {
        let storage = FileStorage::open(&writer_path).unwrap();
        let txn = storage
            .begin_with_isolation(IsolationLevel::ReadCommitted)
            .unwrap();
        storage
            .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
            .unwrap();
        writer_inserted_tx.send(()).unwrap();
        start_writer_deadlock_rx.recv().unwrap();
        storage
            .insert_row(txn, "users", user_row(3, "carol", "c@example.com", true))
            .unwrap();
        storage.rollback(txn).unwrap();
        writer_done_tx.send(()).unwrap();
    });

    writer_inserted_rx.recv().unwrap();

    let reader_path = path.clone();
    let reader = thread::spawn(move || {
        let storage = FileStorage::open(&reader_path).unwrap();
        let txn = storage
            .begin_with_isolation(IsolationLevel::Serializable)
            .unwrap();
        assert!(storage.scan_rows(txn, "users").unwrap().is_empty());
        reader_ready_tx.send(()).unwrap();
        reader_inserting_tx.send(()).unwrap();
        let deadlock = storage
            .insert_row(txn, "users", user_row(2, "bob", "b@example.com", true))
            .unwrap_err();
        assert!(deadlock.to_string().contains("deadlock"));
        storage.rollback(txn).unwrap();
        reader_deadlock_tx.send(()).unwrap();
    });

    reader_ready_rx.recv().unwrap();
    reader_inserting_rx.recv().unwrap();
    thread::sleep(Duration::from_millis(50));
    start_writer_deadlock_tx.send(()).unwrap();

    writer_done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    reader_deadlock_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    writer.join().unwrap();
    reader.join().unwrap();

    let verify_storage = FileStorage::open(&path).unwrap();
    let verify_txn = verify_storage.begin().unwrap();
    assert!(
        verify_storage
            .scan_rows(verify_txn, "users")
            .unwrap()
            .is_empty()
    );
    verify_storage.rollback(verify_txn).unwrap();
}

#[test]
fn storage_v2_page_write_waits_until_holder_commits_then_wakes_up() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    let setup_storage = FileStorage::open(&path).unwrap();
    let setup_txn = setup_storage.begin().unwrap();
    setup_storage
        .create_schema(setup_txn, users_schema())
        .unwrap();
    setup_storage.commit(setup_txn).unwrap();

    let (writer1_locked_tx, writer1_locked_rx) = mpsc::channel();
    let (release_writer1_tx, release_writer1_rx) = mpsc::channel();
    let (writer2_done_tx, writer2_done_rx) = mpsc::channel();

    let writer1_path = path.clone();
    let writer1 = thread::spawn(move || {
        let storage = FileStorage::open(&writer1_path).unwrap();
        let txn = storage.begin().unwrap();
        storage
            .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
            .unwrap();
        writer1_locked_tx.send(()).unwrap();
        release_writer1_rx.recv().unwrap();
        storage.commit(txn).unwrap();
    });

    writer1_locked_rx.recv().unwrap();

    let writer2_path = path.clone();
    let writer2 = thread::spawn(move || {
        let storage = FileStorage::open(&writer2_path).unwrap();
        let txn = storage.begin().unwrap();
        storage
            .insert_row(txn, "users", user_row(2, "bob", "b@example.com", true))
            .unwrap();
        storage.commit(txn).unwrap();
        writer2_done_tx.send(()).unwrap();
    });

    assert!(
        writer2_done_rx
            .recv_timeout(Duration::from_millis(150))
            .is_err()
    );
    release_writer1_tx.send(()).unwrap();
    writer2_done_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    writer1.join().unwrap();
    writer2.join().unwrap();

    let verify_storage = FileStorage::open(&path).unwrap();
    let verify_txn = verify_storage.begin().unwrap();
    assert_eq!(
        verify_storage.scan_rows(verify_txn, "users").unwrap().len(),
        2
    );
    verify_storage.rollback(verify_txn).unwrap();
}

#[test]
fn storage_v2_predicate_write_waits_until_reader_rolls_back_then_wakes_up() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    let setup_storage = FileStorage::open(&path).unwrap();
    let setup_txn = setup_storage.begin().unwrap();
    setup_storage
        .create_schema(setup_txn, users_schema())
        .unwrap();
    setup_storage.commit(setup_txn).unwrap();

    let (reader_locked_tx, reader_locked_rx) = mpsc::channel();
    let (release_reader_tx, release_reader_rx) = mpsc::channel();
    let (writer_done_tx, writer_done_rx) = mpsc::channel();

    let reader_path = path.clone();
    let reader = thread::spawn(move || {
        let storage = FileStorage::open(&reader_path).unwrap();
        let txn = storage
            .begin_with_isolation(IsolationLevel::Serializable)
            .unwrap();
        assert!(storage.scan_rows(txn, "users").unwrap().is_empty());
        reader_locked_tx.send(()).unwrap();
        release_reader_rx.recv().unwrap();
        storage.rollback(txn).unwrap();
    });

    reader_locked_rx.recv().unwrap();

    let writer_path = path.clone();
    let writer = thread::spawn(move || {
        let storage = FileStorage::open(&writer_path).unwrap();
        let txn = storage.begin().unwrap();
        storage
            .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
            .unwrap();
        storage.commit(txn).unwrap();
        writer_done_tx.send(()).unwrap();
    });

    assert!(
        writer_done_rx
            .recv_timeout(Duration::from_millis(150))
            .is_err()
    );
    release_reader_tx.send(()).unwrap();
    writer_done_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    reader.join().unwrap();
    writer.join().unwrap();

    let verify_storage = FileStorage::open(&path).unwrap();
    let verify_txn = verify_storage.begin().unwrap();
    assert_eq!(
        verify_storage.scan_rows(verify_txn, "users").unwrap().len(),
        1
    );
    verify_storage.rollback(verify_txn).unwrap();
}

#[test]
fn storage_v2_purge_physically_removes_deleted_rows_after_old_reader_finishes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();

    let setup_txn = storage.begin().unwrap();
    storage.create_schema(setup_txn, users_schema()).unwrap();
    let row_id = storage
        .insert_row(
            setup_txn,
            "users",
            user_row(1, "alice", "a@example.com", true),
        )
        .unwrap();
    storage.commit(setup_txn).unwrap();

    let reader = storage
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    assert!(storage.get_row(reader, "users", row_id).unwrap().is_some());

    let deleter = storage.begin().unwrap();
    storage.delete_row(deleter, "users", row_id).unwrap();
    storage.commit(deleter).unwrap();

    {
        let pager = Pager::open(&path).unwrap();
        let catalog = load_catalog(&pager).unwrap();
        let tree = BTree::from_root(catalog.table_roots["users"]);
        assert_eq!(tree.scan_all(&pager, 0).unwrap().len(), 1);
    }

    storage.rollback(reader).unwrap();

    {
        let pager = Pager::open(&path).unwrap();
        let catalog = load_catalog(&pager).unwrap();
        let tree = BTree::from_root(catalog.table_roots["users"]);
        assert!(tree.scan_all(&pager, 0).unwrap().is_empty());
    }
}

#[test]
fn storage_v2_purge_advances_even_while_newer_reader_remains_active() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();

    let setup_txn = storage.begin().unwrap();
    storage.create_schema(setup_txn, users_schema()).unwrap();
    let row_id = storage
        .insert_row(
            setup_txn,
            "users",
            user_row(1, "alice", "a@example.com", true),
        )
        .unwrap();
    storage.commit(setup_txn).unwrap();

    let old_reader = storage
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    assert!(
        storage
            .get_row(old_reader, "users", row_id)
            .unwrap()
            .is_some()
    );

    let deleter = storage.begin().unwrap();
    storage.delete_row(deleter, "users", row_id).unwrap();
    storage.commit(deleter).unwrap();

    let newer_reader = storage
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    assert!(
        storage
            .get_row(newer_reader, "users", row_id)
            .unwrap()
            .is_none()
    );

    storage.rollback(old_reader).unwrap();

    {
        let pager = Pager::open(&path).unwrap();
        let catalog = load_catalog(&pager).unwrap();
        let tree = BTree::from_root(catalog.table_roots["users"]);
        assert!(tree.scan_all(&pager, 0).unwrap().is_empty());
    }

    storage.rollback(newer_reader).unwrap();
}

#[test]
fn storage_v2_incremental_purge_drains_history_list_in_batches() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();

    let setup_txn = storage.begin().unwrap();
    storage.create_schema(setup_txn, users_schema()).unwrap();
    let row_ids = (1..=5)
        .map(|id| {
            storage.insert_row(
                setup_txn,
                "users",
                user_row(
                    id,
                    &format!("user{id}"),
                    &format!("u{id}@example.com"),
                    true,
                ),
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    storage.commit(setup_txn).unwrap();

    let old_reader = storage
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    assert_eq!(storage.scan_rows(old_reader, "users").unwrap().len(), 5);

    let deleter = storage.begin().unwrap();
    for row_id in &row_ids {
        storage.delete_row(deleter, "users", *row_id).unwrap();
    }
    storage.commit(deleter).unwrap();

    let newer_reader = storage
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    assert!(storage.scan_rows(newer_reader, "users").unwrap().is_empty());

    storage.rollback(old_reader).unwrap();

    {
        let pager = Pager::open(&path).unwrap();
        let catalog = load_catalog(&pager).unwrap();
        let tree = BTree::from_root(catalog.table_roots["users"]);
        let remaining = tree.scan_all(&pager, 0).unwrap().len();
        assert!(remaining > 0 && remaining < 5);
    }

    for expected_upper_bound in [2, 0] {
        let tick = storage.begin().unwrap();
        storage.rollback(tick).unwrap();

        let pager = Pager::open(&path).unwrap();
        let catalog = load_catalog(&pager).unwrap();
        let tree = BTree::from_root(catalog.table_roots["users"]);
        let remaining = tree.scan_all(&pager, 0).unwrap().len();
        assert!(remaining <= expected_upper_bound);
    }

    storage.rollback(newer_reader).unwrap();
}

#[test]
fn storage_v2_repeatable_read_keeps_index_entry_visible_after_concurrent_delete() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();

    let setup_txn = storage.begin().unwrap();
    storage.create_schema(setup_txn, users_schema()).unwrap();
    storage
        .create_index(setup_txn, "users", unique_email_index("idx_users_email"))
        .unwrap();
    let row_id = storage
        .insert_row(
            setup_txn,
            "users",
            user_row(1, "alice", "alice@example.com", true),
        )
        .unwrap();
    storage.commit(setup_txn).unwrap();

    let reader = storage
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    assert_eq!(
        storage
            .lookup_index(
                reader,
                "users",
                "idx_users_email",
                &[Value::from("alice@example.com")],
            )
            .unwrap(),
        vec![row_id]
    );

    let writer = storage
        .begin_with_isolation(IsolationLevel::ReadCommitted)
        .unwrap();
    storage.delete_row(writer, "users", row_id).unwrap();
    storage.commit(writer).unwrap();

    assert_eq!(
        storage
            .lookup_index(
                reader,
                "users",
                "idx_users_email",
                &[Value::from("alice@example.com")],
            )
            .unwrap(),
        vec![row_id]
    );
    assert!(storage.get_row(reader, "users", row_id).unwrap().is_some());

    let fresh_reader = storage
        .begin_with_isolation(IsolationLevel::ReadCommitted)
        .unwrap();
    assert!(
        storage
            .lookup_index(
                fresh_reader,
                "users",
                "idx_users_email",
                &[Value::from("alice@example.com")],
            )
            .unwrap()
            .is_empty()
    );

    storage.rollback(reader).unwrap();
    storage.rollback(fresh_reader).unwrap();
}

#[test]
fn storage_v2_serializable_range_scan_blocks_conflicting_insert() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let setup_storage = FileStorage::open(&path).unwrap();

    let setup_txn = setup_storage.begin().unwrap();
    setup_storage
        .create_schema(setup_txn, users_schema())
        .unwrap();
    setup_storage
        .create_index(
            setup_txn,
            "users",
            IndexMeta {
                name: "idx_users_active_name".to_string(),
                columns: vec!["active".to_string(), "name".to_string()],
                decorated_columns: None,
                unique: false,
                predicate: None,
            },
        )
        .unwrap();
    setup_storage.commit(setup_txn).unwrap();

    let (reader_locked_tx, reader_locked_rx) = mpsc::channel();
    let (release_reader_tx, release_reader_rx) = mpsc::channel();
    let (writer_done_tx, writer_done_rx) = mpsc::channel();

    let reader_path = path.clone();
    let reader = thread::spawn(move || {
        let storage = FileStorage::open(&reader_path).unwrap();
        let txn = storage
            .begin_with_isolation(IsolationLevel::Serializable)
            .unwrap();
        assert!(
            storage
                .scan_index_range(
                    txn,
                    "users",
                    "idx_users_active_name",
                    &[Value::Boolean(true)],
                    Some((CompareOp::Gte, &Value::from("alice"))),
                    Some((CompareOp::Lte, &Value::from("carol"))),
                )
                .unwrap()
                .is_empty()
        );
        reader_locked_tx.send(()).unwrap();
        release_reader_rx.recv().unwrap();
        storage.rollback(txn).unwrap();
    });

    reader_locked_rx.recv().unwrap();

    let writer_path = path.clone();
    let writer = thread::spawn(move || {
        let storage = FileStorage::open(&writer_path).unwrap();
        let txn = storage
            .begin_with_isolation(IsolationLevel::ReadCommitted)
            .unwrap();
        storage
            .insert_row(txn, "users", user_row(1, "bob", "b@example.com", true))
            .unwrap();
        storage.commit(txn).unwrap();
        writer_done_tx.send(()).unwrap();
    });

    assert!(
        writer_done_rx
            .recv_timeout(Duration::from_millis(150))
            .is_err()
    );
    release_reader_tx.send(()).unwrap();
    writer_done_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    reader.join().unwrap();
    writer.join().unwrap();
}

#[test]
fn storage_v2_page_write_conflict_blocks_concurrent_writers_on_same_table() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let setup_storage = FileStorage::open(&path).unwrap();

    let setup_txn = setup_storage.begin().unwrap();
    setup_storage
        .create_schema(setup_txn, users_schema())
        .unwrap();
    setup_storage.commit(setup_txn).unwrap();

    let (writer1_locked_tx, writer1_locked_rx) = mpsc::channel();
    let (release_writer1_tx, release_writer1_rx) = mpsc::channel();
    let (writer2_done_tx, writer2_done_rx) = mpsc::channel();

    let writer1_path = path.clone();
    let writer1 = thread::spawn(move || {
        let storage = FileStorage::open(&writer1_path).unwrap();
        let txn = storage
            .begin_with_isolation(IsolationLevel::ReadCommitted)
            .unwrap();
        storage
            .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
            .unwrap();
        writer1_locked_tx.send(()).unwrap();
        release_writer1_rx.recv().unwrap();
        storage.rollback(txn).unwrap();
    });

    writer1_locked_rx.recv().unwrap();

    let writer2_path = path.clone();
    let writer2 = thread::spawn(move || {
        let storage = FileStorage::open(&writer2_path).unwrap();
        let txn = storage
            .begin_with_isolation(IsolationLevel::ReadCommitted)
            .unwrap();
        storage
            .insert_row(txn, "users", user_row(2, "bob", "b@example.com", false))
            .unwrap();
        storage.rollback(txn).unwrap();
        writer2_done_tx.send(()).unwrap();
    });

    assert!(
        writer2_done_rx
            .recv_timeout(Duration::from_millis(150))
            .is_err()
    );
    release_writer1_tx.send(()).unwrap();
    writer2_done_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    writer1.join().unwrap();
    writer2.join().unwrap();
}

#[test]
fn storage_v2_table_exclusive_ddl_waits_for_writer_then_wakes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let setup_storage = FileStorage::open(&path).unwrap();

    let setup_txn = setup_storage.begin().unwrap();
    setup_storage
        .create_schema(setup_txn, users_schema())
        .unwrap();
    setup_storage.commit(setup_txn).unwrap();

    let (writer_locked_tx, writer_locked_rx) = mpsc::channel();
    let (release_writer_tx, release_writer_rx) = mpsc::channel();
    let (ddl_done_tx, ddl_done_rx) = mpsc::channel();

    let writer_path = path.clone();
    let writer = thread::spawn(move || {
        let storage = FileStorage::open(&writer_path).unwrap();
        let txn = storage
            .begin_with_isolation(IsolationLevel::ReadCommitted)
            .unwrap();
        storage
            .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
            .unwrap();
        writer_locked_tx.send(()).unwrap();
        release_writer_rx.recv().unwrap();
        storage.rollback(txn).unwrap();
    });

    writer_locked_rx.recv().unwrap();

    let ddl_path = path.clone();
    let ddl = thread::spawn(move || {
        let storage = FileStorage::open(&ddl_path).unwrap();
        let txn = storage
            .begin_with_isolation(IsolationLevel::ReadCommitted)
            .unwrap();
        storage
            .add_column(
                txn,
                "users",
                ColumnDef::new("age", ColumnType::Integer).default_value(
                    rustsql::common::types::ColumnDefault::Literal(Value::Integer(0)),
                ),
            )
            .unwrap();
        storage.commit(txn).unwrap();
        ddl_done_tx.send(()).unwrap();
    });

    assert!(
        ddl_done_rx
            .recv_timeout(Duration::from_millis(150))
            .is_err()
    );
    release_writer_tx.send(()).unwrap();
    ddl_done_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    writer.join().unwrap();
    ddl.join().unwrap();

    let verify_storage = FileStorage::open(&path).unwrap();
    let verify_txn = verify_storage.begin().unwrap();
    let schema = verify_storage
        .get_schema(verify_txn, "users")
        .unwrap()
        .unwrap();
    assert!(schema.columns.iter().any(|column| column.name == "age"));
    verify_storage.rollback(verify_txn).unwrap();
}

#[test]
fn database_with_storage_v2_runs_create_insert_select_across_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let db = Database::with_storage(FileStorage::open(&path).unwrap());
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'alice');")
            .unwrap();
        assert_eq!(
            db.query("SELECT * FROM users WHERE id = 1;").unwrap(),
            vec![vec![Value::Integer(1), Value::from("alice")]]
        );
    }

    let reopened = Database::with_storage(FileStorage::open(&path).unwrap());
    assert_eq!(
        reopened
            .query("SELECT name FROM users WHERE id = 1;")
            .unwrap(),
        vec![vec![Value::from("alice")]]
    );
}

#[test]
fn storage_v2_enforces_unique_index_on_insert_and_backfill() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage
        .create_index(txn, "users", unique_email_index("idx_users_email_unique"))
        .unwrap();

    storage
        .insert_row(
            txn,
            "users",
            user_row(1, "alice", "alice@example.com", true),
        )
        .unwrap();

    let insert_error = storage
        .insert_row(
            txn,
            "users",
            user_row(2, "ally", "alice@example.com", false),
        )
        .unwrap_err();
    assert_eq!(
        insert_error.to_string(),
        "storage error: unique index idx_users_email_unique constraint failed"
    );
    assert_eq!(storage.scan_rows(txn, "users").unwrap().len(), 1);

    storage.rollback(txn).unwrap();

    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();

    storage
        .insert_row(
            txn,
            "users",
            user_row(1, "alice", "alice@example.com", true),
        )
        .unwrap();
    storage
        .insert_row(
            txn,
            "users",
            user_row(2, "ally", "alice@example.com", false),
        )
        .unwrap();

    let backfill_error = storage
        .create_index(txn, "users", unique_email_index("idx_users_email_backfill"))
        .unwrap_err();
    assert_eq!(
        backfill_error.to_string(),
        "storage error: unique index idx_users_email_backfill constraint failed"
    );

    storage.rollback(txn).unwrap();
}

#[test]
fn storage_v2_backfills_multi_column_index_for_existing_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage
        .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
        .unwrap();
    storage
        .insert_row(txn, "users", user_row(2, "bob", "b@example.com", true))
        .unwrap();
    storage
        .create_index(
            txn,
            "users",
            IndexMeta {
                name: "idx_users_name_email".to_string(),
                columns: vec!["name".to_string(), "email".to_string()],
                decorated_columns: None,
                unique: false,
                predicate: None,
            },
        )
        .unwrap();
    let row_ids = storage
        .lookup_index(
            txn,
            "users",
            "idx_users_name_email",
            &[Value::from("alice"), Value::from("a@example.com")],
        )
        .unwrap();
    assert_eq!(row_ids, vec![RowId(1)]);
}

#[test]
fn storage_v2_rejects_duplicate_index_names_on_same_table() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    let index = IndexMeta {
        name: "idx_users_name".to_string(),
        columns: vec!["name".to_string()],
        decorated_columns: None,
        unique: false,
        predicate: None,
    };
    storage.create_index(txn, "users", index.clone()).unwrap();
    let error = storage.create_index(txn, "users", index).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("index idx_users_name already exists")
    );
}

#[test]
fn storage_v2_updates_multi_column_indexes_on_insert_and_delete() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage
        .create_index(
            txn,
            "users",
            IndexMeta {
                name: "idx_users_name_email".to_string(),
                columns: vec!["name".to_string(), "email".to_string()],
                decorated_columns: None,
                unique: false,
                predicate: None,
            },
        )
        .unwrap();
    let row_id = storage
        .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
        .unwrap();

    assert_eq!(
        storage
            .lookup_index(
                txn,
                "users",
                "idx_users_name_email",
                &[Value::from("alice"), Value::from("a@example.com")],
            )
            .unwrap(),
        vec![row_id]
    );

    storage.delete_row(txn, "users", row_id).unwrap();
    assert!(
        storage
            .lookup_index(
                txn,
                "users",
                "idx_users_name_email",
                &[Value::from("alice"), Value::from("a@example.com")],
            )
            .unwrap()
            .is_empty()
    );
}

#[test]
fn storage_v2_sql_update_keeps_single_row_id_with_version_chain_and_same_index_entry() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Database::with_storage(FileStorage::open(&path).unwrap());

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT, active BOOLEAN);
         CREATE UNIQUE INDEX idx_users_email ON users(email);
         INSERT INTO users VALUES (1, 'alice', 'a@example.com', true);",
    )
    .unwrap();

    let storage = FileStorage::open(&path).unwrap();
    let row_id = {
        let txn = storage.begin().unwrap();
        let rows = storage.scan_rows(txn, "users").unwrap();
        storage.rollback(txn).unwrap();
        rows[0].0
    };

    let old_reader = storage
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    assert_eq!(
        storage.get_row(old_reader, "users", row_id).unwrap(),
        Some(user_row(1, "alice", "a@example.com", true))
    );

    db.execute("UPDATE users SET name = 'ally', email = 'ally@example.com' WHERE id = 1;")
        .unwrap();

    assert_eq!(
        storage.get_row(old_reader, "users", row_id).unwrap(),
        Some(user_row(1, "alice", "a@example.com", true))
    );

    let fresh_reader = storage.begin().unwrap();
    assert_eq!(
        storage.get_row(fresh_reader, "users", row_id).unwrap(),
        Some(user_row(1, "ally", "ally@example.com", true))
    );
    assert_eq!(
        storage
            .lookup_index(
                fresh_reader,
                "users",
                "idx_users_email",
                &[Value::from("ally@example.com")],
            )
            .unwrap(),
        vec![row_id]
    );
    assert!(
        storage
            .lookup_index(
                fresh_reader,
                "users",
                "idx_users_email",
                &[Value::from("a@example.com")],
            )
            .unwrap()
            .is_empty()
    );

    {
        let pager = Pager::open(&path).unwrap();
        let catalog = load_catalog(&pager).unwrap();
        let tree = BTree::from_root(catalog.table_roots["users"]);
        let stored = tree.get(&pager, 0, row_id.0).unwrap().unwrap();
        let versioned = rustsql::storage::v2::codec::decode_versioned_row(&stored).unwrap();
        assert_eq!(tree.scan_all(&pager, 0).unwrap().len(), 1);
        assert_eq!(versioned.versions.len(), 2);
        assert_eq!(
            versioned.versions[0].row,
            user_row(1, "ally", "ally@example.com", true)
        );
        assert_eq!(
            versioned.versions[1].row,
            user_row(1, "alice", "a@example.com", true)
        );
    }

    storage.rollback(old_reader).unwrap();
    storage.rollback(fresh_reader).unwrap();
}

#[test]
fn storage_v2_sql_update_purge_compacts_old_version_in_place_after_reader_finishes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Database::with_storage(FileStorage::open(&path).unwrap());

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT, active BOOLEAN);
         INSERT INTO users VALUES (1, 'alice', 'a@example.com', true);",
    )
    .unwrap();

    let storage = FileStorage::open(&path).unwrap();
    let row_id = {
        let txn = storage.begin().unwrap();
        let rows = storage.scan_rows(txn, "users").unwrap();
        storage.rollback(txn).unwrap();
        rows[0].0
    };

    let old_reader = storage
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    assert!(
        storage
            .get_row(old_reader, "users", row_id)
            .unwrap()
            .is_some()
    );

    db.execute("UPDATE users SET name = 'ally' WHERE id = 1;")
        .unwrap();

    {
        let pager = Pager::open(&path).unwrap();
        let catalog = load_catalog(&pager).unwrap();
        let tree = BTree::from_root(catalog.table_roots["users"]);
        let stored = tree.get(&pager, 0, row_id.0).unwrap().unwrap();
        let versioned = rustsql::storage::v2::codec::decode_versioned_row(&stored).unwrap();
        assert_eq!(versioned.versions.len(), 2);
    }

    storage.rollback(old_reader).unwrap();

    {
        let pager = Pager::open(&path).unwrap();
        let catalog = load_catalog(&pager).unwrap();
        let tree = BTree::from_root(catalog.table_roots["users"]);
        let stored = tree.get(&pager, 0, row_id.0).unwrap().unwrap();
        let versioned = rustsql::storage::v2::codec::decode_versioned_row(&stored).unwrap();
        assert_eq!(tree.scan_all(&pager, 0).unwrap().len(), 1);
        assert_eq!(versioned.versions.len(), 1);
        assert_eq!(
            versioned.versions[0].row,
            user_row(1, "ally", "a@example.com", true)
        );
    }
}

#[test]
fn storage_v2_lookup_index_rejects_wrong_key_arity() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage
        .create_index(
            txn,
            "users",
            IndexMeta {
                name: "idx_users_name_email".to_string(),
                columns: vec!["name".to_string(), "email".to_string()],
                decorated_columns: None,
                unique: false,
                predicate: None,
            },
        )
        .unwrap();
    let error = storage
        .lookup_index(
            txn,
            "users",
            "idx_users_name_email",
            &[Value::from("alice")],
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("expected 2 key values but got 1")
    );
}

#[test]
fn storage_v2_scans_index_by_composite_prefix() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage
        .create_index(
            txn,
            "users",
            IndexMeta {
                name: "idx_users_active_name".to_string(),
                columns: vec!["active".to_string(), "name".to_string()],
                decorated_columns: None,
                unique: false,
                predicate: None,
            },
        )
        .unwrap();
    let alice_true = storage
        .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
        .unwrap();
    storage
        .insert_row(txn, "users", user_row(2, "alice", "b@example.com", false))
        .unwrap();
    let bob_true = storage
        .insert_row(txn, "users", user_row(3, "bob", "c@example.com", true))
        .unwrap();

    let row_ids = storage
        .scan_index_prefix(
            txn,
            "users",
            "idx_users_active_name",
            &[Value::Boolean(true)],
        )
        .unwrap();

    assert_eq!(row_ids, vec![alice_true, bob_true]);
}

#[test]
fn storage_v2_scans_index_by_prefix_with_range_bound() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage
        .create_index(
            txn,
            "users",
            IndexMeta {
                name: "idx_users_active_name".to_string(),
                columns: vec!["active".to_string(), "name".to_string()],
                decorated_columns: None,
                unique: false,
                predicate: None,
            },
        )
        .unwrap();
    storage
        .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
        .unwrap();
    let bob_true = storage
        .insert_row(txn, "users", user_row(2, "bob", "b@example.com", true))
        .unwrap();
    storage
        .insert_row(txn, "users", user_row(3, "carol", "c@example.com", false))
        .unwrap();

    let row_ids = storage
        .scan_index_range(
            txn,
            "users",
            "idx_users_active_name",
            &[Value::Boolean(true)],
            Some((CompareOp::Gt, &Value::from("alice"))),
            None,
        )
        .unwrap();

    assert_eq!(row_ids, vec![bob_true]);
}

#[test]
fn storage_v2_scans_index_by_prefix_with_two_sided_range() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage
        .create_index(
            txn,
            "users",
            IndexMeta {
                name: "idx_users_active_name".to_string(),
                columns: vec!["active".to_string(), "name".to_string()],
                decorated_columns: None,
                unique: false,
                predicate: None,
            },
        )
        .unwrap();
    storage
        .insert_row(txn, "users", user_row(1, "alice", "a@example.com", true))
        .unwrap();
    let bob_true = storage
        .insert_row(txn, "users", user_row(2, "bob", "b@example.com", true))
        .unwrap();
    let carol_true = storage
        .insert_row(txn, "users", user_row(3, "carol", "c@example.com", true))
        .unwrap();
    storage
        .insert_row(txn, "users", user_row(4, "david", "d@example.com", true))
        .unwrap();

    let row_ids = storage
        .scan_index_range(
            txn,
            "users",
            "idx_users_active_name",
            &[Value::Boolean(true)],
            Some((CompareOp::Gt, &Value::from("alice"))),
            Some((CompareOp::Lt, &Value::from("david"))),
        )
        .unwrap();

    assert_eq!(row_ids, vec![bob_true, carol_true]);
}

#[test]
fn database_with_storage_v2_supports_create_index() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Database::with_storage(FileStorage::open(&path).unwrap());

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT, active BOOLEAN);",
    )
    .unwrap();
    db.execute("CREATE INDEX idx_users_name_email ON users (name, email);")
        .unwrap();
}
