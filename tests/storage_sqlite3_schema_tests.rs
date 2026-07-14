use std::process::Command;

use rustsql::db::Database;
use tempfile::tempdir;

#[test]
fn sqlite3_engine_lists_schema_from_real_sqlite_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); CREATE INDEX idx_users_name ON users(name);",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("users").unwrap();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "users");
    assert_eq!(indexes[0].name, "idx_users_name");
}

#[test]
fn sqlite3_engine_loads_typeless_columns() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("typeless-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id PRIMARY KEY,
                name,
                created_at DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(
        users.columns[0].column_type,
        rustsql::common::types::ColumnType::Any
    );
    assert_eq!(
        users.columns[1].column_type,
        rustsql::common::types::ColumnType::Any
    );
    assert_eq!(
        users.columns[2].column_type,
        rustsql::common::types::ColumnType::Any
    );
    assert_eq!(
        users.columns[2].default_value,
        Some(rustsql::common::types::ColumnDefault::CurrentTimestamp)
    );
}

#[test]
fn sqlite3_engine_loads_overflowed_sqlite_schema_records() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("overflow-schema.db");

    let mut columns = Vec::new();
    for n in 0..120 {
        columns.push(format!("c{n} TEXT"));
    }
    let create_sql = format!(
        "PRAGMA page_size = 512; VACUUM; CREATE TABLE bigschema (id INTEGER PRIMARY KEY, {});",
        columns.join(", ")
    );

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(&create_sql)
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "bigschema");
    assert_eq!(schemas[0].columns.len(), 121);
    assert_eq!(schemas[0].columns[0].name, "id");
    assert_eq!(schemas[0].columns[120].name, "c119");
}

#[test]
fn sqlite3_engine_loads_multi_page_sqlite_schema_btrees() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("multi-page-schema.db");

    let mut sql = String::from("PRAGMA page_size = 512; VACUUM;");
    for n in 0..80 {
        sql.push_str(&format!(
            "CREATE TABLE t{n} (id INTEGER PRIMARY KEY, name TEXT, value TEXT);"
        ));
        sql.push_str(&format!("CREATE INDEX idx_t{n}_name ON t{n}(name);"));
    }

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(&sql)
        .status()
        .unwrap();
    assert!(status.success());

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        bytes[100], 0x05,
        "expected sqlite_schema root page to be interior-table"
    );

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("t42").unwrap();

    assert_eq!(schemas.len(), 80);
    assert!(schemas.iter().any(|schema| schema.name == "t0"));
    assert!(schemas.iter().any(|schema| schema.name == "t79"));
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx_t42_name");
}

#[test]
fn sqlite3_engine_loads_indexes_declared_with_desc_sort_order() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("desc-index-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
             CREATE INDEX idx_users_name_desc ON users(name DESC);",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("users").unwrap();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "users");
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx_users_name_desc");
    assert_eq!(indexes[0].columns, vec!["name".to_string()]);
}

#[test]
fn sqlite3_engine_loads_collated_columns_and_indexes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("collate-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE);
             CREATE INDEX idx_users_name_nocase ON users(name COLLATE NOCASE DESC);",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("users").unwrap();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "users");
    assert_eq!(schemas[0].columns[1].name, "name");
    assert_eq!(
        schemas[0].columns[1].column_type,
        rustsql::common::types::ColumnType::Text
    );
    assert_eq!(schemas[0].columns[1].collation.as_deref(), Some("NOCASE"));
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx_users_name_nocase");
    assert_eq!(indexes[0].columns, vec!["name".to_string()]);
    assert_eq!(
        indexes[0].decorated_columns,
        Some(vec!["name COLLATE NOCASE DESC".to_string()])
    );
}

#[test]
fn sqlite3_engine_loads_default_then_collate_columns() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("default-then-collate-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                nickname TEXT DEFAULT ('guest') COLLATE NOCASE
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(
        users.columns[1].default_value,
        Some(rustsql::common::types::ColumnDefault::Literal(
            rustsql::common::types::Value::from("guest"),
        ))
    );
    assert_eq!(users.columns[1].collation.as_deref(), Some("NOCASE"));
}

