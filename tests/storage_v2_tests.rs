use rustsql::common::types::{ColumnDef, ColumnType, IndexMeta, RowId, Schema, Value};
use rustsql::db::Database;
use rustsql::engine::{
    CatalogStore, IndexStore, PlanningStorageEngine, TableStore, TransactionManager,
};
use rustsql::sql::ast::CompareOp;
use rustsql::storage::v2::FileStorage;
use rustsql::storage::v2::catalog::{CatalogState, store_catalog};
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
                    unique: false,
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
                unique: false,
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
        unique: false,
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
                unique: false,
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
                unique: false,
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
                unique: false,
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
                unique: false,
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
                unique: false,
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
