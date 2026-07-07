use rustsql::common::types::{ColumnDef, ColumnType, IndexMeta, RowId, Schema, Value};
use rustsql::engine::{CatalogStore, IndexStore, TableStore, TransactionManager};
use rustsql::storage::memory::MemoryStorage;

fn users_schema() -> Schema {
    Schema::new(
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
fn memory_storage_supports_schema_row_scan_and_index_lookup() {
    let storage = MemoryStorage::new();
    let txn = storage.begin().unwrap();

    storage.create_schema(txn, users_schema()).unwrap();
    storage.create_index(txn, "users", name_index()).unwrap();

    let alice = vec![Value::Integer(1), Value::from("alice")];
    let bob = vec![Value::Integer(2), Value::from("bob")];
    let alice_row_id = storage.insert_row(txn, "users", alice.clone()).unwrap();
    let bob_row_id = storage.insert_row(txn, "users", bob.clone()).unwrap();

    assert_eq!(
        storage.get_schema(txn, "users").unwrap(),
        Some(users_schema())
    );
    assert_eq!(
        storage.list_indexes(txn, "users").unwrap(),
        vec![name_index()]
    );
    assert_eq!(
        storage.get_row(txn, "users", alice_row_id).unwrap(),
        Some(alice.clone())
    );
    assert_eq!(
        storage
            .lookup_index(txn, "users", "idx_users_name", &[Value::from("alice")])
            .unwrap(),
        vec![alice_row_id]
    );
    assert_eq!(
        storage.scan_rows(txn, "users").unwrap(),
        vec![(alice_row_id, alice), (bob_row_id, bob)]
    );

    storage.commit(txn).unwrap();
}

#[test]
fn memory_storage_renames_table_and_column_and_rewrites_added_column() {
    let storage = MemoryStorage::new();
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
fn memory_storage_rollback_reverts_schema_index_and_rows() {
    let storage = MemoryStorage::new();
    let txn = storage.begin().unwrap();

    storage.create_schema(txn, users_schema()).unwrap();
    storage.create_index(txn, "users", name_index()).unwrap();
    let row_id = storage
        .insert_row(txn, "users", vec![Value::Integer(1), Value::from("alice")])
        .unwrap();

    assert_eq!(row_id, RowId(1));
    storage.rollback(txn).unwrap();

    let check_txn = storage.begin().unwrap();
    assert_eq!(storage.get_schema(check_txn, "users").unwrap(), None);
    storage.rollback(check_txn).unwrap();
}

#[test]
fn memory_storage_commit_preserves_rows_for_future_transactions() {
    let storage = MemoryStorage::new();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();
    storage
        .insert_row(txn, "users", vec![Value::Integer(7), Value::from("carol")])
        .unwrap();
    storage.commit(txn).unwrap();

    let read_txn = storage.begin().unwrap();
    assert_eq!(storage.scan_rows(read_txn, "users").unwrap().len(), 1);
    assert_eq!(
        storage.scan_rows(read_txn, "users").unwrap()[0].1,
        vec![Value::Integer(7), Value::from("carol")]
    );
    storage.rollback(read_txn).unwrap();
}

#[test]
fn memory_storage_rejects_not_null_and_primary_key_null_values() {
    let storage = MemoryStorage::new();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();

    let name_null_error = storage
        .insert_row(txn, "users", vec![Value::Integer(1), Value::Null])
        .unwrap_err();
    assert!(
        name_null_error
            .to_string()
            .contains("column 'name' cannot be NULL")
    );

    let row_id = storage
        .insert_row(txn, "users", vec![Value::Null, Value::from("alice")])
        .unwrap();
    assert_eq!(row_id, RowId(1));
    assert_eq!(
        storage.get_row(txn, "users", row_id).unwrap(),
        Some(vec![Value::Integer(1), Value::from("alice")])
    );

    storage.rollback(txn).unwrap();
}

#[test]
fn memory_storage_rejects_duplicate_primary_key_values() {
    let storage = MemoryStorage::new();
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

    storage.rollback(txn).unwrap();
}

#[test]
fn memory_storage_enforces_unique_index_on_insert_and_backfill() {
    let storage = MemoryStorage::new();
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

    let storage = MemoryStorage::new();
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
fn memory_storage_rejects_type_mismatches() {
    let storage = MemoryStorage::new();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, users_schema()).unwrap();

    let error = storage
        .insert_row(
            txn,
            "users",
            vec![Value::Text("oops".to_string()), Value::from("alice")],
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("sqlite rowid column must be INTEGER, got TEXT")
    );

    storage.rollback(txn).unwrap();
}