#[test]
fn sqlite3_engine_loads_expression_indexes_but_hides_them_from_usable_index_list() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("expression-index-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
             CREATE INDEX idx_users_lower_name ON users(lower(name));",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("users").unwrap();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "users");
    assert!(indexes.is_empty());
}

#[test]
fn sqlite3_engine_loads_partial_indexes_but_hides_them_from_usable_index_list() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("partial-index-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, active INTEGER);
             CREATE INDEX idx_users_email_active ON users(email) WHERE active = 1;",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("users").unwrap();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "users");
    assert!(indexes.is_empty());
}

#[test]
fn sqlite3_engine_loads_current_timestamp_defaults() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("current-timestamp-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                created_at TEXT DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(
        users.columns[1].default_value,
        Some(rustsql::common::types::ColumnDefault::CurrentTimestamp)
    );
}

#[test]
fn sqlite3_engine_loads_current_date_and_time_defaults() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("current-date-time-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                created_date TEXT DEFAULT CURRENT_DATE,
                created_time TEXT DEFAULT CURRENT_TIME
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(
        users.columns[1].default_value,
        Some(rustsql::common::types::ColumnDefault::CurrentDate)
    );
    assert_eq!(
        users.columns[2].default_value,
        Some(rustsql::common::types::ColumnDefault::CurrentTime)
    );
}

#[test]
fn sqlite3_engine_loads_parenthesized_literal_defaults() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("parenthesized-defaults-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                visits INTEGER DEFAULT (0),
                nickname TEXT DEFAULT ('guest')
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(
        users.columns[1].default_value,
        Some(rustsql::common::types::ColumnDefault::Literal(
            rustsql::common::types::Value::Integer(0),
        ))
    );
    assert_eq!(
        users.columns[2].default_value,
        Some(rustsql::common::types::ColumnDefault::Literal(
            rustsql::common::types::Value::from("guest"),
        ))
    );
}

#[test]
fn sqlite3_engine_loads_autoincrement_table_schemas() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("autoincrement-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);
             INSERT INTO users(name) VALUES ('alice');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(users.columns.len(), 2);
    assert!(users.columns[0].primary_key);
    assert!(users.columns[0].autoincrement);
}

#[test]
fn sqlite3_engine_loads_desc_primary_key_table_schema() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("desc-primary-key-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg("CREATE TABLE users (id INTEGER PRIMARY KEY DESC, name TEXT);")
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(users.columns.len(), 2);
    assert!(users.columns[0].primary_key);
    assert_eq!(
        users.columns[0].column_type,
        rustsql::common::types::ColumnType::Integer
    );
    assert_eq!(
        users.columns[0].primary_key_sort_order,
        Some(rustsql::common::types::SortOrder::Desc)
    );
}

#[test]
fn sqlite3_engine_loads_sqlite_type_aliases_and_modifiers() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("type-alias-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INT PRIMARY KEY,
                name VARCHAR(255),
                slug CHAR(16),
                bio CLOB,
                visits BIGINT,
                amount NUMERIC(10,2),
                price DECIMAL(10,2)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(users.columns.len(), 7);
    assert_eq!(
        users.columns[0].column_type,
        rustsql::common::types::ColumnType::Integer
    );
    assert_eq!(
        users.columns[1].column_type,
        rustsql::common::types::ColumnType::Text
    );
    assert_eq!(
        users.columns[2].column_type,
        rustsql::common::types::ColumnType::Text
    );
    assert_eq!(
        users.columns[3].column_type,
        rustsql::common::types::ColumnType::Text
    );
    assert_eq!(
        users.columns[4].column_type,
        rustsql::common::types::ColumnType::Integer
    );
    assert_eq!(
        users.columns[5].column_type,
        rustsql::common::types::ColumnType::Numeric
    );
    assert_eq!(
        users.columns[6].column_type,
        rustsql::common::types::ColumnType::Numeric
    );
}

