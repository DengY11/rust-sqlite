use rustsql::common::types::{ColumnDef, ColumnType, IndexMeta, Value};
use rustsql::engine::{CatalogStore, IndexStore, TableStore, TransactionManager};
use rustsql::storage::v1::FileStorage;
use tempfile::tempdir;

fn users_schema() -> rustsql::common::types::Schema {
    rustsql::common::types::Schema::new(
        "users",
        vec![
            ColumnDef::primary_key("id", ColumnType::Integer),
            ColumnDef::new("name", ColumnType::Text).nullable(false),
        ],
    )
}

fn name_index() -> IndexMeta {
    IndexMeta {
        name: "idx_users_name".to_string(),
        columns: vec!["name".to_string()],
        decorated_columns: None,
        unique: false,
        predicate: None,
    }
}

fn unique_name_index() -> IndexMeta {
    IndexMeta {
        name: "idx_users_name_unique".to_string(),
        columns: vec!["name".to_string()],
        decorated_columns: None,
        unique: true,
        predicate: None,
    }
}

#[test]
fn file_storage_reopen_preserves_schema_rows_and_index_entries() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let storage = FileStorage::open(&path).unwrap();
        let txn = storage.begin().unwrap();
        storage.create_schema(txn, users_schema()).unwrap();
        storage.create_index(txn, "users", name_index()).unwrap();
        storage
            .insert_row(txn, "users", vec![Value::Integer(1), Value::from("alice")])
            .unwrap();
        storage.commit(txn).unwrap();
    }

    let reopened = FileStorage::open(&path).unwrap();
    let txn = reopened.begin().unwrap();
    assert_eq!(
        reopened.get_schema(txn, "users").unwrap(),
        Some(users_schema())
    );
    assert_eq!(
        reopened.list_indexes(txn, "users").unwrap(),
        vec![name_index()]
    );
    assert_eq!(reopened.scan_rows(txn, "users").unwrap().len(), 1);
    assert_eq!(
        reopened
            .lookup_index(txn, "users", "idx_users_name", &[Value::from("alice")])
            .unwrap()
            .len(),
        1
    );
    reopened.rollback(txn).unwrap();
}

#[test]
fn file_storage_renames_table_and_column_and_rewrites_added_column() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage.create_index(txn, "users", name_index()).unwrap();
    storage
        .insert_row(txn, "users", vec![Value::Integer(1), Value::from("alice")])
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
    assert_eq!(schema.columns[2].name, "age");
    assert_eq!(
        storage.scan_rows(txn, "customers").unwrap()[0].1[2],
        Value::Integer(0)
    );
    assert_eq!(
        storage.list_indexes(txn, "customers").unwrap()[0].columns,
        vec!["full_name".to_string()]
    );
    storage.rollback(txn).unwrap();
}

#[test]
fn file_storage_rollback_discards_uncommitted_changes_after_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let storage = FileStorage::open(&path).unwrap();
        let txn = storage.begin().unwrap();
        storage.create_schema(txn, users_schema()).unwrap();
        storage.create_index(txn, "users", name_index()).unwrap();
        storage
            .insert_row(txn, "users", vec![Value::Integer(1), Value::from("alice")])
            .unwrap();
        storage.rollback(txn).unwrap();
    }

    let reopened = FileStorage::open(&path).unwrap();
    let txn = reopened.begin().unwrap();
    assert_eq!(reopened.get_schema(txn, "users").unwrap(), None);
    assert!(reopened.scan_rows(txn, "users").unwrap().is_empty());
    reopened.rollback(txn).unwrap();
}

#[test]
fn file_storage_commit_then_reopen_allows_index_lookup() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let storage = FileStorage::open(&path).unwrap();
        let txn = storage.begin().unwrap();
        storage.create_schema(txn, users_schema()).unwrap();
        storage.create_index(txn, "users", name_index()).unwrap();
        storage
            .insert_row(txn, "users", vec![Value::Integer(2), Value::from("bob")])
            .unwrap();
        storage.commit(txn).unwrap();
    }

    let reopened = FileStorage::open(&path).unwrap();
    let txn = reopened.begin().unwrap();
    let row_ids = reopened
        .lookup_index(txn, "users", "idx_users_name", &[Value::from("bob")])
        .unwrap();
    assert_eq!(row_ids.len(), 1);
    assert_eq!(
        reopened.get_row(txn, "users", row_ids[0]).unwrap(),
        Some(vec![Value::Integer(2), Value::from("bob")])
    );
    reopened.rollback(txn).unwrap();
}

#[test]
fn file_storage_rejects_duplicate_primary_key_and_persists_only_legal_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let storage = FileStorage::open(&path).unwrap();
        let txn = storage.begin().unwrap();
        storage.create_schema(txn, users_schema()).unwrap();
        storage
            .insert_row(txn, "users", vec![Value::Integer(1), Value::from("alice")])
            .unwrap();

        let error = storage
            .insert_row(txn, "users", vec![Value::Integer(1), Value::from("bob")])
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate primary key value for column 'id'")
        );

        storage.commit(txn).unwrap();
    }

    let reopened = FileStorage::open(&path).unwrap();
    let txn = reopened.begin().unwrap();
    assert_eq!(
        reopened.scan_rows(txn, "users").unwrap(),
        vec![(
            rustsql::common::types::RowId(1),
            vec![Value::Integer(1), Value::from("alice")],
        )]
    );
    reopened.rollback(txn).unwrap();
}

#[test]
fn file_storage_enforces_unique_index_on_insert_and_backfill() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage
        .create_index(txn, "users", unique_name_index())
        .unwrap();

    storage
        .insert_row(txn, "users", vec![Value::Integer(1), Value::from("alice")])
        .unwrap();

    let insert_error = storage
        .insert_row(txn, "users", vec![Value::Integer(2), Value::from("alice")])
        .unwrap_err();
    assert_eq!(
        insert_error.to_string(),
        "storage error: unique index idx_users_name_unique constraint failed"
    );
    assert_eq!(storage.scan_rows(txn, "users").unwrap().len(), 1);

    storage.rollback(txn).unwrap();

    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();

    storage
        .insert_row(txn, "users", vec![Value::Integer(1), Value::from("alice")])
        .unwrap();
    storage
        .insert_row(txn, "users", vec![Value::Integer(2), Value::from("alice")])
        .unwrap();

    let backfill_error = storage
        .create_index(
            txn,
            "users",
            IndexMeta {
                name: "idx_users_name_backfill".to_string(),
                columns: vec!["name".to_string()],
                decorated_columns: None,
                unique: true,
                predicate: None,
            },
        )
        .unwrap_err();
    assert_eq!(
        backfill_error.to_string(),
        "storage error: unique index idx_users_name_backfill constraint failed"
    );

    storage.rollback(txn).unwrap();
}

#[test]
fn file_storage_rejects_not_null_and_type_mismatches() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();

    let not_null_error = storage
        .insert_row(txn, "users", vec![Value::Integer(1), Value::Null])
        .unwrap_err();
    assert!(
        not_null_error
            .to_string()
            .contains("column 'name' cannot be NULL")
    );

    let type_error = storage
        .insert_row(
            txn,
            "users",
            vec![Value::Text("oops".to_string()), Value::from("alice")],
        )
        .unwrap_err();
    assert!(
        type_error
            .to_string()
            .contains("column 'id' expected INTEGER but got TEXT")
    );

    storage.rollback(txn).unwrap();
}
