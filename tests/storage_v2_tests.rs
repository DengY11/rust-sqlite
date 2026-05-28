use rustsql::common::types::{ColumnDef, ColumnType, Schema, Value};
use rustsql::db::Database;
use rustsql::engine::{CatalogStore, PlanningStorageEngine, TableStore, TransactionManager};
use rustsql::storage::v2::FileStorage;
use tempfile::tempdir;

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
        reopened.get_row(txn, "users", rustsql::common::types::RowId(1)).unwrap(),
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
        reopened.query("SELECT name FROM users WHERE id = 1;").unwrap(),
        vec![vec![Value::from("alice")]]
    );
}

#[test]
fn database_with_storage_v2_rejects_create_index_for_now() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Database::with_storage(FileStorage::open(&path).unwrap());

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);")
        .unwrap();
    let error = db
        .execute("CREATE INDEX idx_users_name ON users (name);")
        .unwrap_err();
    assert!(error.to_string().contains("secondary indexes"));
}