#[test]
fn sqlite3_engine_loads_unique_constraints_and_autoindexes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("unique-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                email TEXT UNIQUE,
                username TEXT,
                UNIQUE(username)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("users").unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert!(users.columns[1].unique);
    assert_eq!(
        users.unique_constraints,
        vec![
            rustsql::common::types::UniqueConstraint::new(vec!["username".to_string(),])
                .with_decorated_columns(vec!["username".to_string()])
        ]
    );
    assert_eq!(indexes.len(), 2);
    assert!(indexes.iter().all(|index| index.unique));
    assert!(
        indexes
            .iter()
            .any(|index| index.columns == vec!["email".to_string()])
    );
    assert!(
        indexes
            .iter()
            .any(|index| index.columns == vec!["username".to_string()])
    );
}

#[test]
fn sqlite3_engine_loads_composite_primary_key_tables() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("composite-primary-key-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                role TEXT,
                PRIMARY KEY(user_id, group_id)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let memberships = schemas
        .iter()
        .find(|schema| schema.name == "memberships")
        .unwrap();

    assert_eq!(memberships.columns.len(), 3);
    assert!(memberships.columns[0].primary_key);
    assert!(memberships.columns[1].primary_key);
    assert!(!memberships.columns[2].primary_key);
}

#[test]
fn sqlite3_engine_loads_named_composite_primary_key_constraints() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-composite-primary-key-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                role TEXT,
                CONSTRAINT pk_memberships PRIMARY KEY(user_id, group_id)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let memberships = schemas
        .iter()
        .find(|schema| schema.name == "memberships")
        .unwrap();

    assert_eq!(memberships.columns.len(), 3);
    assert!(memberships.columns[0].primary_key);
    assert!(memberships.columns[1].primary_key);
    assert_eq!(
        memberships
            .primary_key_constraint
            .as_ref()
            .and_then(|constraint| constraint.constraint_name.as_deref()),
        Some("pk_memberships")
    );
    assert_eq!(
        memberships
            .primary_key_constraint
            .as_ref()
            .map(|constraint| constraint.columns.clone()),
        Some(vec!["user_id".to_string(), "group_id".to_string()])
    );
}

#[test]
fn sqlite3_engine_loads_strict_tables() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("strict-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                name TEXT
            ) STRICT;",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert!(users.strict);
}

#[test]
fn sqlite3_engine_loads_without_rowid_table_schema() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("without-rowid-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                PRIMARY KEY(user_id, group_id)
            ) WITHOUT ROWID;",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let memberships = schemas
        .iter()
        .find(|schema| schema.name == "memberships")
        .unwrap();

    assert!(memberships.without_rowid);
}

#[test]
fn sqlite3_engine_opens_database_containing_without_rowid_table_and_reads_rowid_tables() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("without-rowid-mixed-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                PRIMARY KEY(user_id, group_id)
            ) WITHOUT ROWID;
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');
             INSERT INTO logs VALUES (2, 'after');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let rows = db.query("SELECT id, note FROM logs ORDER BY id;").unwrap();

    assert_eq!(
        rows,
        vec![
            vec![
                rustsql::common::types::Value::Integer(1),
                rustsql::common::types::Value::from("before"),
            ],
            vec![
                rustsql::common::types::Value::Integer(2),
                rustsql::common::types::Value::from("after"),
            ],
        ]
    );
}

#[test]
fn sqlite3_engine_loads_stored_generated_columns() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("generated-column-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER GENERATED ALWAYS AS (base + 1) STORED
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let metrics = schemas
        .iter()
        .find(|schema| schema.name == "metrics")
        .unwrap();

    assert_eq!(metrics.columns.len(), 2);
    assert_eq!(metrics.columns[1].name, "plus_one");
    assert_eq!(
        metrics.columns[1].generated_expr.as_deref(),
        Some("base + 1")
    );
    assert!(metrics.columns[1].generated_stored);
}

#[test]
fn sqlite3_engine_loads_virtual_generated_columns() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("generated-column-virtual-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER GENERATED ALWAYS AS (base + 1) VIRTUAL
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let metrics = schemas
        .iter()
        .find(|schema| schema.name == "metrics")
        .unwrap();

    assert_eq!(metrics.columns.len(), 2);
    assert_eq!(metrics.columns[1].name, "plus_one");
    assert_eq!(
        metrics.columns[1].generated_expr.as_deref(),
        Some("base + 1")
    );
    assert!(!metrics.columns[1].generated_stored);
}

#[test]
fn sqlite3_engine_loads_implicit_virtual_generated_columns() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("generated-column-implicit-virtual-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER GENERATED ALWAYS AS (base + 1)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let metrics = schemas
        .iter()
        .find(|schema| schema.name == "metrics")
        .unwrap();

    assert_eq!(metrics.columns.len(), 2);
    assert_eq!(metrics.columns[1].name, "plus_one");
    assert_eq!(
        metrics.columns[1].generated_expr.as_deref(),
        Some("base + 1")
    );
    assert!(!metrics.columns[1].generated_stored);
}

#[test]
fn sqlite3_engine_loads_as_generated_columns() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("generated-column-as-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER AS (base + 1)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let metrics = schemas
        .iter()
        .find(|schema| schema.name == "metrics")
        .unwrap();

    assert_eq!(metrics.columns.len(), 2);
    assert_eq!(metrics.columns[1].name, "plus_one");
    assert_eq!(
        metrics.columns[1].generated_expr.as_deref(),
        Some("base + 1")
    );
    assert!(!metrics.columns[1].generated_stored);
    assert!(!metrics.columns[1].generated_always_explicit);
}

#[test]
fn sqlite3_engine_loads_named_column_constraints() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-column-constraints-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER CONSTRAINT pk PRIMARY KEY,
                age INTEGER CONSTRAINT age_nonneg CHECK (age >= 0),
                email TEXT CONSTRAINT uq UNIQUE,
                user_id INTEGER CONSTRAINT fk REFERENCES accounts(id)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert!(users.columns[0].primary_key);
    assert_eq!(
        users.columns[0].primary_key_constraint_name.as_deref(),
        Some("pk")
    );
    assert_eq!(users.columns[1].checks.len(), 1);
    assert_eq!(users.columns[1].checks[0].name, "age_nonneg");
    assert!(users.columns[2].unique);
    assert_eq!(
        users.columns[2].unique_constraint_name.as_deref(),
        Some("uq")
    );
    assert_eq!(
        users.columns[3].foreign_key,
        Some(
            rustsql::common::types::ForeignKey::single_column("user_id", "accounts", "id")
                .named("fk"),
        )
    );
}

#[test]
fn sqlite3_engine_loads_named_column_primary_key_and_unique_constraints() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-column-pk-unique-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER CONSTRAINT pk PRIMARY KEY,
                email TEXT CONSTRAINT uq UNIQUE
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(
        users.columns[0].primary_key_constraint_name.as_deref(),
        Some("pk")
    );
    assert_eq!(
        users.columns[1].unique_constraint_name.as_deref(),
        Some("uq")
    );
}

#[test]
fn sqlite3_engine_loads_named_not_null_constraints() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-not-null-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                name TEXT CONSTRAINT nn NOT NULL
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert!(!users.columns[1].nullable);
    assert_eq!(
        users.columns[1].not_null_constraint_name.as_deref(),
        Some("nn")
    );
}

#[test]
fn sqlite3_engine_loads_named_check_constraints() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-check-constraints-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                age INTEGER CONSTRAINT age_nonneg CHECK (age >= 0),
                score INTEGER,
                CONSTRAINT score_cap CHECK (score <= 100)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(users.columns[1].checks.len(), 1);
    assert_eq!(users.columns[1].checks[0].name, "age_nonneg");
    assert_eq!(users.checks.len(), 1);
    assert_eq!(users.checks[0].name, "score_cap");
}

#[test]
fn sqlite3_engine_loads_on_conflict_constraints() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("on-conflict-constraints-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY ON CONFLICT REPLACE,
                email TEXT UNIQUE ON CONFLICT IGNORE,
                name TEXT NOT NULL ON CONFLICT FAIL,
                nickname TEXT,
                CONSTRAINT uq UNIQUE(name, nickname) ON CONFLICT ABORT
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(users.columns.len(), 4);
    assert!(users.columns[0].primary_key);
    assert!(users.columns[1].unique);
    assert!(!users.columns[2].nullable);
}

#[test]
fn sqlite3_engine_loads_preserved_on_conflict_clauses() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("on-conflict-preserved-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY ON CONFLICT REPLACE,
                email TEXT UNIQUE ON CONFLICT IGNORE,
                name TEXT CONSTRAINT nn NOT NULL ON CONFLICT FAIL,
                nickname TEXT,
                CONSTRAINT uq UNIQUE(name, nickname) ON CONFLICT ABORT
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(
        users.columns[0].primary_key_conflict_clause.as_deref(),
        Some("REPLACE")
    );
    assert_eq!(
        users.columns[1].unique_conflict_clause.as_deref(),
        Some("IGNORE")
    );
    assert_eq!(
        users.columns[2].not_null_constraint_name.as_deref(),
        Some("nn")
    );
    assert_eq!(
        users.columns[2].not_null_conflict_clause.as_deref(),
        Some("FAIL")
    );
    assert_eq!(
        users.unique_constraints[0].conflict_clause.as_deref(),
        Some("ABORT")
    );
}

#[test]
fn sqlite3_engine_loads_foreign_key_action_clauses() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("foreign-key-actions-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER REFERENCES users(id) ON DELETE CASCADE ON UPDATE RESTRICT,
                author_id INTEGER,
                FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE SET NULL ON UPDATE NO ACTION
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let posts = schemas
        .iter()
        .find(|schema| schema.name == "posts")
        .unwrap();

    assert_eq!(posts.columns.len(), 3);
    assert_eq!(
        posts.columns[1].foreign_key,
        Some(
            rustsql::common::types::ForeignKey::single_column("user_id", "users", "id",)
                .with_on_delete("CASCADE")
                .with_on_update("RESTRICT")
        )
    );
    assert_eq!(
        posts.foreign_keys[0],
        rustsql::common::types::ForeignKey::single_column("author_id", "users", "id")
            .with_on_delete("SET NULL")
            .with_on_update("NO ACTION")
    );
}

#[test]
fn sqlite3_engine_loads_foreign_keys_with_parent_primary_key_shorthand() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("foreign-key-parent-primary-key-shorthand.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY);
             CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER REFERENCES users
             );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let posts = schemas
        .iter()
        .find(|schema| schema.name == "posts")
        .unwrap();

    assert_eq!(
        posts.columns[1].foreign_key,
        Some(rustsql::common::types::ForeignKey::to_parent_primary_key(
            "user_id", "users",
        ))
    );
}

#[test]
fn sqlite3_engine_loads_deferrable_foreign_keys() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("deferrable-foreign-keys-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER REFERENCES users(id) DEFERRABLE INITIALLY DEFERRED,
                author_id INTEGER,
                FOREIGN KEY (author_id) REFERENCES users(id) NOT DEFERRABLE INITIALLY IMMEDIATE
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let posts = schemas
        .iter()
        .find(|schema| schema.name == "posts")
        .unwrap();

    assert_eq!(posts.columns.len(), 3);
    assert_eq!(
        posts.columns[1].foreign_key,
        Some(
            rustsql::common::types::ForeignKey::single_column("user_id", "users", "id",)
                .deferrable(true)
                .initially_deferred(true)
        )
    );
    assert_eq!(
        posts.foreign_keys[0],
        rustsql::common::types::ForeignKey::single_column("author_id", "users", "id")
            .deferrable(false)
            .initially_deferred(false)
    );
}

#[test]
fn sqlite3_engine_loads_foreign_key_match_clauses() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("foreign-key-match-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER REFERENCES users(id) MATCH FULL,
                author_id INTEGER,
                FOREIGN KEY (author_id) REFERENCES users(id) MATCH SIMPLE
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let posts = schemas
        .iter()
        .find(|schema| schema.name == "posts")
        .unwrap();

    assert_eq!(
        posts.columns[1].foreign_key,
        Some(
            rustsql::common::types::ForeignKey::single_column("user_id", "users", "id")
                .with_match("FULL")
        )
    );
    assert_eq!(
        posts.foreign_keys[0],
        rustsql::common::types::ForeignKey::single_column("author_id", "users", "id")
            .with_match("SIMPLE")
    );
}

#[test]
fn sqlite3_engine_loads_named_foreign_keys() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-foreign-keys-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER CONSTRAINT fk_user REFERENCES users(id),
                author_id INTEGER,
                CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let posts = schemas
        .iter()
        .find(|schema| schema.name == "posts")
        .unwrap();

    assert_eq!(
        posts.columns[1]
            .foreign_key
            .as_ref()
            .and_then(|foreign_key| foreign_key.constraint_name.as_deref()),
        Some("fk_user")
    );
    assert_eq!(
        posts.foreign_keys[0].constraint_name.as_deref(),
        Some("fk_author")
    );
}

#[test]
fn sqlite3_engine_loads_named_unique_constraints() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-unique-constraints-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                name TEXT,
                nickname TEXT,
                CONSTRAINT uq_user_names UNIQUE(name, nickname)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(users.unique_constraints.len(), 1);
    assert_eq!(
        users.unique_constraints[0].constraint_name.as_deref(),
        Some("uq_user_names")
    );
    assert_eq!(
        users.unique_constraints[0].columns,
        vec!["name".to_string(), "nickname".to_string()]
    );
}

#[test]
fn sqlite3_engine_loads_decorated_table_constraint_columns() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("decorated-table-constraints-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                name TEXT,
                email TEXT,
                CONSTRAINT uq UNIQUE(name COLLATE NOCASE DESC, email ASC),
                PRIMARY KEY(name COLLATE BINARY ASC)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(users.columns.len(), 2);
    assert_eq!(users.columns[0].name, "name");
    assert_eq!(users.columns[1].name, "email");
    assert_eq!(
        users.unique_constraints[0].columns,
        vec!["name".to_string(), "email".to_string()]
    );
    assert_eq!(
        users.unique_constraints[0].decorated_columns.clone(),
        Some(vec![
            "name COLLATE NOCASE DESC".to_string(),
            "email ASC".to_string(),
        ])
    );
    assert_eq!(
        users
            .primary_key_constraint
            .as_ref()
            .map(|constraint| constraint.columns.clone()),
        Some(vec!["name".to_string()])
    );
    assert_eq!(
        users
            .primary_key_constraint
            .as_ref()
            .and_then(|constraint| constraint.decorated_columns.clone()),
        Some(vec!["name COLLATE BINARY ASC".to_string()])
    );
}

#[test]
fn sqlite3_engine_loads_primary_key_on_conflict_clause() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("primary-key-on-conflict-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                name TEXT,
                email TEXT,
                PRIMARY KEY(name, email) ON CONFLICT FAIL
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(
        users
            .primary_key_constraint
            .as_ref()
            .and_then(|constraint| constraint.conflict_clause.as_deref()),
        Some("FAIL")
    );
}

#[test]
fn sqlite3_engine_loads_schemas_with_quoted_identifiers() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("quoted-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE \"users\" (\"id\" INTEGER PRIMARY KEY, \"name\" TEXT);
             CREATE INDEX \"idx_users_name\" ON \"users\"(\"name\");",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("users").unwrap();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "users");
    assert_eq!(schemas[0].columns[0].name, "id");
    assert_eq!(schemas[0].columns[1].name, "name");
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx_users_name");
}

#[test]
fn sqlite3_engine_matches_sqlite_catalog_behavior_for_if_not_exists_schema_sql() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("if-not-exists-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT);
             CREATE INDEX IF NOT EXISTS idx_users_name ON users(name);",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("users").unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();

    assert_eq!(users.name, "users");
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx_users_name");
}

#[test]
fn sqlite3_engine_loads_schemas_with_backtick_identifiers() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("backtick-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE `users` (`id` INTEGER PRIMARY KEY, `name` TEXT);
             CREATE INDEX `idx_users_name` ON `users`(`name`);",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("users").unwrap();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "users");
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx_users_name");
}

#[test]
fn sqlite3_engine_loads_schemas_with_bracket_identifiers() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bracket-schema.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE [users] ([id] INTEGER PRIMARY KEY, [name] TEXT);
             CREATE INDEX [idx_users_name] ON [users]([name]);",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let schemas = db.list_schemas().unwrap();
    let indexes = db.list_indexes("users").unwrap();

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "users");
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx_users_name");
}
