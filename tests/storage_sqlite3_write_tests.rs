use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustsql::common::types::Value;
use rustsql::db::Database;
use rustsql::engine::traits::{IndexStore, TransactionManager};
use tempfile::{TempDir, tempdir};

struct WritableSqliteFixture {
    _dir: TempDir,
    path: PathBuf,
}

fn writable_sqlite_fixture(file_name: &str) -> WritableSqliteFixture {
    let dir = tempdir().unwrap();
    let path = dir.path().join(file_name);
    WritableSqliteFixture { _dir: dir, path }
}

fn sqlite3_scalar(path: &Path, sql: &str) -> String {
    let output = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn assert_rejects_trigger_aggregate_like_sqlite(
    file_name: &str,
    aggregate_expr: &str,
    expected_error: &str,
) {
    let fixture = writable_sqlite_fixture(file_name);
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let sql = format!(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER, bonus INTEGER);
         CREATE TABLE audit(value REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT {aggregate_expr} FROM templates WHERE kind = 'admin' LIMIT 0;
         END;
         INSERT INTO templates VALUES ('admin', 3, 1);
         INSERT INTO users VALUES (1, 'admin');"
    );

    let error = db.execute(&sql).unwrap_err();
    assert!(
        error.to_string().contains(expected_error),
        "unexpected error: {error}"
    );
}

fn sqlite_page_size(bytes: &[u8]) -> usize {
    let raw = u16::from_be_bytes([bytes[16], bytes[17]]);
    if raw == 1 { 65_536 } else { usize::from(raw) }
}

fn sqlite_page_start(page_no: u32, page_size: usize) -> usize {
    usize::try_from(page_no - 1).unwrap() * page_size
}

fn sqlite_page_type(path: &Path, page_no: u32) -> u8 {
    let bytes = fs::read(path).unwrap();
    let page_size = sqlite_page_size(&bytes);
    bytes[sqlite_page_start(page_no, page_size)]
}

fn sqlite_index_root_page(path: &Path, index_name: &str) -> u32 {
    sqlite3_scalar(
        path,
        &format!(
            "SELECT rootpage FROM sqlite_master WHERE type = 'index' AND name = '{}';",
            index_name
        ),
    )
    .parse()
    .unwrap()
}

fn decode_sqlite_varint(bytes: &[u8]) -> (u64, usize) {
    let mut value = 0_u64;
    let mut consumed = 0_usize;
    for &byte in bytes.iter().take(8) {
        value = (value << 7) | u64::from(byte & 0x7f);
        consumed += 1;
        if byte & 0x80 == 0 {
            return (value, consumed);
        }
    }

    value = (value << 8) | u64::from(bytes[8]);
    (value, 9)
}

fn sqlite_index_max_local(page_size: usize) -> usize {
    (((page_size - 12) * 64) / 255) - 23
}

fn sqlite_file_has_overflowed_interior_index_cell(path: &Path) -> bool {
    let bytes = fs::read(path).unwrap();
    let page_size = sqlite_page_size(&bytes);
    let max_local = sqlite_index_max_local(page_size);
    let page_count = bytes.len() / page_size;

    for page_no in 1..=page_count {
        let start = (page_no - 1) * page_size;
        let page = &bytes[start..start + page_size];
        if page[0] != 0x02 {
            continue;
        }

        let cell_count = usize::from(u16::from_be_bytes([page[3], page[4]]));
        for cell_index in 0..cell_count {
            let pointer_offset = 12 + (cell_index * 2);
            let cell_offset = usize::from(u16::from_be_bytes([
                page[pointer_offset],
                page[pointer_offset + 1],
            ]));
            let (payload_size, _) = decode_sqlite_varint(&page[cell_offset + 4..]);
            if usize::try_from(payload_size).unwrap() > max_local {
                return true;
            }
        }
    }

    false
}

#[test]
fn rustsql_creates_sqlite_file_readable_by_sqlite3_cli() {
    let fixture = writable_sqlite_fixture("write-users.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');",
    )
    .unwrap();

    let cli_output = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(
        cli_output.status.success(),
        "sqlite3 CLI failed: status={:?}, stderr={}",
        cli_output.status.code(),
        String::from_utf8_lossy(&cli_output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&cli_output.stdout),
        "1|alice\n2|bob\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM users ORDER BY id;")
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn rustsql_persists_create_table_as_select_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("ctas-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         CREATE TABLE archive_users AS
         SELECT id, UPPER(name) AS name FROM users WHERE id >= 2;",
    )
    .unwrap();

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM archive_users ORDER BY id;",
    );
    assert_eq!(cli_rows, "2|BOB");

    let cli_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'archive_users';",
    );
    assert_eq!(cli_schema, "CREATE TABLE archive_users (id INT, name)");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM archive_users ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2), Value::from("BOB")]]);
}

#[test]
fn rustsql_persists_create_table_as_select_declared_types_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("ctas-declared-types-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (
            id INTEGER,
            amount NUMERIC(10,2),
            name TEXT,
            score REAL,
            payload BLOB,
            flag ANY,
            typeless
         );
         CREATE TABLE copied AS
         SELECT id, amount, name, score, payload, flag, typeless, UPPER(name) AS upper_name
         FROM users;",
    )
    .unwrap();

    let cli_table_info = sqlite3_scalar(
        &fixture.path,
        "SELECT cid || '|' || name || '|' || type FROM pragma_table_info('copied') ORDER BY cid;",
    );
    assert_eq!(
        cli_table_info,
        "0|id|INT\n1|amount|NUM\n2|name|TEXT\n3|score|REAL\n4|payload|\n5|flag|NUM\n6|typeless|\n7|upper_name|"
    );
}

#[test]
fn rustsql_persists_create_table_as_select_expression_declared_types_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("ctas-expression-declared-types-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER, name TEXT);
         CREATE TABLE copied AS
         SELECT +id AS plus_id,
                CAST(id AS TEXT) AS text_id,
                name COLLATE NOCASE AS c_name
         FROM users;",
    )
    .unwrap();

    let cli_table_info = sqlite3_scalar(
        &fixture.path,
        "SELECT cid || '|' || name || '|' || type FROM pragma_table_info('copied') ORDER BY cid;",
    );
    assert_eq!(cli_table_info, "0|plus_id|\n1|text_id|TEXT\n2|c_name|TEXT");
}

#[test]
fn rustsql_persists_create_table_as_values_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("ctas-values-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute("CREATE TABLE copied AS VALUES (1, 'alice'), (2, 'bob');")
        .unwrap();

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT column1 || '|' || column2 FROM copied ORDER BY column1;",
    );
    assert_eq!(cli_rows, "1|alice\n2|bob");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT column1, column2 FROM copied ORDER BY column1;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn rustsql_persists_create_table_as_with_values_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("ctas-with-values-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE picked AS
         WITH vals(id, name) AS (VALUES (2, 'bob'), (1, 'alice'))
         VALUES ((SELECT id FROM vals WHERE name = 'alice'), 'picked');",
    )
    .unwrap();

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT column1 || '|' || column2 FROM picked;",
    );
    assert_eq!(cli_rows, "1|picked");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT column1, column2 FROM picked;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("picked")]]);
}

#[test]
fn rustsql_persists_declared_type_for_any_and_typeless_columns_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("declared-type-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute("CREATE TABLE users (id ANY PRIMARY KEY, payload);")
        .unwrap();

    let cli_table_info = sqlite3_scalar(
        &fixture.path,
        "SELECT cid || '|' || name || '|' || type || '|' || pk FROM pragma_table_info('users') ORDER BY cid;",
    );
    assert_eq!(cli_table_info, "0|id|ANY|1\n1|payload||0");
}

#[test]
fn rustsql_persists_create_view_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("create-view-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (0, 'hidden');
         CREATE VIEW active_users AS SELECT id, name FROM users WHERE id > 0;",
    )
    .unwrap();

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM active_users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|alice");

    let cli_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT type || '|' || name || '|' || tbl_name || '|' || rootpage || '|' || sql \
         FROM sqlite_master WHERE type = 'view';",
    );
    assert_eq!(
        cli_schema,
        "view|active_users|active_users|0|CREATE VIEW active_users AS SELECT id, name FROM users WHERE id > 0"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM active_users ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);
}

#[test]
fn rustsql_persists_create_view_with_column_names_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("create-view-columns-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         CREATE VIEW renamed(uid, username) AS SELECT id, name FROM users;",
    )
    .unwrap();

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT uid || '|' || username FROM renamed;");
    assert_eq!(cli_rows, "1|alice");

    let cli_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'renamed';",
    );
    assert_eq!(
        cli_schema,
        "CREATE VIEW renamed(uid, username) AS SELECT id, name FROM users"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT uid, username FROM renamed;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);
}

#[test]
fn rustsql_persists_create_view_if_not_exists_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("create-view-if-not-exists-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         CREATE VIEW IF NOT EXISTS active_users AS SELECT id FROM users;
         CREATE VIEW IF NOT EXISTS active_users AS SELECT missing FROM no_such_table;",
    )
    .unwrap();

    let cli_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'active_users';",
    );
    assert_eq!(
        cli_schema,
        "CREATE VIEW active_users AS SELECT id FROM users"
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT id FROM active_users;");
    assert_eq!(cli_rows, "1");
}

#[test]
fn rustsql_persists_create_view_as_values_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("create-view-values-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute("CREATE VIEW literals AS VALUES (1, 'a'), (2, 'b');")
        .unwrap();

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT column1 || '|' || column2 FROM literals ORDER BY column1;",
    );
    assert_eq!(cli_rows, "1|a\n2|b");

    let cli_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT type || '|' || name || '|' || rootpage || '|' || sql \
         FROM sqlite_master WHERE type = 'view' AND name = 'literals';",
    );
    assert_eq!(
        cli_schema,
        "view|literals|0|CREATE VIEW literals AS SELECT * FROM (VALUES (1, 'a'), (2, 'b'))"
    );
}

#[test]
fn rustsql_persists_schema_qualified_main_ddl_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("schema-qualified-main-ddl-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE main.users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO main.users VALUES (1, 'alice');
         CREATE INDEX main.idx_users_name ON users(name);
         CREATE VIEW main.active_users AS SELECT id, name FROM users;",
    )
    .unwrap();

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM active_users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|alice");

    let cli_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT type || '|' || name || '|' || tbl_name || '|' || sql \
         FROM sqlite_master WHERE name IN ('users', 'idx_users_name', 'active_users') \
         ORDER BY type, name;",
    );
    assert_eq!(
        cli_schema,
        "index|idx_users_name|users|CREATE INDEX idx_users_name ON users (name)\n\
table|users|users|CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)\n\
view|active_users|active_users|CREATE VIEW active_users AS SELECT id, name FROM users"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM main.active_users;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);
}

#[test]
fn rustsql_reopens_sqlite_file_and_appends_rows() {
    let fixture = writable_sqlite_fixture("reopen-append.db");

    let first = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    first
        .execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO users VALUES (1, 'alice');",
        )
        .unwrap();

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    reopened
        .execute("INSERT INTO users VALUES (2, 'bob');")
        .unwrap();

    let cli_output = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_output.stdout),
        "1|alice\n2|bob\n"
    );

    let final_db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = final_db
        .query("SELECT id, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn rustsql_writes_blob_values_from_sql_literals() {
    let fixture = writable_sqlite_fixture("write-blob.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE files (id INTEGER PRIMARY KEY, payload BLOB);
         INSERT INTO files VALUES (1, X'0001FEFF');",
    )
    .unwrap();

    let cli_output = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, hex(payload) FROM files ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_output.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_output.stdout), "1|0001FEFF\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, payload FROM files ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::Blob(vec![0x00, 0x01, 0xfe, 0xff]),
        ]]
    );
}

#[test]
fn rustsql_supports_insert_on_conflict_do_nothing_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("insert-on-conflict-do-nothing.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (1, 'bob') ON CONFLICT DO NOTHING;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "1|alice\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);
}

#[test]
fn rustsql_supports_insert_on_conflict_target_do_nothing_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("insert-on-conflict-target-do-nothing.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );
         INSERT INTO users VALUES (1, 'a@example.com', 'alice');
         INSERT INTO users VALUES (1, 'b@example.com', 'bob') ON CONFLICT(id) DO NOTHING;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, email, name FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|a@example.com|alice\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, email, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("alice"),
        ]]
    );
}

#[test]
fn rustsql_supports_insert_select_on_conflict_do_nothing_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("insert-select-on-conflict-do-nothing.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE archive_users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO archive_users VALUES (1, 'existing');
         INSERT INTO archive_users
         SELECT id, name FROM users
         ON CONFLICT DO NOTHING;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM archive_users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|existing\n2|bob\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM archive_users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("existing")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn rustsql_supports_insert_select_on_conflict_target_do_nothing_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("insert-select-on-conflict-target-do-nothing.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE source_users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );
         CREATE TABLE archive_users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );
         INSERT INTO source_users VALUES (1, 'source@example.com', 'bob');
         INSERT INTO archive_users VALUES (1, 'archive@example.com', 'alice');
         INSERT INTO archive_users
         SELECT id, email, name FROM source_users
         ON CONFLICT(id) DO NOTHING;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, email, name FROM archive_users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|archive@example.com|alice\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, email, name FROM archive_users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("archive@example.com"),
            Value::from("alice"),
        ]]
    );
}

#[test]
fn rustsql_persists_blob_secondary_index_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("write-blob-index.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE files (id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
         INSERT INTO files VALUES (1, X'0001FEFF');
         INSERT INTO files VALUES (2, X'ABCD');
         INSERT INTO files VALUES (3, X'0001FE00');
         CREATE INDEX idx_files_payload ON files(payload);",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, hex(payload) FROM files WHERE payload = X'0001FEFF';")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "1|0001FEFF\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let plan = reopened
        .query("EXPLAIN QUERY PLAN SELECT id FROM files WHERE payload = X'0001FEFF';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=files index=idx_files_payload mode=lookup key_prefix=[X'0001FEFF']")
    );

    let rows = reopened
        .query("SELECT id FROM files WHERE payload = X'0001FEFF';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let storage = rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap();
    let txn = storage.begin().unwrap();
    let row_ids = storage
        .lookup_index(
            txn,
            "files",
            "idx_files_payload",
            &[Value::Blob(vec![0x00, 0x01, 0xfe, 0xff])],
        )
        .unwrap();
    storage.rollback(txn).unwrap();

    assert_eq!(row_ids, vec![rustsql::common::types::RowId(1)]);
}

#[test]
fn rustsql_supports_multi_row_insert_values() {
    let fixture = writable_sqlite_fixture("multi-row-insert.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice'), (2, 'bob');",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|alice\n2|bob\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn rustsql_persists_create_index_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("write-index.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         CREATE INDEX idx_users_name ON users(name);",
    )
    .unwrap();

    let cli_indexes = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name;")
        .output()
        .unwrap();

    assert!(cli_indexes.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_indexes.stdout),
        "idx_users_name\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let indexes = reopened.list_indexes("users").unwrap();
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx_users_name");
    assert_eq!(indexes[0].columns, vec!["name".to_string()]);

    let storage = rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap();
    let txn = storage.begin().unwrap();
    let row_ids = storage
        .lookup_index(txn, "users", "idx_users_name", &[Value::from("alice")])
        .unwrap();
    storage.rollback(txn).unwrap();

    assert_eq!(row_ids, vec![rustsql::common::types::RowId(1)]);
}

#[test]
fn rustsql_rewrites_sqlite_indexes_preserving_column_decorations() {
    let fixture = writable_sqlite_fixture("index-column-decorations-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE);
             CREATE INDEX idx_users_name_nocase ON users(name COLLATE NOCASE DESC);
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let index_sql = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_users_name_nocase';",
    );
    assert_eq!(
        index_sql,
        "CREATE INDEX idx_users_name_nocase ON users (name COLLATE NOCASE DESC)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_enforces_unique_index_on_create_and_insert() {
    let fixture = writable_sqlite_fixture("unique-index.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
         INSERT INTO users VALUES (1, 'a@example.com');
         INSERT INTO users VALUES (2, 'a@example.com');",
    )
    .unwrap();

    let create_error = db
        .execute("CREATE UNIQUE INDEX idx_users_email_unique ON users(email);")
        .unwrap_err();
    assert!(
        create_error
            .to_string()
            .contains("unique index idx_users_email_unique constraint failed"),
        "unexpected error: {create_error}"
    );

    let clean_fixture = writable_sqlite_fixture("unique-index-clean.db");
    let clean_db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&clean_fixture.path).unwrap(),
    );
    clean_db
        .execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
             INSERT INTO users VALUES (1, 'a@example.com');
             CREATE UNIQUE INDEX idx_users_email_unique ON users(email);",
        )
        .unwrap();

    let insert_error = clean_db
        .execute("INSERT INTO users VALUES (2, 'a@example.com');")
        .unwrap_err();
    assert!(
        insert_error
            .to_string()
            .contains("unique index idx_users_email_unique constraint failed"),
        "unexpected error: {insert_error}"
    );
}

#[test]
fn rustsql_enforces_unique_partial_index_on_create_and_insert() {
    let fixture = writable_sqlite_fixture("unique-partial-index.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, active INTEGER);
         INSERT INTO users VALUES (1, 'a@example.com', 1);
         INSERT INTO users VALUES (2, 'a@example.com', 0);",
    )
    .unwrap();

    db.execute(
        "CREATE UNIQUE INDEX idx_users_email_active_unique ON users(email) WHERE active = 1;",
    )
    .unwrap();

    let insert_error = db
        .execute("INSERT INTO users VALUES (3, 'a@example.com', 1);")
        .unwrap_err();
    assert!(
        insert_error
            .to_string()
            .contains("unique index idx_users_email_active_unique constraint failed"),
        "unexpected error: {insert_error}"
    );

    db.execute("INSERT INTO users VALUES (4, 'a@example.com', 0);")
        .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, email, active FROM users ORDER BY id;")
        .output()
        .unwrap();
    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|a@example.com|1\n2|a@example.com|0\n4|a@example.com|0\n"
    );
}

#[test]
fn rustsql_evaluates_glob_character_classes_in_expression_indexes() {
    let fixture = writable_sqlite_fixture("expression-index-glob-class.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, 'carol');
         INSERT INTO users VALUES (4, 'dave');
         CREATE INDEX idx_users_name_non_b ON users(name GLOB '[^b]*');",
    )
    .unwrap();

    let rows = db
        .query("SELECT id FROM users WHERE name GLOB '[^b]*' ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(3)],
            vec![Value::Integer(4)],
        ]
    );

    let storage = rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap();
    let txn = storage.begin().unwrap();
    let true_row_ids = storage
        .lookup_index(txn, "users", "idx_users_name_non_b", &[Value::Integer(1)])
        .unwrap();
    let false_row_ids = storage
        .lookup_index(txn, "users", "idx_users_name_non_b", &[Value::Integer(0)])
        .unwrap();
    storage.rollback(txn).unwrap();

    assert_eq!(
        true_row_ids,
        vec![
            rustsql::common::types::RowId(1),
            rustsql::common::types::RowId(3),
            rustsql::common::types::RowId(4),
        ]
    );
    assert_eq!(false_row_ids, vec![rustsql::common::types::RowId(2)]);
}

#[test]
fn rustsql_enforces_partial_index_with_glob_character_classes() {
    let fixture = writable_sqlite_fixture("partial-index-glob-class.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, name TEXT);
         INSERT INTO users VALUES (1, 'shared@example.com', 'alice');
         INSERT INTO users VALUES (2, 'shared@example.com', 'bob');
         CREATE UNIQUE INDEX idx_users_email_non_b ON users(email) WHERE name GLOB '[^b]*';",
    )
    .unwrap();

    let insert_error = db
        .execute("INSERT INTO users VALUES (3, 'shared@example.com', 'carol');")
        .unwrap_err();
    assert!(
        insert_error
            .to_string()
            .contains("unique index idx_users_email_non_b constraint failed"),
        "unexpected error: {insert_error}"
    );

    db.execute("INSERT INTO users VALUES (4, 'shared@example.com', 'brenda');")
        .unwrap();

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
            vec![Value::Integer(4), Value::from("brenda")],
        ]
    );
}

#[test]
fn rustsql_enforces_partial_index_with_like_predicate() {
    let fixture = writable_sqlite_fixture("partial-index-like.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, name TEXT);
         INSERT INTO users VALUES (1, 'shared@example.com', 'alice');
         INSERT INTO users VALUES (2, 'shared@example.com', 'bob');
         CREATE UNIQUE INDEX idx_users_email_a ON users(email) WHERE name LIKE 'a%';",
    )
    .unwrap();

    let insert_error = db
        .execute("INSERT INTO users VALUES (3, 'shared@example.com', 'alicia');")
        .unwrap_err();
    assert!(
        insert_error
            .to_string()
            .contains("unique index idx_users_email_a constraint failed"),
        "unexpected error: {insert_error}"
    );

    db.execute("INSERT INTO users VALUES (4, 'shared@example.com', 'brenda');")
        .unwrap();

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
            vec![Value::Integer(4), Value::from("brenda")],
        ]
    );
}

#[test]
fn rustsql_enforces_partial_index_with_dynamic_like_escape_like_sqlite() {
    let fixture = writable_sqlite_fixture("partial-index-dynamic-like-escape.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, name TEXT);
         INSERT INTO users VALUES (1, 'shared@example.com', 'a_');
         INSERT INTO users VALUES (2, 'shared@example.com', 'ab');
         CREATE UNIQUE INDEX idx_users_email_literal_a_ ON users(email)
             WHERE name LIKE 'a!_' ESCAPE ('!' || '');",
    )
    .unwrap();

    let insert_error = db
        .execute("INSERT INTO users VALUES (3, 'shared@example.com', 'a_');")
        .unwrap_err();
    assert!(
        insert_error
            .to_string()
            .contains("unique index idx_users_email_literal_a_ constraint failed"),
        "unexpected error: {insert_error}"
    );

    db.execute("INSERT INTO users VALUES (4, 'shared@example.com', 'ax');")
        .unwrap();

    let rows = db
        .query("SELECT id FROM users WHERE name LIKE 'a!_' ESCAPE ('!' || '') ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_enforces_partial_index_with_dynamic_like_pattern_like_sqlite() {
    let fixture = writable_sqlite_fixture("partial-index-dynamic-like-pattern.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, name TEXT);
         INSERT INTO users VALUES (1, 'shared@example.com', 'alice');
         INSERT INTO users VALUES (2, 'shared@example.com', 'bob');
         CREATE UNIQUE INDEX idx_users_email_dynamic_a ON users(email)
             WHERE name LIKE ('a' || '%');",
    )
    .unwrap();

    let insert_error = db
        .execute("INSERT INTO users VALUES (3, 'shared@example.com', 'alicia');")
        .unwrap_err();
    assert!(
        insert_error
            .to_string()
            .contains("unique index idx_users_email_dynamic_a constraint failed"),
        "unexpected error: {insert_error}"
    );

    db.execute("INSERT INTO users VALUES (4, 'shared@example.com', 'brenda');")
        .unwrap();

    let rows = db
        .query("SELECT id FROM users WHERE name LIKE ('a' || '%') ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_enforces_partial_index_with_dynamic_glob_pattern_like_sqlite() {
    let fixture = writable_sqlite_fixture("partial-index-dynamic-glob-pattern.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, name TEXT);
         INSERT INTO users VALUES (1, 'shared@example.com', 'alice');
         INSERT INTO users VALUES (2, 'shared@example.com', 'bob');
         CREATE UNIQUE INDEX idx_users_email_dynamic_glob_a ON users(email)
             WHERE name GLOB ('a' || '*');",
    )
    .unwrap();

    let insert_error = db
        .execute("INSERT INTO users VALUES (3, 'shared@example.com', 'alicia');")
        .unwrap_err();
    assert!(
        insert_error
            .to_string()
            .contains("unique index idx_users_email_dynamic_glob_a constraint failed"),
        "unexpected error: {insert_error}"
    );

    db.execute("INSERT INTO users VALUES (4, 'shared@example.com', 'brenda');")
        .unwrap();

    let rows = db
        .query("SELECT id FROM users WHERE name GLOB ('a' || '*') ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_enforces_partial_index_with_like_glob_text_coercion_like_sqlite() {
    let fixture = writable_sqlite_fixture("partial-index-like-glob-coercion.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, code ANY);
         INSERT INTO users VALUES (1, 'shared@example.com', 123);
         INSERT INTO users VALUES (2, 'shared@example.com', 456);
         CREATE UNIQUE INDEX idx_users_email_code_12 ON users(email)
             WHERE code LIKE '12%' OR code GLOB '12*';",
    )
    .unwrap();

    let insert_error = db
        .execute("INSERT INTO users VALUES (3, 'shared@example.com', X'313233');")
        .unwrap_err();
    assert!(
        insert_error
            .to_string()
            .contains("unique index idx_users_email_code_12 constraint failed"),
        "unexpected error: {insert_error}"
    );

    db.execute("INSERT INTO users VALUES (4, 'shared@example.com', 789);")
        .unwrap();

    let rows = db
        .query("SELECT id FROM users WHERE code LIKE '12%' OR code GLOB '12*' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_enforces_unique_partial_index_on_update() {
    let fixture = writable_sqlite_fixture("unique-partial-index-update.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, active INTEGER);
         INSERT INTO users VALUES (1, 'a@example.com', 1);
         INSERT INTO users VALUES (2, 'a@example.com', 0);
         CREATE UNIQUE INDEX idx_users_email_active_unique ON users(email) WHERE active = 1;",
    )
    .unwrap();

    let update_error = db
        .execute("UPDATE users SET active = 1 WHERE id = 2;")
        .unwrap_err();
    assert!(
        update_error
            .to_string()
            .contains("unique index idx_users_email_active_unique constraint failed"),
        "unexpected error: {update_error}"
    );

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, email, active FROM users ORDER BY id;")
        .output()
        .unwrap();
    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|a@example.com|1\n2|a@example.com|0\n"
    );
}

#[test]
fn rustsql_insert_or_replace_uses_unique_partial_index_conflicts() {
    let fixture = writable_sqlite_fixture("unique-partial-index-replace.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, active INTEGER);
         INSERT INTO users VALUES (1, 'a@example.com', 1);
         INSERT INTO users VALUES (2, 'a@example.com', 0);
         CREATE UNIQUE INDEX idx_users_email_active_unique ON users(email) WHERE active = 1;",
    )
    .unwrap();

    db.execute("INSERT OR REPLACE INTO users VALUES (3, 'a@example.com', 1);")
        .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, email, active FROM users ORDER BY id;")
        .output()
        .unwrap();
    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "2|a@example.com|0\n3|a@example.com|1\n"
    );
}

#[test]
fn rustsql_persists_unique_constraints_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("unique-constraint.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            email TEXT UNIQUE,
            username TEXT,
            UNIQUE(username)
         );
         INSERT INTO users VALUES (1, 'a@example.com', 'alice');",
    )
    .unwrap();

    let schema_output = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';")
        .output()
        .unwrap();

    assert!(schema_output.status.success());
    let schema_sql = String::from_utf8_lossy(&schema_output.stdout);
    assert!(schema_sql.contains("email TEXT UNIQUE"));
    assert!(schema_sql.contains("UNIQUE(username)"));

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let schema = reopened
        .list_schemas()
        .unwrap()
        .into_iter()
        .find(|schema| schema.name == "users")
        .unwrap();
    let indexes = reopened.list_indexes("users").unwrap();

    assert!(
        schema
            .columns
            .iter()
            .find(|column| column.name == "email")
            .unwrap()
            .unique
    );
    assert_eq!(
        schema.unique_constraints,
        vec![
            rustsql::common::types::UniqueConstraint::new(vec!["username".to_string(),])
                .with_decorated_columns(vec!["username".to_string()])
        ]
    );
    assert_eq!(indexes.len(), 2);
    assert!(indexes.iter().all(|index| index.unique));

    let error = reopened
        .execute("INSERT INTO users VALUES (2, 'a@example.com', 'bob');")
        .unwrap_err();
    assert!(error.to_string().contains("unique index"));
}

#[test]
fn rustsql_drops_index_and_table_from_sqlite_file() {
    let fixture = writable_sqlite_fixture("drop-ddl.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         CREATE INDEX idx_users_name ON users(name);",
    )
    .unwrap();
    db.execute("DROP INDEX idx_users_name;").unwrap();
    db.execute("DROP TABLE users;").unwrap();

    let cli_schema = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "SELECT type || '|' || name FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name;",
        )
        .output()
        .unwrap();

    assert!(cli_schema.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_schema.stdout), "");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    assert!(reopened.list_schemas().unwrap().is_empty());
    assert!(reopened.list_indexes("users").unwrap().is_empty());
}

#[test]
fn rustsql_updates_and_deletes_rows_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("update-delete.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 20);
         INSERT INTO users VALUES (2, 'bob', 30);
         UPDATE users SET name = 'bobby', age = 31 WHERE id = 2;
         DELETE FROM users WHERE id = 1;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name, age FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "2|bobby|31\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name, age FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(2),
            Value::from("bobby"),
            Value::Integer(31),
        ]]
    );
}

#[test]
fn rustsql_assigns_rowid_for_integer_primary_key_null_inserts() {
    let fixture = writable_sqlite_fixture("rowid-autoassign.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users(name) VALUES ('alice');
         INSERT INTO users VALUES (NULL, 'bob');",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|alice\n2|bob\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn rustsql_supports_insert_values_scalar_expressions_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("insert-values-expr.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, score INTEGER, created TEXT);
         INSERT INTO users VALUES
            (1 + 1, LOWER('ALICE'), 10 * 2, SUBSTR('2024-01-02', 1, 4)),
            (3, COALESCE(NULL, 'BOB'), ABS(-7), DATE('2024-01-02'));",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name, score, created FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "2|alice|20|2024\n3|BOB|7|2024-01-02\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name, score, created FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(2),
                Value::from("alice"),
                Value::Integer(20),
                Value::from("2024"),
            ],
            vec![
                Value::Integer(3),
                Value::from("BOB"),
                Value::Integer(7),
                Value::from("2024-01-02"),
            ],
        ]
    );
}

#[test]
fn rustsql_persists_autoincrement_primary_key_in_sqlite_schema() {
    let fixture = writable_sqlite_fixture("autoincrement-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);
         INSERT INTO users(name) VALUES ('alice');
         INSERT INTO users(name) VALUES ('bob');",
    )
    .unwrap();

    let cli_schema = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';")
        .output()
        .unwrap();

    assert!(cli_schema.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_schema.stdout).trim(),
        "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT)"
    );

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM users ORDER BY id;")
        .output()
        .unwrap();
    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|alice\n2|bob\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let schemas = reopened.list_schemas().unwrap();
    let users = schemas
        .iter()
        .find(|schema| schema.name == "users")
        .unwrap();
    assert!(users.columns[0].autoincrement);
}

#[test]
fn rustsql_persists_desc_primary_key_in_sqlite_schema() {
    let fixture = writable_sqlite_fixture("desc-primary-key-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY DESC, name TEXT);
         INSERT INTO users(name) VALUES ('alice');
         INSERT INTO users(id, name) VALUES (7, 'bob');",
    )
    .unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (id INTEGER PRIMARY KEY DESC, name TEXT)"
    );

    let autoindex_name = sqlite3_scalar(
        &fixture.path,
        "SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name;",
    );
    assert_eq!(autoindex_name, "sqlite_autoindex_users_1");

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT rowid || '|' || ifnull(CAST(id AS TEXT), 'NULL') || '|' || name FROM users ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "1|NULL|alice\n2|7|bob");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM users ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Null, Value::from("alice")],
            vec![Value::Integer(7), Value::from("bob")],
        ]
    );
}

#[test]
fn rustsql_queries_explicit_rowid_from_sqlite_storage() {
    let fixture = writable_sqlite_fixture("rowid-query.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE weird (id INTEGER PRIMARY KEY DESC, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (7, 'bob');
         INSERT INTO weird(name) VALUES ('carol');
         INSERT INTO weird(id, name) VALUES (9, 'dave');",
    )
    .unwrap();

    let user_rows = db
        .query("SELECT rowid, id, name FROM users ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        user_rows,
        vec![
            vec![Value::Integer(1), Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(7), Value::Integer(7), Value::from("bob")],
        ]
    );

    let weird_rows = db
        .query("SELECT rowid, id, name FROM weird ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        weird_rows,
        vec![
            vec![Value::Integer(1), Value::Null, Value::from("carol")],
            vec![Value::Integer(2), Value::Integer(9), Value::from("dave")],
        ]
    );
}

#[test]
fn rustsql_queries_rowid_aliases_and_shadowing_from_sqlite_storage() {
    let fixture = writable_sqlite_fixture("rowid-alias-query.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE shadowed (rowid INTEGER, name TEXT);
         CREATE TABLE oid_shadow (oid INTEGER, name TEXT);
         CREATE TABLE hidden_shadow (_rowid_ INTEGER, name TEXT);
         INSERT INTO users VALUES (7, 'bob');
         INSERT INTO shadowed VALUES (5, 'x');
         INSERT INTO oid_shadow VALUES (6, 'y');
         INSERT INTO hidden_shadow VALUES (8, 'z');",
    )
    .unwrap();

    let alias_rows = db
        .query("SELECT rowid, oid, _rowid_, id FROM users;")
        .unwrap();
    assert_eq!(
        alias_rows,
        vec![vec![
            Value::Integer(7),
            Value::Integer(7),
            Value::Integer(7),
            Value::Integer(7),
        ]]
    );

    let shadowed_rows = db
        .query("SELECT rowid, oid, _rowid_ FROM shadowed;")
        .unwrap();
    assert_eq!(
        shadowed_rows,
        vec![vec![
            Value::Integer(5),
            Value::Integer(1),
            Value::Integer(1)
        ]]
    );

    let oid_rows = db
        .query("SELECT rowid, oid, _rowid_ FROM oid_shadow;")
        .unwrap();
    assert_eq!(
        oid_rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(6),
            Value::Integer(1)
        ]]
    );

    let hidden_rows = db
        .query("SELECT rowid, oid, _rowid_ FROM hidden_shadow;")
        .unwrap();
    assert_eq!(
        hidden_rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(8)
        ]]
    );
}

#[test]
fn rustsql_does_not_expose_rowid_for_without_rowid_tables() {
    let fixture = writable_sqlite_fixture("without-rowid-no-rowid.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            role TEXT,
            PRIMARY KEY(user_id, group_id)
         ) WITHOUT ROWID;
         INSERT INTO memberships VALUES (1, 10, 'owner');",
    )
    .unwrap();

    let error = db
        .query("SELECT rowid, user_id FROM memberships;")
        .unwrap_err();
    assert!(
        error.to_string().contains("unknown column rowid"),
        "unexpected error: {error}"
    );

    let cli_error = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT rowid, user_id FROM memberships;")
        .output()
        .unwrap();
    assert!(!cli_error.status.success());
    assert!(
        String::from_utf8_lossy(&cli_error.stderr).contains("no such column: rowid"),
        "unexpected sqlite3 stderr: {}",
        String::from_utf8_lossy(&cli_error.stderr)
    );
}

#[test]
fn rustsql_queries_sqlite_master_and_schema_from_sqlite_storage() {
    let fixture = writable_sqlite_fixture("sqlite-master-query.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_name ON users(name);",
    )
    .unwrap();

    let master_rows = db
        .query(
            "SELECT type, name, tbl_name, sql
             FROM sqlite_master
             ORDER BY type, name;",
        )
        .unwrap();
    assert_eq!(
        master_rows,
        vec![
            vec![
                Value::from("index"),
                Value::from("idx_users_name"),
                Value::from("users"),
                Value::from("CREATE INDEX idx_users_name ON users(name)"),
            ],
            vec![
                Value::from("table"),
                Value::from("users"),
                Value::from("users"),
                Value::from("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"),
            ],
        ]
    );

    let schema_rows = db
        .query("SELECT type, name FROM sqlite_schema ORDER BY type, name;")
        .unwrap();
    assert_eq!(
        schema_rows,
        vec![
            vec![Value::from("index"), Value::from("idx_users_name")],
            vec![Value::from("table"), Value::from("users")],
        ]
    );
}

#[test]
fn rustsql_catalog_queries_include_partial_indexes_from_sqlite_storage() {
    let fixture = writable_sqlite_fixture("sqlite-master-partial-index-query.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, active INTEGER);
         CREATE INDEX idx_users_email_active ON users(email) WHERE active = 1;",
    )
    .unwrap();

    let master_rows = db
        .query(
            "SELECT type, name, tbl_name, sql
             FROM sqlite_master
             ORDER BY type, name;",
        )
        .unwrap();
    assert_eq!(
        master_rows,
        vec![
            vec![
                Value::from("index"),
                Value::from("idx_users_email_active"),
                Value::from("users"),
                Value::from("CREATE INDEX idx_users_email_active ON users(email) WHERE active = 1",),
            ],
            vec![
                Value::from("table"),
                Value::from("users"),
                Value::from("users"),
                Value::from(
                    "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, active INTEGER)",
                ),
            ],
        ]
    );

    let schema_rows = db
        .query("SELECT type, name FROM sqlite_schema ORDER BY type, name;")
        .unwrap();
    assert_eq!(
        schema_rows,
        vec![
            vec![Value::from("index"), Value::from("idx_users_email_active")],
            vec![Value::from("table"), Value::from("users")],
        ]
    );

    assert!(db.list_indexes("users").unwrap().is_empty());
}

#[test]
fn rustsql_catalog_queries_include_expression_indexes_from_sqlite_storage() {
    let fixture = writable_sqlite_fixture("sqlite-master-expression-index-query.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_lower_name ON users(lower(name));",
    )
    .unwrap();

    let master_rows = db
        .query(
            "SELECT type, name, tbl_name, sql
             FROM sqlite_master
             ORDER BY type, name;",
        )
        .unwrap();
    assert_eq!(
        master_rows,
        vec![
            vec![
                Value::from("index"),
                Value::from("idx_users_lower_name"),
                Value::from("users"),
                Value::from("CREATE INDEX idx_users_lower_name ON users(lower(name))"),
            ],
            vec![
                Value::from("table"),
                Value::from("users"),
                Value::from("users"),
                Value::from("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"),
            ],
        ]
    );

    let schema_rows = db
        .query("SELECT type, name FROM sqlite_schema ORDER BY type, name;")
        .unwrap();
    assert_eq!(
        schema_rows,
        vec![
            vec![Value::from("index"), Value::from("idx_users_lower_name")],
            vec![Value::from("table"), Value::from("users")],
        ]
    );

    assert!(db.list_indexes("users").unwrap().is_empty());
}

#[test]
fn rustsql_uses_expression_index_for_exact_scalar_predicate() {
    let fixture = writable_sqlite_fixture("expression-index-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_lower_name ON users(lower(name));
         INSERT INTO users VALUES (1, 'Alice');
         INSERT INTO users VALUES (2, 'Bob');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM users WHERE lower(name) = 'alice';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=users index=idx_users_lower_name mode=lookup key_prefix=[alice]")
    );

    let rows = db
        .query("SELECT id FROM users WHERE lower(name) = 'alice';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_overflowed_integer_addition_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-overflowed-addition.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value INTEGER);
         CREATE INDEX idx_metrics_value_plus_one ON metrics(value + 1);
         INSERT INTO metrics VALUES (1, 9223372036854775807);
         INSERT INTO metrics VALUES (2, 7);",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN
             SELECT id FROM metrics
             WHERE value + 1 = 9223372036854775808;",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE value + 1 = 9223372036854775808;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || (value + 1) || '|' || typeof(value + 1) FROM metrics ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|9.22337203685478e+18|real\n2|8|integer");
}

#[test]
fn rustsql_uses_expression_index_for_text_numeric_prefix_arithmetic_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-text-numeric-prefix-arithmetic.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value TEXT);
         CREATE INDEX idx_metrics_value_plus_two ON metrics(value + 2);
         INSERT INTO metrics VALUES (1, '5abc');
         INSERT INTO metrics VALUES (2, '5.5xyz');
         INSERT INTO metrics VALUES (3, 'abc');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE value + 2 = 7;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE value + 2 = 7 ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || (value + 2) || '|' || typeof(value + 2) FROM metrics ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|7|integer\n2|7.5|real\n3|2|integer");
}

#[test]
fn rustsql_uses_expression_index_for_unary_minus_text_prefix_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-unary-minus-text-prefix.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value TEXT);
         CREATE INDEX idx_metrics_negative_value ON metrics(-value);
         INSERT INTO metrics VALUES (1, '5abc');
         INSERT INTO metrics VALUES (2, '5.5xyz');
         INSERT INTO metrics VALUES (3, 'abc');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE -value = -5;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE -value = -5 ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || (-value) || '|' || typeof(-value) || '|' || (~value)
         FROM metrics ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|-5|integer|-6\n2|-5.5|real|-6\n3|0|integer|-1");
}

#[test]
fn rustsql_uses_expression_index_for_abs_round_text_prefix_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-abs-round-text-prefix.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value TEXT);
         CREATE INDEX idx_metrics_abs_value ON metrics(abs(value));
         CREATE INDEX idx_metrics_round_value ON metrics(round(value));
         INSERT INTO metrics VALUES (1, '5abc');
         INSERT INTO metrics VALUES (2, '5.5xyz');
         INSERT INTO metrics VALUES (3, 'abc');",
    )
    .unwrap();

    let abs_plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE abs(value) = 5.0;")
        .unwrap();
    assert_eq!(abs_plan.len(), 1);
    assert_eq!(abs_plan[0][0], Value::from("IndexScan"));

    let abs_rows = db
        .query("SELECT id FROM metrics WHERE abs(value) = 5.0 ORDER BY id;")
        .unwrap();
    assert_eq!(abs_rows, vec![vec![Value::Integer(1)]]);

    let round_rows = db
        .query("SELECT id FROM metrics WHERE round(value) = 6.0 ORDER BY id;")
        .unwrap();
    assert_eq!(round_rows, vec![vec![Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || abs(value) || '|' || typeof(abs(value)) || '|' ||
                round(value) || '|' || typeof(round(value))
         FROM metrics ORDER BY id;",
    );
    assert_eq!(
        cli_rows,
        "1|5.0|real|5.0|real\n2|5.5|real|6.0|real\n3|0.0|real|0.0|real"
    );
}

#[test]
fn rustsql_uses_expression_index_for_real_modulo_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-real-modulo.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value REAL);
         CREATE INDEX idx_metrics_value_mod ON metrics(value % 2);
         INSERT INTO metrics VALUES (1, 5.5);
         INSERT INTO metrics VALUES (2, 4.5);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE value % 2 = 1.0;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE value % 2 = 1.0 ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || (value % 2) || '|' || typeof(value % 2) FROM metrics ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|1.0|real\n2|0.0|real");
}

#[test]
fn rustsql_uses_expression_index_for_negative_shift_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-negative-shift.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value INTEGER);
         CREATE INDEX idx_metrics_shift_value ON metrics(value << -1);
         INSERT INTO metrics VALUES (1, 1);
         INSERT INTO metrics VALUES (2, 4);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE value << -1 = 0;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE value << -1 = 0 ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || (value << -1) || '|' || (value >> -1) FROM metrics ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|0|2\n2|2|8");
}

#[test]
fn rustsql_uses_partial_expression_index_when_filter_implies_predicate_like_sqlite() {
    let fixture = writable_sqlite_fixture("partial-expression-index-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active INTEGER);
         CREATE INDEX idx_users_active_lower_name ON users(lower(name)) WHERE active = 1;
         INSERT INTO users VALUES (1, 'Alice', 1);
         INSERT INTO users VALUES (2, 'Bob', 1);
         INSERT INTO users VALUES (3, 'Alice', 0);",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM users WHERE active = 1 AND lower(name) = lower('ALICE');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=users index=idx_users_active_lower_name mode=lookup key_prefix=[alice]")
    );

    let rows = db
        .query("SELECT id FROM users WHERE active = 1 AND lower(name) = lower('ALICE');")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_partial_expression_index_when_filter_implies_conjunctive_predicate_like_sqlite() {
    let fixture = writable_sqlite_fixture("partial-expression-index-conjunctive-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active INTEGER, tenant_id INTEGER);
         CREATE INDEX idx_users_active_tenant_lower_name ON users(lower(name)) WHERE active = 1 AND tenant_id = 7;
         INSERT INTO users VALUES (1, 'Alice', 1, 7);
         INSERT INTO users VALUES (2, 'Bob', 1, 7);
         INSERT INTO users VALUES (3, 'Alice', 1, 8);
         INSERT INTO users VALUES (4, 'Alice', 0, 7);",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM users WHERE active = 1 AND tenant_id = 7 AND lower(name) = lower('ALICE');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=users index=idx_users_active_tenant_lower_name mode=lookup key_prefix=[alice]"
        )
    );

    let rows = db
        .query(
            "SELECT id FROM users WHERE active = 1 AND tenant_id = 7 AND lower(name) = lower('ALICE');",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_partial_expression_index_when_filter_implies_is_null_predicate_like_sqlite() {
    let fixture = writable_sqlite_fixture("partial-expression-index-is-null-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, deleted_at TEXT);
         CREATE INDEX idx_users_live_lower_name ON users(lower(name)) WHERE deleted_at IS NULL;
         INSERT INTO users VALUES (1, 'Alice', NULL);
         INSERT INTO users VALUES (2, 'Bob', NULL);
         INSERT INTO users VALUES (3, 'Alice', '2024-01-01');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM users WHERE deleted_at IS NULL AND lower(name) = lower('ALICE');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=users index=idx_users_live_lower_name mode=lookup key_prefix=[alice]")
    );

    let rows = db
        .query("SELECT id FROM users WHERE deleted_at IS NULL AND lower(name) = lower('ALICE');")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_partial_expression_index_when_filter_implies_is_not_null_predicate_like_sqlite() {
    let fixture = writable_sqlite_fixture("partial-expression-index-is-not-null-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, deleted_at TEXT);
         CREATE INDEX idx_users_deleted_lower_name ON users(lower(name)) WHERE deleted_at IS NOT NULL;
         INSERT INTO users VALUES (1, 'Alice', NULL);
         INSERT INTO users VALUES (2, 'Alice', '2024-01-01');
         INSERT INTO users VALUES (3, 'Bob', '2024-01-02');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM users WHERE deleted_at IS NOT NULL AND lower(name) = lower('ALICE');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=users index=idx_users_deleted_lower_name mode=lookup key_prefix=[alice]"
        )
    );

    let rows = db
        .query(
            "SELECT id FROM users WHERE deleted_at IS NOT NULL AND lower(name) = lower('ALICE');",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_lower_on_real_values_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-lower-real-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading REAL);
         CREATE INDEX idx_metrics_lower_reading ON metrics(lower(reading));
         INSERT INTO metrics VALUES (1, 3.0);
         INSERT INTO metrics VALUES (2, 4.5);
         INSERT INTO metrics VALUES (3, 1e999);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE lower(reading) = '3.0';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_lower_reading mode=lookup key_prefix=[3.0]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE lower(reading) = '3.0';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let infinity_plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE lower(reading) = 'inf';")
        .unwrap();
    assert_eq!(infinity_plan.len(), 1);
    assert_eq!(infinity_plan[0][0], Value::from("IndexScan"));

    let infinity_rows = db
        .query("SELECT id FROM metrics WHERE lower(reading) = 'inf';")
        .unwrap();
    assert_eq!(infinity_rows, vec![vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_expression_indexes_for_text_coercing_scalar_functions_on_real_values() {
    let fixture = writable_sqlite_fixture("expression-index-real-text-funcs-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading REAL);
         CREATE INDEX idx_metrics_substr_reading ON metrics(substr(reading, 1, 3));
         CREATE INDEX idx_metrics_replace_reading ON metrics(replace(reading, '.', '_'));
         CREATE INDEX idx_metrics_trim_reading ON metrics(trim(reading));
         CREATE INDEX idx_metrics_unicode_reading ON metrics(unicode(reading));
         INSERT INTO metrics VALUES (1, 3.0);
         INSERT INTO metrics VALUES (2, 4.5);",
    )
    .unwrap();

    let substr_rows = db
        .query("SELECT id FROM metrics WHERE substr(reading, 1, 3) = '3.0';")
        .unwrap();
    assert_eq!(substr_rows, vec![vec![Value::Integer(1)]]);

    let replace_rows = db
        .query("SELECT id FROM metrics WHERE replace(reading, '.', '_') = '3_0';")
        .unwrap();
    assert_eq!(replace_rows, vec![vec![Value::Integer(1)]]);

    let trim_rows = db
        .query("SELECT id FROM metrics WHERE trim(reading) = '3.0';")
        .unwrap();
    assert_eq!(trim_rows, vec![vec![Value::Integer(1)]]);

    let unicode_rows = db
        .query("SELECT id FROM metrics WHERE unicode(reading) = 51;")
        .unwrap();
    assert_eq!(unicode_rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_scalar_is_null_predicate() {
    let fixture = writable_sqlite_fixture("expression-index-is-null-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_non_unknown_name ON users(nullif(name, 'unknown'));
         INSERT INTO users VALUES (1, 'unknown');
         INSERT INTO users VALUES (2, 'alice');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM users WHERE nullif(name, 'unknown') IS NULL;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=users index=idx_users_non_unknown_name mode=lookup key_prefix=[NULL]")
    );

    let rows = db
        .query("SELECT id FROM users WHERE nullif(name, 'unknown') IS NULL;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_nullif_mixed_numeric_types_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-nullif-mixed-numeric-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading INTEGER);
         CREATE INDEX idx_metrics_nullif_reading ON metrics(nullif(reading, 5.0));
         INSERT INTO metrics VALUES (1, 5);
         INSERT INTO metrics VALUES (2, 6);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE nullif(reading, 5.0) IS NULL;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_nullif_reading mode=lookup key_prefix=[NULL]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE nullif(reading, 5.0) IS NULL;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_case_mixed_numeric_compare_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-case-mixed-numeric-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading INTEGER);
         CREATE INDEX idx_metrics_case_reading
         ON metrics(CASE WHEN reading = 5.0 THEN 'eq' ELSE 'ne' END);
         INSERT INTO metrics VALUES (1, 5);
         INSERT INTO metrics VALUES (2, 6);",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN
             SELECT id FROM metrics
             WHERE CASE WHEN reading = 5.0 THEN 'eq' ELSE 'ne' END = 'eq';",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_case_reading mode=lookup key_prefix=[eq]")
    );

    let rows = db
        .query(
            "SELECT id FROM metrics
             WHERE CASE WHEN reading = 5.0 THEN 'eq' ELSE 'ne' END = 'eq';",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_abs_text_numeric_coercion_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-abs-text-numeric-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading TEXT);
         CREATE INDEX idx_metrics_abs_reading ON metrics(abs(reading));
         INSERT INTO metrics VALUES (1, '5.5');
         INSERT INTO metrics VALUES (2, 'abc');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE abs(reading) = 5.5;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_abs_reading mode=lookup key_prefix=[5.5]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE abs(reading) = 5.5;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let zero_rows = db
        .query("SELECT id FROM metrics WHERE abs(reading) = 0.0;")
        .unwrap();
    assert_eq!(zero_rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_cast_integer_clamp_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-cast-integer-clamp.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value TEXT);
         CREATE INDEX idx_metrics_cast_value_int ON metrics(CAST(value AS INTEGER));
         INSERT INTO metrics VALUES (1, '9223372036854775808');
         INSERT INTO metrics VALUES (2, '-9223372036854775809');
         INSERT INTO metrics VALUES (3, '123abc');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN
             SELECT id FROM metrics
             WHERE CAST(value AS INTEGER) = 9223372036854775807;",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE CAST(value AS INTEGER) = 9223372036854775807;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || CAST(value AS INTEGER) FROM metrics ORDER BY id;",
    );
    assert_eq!(
        cli_rows,
        "1|9223372036854775807\n2|-9223372036854775808\n3|123"
    );
}

#[test]
fn rustsql_uses_expression_index_for_length_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-length-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading REAL);
         CREATE INDEX idx_metrics_length_reading ON metrics(length(reading));
         INSERT INTO metrics VALUES (1, 3.0);
         INSERT INTO metrics VALUES (2, 12.5);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE length(reading) = 3;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_length_reading mode=lookup key_prefix=[3]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE length(reading) = 3;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_text_nul_length_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-length-text-nul.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading TEXT);
         CREATE INDEX idx_metrics_length_text_nul ON metrics(length(reading));
         INSERT INTO metrics VALUES (1, 'a' || char(0) || 'b');
         INSERT INTO metrics VALUES (2, 'ab');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE length(reading) = 1;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE length(reading) = 1;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_hex_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-hex-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading INTEGER);
         CREATE INDEX idx_metrics_hex_reading ON metrics(hex(reading));
         INSERT INTO metrics VALUES (1, 123);
         INSERT INTO metrics VALUES (2, 456);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE hex(reading) = '313233';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_hex_reading mode=lookup key_prefix=[313233]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE hex(reading) = '313233';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_sign_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-sign-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading);
         CREATE INDEX idx_metrics_sign_reading ON metrics(sign(reading));
         INSERT INTO metrics VALUES (1, 3.14);
         INSERT INTO metrics VALUES (2, -2.0);
         INSERT INTO metrics VALUES (3, 0.0);
         INSERT INTO metrics VALUES (4, '  -12');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE sign(reading) = 1;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_sign_reading mode=lookup key_prefix=[1]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE sign(reading) = 1;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let negative_rows = db
        .query("SELECT id FROM metrics WHERE sign(reading) = -1 ORDER BY id;")
        .unwrap();
    assert_eq!(
        negative_rows,
        vec![vec![Value::Integer(2)], vec![Value::Integer(4)]]
    );
}

#[test]
fn rustsql_uses_expression_index_for_round_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-round-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading REAL);
         CREATE INDEX idx_metrics_round_reading ON metrics(round(reading, 1));
         CREATE INDEX idx_metrics_round_negative_precision ON metrics(round(reading, -1));
         CREATE INDEX idx_metrics_round_large_precision ON metrics(round(reading, 400));
         INSERT INTO metrics VALUES (1, 3.14);
         INSERT INTO metrics VALUES (2, 2.71);
         INSERT INTO metrics VALUES (3, 1234.56);
         INSERT INTO metrics VALUES (4, 1.234567890123456);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE round(reading, 1) = 3.1;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_round_reading mode=lookup key_prefix=[3.1]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE round(reading, 1) = 3.1;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let negative_precision_plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE round(reading, -1) = 1235.0;")
        .unwrap();
    assert_eq!(negative_precision_plan.len(), 1);
    assert_eq!(negative_precision_plan[0][0], Value::from("IndexScan"));

    let negative_precision_rows = db
        .query("SELECT id FROM metrics WHERE round(reading, -1) = 1235.0;")
        .unwrap();
    assert_eq!(negative_precision_rows, vec![vec![Value::Integer(3)]]);

    let large_precision_plan = db
        .query(
            "EXPLAIN QUERY PLAN
             SELECT id FROM metrics WHERE round(reading, 400) = 1.234567890123456;",
        )
        .unwrap();
    assert_eq!(large_precision_plan.len(), 1);
    assert_eq!(large_precision_plan[0][0], Value::from("IndexScan"));

    let large_precision_rows = db
        .query("SELECT id FROM metrics WHERE round(reading, 400) = 1.234567890123456;")
        .unwrap();
    assert_eq!(large_precision_rows, vec![vec![Value::Integer(4)]]);
}

#[test]
fn rustsql_uses_expression_index_for_octet_length_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-octet-length-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading REAL);
         CREATE INDEX idx_metrics_octet_length_reading ON metrics(octet_length(reading));
         INSERT INTO metrics VALUES (1, 3.0);
         INSERT INTO metrics VALUES (2, 12.5);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE octet_length(reading) = 3;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=metrics index=idx_metrics_octet_length_reading mode=lookup key_prefix=[3]"
        )
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE octet_length(reading) = 3;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_char_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-char-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading TEXT);
         CREATE INDEX idx_metrics_char_reading ON metrics(char(reading));
         INSERT INTO metrics VALUES (1, '65');
         INSERT INTO metrics VALUES (2, '66');
         INSERT INTO metrics VALUES (3, '-1');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE char(reading) = 'A';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_char_reading mode=lookup key_prefix=[A]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE char(reading) = 'A';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let replacement_rows = db
        .query("SELECT id FROM metrics WHERE char(reading) = '�';")
        .unwrap();
    assert_eq!(replacement_rows, vec![vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_expression_index_for_zeroblob_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-zeroblob-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading TEXT);
         CREATE INDEX idx_metrics_zeroblob_reading ON metrics(length(zeroblob(reading)));
         INSERT INTO metrics VALUES (1, '3');
         INSERT INTO metrics VALUES (2, '4');
         INSERT INTO metrics VALUES (3, '-1');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE length(zeroblob(reading)) = 3;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_zeroblob_reading mode=lookup key_prefix=[3]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE length(zeroblob(reading)) = 3;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let empty_rows = db
        .query("SELECT id FROM metrics WHERE length(zeroblob(reading)) = 0;")
        .unwrap();
    assert_eq!(empty_rows, vec![vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_expression_index_for_likely_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-likely-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading INTEGER);
         CREATE INDEX idx_metrics_likely_reading ON metrics(likely(reading));
         INSERT INTO metrics VALUES (1, 1);
         INSERT INTO metrics VALUES (2, 2);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE likely(reading) = 1;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_likely_reading mode=lookup key_prefix=[1]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE likely(reading) = 1;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_indexes_for_math_scalar_functions_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-math-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, base REAL, value REAL, angle REAL);
         CREATE INDEX idx_metrics_sqrt_value ON metrics(sqrt(value));
         CREATE INDEX idx_metrics_log_base_value ON metrics(log(base, value));
         CREATE INDEX idx_metrics_degrees_angle ON metrics(degrees(angle));
         INSERT INTO metrics VALUES (1, 2.0, 8.0, 3.141592653589793);
         INSERT INTO metrics VALUES (2, 10.0, 1000.0, 1.5707963267948966);
         INSERT INTO metrics VALUES (3, 2.0, -1.0, NULL);",
    )
    .unwrap();

    let sqrt_plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE sqrt(value) = 2.8284271247461903;")
        .unwrap();
    assert_eq!(sqrt_plan.len(), 1);
    assert_eq!(sqrt_plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        sqrt_plan[0][1],
        Value::from(
            "table=metrics index=idx_metrics_sqrt_value mode=lookup key_prefix=[2.8284271247461903]"
        )
    );

    let sqrt_rows = db
        .query("SELECT id FROM metrics WHERE sqrt(value) = 2.8284271247461903;")
        .unwrap();
    assert_eq!(sqrt_rows, vec![vec![Value::Integer(1)]]);

    let log_plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE log(base, value) = 3.0;")
        .unwrap();
    assert_eq!(log_plan.len(), 1);
    assert_eq!(log_plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        log_plan[0][1],
        Value::from("table=metrics index=idx_metrics_log_base_value mode=lookup key_prefix=[3]")
    );

    let log_rows = db
        .query("SELECT id FROM metrics WHERE log(base, value) = 3.0 ORDER BY id;")
        .unwrap();
    assert_eq!(log_rows, vec![vec![Value::Integer(1)]]);

    let degrees_plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE degrees(angle) = 180.0;")
        .unwrap();
    assert_eq!(degrees_plan.len(), 1);
    assert_eq!(degrees_plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        degrees_plan[0][1],
        Value::from("table=metrics index=idx_metrics_degrees_angle mode=lookup key_prefix=[180]")
    );

    let degrees_rows = db
        .query("SELECT id FROM metrics WHERE degrees(angle) = 180.0;")
        .unwrap();
    assert_eq!(degrees_rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_concat_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-concat-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, a TEXT, b INTEGER);
         CREATE INDEX idx_metrics_concat_ab ON metrics(concat(a, b));
         INSERT INTO metrics VALUES (1, 'x', 1);
         INSERT INTO metrics VALUES (2, 'y', 2);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE concat(a, b) = 'x1';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_concat_ab mode=lookup key_prefix=[x1]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE concat(a, b) = 'x1';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_max_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-max-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, a INTEGER, b REAL);
         CREATE INDEX idx_metrics_max_ab ON metrics(max(a, b));
         INSERT INTO metrics VALUES (1, 1, 2.5);
         INSERT INTO metrics VALUES (2, 5, 4.0);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE max(a, b) = 5;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_max_ab mode=lookup key_prefix=[5]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE max(a, b) = 5 ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_min_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-min-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, a INTEGER, b REAL);
         CREATE INDEX idx_metrics_min_ab ON metrics(min(a, b));
         INSERT INTO metrics VALUES (1, 1, 2.5);
         INSERT INTO metrics VALUES (2, 5, 4.0);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE min(a, b) = 1;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_min_ab mode=lookup key_prefix=[1]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE min(a, b) = 1 ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_scalar_min_max_storage_class_order_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-min-max-storage-class-order.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value);
         CREATE INDEX idx_metrics_min_value ON metrics(min(value, 2));
         CREATE INDEX idx_metrics_max_value ON metrics(max(value, 2));
         INSERT INTO metrics VALUES (1, '10');
         INSERT INTO metrics VALUES (2, 10);",
    )
    .unwrap();

    let min_plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE min(value, 2) = 2;")
        .unwrap();
    assert_eq!(min_plan.len(), 1);
    assert_eq!(min_plan[0][0], Value::from("IndexScan"));

    let min_rows = db
        .query("SELECT id FROM metrics WHERE min(value, 2) = 2 ORDER BY id;")
        .unwrap();
    assert_eq!(
        min_rows,
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );

    let max_rows = db
        .query("SELECT id FROM metrics WHERE max(value, 2) = '10' ORDER BY id;")
        .unwrap();
    assert_eq!(max_rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
         CREATE INDEX idx_metrics_printf_ab ON metrics(printf('%s-%02d', b, a));
         INSERT INTO metrics VALUES (1, 1, 'x');
         INSERT INTO metrics VALUES (2, 2, 'y');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%s-%02d', b, a) = 'x-01';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_printf_ab mode=lookup key_prefix=[x-01]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%s-%02d', b, a) = 'x-01' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_quoted_strings_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-quoted-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_metrics_printf_quoted_name ON metrics(printf('%Q', name));
         INSERT INTO metrics VALUES (1, 'O''Reilly');
         INSERT INTO metrics VALUES (2, NULL);",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%Q', name) = '''O''''Reilly''';",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%Q', name) = 'NULL' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_infinity_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-infinity-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading REAL);
         CREATE INDEX idx_metrics_printf_infinity ON metrics(printf('%f', reading));
         INSERT INTO metrics VALUES (1, 1e999);
         INSERT INTO metrics VALUES (2, -1e999);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%f', reading) = 'Inf';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%f', reading) = 'Inf';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || printf('%f', reading) FROM metrics ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|Inf\n2|-Inf");
}

#[test]
fn rustsql_uses_expression_index_for_printf_unsupported_prefix_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-unsupported-prefix.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_metrics_printf_unsupported_prefix ON metrics(printf('a%Sb', name));
         INSERT INTO metrics VALUES (1, 'alice');
         INSERT INTO metrics VALUES (2, NULL);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('a%Sb', name) = 'a';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('a%Sb', name) = 'a' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(printf('a%Sb', name)) FROM metrics ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'a'\n2|'a'");
}

#[test]
fn rustsql_uses_expression_index_for_printf_unsigned_integer_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-unsigned-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, n INTEGER);
         CREATE INDEX idx_metrics_printf_unsigned ON metrics(printf('%u', n));
         INSERT INTO metrics VALUES (1, -1);
         INSERT INTO metrics VALUES (2, 15);",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%u', n) = '18446744073709551615';",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%08u', n) = '00000015' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_scientific_float_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-scientific-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, n REAL);
         CREATE INDEX idx_metrics_printf_scientific ON metrics(printf('%.2e', n));
         INSERT INTO metrics VALUES (1, 1234.5);
         INSERT INTO metrics VALUES (2, 3.5);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%.2e', n) = '1.23e+03';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%.2e', n) = '3.50e+00' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_grouped_integer_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-grouped-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, n INTEGER);
         CREATE INDEX idx_metrics_printf_grouped ON metrics(printf('%,d', n));
         INSERT INTO metrics VALUES (1, 1234567);
         INSERT INTO metrics VALUES (2, -1234567);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%,d', n) = '1,234,567';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%,d', n) = '-1,234,567' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_pointer_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-pointer-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, n INTEGER);
         CREATE INDEX idx_metrics_printf_pointer ON metrics(printf('%p', n));
         INSERT INTO metrics VALUES (1, 255);
         INSERT INTO metrics VALUES (2, NULL);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%p', n) = 'FF';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%p', n) = '0' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_ordinal_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-ordinal-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, n INTEGER);
         CREATE INDEX idx_metrics_printf_ordinal ON metrics(printf('%r', n));
         INSERT INTO metrics VALUES (1, 1);
         INSERT INTO metrics VALUES (2, 2);
         INSERT INTO metrics VALUES (3, 11);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%r', n) = '2nd';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%r', n) = '11th' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_character_precision_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-char-precision-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, label TEXT);
         CREATE INDEX idx_metrics_printf_char_precision ON metrics(printf('%.3c', label));
         INSERT INTO metrics VALUES (1, 'xray');
         INSERT INTO metrics VALUES (2, 'yellow');
         INSERT INTO metrics VALUES (3, '你好');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%.3c', label) = 'xxx';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%.3c', label) = '你你你' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_alternate_form_2_text_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-alt2-text-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, label TEXT);
         CREATE INDEX idx_metrics_printf_alt2_text ON metrics(printf('%!.2s', label));
         INSERT INTO metrics VALUES (1, '你好abc');
         INSERT INTO metrics VALUES (2, 'abcdef');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%!.2s', label) = '你好';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%.2s', label) = 'ab' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_unistr_quote_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-unistr-quote-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, label TEXT);
         CREATE INDEX idx_metrics_printf_unistr_quote ON metrics(printf('%#Q', label));
         INSERT INTO metrics VALUES (1, 'line' || char(10) || 'break');
         INSERT INTO metrics VALUES (2, char(1));",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN
             SELECT id FROM metrics
             WHERE printf('%#Q', label) = 'unistr(''line\\u000abreak'')';",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query(
            "SELECT id FROM metrics
             WHERE printf('%#q', label) = '\\u0001'
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_length_modifier_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-length-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, n INTEGER);
         CREATE INDEX idx_metrics_printf_length ON metrics(printf('%lld', n));
         INSERT INTO metrics VALUES (1, 7);
         INSERT INTO metrics VALUES (2, 8);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%lld', n) = '7';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%lx', n) = '8' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_json_pretty_null_indent_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-json-pretty-null-indent.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, doc TEXT);
         CREATE INDEX idx_metrics_json_pretty_null_indent ON metrics(json_pretty(doc, NULL));
         INSERT INTO metrics VALUES (1, '{\"a\":1}');
         INSERT INTO metrics VALUES (2, '{\"b\":2}');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN
             SELECT id FROM metrics WHERE json_pretty(doc, NULL) = '{\n    \"a\": 1\n}';",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query(
            "SELECT id FROM metrics
             WHERE json_pretty(doc, NULL) = '{\n    \"a\": 1\n}'
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_json_quoted_object_key_path_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-json-quoted-key-path.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, doc TEXT);
         CREATE UNIQUE INDEX idx_metrics_json_quoted_key
             ON metrics(json_extract(doc, '$.\"a.b\"'));
         INSERT INTO metrics VALUES (1, '{\"a.b\":7}');
         INSERT INTO metrics VALUES (2, '{\"a.b\":8}');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN
             SELECT id FROM metrics WHERE json_extract(doc, '$.\"a.b\"') = 7;",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query(
            "SELECT id FROM metrics
             WHERE json_extract(doc, '$.\"a.b\"') = 7
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let duplicate = db
        .execute("INSERT INTO metrics VALUES (3, '{\"a.b\":7}');")
        .unwrap_err();
    assert!(
        duplicate
            .to_string()
            .contains("unique index idx_metrics_json_quoted_key constraint failed"),
        "unexpected error: {duplicate}"
    );
}

#[test]
fn rustsql_uses_expression_index_for_json_extract_multiple_paths_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-json-extract-multiple-paths.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, doc TEXT);
         CREATE UNIQUE INDEX idx_metrics_json_multi_path
             ON metrics(json_extract(doc, '$.a', '$.b'));
         INSERT INTO metrics VALUES (1, '{\"a\":1,\"b\":2}');
         INSERT INTO metrics VALUES (2, '{\"a\":1,\"b\":3}');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN
             SELECT id FROM metrics WHERE json_extract(doc, '$.a', '$.b') = '[1,2]';",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query(
            "SELECT id FROM metrics
             WHERE json_extract(doc, '$.a', '$.b') = '[1,2]'
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let duplicate = db
        .execute("INSERT INTO metrics VALUES (3, '{\"a\":1,\"b\":2}');")
        .unwrap_err();
    assert!(
        duplicate
            .to_string()
            .contains("unique index idx_metrics_json_multi_path constraint failed"),
        "unexpected error: {duplicate}"
    );
}

#[test]
fn rustsql_uses_expression_index_for_printf_alternate_form_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-alternate-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, n INTEGER);
         CREATE INDEX idx_metrics_printf_alternate ON metrics(printf('%#x', n));
         INSERT INTO metrics VALUES (1, 255);
         INSERT INTO metrics VALUES (2, 8);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%#x', n) = '0xff';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%#o', n) = '010' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_printf_dynamic_width_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-printf-dynamic-width-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, width INTEGER, n INTEGER);
         CREATE INDEX idx_metrics_printf_dynamic_width ON metrics(printf('%0*d', width, n));
         INSERT INTO metrics VALUES (1, 4, 12);
         INSERT INTO metrics VALUES (2, 6, 12);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE printf('%0*d', width, n) = '0012';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE printf('%0*d', width, n) = '000012' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_iif_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-iif-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER);
         CREATE INDEX idx_metrics_iif_ab ON metrics(iif(a > 0, b, a));
         INSERT INTO metrics VALUES (1, 1, 10);
         INSERT INTO metrics VALUES (2, -2, 20);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE iif(a > 0, b, a) = 10;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_iif_ab mode=lookup key_prefix=[10]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE iif(a > 0, b, a) = 10 ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_unhex_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-unhex-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, code TEXT);
         CREATE INDEX idx_metrics_unhex_code ON metrics(unhex(code));
         INSERT INTO metrics VALUES (1, '4142');
         INSERT INTO metrics VALUES (2, '4344');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE unhex(code) = X'4142';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_unhex_code mode=lookup key_prefix=[X'4142']")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE unhex(code) = X'4142' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_unhex_expression_index_does_not_ignore_hex_digits_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-unhex-ignore-hex-digits.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, code TEXT, ignored TEXT);
         CREATE UNIQUE INDEX idx_metrics_unhex_ignored_code ON metrics(unhex(code, ignored));
         INSERT INTO metrics VALUES (1, '4142', '1');
         INSERT INTO metrics VALUES (2, '41A42', 'A');
         INSERT INTO metrics VALUES (3, '41A42', 'A');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO metrics VALUES (4, '4142', '-');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_metrics_unhex_ignored_code constraint failed"),
        "unexpected error: {error}"
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(hex(unhex(code, ignored))) FROM metrics ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'4142'\n2|''\n3|''");
}

#[test]
fn rustsql_uses_expression_index_for_date_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-date-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_date_ts ON metrics(date(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 03:04:05');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE date(ts) = '2024-01-02';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_date_ts mode=lookup key_prefix=[2024-01-02]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE date(ts) = '2024-01-02' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_date_constant_function_rhs_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-date-constant-rhs-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_date_ts ON metrics(date(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 04:05:06');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE date(ts) = date('2024-01-02 03:04:05');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_date_ts mode=lookup key_prefix=[2024-01-02]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE date(ts) = date('2024-01-02 03:04:05') ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_date_constant_function_lhs_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-date-constant-lhs-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_date_ts ON metrics(date(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 04:05:06');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE date('2024-01-02 03:04:05') = date(ts);",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_date_ts mode=lookup key_prefix=[2024-01-02]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE date('2024-01-02 03:04:05') = date(ts) ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_in_list_constant_function_rhs_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-in-list-constant-rhs-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_date_ts ON metrics(date(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 04:05:06');
         INSERT INTO metrics VALUES (3, '2024-01-04 05:06:07');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM metrics \
             WHERE date(ts) IN (date('2024-01-02 03:04:05'), date('2024-01-04 05:06:07'));",
        )
        .unwrap();
    assert_eq!(plan.len(), 3);
    assert_eq!(plan[0][0], Value::from("IndexUnion"));
    assert_eq!(plan[1][0], Value::from("  IndexScan"));
    assert_eq!(
        plan[1][1],
        Value::from("index=idx_metrics_date_ts mode=lookup key_prefix=[2024-01-02]")
    );
    assert_eq!(plan[2][0], Value::from("  IndexScan"));
    assert_eq!(
        plan[2][1],
        Value::from("index=idx_metrics_date_ts mode=lookup key_prefix=[2024-01-04]")
    );

    let rows = db
        .query(
            "SELECT id FROM metrics \
             WHERE date(ts) IN (date('2024-01-02 03:04:05'), date('2024-01-04 05:06:07')) \
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_expression_index_for_between_constant_function_rhs_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-between-constant-rhs-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_date_ts ON metrics(date(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 04:05:06');
         INSERT INTO metrics VALUES (3, '2024-01-04 05:06:07');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM metrics \
             WHERE date(ts) BETWEEN date('2024-01-02 03:04:05') AND date('2024-01-03 04:05:06');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=metrics index=idx_metrics_date_ts mode=range key_prefix=[] range=date(ts):Gte 2024-01-02..Lte 2024-01-03"
        )
    );

    let rows = db
        .query(
            "SELECT id FROM metrics \
             WHERE date(ts) BETWEEN date('2024-01-02 03:04:05') AND date('2024-01-03 04:05:06') \
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_uses_expression_index_for_time_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-time-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_time_ts ON metrics(time(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 04:05:06');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE time(ts) = '03:04:05';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=metrics index=idx_metrics_time_ts mode=lookup key_prefix=[03:04:05]")
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE time(ts) = '03:04:05' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_datetime_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-datetime-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_datetime_ts ON metrics(datetime(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 04:05:06');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE datetime(ts) = '2024-01-02 03:04:05';",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=metrics index=idx_metrics_datetime_ts mode=lookup key_prefix=[2024-01-02 03:04:05]"
        )
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE datetime(ts) = '2024-01-02 03:04:05' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_strftime_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-strftime-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_strftime_ts ON metrics(strftime('%F', ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 04:05:06');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE strftime('%F', ts) = '2024-01-02';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=metrics index=idx_metrics_strftime_ts mode=lookup key_prefix=[2024-01-02]"
        )
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE strftime('%F', ts) = '2024-01-02' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_strftime_constant_function_rhs_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-strftime-constant-rhs-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_strftime_ts ON metrics(strftime('%F', ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 04:05:06');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE strftime('%F', ts) = date('2024-01-02 03:04:05');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=metrics index=idx_metrics_strftime_ts mode=lookup key_prefix=[2024-01-02]"
        )
    );

    let rows = db
        .query(
            "SELECT id FROM metrics WHERE strftime('%F', ts) = date('2024-01-02 03:04:05') ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_julianday_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-julianday-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_julianday_ts ON metrics(julianday(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02');
         INSERT INTO metrics VALUES (2, '2024-01-03');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE julianday(ts) = 2460311.5;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert!(
        matches!(&plan[0][1], Value::Text(text) if text.contains("table=metrics index=idx_metrics_julianday_ts mode=lookup key_prefix=[")),
        "unexpected query plan detail: {:?}",
        plan[0][1]
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE julianday(ts) = 2460311.5 ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_julianday_constant_function_rhs_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-julianday-constant-rhs-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_julianday_ts ON metrics(julianday(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02');
         INSERT INTO metrics VALUES (2, '2024-01-03');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE julianday(ts) = julianday('2024-01-02');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert!(
        matches!(&plan[0][1], Value::Text(text) if text.contains("table=metrics index=idx_metrics_julianday_ts mode=lookup key_prefix=[")),
        "unexpected query plan detail: {:?}",
        plan[0][1]
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE julianday(ts) = julianday('2024-01-02') ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_unixepoch_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-unixepoch-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_unixepoch_ts ON metrics(unixepoch(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 04:05:06');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE unixepoch(ts) = 1704164645;")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=metrics index=idx_metrics_unixepoch_ts mode=lookup key_prefix=[1704164645]"
        )
    );

    let rows = db
        .query("SELECT id FROM metrics WHERE unixepoch(ts) = 1704164645 ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_unixepoch_constant_function_rhs_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-unixepoch-constant-rhs-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, ts TEXT);
         CREATE INDEX idx_metrics_unixepoch_ts ON metrics(unixepoch(ts));
         INSERT INTO metrics VALUES (1, '2024-01-02 03:04:05');
         INSERT INTO metrics VALUES (2, '2024-01-03 04:05:06');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE unixepoch(ts) = unixepoch('2024-01-02 03:04:05');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=metrics index=idx_metrics_unixepoch_ts mode=lookup key_prefix=[1704164645]"
        )
    );

    let rows = db
        .query(
            "SELECT id FROM metrics WHERE unixepoch(ts) = unixepoch('2024-01-02 03:04:05') ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_expression_index_for_scalar_like_prefix_predicate() {
    let fixture = writable_sqlite_fixture("expression-index-like-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_lower_name ON users(lower(name));
         INSERT INTO users VALUES (1, 'Alice');
         INSERT INTO users VALUES (2, 'Bob');
         INSERT INTO users VALUES (3, 'ALINA');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM users WHERE lower(name) LIKE 'ali%';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=users index=idx_users_lower_name mode=range key_prefix=[] range=lower(name):Gte ali..Lt alj"
        )
    );

    let rows = db
        .query("SELECT id FROM users WHERE lower(name) LIKE 'ali%' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_plain_index_for_glob_prefix_predicate() {
    let fixture = writable_sqlite_fixture("plain-index-glob-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_name ON users(name);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, 'alina');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM users WHERE name GLOB 'ali*';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=users index=idx_users_name mode=range key_prefix=[] range=name:Gte ali..Lt alj"
        )
    );

    let rows = db
        .query("SELECT id FROM users WHERE name GLOB 'ali*' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_expression_index_for_scalar_range_predicate() {
    let fixture = writable_sqlite_fixture("expression-index-range-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_lower_name ON users(lower(name));
         INSERT INTO users VALUES (1, 'Alice');
         INSERT INTO users VALUES (2, 'Bob');
         INSERT INTO users VALUES (3, 'Charlie');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM users WHERE lower(name) >= 'bob';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=users index=idx_users_lower_name mode=range key_prefix=[] range=lower(name):Gte bob..unbounded"
        )
    );

    let rows = db
        .query("SELECT id FROM users WHERE lower(name) >= 'bob' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_composite_expression_index_for_prefixed_constant_function_range_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-composite-prefixed-range.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, tenant_id INTEGER, name TEXT);
         CREATE INDEX idx_users_tenant_lower_name ON users(tenant_id, lower(name));
         INSERT INTO users VALUES (1, 1, 'Alice');
         INSERT INTO users VALUES (2, 1, 'Bob');
         INSERT INTO users VALUES (3, 1, 'Carol');
         INSERT INTO users VALUES (4, 1, 'Dave');
         INSERT INTO users VALUES (5, 2, 'Bob');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM users \
             WHERE tenant_id = 1 AND lower(name) >= lower('BOB') AND lower(name) < lower('D');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=users index=idx_users_tenant_lower_name mode=range key_prefix=[1] range=lower(name):Gte bob..Lt d"
        )
    );

    let rows = db
        .query(
            "SELECT id FROM users \
             WHERE tenant_id = 1 AND lower(name) >= lower('BOB') AND lower(name) < lower('D') \
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_expression_index_for_scalar_glob_prefix_predicate() {
    let fixture = writable_sqlite_fixture("expression-index-glob-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_lower_name ON users(lower(name));
         INSERT INTO users VALUES (1, 'Alice');
         INSERT INTO users VALUES (2, 'Bob');
         INSERT INTO users VALUES (3, 'ALINA');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM users WHERE lower(name) GLOB 'ali*';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=users index=idx_users_lower_name mode=range key_prefix=[] range=lower(name):Gte ali..Lt alj"
        )
    );

    let rows = db
        .query("SELECT id FROM users WHERE lower(name) GLOB 'ali*' ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_expression_index_for_scalar_between_predicate() {
    let fixture = writable_sqlite_fixture("expression-index-between-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_lower_name ON users(lower(name));
         INSERT INTO users VALUES (1, 'Alice');
         INSERT INTO users VALUES (2, 'Bob');
         INSERT INTO users VALUES (3, 'Carol');
         INSERT INTO users VALUES (4, 'Zed');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM users WHERE lower(name) BETWEEN 'alice' AND 'carol';",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=users index=idx_users_lower_name mode=range key_prefix=[] range=lower(name):Gte alice..Lte carol"
        )
    );

    let rows = db
        .query("SELECT id FROM users WHERE lower(name) BETWEEN 'alice' AND 'carol' ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );
}

#[test]
fn rustsql_uses_expression_index_for_scalar_in_list_predicate() {
    let fixture = writable_sqlite_fixture("expression-index-in-list-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_lower_name ON users(lower(name));
         INSERT INTO users VALUES (1, 'Alice');
         INSERT INTO users VALUES (2, 'Bob');
         INSERT INTO users VALUES (3, 'Carol');
         INSERT INTO users VALUES (4, 'Zed');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM users WHERE lower(name) IN ('alice', 'carol');",
        )
        .unwrap();
    assert_eq!(
        plan,
        vec![
            vec![
                Value::from("IndexUnion"),
                Value::from("table=users scans=2")
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_lower_name mode=lookup key_prefix=[alice]"),
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_lower_name mode=lookup key_prefix=[carol]"),
            ],
        ]
    );

    let rows = db
        .query("SELECT id FROM users WHERE lower(name) IN ('alice', 'carol') ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_composite_expression_index_for_prefixed_constant_function_in_list_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-composite-prefixed-in-list.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, tenant_id INTEGER, name TEXT);
         CREATE INDEX idx_users_tenant_lower_name ON users(tenant_id, lower(name));
         INSERT INTO users VALUES (1, 1, 'Alice');
         INSERT INTO users VALUES (2, 1, 'Bob');
         INSERT INTO users VALUES (3, 1, 'Carol');
         INSERT INTO users VALUES (4, 2, 'Alice');
         INSERT INTO users VALUES (5, 2, 'Carol');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM users \
             WHERE tenant_id = 1 AND lower(name) IN (lower('ALICE'), lower('CAROL'));",
        )
        .unwrap();
    assert_eq!(
        plan,
        vec![
            vec![
                Value::from("IndexUnion"),
                Value::from("table=users scans=2")
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_tenant_lower_name mode=lookup key_prefix=[1, alice]"),
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_tenant_lower_name mode=lookup key_prefix=[1, carol]"),
            ],
        ]
    );

    let rows = db
        .query(
            "SELECT id FROM users \
             WHERE tenant_id = 1 AND lower(name) IN (lower('ALICE'), lower('CAROL')) \
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_composite_expression_index_for_dual_in_lists_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-composite-dual-in-lists.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, tenant_id INTEGER, name TEXT);
         CREATE INDEX idx_users_tenant_lower_name ON users(tenant_id, lower(name));
         INSERT INTO users VALUES (1, 1, 'Alice');
         INSERT INTO users VALUES (2, 1, 'Bob');
         INSERT INTO users VALUES (3, 1, 'Carol');
         INSERT INTO users VALUES (4, 2, 'Alice');
         INSERT INTO users VALUES (5, 2, 'Carol');
         INSERT INTO users VALUES (6, 3, 'Alice');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM users \
             WHERE tenant_id IN (1, 2) AND lower(name) IN (lower('ALICE'), lower('CAROL'));",
        )
        .unwrap();
    assert_eq!(
        plan,
        vec![
            vec![
                Value::from("IndexUnion"),
                Value::from("table=users scans=4")
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_tenant_lower_name mode=lookup key_prefix=[1, alice]"),
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_tenant_lower_name mode=lookup key_prefix=[1, carol]"),
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_tenant_lower_name mode=lookup key_prefix=[2, alice]"),
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_tenant_lower_name mode=lookup key_prefix=[2, carol]"),
            ],
        ]
    );

    let rows = db
        .query(
            "SELECT id FROM users \
             WHERE tenant_id IN (1, 2) AND lower(name) IN (lower('ALICE'), lower('CAROL')) \
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(3)],
            vec![Value::Integer(4)],
            vec![Value::Integer(5)],
        ]
    );
}

#[test]
fn rustsql_uses_composite_expression_index_for_in_list_prefixed_range_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-composite-in-list-range.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, tenant_id INTEGER, name TEXT);
         CREATE INDEX idx_users_tenant_lower_name ON users(tenant_id, lower(name));
         INSERT INTO users VALUES (1, 1, 'Alice');
         INSERT INTO users VALUES (2, 1, 'Bob');
         INSERT INTO users VALUES (3, 1, 'Carol');
         INSERT INTO users VALUES (4, 2, 'Alice');
         INSERT INTO users VALUES (5, 2, 'Carol');
         INSERT INTO users VALUES (6, 2, 'Dave');
         INSERT INTO users VALUES (7, 3, 'Alice');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM users \
             WHERE tenant_id IN (1, 2) AND lower(name) >= lower('ALICE') AND lower(name) < lower('D');",
        )
        .unwrap();
    assert_eq!(
        plan,
        vec![
            vec![
                Value::from("IndexUnion"),
                Value::from("table=users scans=2")
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from(
                    "index=idx_users_tenant_lower_name mode=range key_prefix=[1] range=lower(name):Gte alice..Lt d"
                ),
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from(
                    "index=idx_users_tenant_lower_name mode=range key_prefix=[2] range=lower(name):Gte alice..Lt d"
                ),
            ],
        ]
    );

    let rows = db
        .query(
            "SELECT id FROM users \
             WHERE tenant_id IN (1, 2) AND lower(name) >= lower('ALICE') AND lower(name) < lower('D') \
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
            vec![Value::Integer(4)],
            vec![Value::Integer(5)],
        ]
    );
}

#[test]
fn rustsql_uses_composite_expression_index_for_and_wrapped_or_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-composite-and-or.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, tenant_id INTEGER, name TEXT);
         CREATE INDEX idx_users_tenant_lower_name ON users(tenant_id, lower(name));
         INSERT INTO users VALUES (1, 1, 'Alice');
         INSERT INTO users VALUES (2, 1, 'Bob');
         INSERT INTO users VALUES (3, 1, 'Carol');
         INSERT INTO users VALUES (4, 2, 'Alice');
         INSERT INTO users VALUES (5, 2, 'Carol');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM users \
             WHERE tenant_id = 1 AND (lower(name) = lower('ALICE') OR lower(name) = lower('CAROL'));",
        )
        .unwrap();
    assert_eq!(
        plan,
        vec![
            vec![
                Value::from("IndexUnion"),
                Value::from("table=users scans=2")
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_tenant_lower_name mode=lookup key_prefix=[1, alice]"),
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_tenant_lower_name mode=lookup key_prefix=[1, carol]"),
            ],
        ]
    );

    let rows = db
        .query(
            "SELECT id FROM users \
             WHERE tenant_id = 1 AND (lower(name) = lower('ALICE') OR lower(name) = lower('CAROL')) \
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_expression_index_for_single_value_scalar_in_list_predicate() {
    let fixture = writable_sqlite_fixture("expression-index-single-in-list-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_lower_name ON users(lower(name));
         INSERT INTO users VALUES (1, 'Alice');
         INSERT INTO users VALUES (2, 'Bob');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM users WHERE lower(name) IN ('alice');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=users index=idx_users_lower_name mode=lookup key_prefix=[alice]")
    );

    let rows = db
        .query("SELECT id FROM users WHERE lower(name) IN ('alice') ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_uses_plain_index_for_in_list_predicate() {
    let fixture = writable_sqlite_fixture("plain-index-in-list-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_name ON users(name);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, 'carol');
         INSERT INTO users VALUES (4, 'zed');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM users WHERE name IN ('alice', 'carol');",
        )
        .unwrap();
    assert_eq!(
        plan,
        vec![
            vec![
                Value::from("IndexUnion"),
                Value::from("table=users scans=2")
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_name mode=lookup key_prefix=[alice]"),
            ],
            vec![
                Value::from("  IndexScan"),
                Value::from("index=idx_users_name mode=lookup key_prefix=[carol]"),
            ],
        ]
    );

    let rows = db
        .query("SELECT id FROM users WHERE name IN ('alice', 'carol') ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

#[test]
fn rustsql_uses_plain_index_for_single_value_in_list_predicate() {
    let fixture = writable_sqlite_fixture("plain-index-single-in-list-query-plan.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_name ON users(name);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT id FROM users WHERE name IN ('alice');",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=users index=idx_users_name mode=lookup key_prefix=[alice]")
    );

    let rows = db
        .query("SELECT id FROM users WHERE name IN ('alice') ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_desc_primary_keys() {
    let fixture = writable_sqlite_fixture("desc-primary-key-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY DESC, name TEXT);
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (id INTEGER PRIMARY KEY DESC, name TEXT)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_column_collations() {
    let fixture = writable_sqlite_fixture("column-collation-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                name TEXT COLLATE NOCASE,
                alias TEXT COLLATE BINARY
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE, alias TEXT COLLATE BINARY)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_default_then_collate_columns() {
    let fixture = writable_sqlite_fixture("default-then-collate-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                nickname TEXT DEFAULT ('guest') COLLATE NOCASE
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, nickname TEXT COLLATE NOCASE DEFAULT 'guest')"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_named_check_constraints() {
    let fixture = writable_sqlite_fixture("named-check-constraints-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                age INTEGER CONSTRAINT age_nonneg CHECK (age >= 0),
                score INTEGER,
                CONSTRAINT score_cap CHECK (score <= 100)
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER CONSTRAINT age_nonneg CHECK (age >= 0), score INTEGER, CONSTRAINT score_cap CHECK (score <= 100))"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_regexp_check_constraints() {
    let fixture = writable_sqlite_fixture("regexp-check-constraints-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE patterns (
                id INTEGER PRIMARY KEY,
                name TEXT CHECK (name REGEXP '^a'),
                alias TEXT CHECK (alias NOT REGEXP '^a')
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let pattern_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'patterns';",
    );
    assert_eq!(
        pattern_schema,
        "CREATE TABLE patterns (id INTEGER PRIMARY KEY, name TEXT CHECK (name REGEXP '^a'), alias TEXT CHECK (alias NOT REGEXP '^a'))"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_named_column_primary_key_and_unique_constraints() {
    let fixture = writable_sqlite_fixture("named-column-pk-unique-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (
                id INTEGER CONSTRAINT pk PRIMARY KEY,
                email TEXT CONSTRAINT uq UNIQUE
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (id INTEGER CONSTRAINT pk PRIMARY KEY, email TEXT CONSTRAINT uq UNIQUE)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_named_not_null_constraints() {
    let fixture = writable_sqlite_fixture("named-not-null-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                name TEXT CONSTRAINT nn NOT NULL
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT CONSTRAINT nn NOT NULL)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_on_conflict_clauses() {
    let fixture = writable_sqlite_fixture("on-conflict-preserved-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY ON CONFLICT REPLACE,
                email TEXT UNIQUE ON CONFLICT IGNORE,
                name TEXT CONSTRAINT nn NOT NULL ON CONFLICT FAIL,
                nickname TEXT,
                CONSTRAINT uq UNIQUE(name, nickname) ON CONFLICT ABORT
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (id INTEGER PRIMARY KEY ON CONFLICT REPLACE, email TEXT UNIQUE ON CONFLICT IGNORE, name TEXT CONSTRAINT nn NOT NULL ON CONFLICT FAIL, nickname TEXT, CONSTRAINT uq UNIQUE(name, nickname) ON CONFLICT ABORT)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_named_foreign_keys() {
    let fixture = writable_sqlite_fixture("named-foreign-keys-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER CONSTRAINT fk_user REFERENCES users(id),
                author_id INTEGER,
                CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id)
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let post_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'posts';",
    );
    assert_eq!(
        post_schema,
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER CONSTRAINT fk_user REFERENCES users(id), author_id INTEGER, CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id))"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_foreign_key_actions_and_deferrable_clauses() {
    let fixture = writable_sqlite_fixture("foreign-key-actions-deferrable-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER REFERENCES users(id) ON DELETE CASCADE ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED,
                author_id INTEGER,
                CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE SET NULL ON UPDATE NO ACTION NOT DEFERRABLE INITIALLY IMMEDIATE
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let post_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'posts';",
    );
    assert_eq!(
        post_schema,
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id) ON DELETE CASCADE ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED, author_id INTEGER, CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE SET NULL ON UPDATE NO ACTION NOT DEFERRABLE INITIALLY IMMEDIATE)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_foreign_key_match_clauses() {
    let fixture = writable_sqlite_fixture("foreign-key-match-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER REFERENCES users(id) MATCH FULL,
                author_id INTEGER,
                CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id) MATCH SIMPLE
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let post_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'posts';",
    );
    assert_eq!(
        post_schema,
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id) MATCH FULL, author_id INTEGER, CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id) MATCH SIMPLE)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_named_unique_constraints() {
    let fixture = writable_sqlite_fixture("named-unique-constraints-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                name TEXT,
                nickname TEXT,
                CONSTRAINT uq_user_names UNIQUE(name, nickname)
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, nickname TEXT, CONSTRAINT uq_user_names UNIQUE(name, nickname))"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_decorated_table_constraint_columns() {
    let fixture = writable_sqlite_fixture("decorated-table-constraints-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (
                name TEXT,
                email TEXT,
                CONSTRAINT uq UNIQUE(name COLLATE NOCASE DESC, email ASC),
                PRIMARY KEY(name COLLATE BINARY ASC)
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (name TEXT, email TEXT, CONSTRAINT uq UNIQUE(name COLLATE NOCASE DESC, email ASC), PRIMARY KEY(name COLLATE BINARY ASC))"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_primary_key_on_conflict_clause() {
    let fixture = writable_sqlite_fixture("primary-key-on-conflict-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (
                name TEXT,
                email TEXT,
                PRIMARY KEY(name, email) ON CONFLICT FAIL
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (name TEXT, email TEXT, PRIMARY KEY(name, email) ON CONFLICT FAIL)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_matching_sqlite_catalog_behavior_for_if_not_exists() {
    let fixture = writable_sqlite_fixture("if-not-exists-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT);
             CREATE INDEX IF NOT EXISTS idx_users_name ON users(name);
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let user_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';",
    );
    assert_eq!(
        user_schema,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"
    );

    let index_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_users_name';",
    );
    assert_eq!(index_schema, "CREATE INDEX idx_users_name ON users (name)");

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_generated_columns() {
    let fixture = writable_sqlite_fixture("generated-column-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER GENERATED ALWAYS AS (base + 1) STORED
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let generated_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'metrics';",
    );
    assert_eq!(
        generated_schema,
        "CREATE TABLE metrics (base INTEGER, plus_one INTEGER GENERATED ALWAYS AS (base + 1) STORED)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_triggers() {
    let fixture = writable_sqlite_fixture("trigger-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE audit (user_id INTEGER, name TEXT);
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             CREATE TRIGGER trg_users_ai AFTER INSERT ON users
             BEGIN
                 INSERT INTO audit VALUES (new.id, new.name);
             END;
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let trigger_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT type || '|' || name || '|' || tbl_name || '|' || rootpage || '|' || sql \
         FROM sqlite_master WHERE type = 'trigger' AND name = 'trg_users_ai';",
    );
    assert_eq!(
        trigger_schema,
        "trigger|trg_users_ai|users|0|CREATE TRIGGER trg_users_ai AFTER INSERT ON users\n             BEGIN\n                 INSERT INTO audit VALUES (new.id, new.name);\n             END"
    );

    let trigger_status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("INSERT INTO users VALUES (7, 'carol');")
        .status()
        .unwrap();
    assert!(trigger_status.success());

    let audit_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || name FROM audit ORDER BY user_id;",
    );
    assert_eq!(audit_rows, "7|carol");
}

#[test]
fn rustsql_updates_trigger_schema_on_table_rename_like_sqlite() {
    let fixture = writable_sqlite_fixture("trigger-table-rename.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE audit(user_id INTEGER, name TEXT);
             CREATE TRIGGER trg_users_ai AFTER INSERT ON users
             BEGIN
                 INSERT INTO audit VALUES (new.id, new.name);
             END;",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("ALTER TABLE users RENAME TO customers;")
        .unwrap();

    let trigger_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT type || '|' || name || '|' || tbl_name || '|' || sql \
         FROM sqlite_master WHERE type = 'trigger' AND name = 'trg_users_ai';",
    );
    assert_eq!(
        trigger_schema,
        "trigger|trg_users_ai|customers|CREATE TRIGGER trg_users_ai AFTER INSERT ON \"customers\"\n             BEGIN\n                 INSERT INTO audit VALUES (new.id, new.name);\n             END"
    );

    let trigger_status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("INSERT INTO customers VALUES (8, 'dave');")
        .status()
        .unwrap();
    assert!(trigger_status.success());

    let audit_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || name FROM audit ORDER BY user_id;",
    );
    assert_eq!(audit_rows, "8|dave");
}

#[test]
fn rustsql_updates_trigger_schema_on_column_rename_like_sqlite() {
    let fixture = writable_sqlite_fixture("trigger-column-rename.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE audit(user_id INTEGER, renamed TEXT);
             CREATE TRIGGER trg_users_ai AFTER INSERT ON users
             BEGIN
                 INSERT INTO audit VALUES (new.id, new.name);
             END;",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("ALTER TABLE users RENAME COLUMN name TO full_name;")
        .unwrap();

    let trigger_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT type || '|' || name || '|' || tbl_name || '|' || sql \
         FROM sqlite_master WHERE type = 'trigger' AND name = 'trg_users_ai';",
    );
    assert_eq!(
        trigger_schema,
        "trigger|trg_users_ai|users|CREATE TRIGGER trg_users_ai AFTER INSERT ON users\n             BEGIN\n                 INSERT INTO audit VALUES (new.id, new.full_name);\n             END"
    );

    let trigger_status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("INSERT INTO users VALUES (10, 'frank');")
        .status()
        .unwrap();
    assert!(trigger_status.success());

    let audit_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || renamed FROM audit ORDER BY user_id;",
    );
    assert_eq!(audit_rows, "10|frank");
}

#[test]
fn rustsql_drops_triggers_for_dropped_table_like_sqlite() {
    let fixture = writable_sqlite_fixture("trigger-table-drop.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE audit(user_id INTEGER, name TEXT);
             CREATE TRIGGER trg_users_ai AFTER INSERT ON users
             BEGIN
                 INSERT INTO audit VALUES (new.id, new.name);
             END;",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("DROP TABLE users;").unwrap();

    let trigger_count = sqlite3_scalar(
        &fixture.path,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger';",
    );
    assert_eq!(trigger_count, "0");
}

#[test]
fn rustsql_supports_drop_trigger_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("drop-trigger.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE audit(user_id INTEGER, name TEXT);
             CREATE TRIGGER trg_users_ai AFTER INSERT ON users
             BEGIN
                 INSERT INTO audit VALUES (new.id, new.name);
             END;",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("DROP TRIGGER trg_users_ai;").unwrap();
    db.execute("DROP TRIGGER IF EXISTS trg_users_ai;").unwrap();

    let trigger_count = sqlite3_scalar(
        &fixture.path,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger';",
    );
    assert_eq!(trigger_count, "0");

    let insert_status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("INSERT INTO users VALUES (1, 'alice');")
        .status()
        .unwrap();
    assert!(insert_status.success());

    let audit_count = sqlite3_scalar(&fixture.path, "SELECT COUNT(*) FROM audit;");
    assert_eq!(audit_count, "0");
}

#[test]
fn rustsql_supports_create_trigger_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("create-trigger.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(user_id INTEGER, name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.id, new.name);
         END;",
    )
    .unwrap();

    let trigger_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT type || '|' || name || '|' || tbl_name || '|' || rootpage || '|' || sql \
         FROM sqlite_master WHERE type = 'trigger' AND name = 'trg_users_ai';",
    );
    assert_eq!(
        trigger_schema,
        "trigger|trg_users_ai|users|0|CREATE TRIGGER trg_users_ai AFTER INSERT ON users BEGIN INSERT INTO audit VALUES (new.id, new.name); END"
    );

    let trigger_status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("INSERT INTO users VALUES (9, 'erin');")
        .status()
        .unwrap();
    assert!(trigger_status.success());

    let audit_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || name FROM audit ORDER BY user_id;",
    );
    assert_eq!(audit_rows, "9|erin");
}

#[test]
fn rustsql_executes_simple_after_insert_trigger_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-insert-trigger.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(user_id INTEGER, name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.id, new.name);
         END;
         INSERT INTO users VALUES (11, 'grace');",
    )
    .unwrap();

    let rows = db
        .query("SELECT user_id, name FROM audit ORDER BY user_id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(11), Value::from("grace")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || name FROM audit ORDER BY user_id;",
    );
    assert_eq!(cli_rows, "11|grace");
}

#[test]
fn rustsql_executes_simple_after_insert_trigger_when_condition_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-insert-trigger-when.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(user_id INTEGER, name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         WHEN new.name <> 'skip'
         BEGIN
             INSERT INTO audit VALUES (new.id, new.name);
         END;
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'skip');",
    )
    .unwrap();

    let rows = db
        .query("SELECT user_id, name FROM audit ORDER BY user_id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || name FROM audit ORDER BY user_id;",
    );
    assert_eq!(cli_rows, "1|alice");
}

#[test]
fn rustsql_executes_simple_after_insert_trigger_when_in_condition_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-insert-trigger-when-in.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(user_id INTEGER, name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         WHEN new.name IN ('alice', 'bob')
         BEGIN
             INSERT INTO audit VALUES (new.id, new.name);
         END;
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'carol');",
    )
    .unwrap();

    let rows = db
        .query("SELECT user_id, name FROM audit ORDER BY user_id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || name FROM audit ORDER BY user_id;",
    );
    assert_eq!(cli_rows, "1|alice");
}

#[test]
fn rustsql_executes_simple_after_insert_trigger_when_between_condition_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-insert-trigger-when-between.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE audit(score INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         WHEN new.score BETWEEN 5 AND 7
         BEGIN
             INSERT INTO audit VALUES (new.score);
         END;
         INSERT INTO users VALUES (1, 5);
         INSERT INTO users VALUES (2, 8);",
    )
    .unwrap();

    let rows = db.query("SELECT score FROM audit ORDER BY score;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(5)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT score FROM audit ORDER BY score;");
    assert_eq!(cli_rows, "5");
}

#[test]
fn rustsql_executes_simple_after_insert_trigger_when_like_condition_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-insert-trigger-when-like.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         WHEN new.name LIKE 'a%'
         BEGIN
             INSERT INTO audit VALUES (new.name);
         END;
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit ORDER BY name;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit ORDER BY name;");
    assert_eq!(cli_rows, "alice");
}

#[test]
fn rustsql_executes_simple_after_insert_trigger_when_like_dynamic_escape_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-insert-trigger-when-like-escape.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         WHEN new.name LIKE 'a!_%' ESCAPE ('!' || '')
         BEGIN
             INSERT INTO audit VALUES (new.name);
         END;
         INSERT INTO users VALUES (1, 'a_foo');
         INSERT INTO users VALUES (2, 'abfoo');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit ORDER BY name;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("a_foo")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit ORDER BY name;");
    assert_eq!(cli_rows, "a_foo");
}

#[test]
fn rustsql_executes_simple_after_insert_trigger_when_like_null_escape_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-insert-trigger-when-like-null-escape.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         WHEN new.name LIKE 'a%' ESCAPE NULL
         BEGIN
             INSERT INTO audit VALUES (new.name);
         END;
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit ORDER BY name;").unwrap();
    assert_eq!(rows, Vec::<Vec<Value>>::new());

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT count(*) FROM audit;");
    assert_eq!(cli_rows, "0");
}

#[test]
fn rustsql_executes_simple_after_insert_trigger_when_glob_condition_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-insert-trigger-when-glob.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         WHEN new.name GLOB 'A*'
         BEGIN
             INSERT INTO audit VALUES (new.name);
         END;
         INSERT INTO users VALUES (1, 'Alice');
         INSERT INTO users VALUES (2, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit ORDER BY name;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("Alice")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit ORDER BY name;");
    assert_eq!(cli_rows, "Alice");
}

#[test]
fn rustsql_executes_simple_after_insert_trigger_when_case_condition_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-insert-trigger-when-case.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         WHEN CASE new.name WHEN 'alice' THEN 1 ELSE 0 END
         BEGIN
             INSERT INTO audit VALUES (new.name);
         END;
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit ORDER BY name;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit ORDER BY name;");
    assert_eq!(cli_rows, "alice");
}

#[test]
fn rustsql_executes_simple_trigger_raise_abort_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-raise-abort.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TRIGGER trg_users_bi BEFORE INSERT ON users
         WHEN new.name = 'blocked'
         BEGIN
             SELECT RAISE(ABORT, 'blocked user');
         END;
         INSERT INTO users VALUES (1, 'ok');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'blocked');")
        .unwrap_err();
    assert!(
        error.to_string().contains("blocked user"),
        "unexpected error: {error}"
    );

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("ok")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|ok");
}

#[test]
fn rustsql_executes_simple_trigger_raise_abort_expression_message_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-raise-abort-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TRIGGER trg_users_bi BEFORE INSERT ON users
         WHEN new.name = 'blocked'
         BEGIN
             SELECT RAISE(ABORT, 'blocked ' || new.name);
         END;
         INSERT INTO users VALUES (1, 'ok');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'blocked');")
        .unwrap_err();
    assert!(
        error.to_string().contains("blocked blocked"),
        "unexpected error: {error}"
    );

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("ok")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|ok");
}

#[test]
fn rustsql_executes_simple_trigger_raise_fail_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-raise-fail.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TRIGGER trg_users_bi BEFORE INSERT ON users
         WHEN new.name = 'blocked'
         BEGIN
             SELECT RAISE(FAIL, 'fail user');
         END;
         INSERT INTO users VALUES (1, 'ok');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'blocked');")
        .unwrap_err();
    assert!(
        error.to_string().contains("fail user"),
        "unexpected error: {error}"
    );

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("ok")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|ok");
}

#[test]
fn rustsql_executes_simple_trigger_raise_rollback_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-raise-rollback.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TRIGGER trg_users_bi BEFORE INSERT ON users
         WHEN new.name = 'blocked'
         BEGIN
             SELECT RAISE(ROLLBACK, 'rollback user');
         END;
         INSERT INTO users VALUES (1, 'ok');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'blocked');")
        .unwrap_err();
    assert!(
        error.to_string().contains("rollback user"),
        "unexpected error: {error}"
    );

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("ok")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|ok");
}

#[test]
fn rustsql_executes_simple_trigger_raise_ignore_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-raise-ignore.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TRIGGER trg_users_bi BEFORE INSERT ON users
         WHEN new.name = 'skip'
         BEGIN
             SELECT RAISE(IGNORE);
         END;
         INSERT INTO users VALUES (1, 'ok');",
    )
    .unwrap();

    db.execute("INSERT INTO users VALUES (2, 'skip');").unwrap();
    let rows = db.query("SELECT changes();").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(0)]]);

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("ok")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|ok");
}

#[test]
fn rustsql_executes_simple_trigger_insert_with_column_list_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-column-list.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT, user_id INTEGER, tag TEXT DEFAULT 'seen');
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit(user_id, name) VALUES (new.id, new.name);
         END;
         INSERT INTO users VALUES (7, 'hank');",
    )
    .unwrap();

    let rows = db.query("SELECT name, user_id, tag FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("hank"),
            Value::Integer(7),
            Value::from("seen"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || user_id || '|' || tag FROM audit;",
    );
    assert_eq!(cli_rows, "hank|7|seen");
}

#[test]
fn rustsql_executes_simple_trigger_insert_value_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-value-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (upper(new.name));
         END;
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("ALICE")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit;");
    assert_eq!(cli_rows, "ALICE");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT new.name;
         END;
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit;");
    assert_eq!(cli_rows, "alice");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_alias_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-alias-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT new.name AS inserted_name;
         END;
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit;");
    assert_eq!(cli_rows, "alice");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_where_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-where-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT new.name WHERE new.name <> 'skip';
         END;
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'skip');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit ORDER BY name;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit ORDER BY name;");
    assert_eq!(cli_rows, "alice");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-from-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = new.name;
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'AA');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("AA")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nAA");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_count_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-count-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT count(*) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'B');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT cnt FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT cnt FROM audit;");
    assert_eq!(cli_rows, "2");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_count_expr_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-count-expr-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT count(label) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'B');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT cnt FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT cnt FROM audit;");
    assert_eq!(cli_rows, "2");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_aggregate_all_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-aggregate-all.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, score INTEGER);
         CREATE TABLE audit(value);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT count(ALL label) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT sum(ALL score) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT avg(ALL score) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT total(ALL score) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT group_concat(ALL label, '|') FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A', 3);
         INSERT INTO templates VALUES ('admin', 'B', 4);
         INSERT INTO templates VALUES ('admin', NULL, NULL);
         INSERT INTO templates VALUES ('guest', 'G', 99);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY rowid;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(2)],
            vec![Value::Integer(7)],
            vec![Value::Real(3.5)],
            vec![Value::Real(7.0)],
            vec![Value::from("A|B")],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT quote(value) FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "2\n7\n3.5\n7.0\n'A|B'");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_aggregate_filter_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-aggregate-filter.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, score INTEGER);
         CREATE TABLE audit(value);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT count(*) FILTER (WHERE score >= 3) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT sum(score) FILTER (WHERE label IS NOT NULL) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2);
         INSERT INTO templates VALUES ('admin', NULL, 3);
         INSERT INTO templates VALUES ('admin', 'B', 4);
         INSERT INTO templates VALUES ('guest', 'G', 99);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY rowid;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT quote(value) FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "2\n6");
}

#[test]
fn rustsql_executes_simple_trigger_group_concat_order_by_with_filter_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-concat-order-by-with-filter.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER, enabled INTEGER);
         CREATE TABLE audit(summary TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT group_concat(label ORDER BY priority DESC) FILTER (WHERE enabled) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT group_concat(DISTINCT label ORDER BY priority DESC) FILTER (WHERE enabled) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2, 1);
         INSERT INTO templates VALUES ('admin', 'B', 1, 0);
         INSERT INTO templates VALUES ('admin', 'C', 3, 1);
         INSERT INTO templates VALUES ('guest', 'G', 0, 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT summary FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("C,A")], vec![Value::from("C,A")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT summary FROM audit ORDER BY rowid;");
    assert_eq!(cli_rows, "C,A\nC,A");
}

#[test]
fn rustsql_executes_simple_trigger_group_concat_order_by_filter_with_source_alias_like_sqlite() {
    let fixture =
        writable_sqlite_fixture("execute-trigger-group-concat-order-by-filter-source-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER, enabled INTEGER);
         CREATE TABLE audit(summary TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT group_concat(t.label ORDER BY t.priority) FILTER (WHERE t.enabled) FROM templates AS t WHERE t.kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2, 1);
         INSERT INTO templates VALUES ('admin', 'B', 1, 0);
         INSERT INTO templates VALUES ('admin', 'C', 3, 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT summary FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A,C")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT summary FROM audit;");
    assert_eq!(cli_rows, "A,C");
}

#[test]
fn rustsql_executes_simple_trigger_group_concat_filter_with_new_row_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-concat-filter-new-row.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(summary TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT group_concat(label ORDER BY priority) FILTER (WHERE kind = new.name) FROM templates;
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2);
         INSERT INTO templates VALUES ('guest', 'G', 1);
         INSERT INTO templates VALUES ('admin', 'B', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT summary FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("B,A")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT summary FROM audit;");
    assert_eq!(cli_rows, "B,A");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_group_by_aggregate_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-group-by-aggregate.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, cnt INTEGER, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, count(*), sum(score) FROM templates GROUP BY kind ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2);
         INSERT INTO templates VALUES ('guest', 'G', 5);
         INSERT INTO templates VALUES ('admin', 'B', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, cnt, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(2), Value::Integer(6)],
            vec![Value::from("guest"), Value::Integer(1), Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || cnt || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|2|6\nguest|1|5");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_group_by_having_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-group-by-having.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, cnt INTEGER, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, count(*), sum(score) FROM templates GROUP BY kind HAVING sum(score) >= 6 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2);
         INSERT INTO templates VALUES ('guest', 'G', 5);
         INSERT INTO templates VALUES ('admin', 'B', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, cnt, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("admin"),
            Value::Integer(2),
            Value::Integer(6),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || cnt || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|2|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_order_by_aggregate_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-order-by-aggregate-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind ORDER BY total DESC;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("guest"), Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6\nguest|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_order_by_result_position_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-order-by-result-position.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) FROM templates GROUP BY kind ORDER BY 2 DESC;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("guest"), Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6\nguest|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_result_position_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-result-position.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) FROM templates GROUP BY 1 ORDER BY 1;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("guest"), Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6\nguest|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_result_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-result-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k ORDER BY k;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("guest"), Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6\nguest|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_scalar_expression_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-scalar-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT lower(kind), sum(score) FROM templates GROUP BY lower(kind) ORDER BY lower(kind);
         END;
         INSERT INTO templates VALUES ('Admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('Guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("guest"), Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6\nguest|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_multiple_columns_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-multiple-columns.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, region TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, region TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, region, sum(score) FROM templates GROUP BY kind, region ORDER BY kind, region;
         END;
         INSERT INTO templates VALUES ('admin', 'us', 2);
         INSERT INTO templates VALUES ('admin', 'eu', 4);
         INSERT INTO templates VALUES ('admin', 'us', 3);
         INSERT INTO templates VALUES ('guest', 'us', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, region, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::from("eu"), Value::Integer(4),],
            vec![Value::from("admin"), Value::from("us"), Value::Integer(5),],
            vec![Value::from("guest"), Value::from("us"), Value::Integer(5),],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || region || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|eu|4\nadmin|us|5\nguest|us|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_order_by_limit_offset_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-order-by-limit-offset.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind ORDER BY total DESC LIMIT 1 OFFSET 1;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('ops', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("guest"), Value::Integer(5)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "guest|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_aggregate_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-aggregate-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING total >= 6 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_aggregate_filter_and_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-aggregate-filter-and.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) FILTER (WHERE score >= 3 AND label IS NOT NULL) FROM templates GROUP BY kind ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2);
         INSERT INTO templates VALUES ('admin', NULL, 3);
         INSERT INTO templates VALUES ('admin', 'B', 4);
         INSERT INTO templates VALUES ('guest', 'G', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(4)],
            vec![Value::from("guest"), Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || coalesce(total, 'NULL') FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|4\nguest|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_and_aliases_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-and-aliases.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, cnt INTEGER, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, count(*) AS cnt, sum(score) AS total FROM templates GROUP BY kind HAVING total >= 6 AND cnt >= 2 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2);
         INSERT INTO templates VALUES ('admin', 'B', 4);
         INSERT INTO templates VALUES ('guest', 'G', 7);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, cnt, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("admin"),
            Value::Integer(2),
            Value::Integer(6),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || cnt || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|2|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_between_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-between-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING total BETWEEN 5 AND 7 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 8);
         INSERT INTO templates VALUES ('ops', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("ops"), Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6\nops|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_not_between_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-not-between-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING total NOT BETWEEN 0 AND 5 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_in_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-in-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING total IN (5, 6) ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 8);
         INSERT INTO templates VALUES ('ops', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("ops"), Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6\nops|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_not_in_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-not-in-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING total NOT IN (5) ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_is_not_null_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-is-not-null-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING total IS NOT NULL ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('empty', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_is_true_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-is-true-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING total IS TRUE ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('zero', 0);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_is_false_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-is-false-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING total IS FALSE ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('zero', 0);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("zero"), Value::Integer(0)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "zero|0");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_scalar_func_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-scalar-func-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING typeof(total) = 'integer' ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('empty', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_date_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-date-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, d TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, d TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, d AS day, sum(score) FROM templates GROUP BY kind, day HAVING date(day) = '2026-07-14' ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', '2026-07-14 12:34:56', 2);
         INSERT INTO templates VALUES ('admin', '2026-07-14 12:34:56', 4);
         INSERT INTO templates VALUES ('guest', '2026-07-15', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, d, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("admin"),
            Value::from("2026-07-14 12:34:56"),
            Value::Integer(6),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || d || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|2026-07-14 12:34:56|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_time_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-time-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, ts TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, ts TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, ts AS stamp, sum(score) FROM templates GROUP BY kind, stamp HAVING time(stamp) = '12:34:56' ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', '2026-07-14 12:34:56', 2);
         INSERT INTO templates VALUES ('admin', '2026-07-14 12:34:56', 4);
         INSERT INTO templates VALUES ('guest', '2026-07-14 01:02:03', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, ts, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("admin"),
            Value::from("2026-07-14 12:34:56"),
            Value::Integer(6),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || ts || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|2026-07-14 12:34:56|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_datetime_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-datetime-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, ts TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, ts TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, ts AS stamp, sum(score) FROM templates GROUP BY kind, stamp HAVING datetime(stamp) = '2026-07-14 12:34:56' ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', '2026-07-14 12:34:56', 2);
         INSERT INTO templates VALUES ('admin', '2026-07-14 12:34:56', 4);
         INSERT INTO templates VALUES ('guest', '2026-07-14 01:02:03', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, ts, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("admin"),
            Value::from("2026-07-14 12:34:56"),
            Value::Integer(6),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || ts || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|2026-07-14 12:34:56|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_coalesce_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-coalesce-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING coalesce(total, 0) >= 0 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('empty', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("empty"), Value::Null],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || coalesce(total, 'NULL') FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6\nempty|NULL");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_nullif_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-nullif-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING nullif(total, 6) IS NULL ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_cast_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-cast-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING CAST(total AS TEXT) = '6' ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_concat_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-concat-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING total || '' = '6' ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_concat_func_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-concat-func-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) AS total FROM templates GROUP BY k HAVING concat(k, total) = 'admin6' ORDER BY k;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_case_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-case-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING CASE WHEN total > 5 THEN 1 ELSE 0 END ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_unary_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-unary-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING -total < -5 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_abs_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-abs-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING abs(total) > 5 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', -8);
         INSERT INTO templates VALUES ('ops', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("guest"), Value::Integer(-8)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6\nguest|-8");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_sign_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-sign-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING sign(total - 6) = 0 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_shift_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-shift-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING (total << 1) = 12 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_scalar_max_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-scalar-max-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING max(total, 0) = 6 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_round_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-round-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score REAL);
         CREATE TABLE audit(kind TEXT, avg_score REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, avg(score) AS avg_score FROM templates GROUP BY kind HAVING round(avg_score, 1) = 3.5 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 3.0);
         INSERT INTO templates VALUES ('admin', 4.0);
         INSERT INTO templates VALUES ('guest', 5.0);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, avg_score FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Real(3.5)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || avg_score FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|3.5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_length_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-length-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(kind TEXT, names TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, group_concat(label, '|') AS names FROM templates GROUP BY kind HAVING length(names) > 3 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('admin', 'BB');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, names FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::from("A|BB")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || names FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|A|BB");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_octet_length_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-octet-length-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k HAVING octet_length(k) = 5 ORDER BY k;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("guest"), Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6\nguest|5");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_lower_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-lower-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k HAVING lower(k) = 'admin' ORDER BY k;
         END;
         INSERT INTO templates VALUES ('Admin', 2);
         INSERT INTO templates VALUES ('Guest', 5);
         INSERT INTO templates VALUES ('Admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("Admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "Admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_trim_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-trim-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k HAVING trim(k) = 'admin' ORDER BY k;
         END;
         INSERT INTO templates VALUES (' admin ', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES (' admin ', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from(" admin "), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin |6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_replace_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-replace-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k HAVING replace(k, ' ', '') = 'admin' ORDER BY k;
         END;
         INSERT INTO templates VALUES ('ad min', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('ad min', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("ad min"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "ad min|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_instr_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-instr-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k HAVING instr(k, 'min') > 0 ORDER BY k;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_quote_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-quote-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING quote(total) = '6' ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_printf_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-printf-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING printf('%d', total) = '6' ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_hex_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-hex-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k HAVING hex(k) = '61646D696E' ORDER BY k;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_zeroblob_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-zeroblob-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING length(zeroblob(total)) = 6 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_unicode_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-unicode-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k HAVING unicode(k) = 97 ORDER BY k;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_char_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-char-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING char(total + 59) = 'A' ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_substr_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-substr-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k HAVING substr(k, 1, 3) = 'adm' ORDER BY k;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_like_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-like-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k HAVING k LIKE 'adm%' ORDER BY k;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_glob_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-glob-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind AS k, sum(score) FROM templates GROUP BY k HAVING k GLOB 'adm*' ORDER BY k;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('Admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_not_alias_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-not-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) AS total FROM templates GROUP BY kind HAVING NOT (total IS NULL) ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('empty', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|6");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_or_order_by_aggregate_like_sqlite() {
    let fixture =
        writable_sqlite_fixture("execute-trigger-group-by-having-or-order-by-aggregate.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, cnt INTEGER, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, count(*), sum(score) FROM templates GROUP BY kind HAVING sum(score) >= 7 OR count(*) >= 2 ORDER BY count(*) DESC, kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 7);
         INSERT INTO templates VALUES ('ops', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, cnt, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::Integer(2), Value::Integer(6)],
            vec![Value::from("guest"), Value::Integer(1), Value::Integer(7)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || cnt || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|2|6\nguest|1|7");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_order_by_aggregate_arithmetic_like_sqlite() {
    let fixture =
        writable_sqlite_fixture("execute-trigger-group-by-order-by-aggregate-arithmetic.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, sum(score) FROM templates GROUP BY kind ORDER BY sum(score) + count(*) DESC;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 8);
         INSERT INTO templates VALUES ('ops', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, total FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("guest"), Value::Integer(8)],
            vec![Value::from("admin"), Value::Integer(6)],
            vec![Value::from("ops"), Value::Integer(1)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || total FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "guest|8\nadmin|6\nops|1");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_count_distinct_having_alias_like_sqlite() {
    let fixture =
        writable_sqlite_fixture("execute-trigger-group-by-count-distinct-having-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(kind TEXT, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, count(DISTINCT label) AS cnt FROM templates GROUP BY kind HAVING cnt >= 2 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('admin', 'B');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, cnt FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || cnt FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|2");
}

#[test]
fn rustsql_executes_simple_trigger_group_by_having_aggregate_filter_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-group-by-having-aggregate-filter.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT kind, count(*) FROM templates GROUP BY kind HAVING count(*) FILTER (WHERE score >= 3) >= 2 ORDER BY kind;
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('guest', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT kind, cnt FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("admin"), Value::Integer(3)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || cnt FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "admin|3");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_order_by_inside_order_insensitive_aggregates() {
    let fixture = writable_sqlite_fixture(
        "execute-trigger-insert-select-order-by-inside-order-insensitive-aggregates.db",
    );
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, score INTEGER, priority INTEGER);
         CREATE TABLE audit(value);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT count(label ORDER BY priority) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT sum(score ORDER BY priority DESC) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT avg(score ORDER BY priority) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT total(score ORDER BY priority) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT min(score ORDER BY priority) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT max(score ORDER BY priority) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2, 2);
         INSERT INTO templates VALUES ('admin', 'B', 4, 1);
         INSERT INTO templates VALUES ('admin', NULL, NULL, 3);
         INSERT INTO templates VALUES ('guest', 'G', 99, 0);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY rowid;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(2)],
            vec![Value::Integer(6)],
            vec![Value::Real(3.0)],
            vec![Value::Real(6.0)],
            vec![Value::Integer(2)],
            vec![Value::Integer(4)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT quote(value) FROM audit ORDER BY rowid;",
    );
    assert_eq!(cli_rows, "2\n6\n3.0\n6.0\n2\n4");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_count_distinct_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-count-distinct-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT count(distinct label) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('admin', 'B');
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO templates VALUES ('guest', 'C');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT cnt FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT cnt FROM audit;");
    assert_eq!(cli_rows, "2");
}

#[test]
fn rustsql_rejects_simple_trigger_count_distinct_multiple_args_like_sqlite() {
    let fixture = writable_sqlite_fixture("reject-trigger-count-distinct-multiple-args.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    let error = db
        .execute(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE templates(kind TEXT, label TEXT);
             CREATE TABLE audit(cnt INTEGER);
             CREATE TRIGGER trg_users_ai AFTER INSERT ON users
             BEGIN
                 INSERT INTO audit SELECT count(distinct label, kind) FROM templates WHERE kind = 'admin';
             END;
             INSERT INTO templates VALUES ('admin', 'A');
             INSERT INTO users VALUES (1, 'admin');",
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("wrong number of arguments to function count()")
    );
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_sum_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-sum-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT sum(score) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('guest', 99);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT total FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(7)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT total FROM audit;");
    assert_eq!(cli_rows, "7");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_sum_distinct_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-sum-distinct-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(total INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT sum(distinct score) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('guest', 99);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT total FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(7)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT total FROM audit;");
    assert_eq!(cli_rows, "7");
}

#[test]
fn rustsql_rejects_simple_trigger_sum_distinct_multiple_args_like_sqlite() {
    assert_rejects_trigger_aggregate_like_sqlite(
        "reject-trigger-sum-distinct-multiple-args.db",
        "sum(distinct score, bonus)",
        "wrong number of arguments to function sum()",
    );
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_avg_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-avg-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(avg_score REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT avg(score) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('guest', 99);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT avg_score FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Real(3.5)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT avg_score FROM audit;");
    assert_eq!(cli_rows, "3.5");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_avg_distinct_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-avg-distinct-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(avg_score REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT avg(distinct score) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('guest', 99);
         INSERT INTO templates VALUES ('admin', 5);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT avg_score FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Real(4.0)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT avg_score FROM audit;");
    assert_eq!(cli_rows, "4.0");
}

#[test]
fn rustsql_rejects_simple_trigger_avg_distinct_multiple_args_like_sqlite() {
    assert_rejects_trigger_aggregate_like_sqlite(
        "reject-trigger-avg-distinct-multiple-args.db",
        "avg(distinct score, bonus)",
        "wrong number of arguments to function avg()",
    );
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_sum_real_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-sum-real-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, reading REAL);
         CREATE TABLE audit(total REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT sum(reading) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 1.5);
         INSERT INTO templates VALUES ('guest', 99.0);
         INSERT INTO templates VALUES ('admin', 2.25);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT total FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Real(3.75)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT total FROM audit;");
    assert_eq!(cli_rows, "3.75");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_avg_real_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-avg-real-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, reading REAL);
         CREATE TABLE audit(avg_reading REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT avg(reading) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 1.5);
         INSERT INTO templates VALUES ('guest', 99.0);
         INSERT INTO templates VALUES ('admin', 2.5);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT avg_reading FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Real(2.0)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT avg_reading FROM audit;");
    assert_eq!(cli_rows, "2.0");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_total_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-total-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(total_score REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT total(score) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('guest', 99);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT total_score FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Real(7.0)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT total_score FROM audit;");
    assert_eq!(cli_rows, "7.0");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_total_distinct_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-total-distinct-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(total_score REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT total(distinct score) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('guest', 99);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT total_score FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Real(7.0)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT total_score FROM audit;");
    assert_eq!(cli_rows, "7.0");
}

#[test]
fn rustsql_rejects_simple_trigger_total_distinct_multiple_args_like_sqlite() {
    assert_rejects_trigger_aggregate_like_sqlite(
        "reject-trigger-total-distinct-multiple-args.db",
        "total(distinct score, bonus)",
        "wrong number of arguments to function total()",
    );
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_group_concat_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-group-concat-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(summary TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT group_concat(label, '|') FROM templates WHERE kind = 'admin' ORDER BY priority;
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2);
         INSERT INTO templates VALUES ('guest', 'G', 1);
         INSERT INTO templates VALUES ('admin', NULL, 3);
         INSERT INTO templates VALUES ('admin', 'B', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT summary FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A|B")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT summary FROM audit;");
    assert_eq!(cli_rows, "A|B");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_group_concat_order_by_arg_like_sqlite() {
    let fixture =
        writable_sqlite_fixture("execute-trigger-insert-select-group-concat-order-by-arg.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(summary TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT group_concat(label ORDER BY priority) FROM templates WHERE kind = 'admin';
             INSERT INTO audit SELECT group_concat(label, '|' ORDER BY priority DESC) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2);
         INSERT INTO templates VALUES ('admin', 'B', 1);
         INSERT INTO templates VALUES ('guest', 'G', 0);
         INSERT INTO templates VALUES ('admin', 'C', 3);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db
        .query("SELECT summary FROM audit ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("B,A,C")], vec![Value::from("C|A|B")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT summary FROM audit ORDER BY rowid;");
    assert_eq!(cli_rows, "B,A,C\nC|A|B");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_group_concat_distinct_from_in_sqlite_file() {
    let fixture =
        writable_sqlite_fixture("execute-trigger-insert-select-group-concat-distinct-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(summary TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT group_concat(distinct label) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO templates VALUES ('admin', 'B');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT summary FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A,B")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT summary FROM audit;");
    assert_eq!(cli_rows, "A,B");
}

#[test]
fn rustsql_rejects_simple_trigger_insert_group_concat_distinct_separator_like_sqlite() {
    let fixture =
        writable_sqlite_fixture("reject-trigger-insert-group-concat-distinct-separator.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    let error = db
        .execute(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE templates(kind TEXT, label TEXT);
             CREATE TABLE audit(summary TEXT);
             CREATE TRIGGER trg_users_ai AFTER INSERT ON users
             BEGIN
                 INSERT INTO audit SELECT group_concat(distinct label, '|') FROM templates WHERE kind = 'admin';
             END;
             INSERT INTO templates VALUES ('admin', 'A');
             INSERT INTO templates VALUES ('admin', 'B');
             INSERT INTO users VALUES (1, 'admin');",
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("DISTINCT aggregates must have exactly one argument")
    );
}

#[test]
fn rustsql_rejects_simple_trigger_group_concat_distinct_separator_before_limit_like_sqlite() {
    let fixture =
        writable_sqlite_fixture("reject-trigger-group-concat-distinct-separator-before-limit.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    let error = db
        .execute(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE templates(kind TEXT, label TEXT);
             CREATE TABLE audit(summary TEXT);
             CREATE TRIGGER trg_users_ai AFTER INSERT ON users
             BEGIN
                 INSERT INTO audit SELECT group_concat(distinct label, '|') FROM templates WHERE kind = 'admin' LIMIT 0;
             END;
             INSERT INTO templates VALUES ('admin', 'A');
             INSERT INTO users VALUES (1, 'admin');",
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("DISTINCT aggregates must have exactly one argument")
    );
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_min_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-min-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT min(score) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('guest', 99);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO templates VALUES ('admin', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit;");
    assert_eq!(cli_rows, "1");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_max_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-max-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT max(score) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('guest', 99);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO templates VALUES ('admin', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(4)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit;");
    assert_eq!(cli_rows, "4");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_min_multi_arg_as_scalar_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-min-multi-arg-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER, fallback INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT min(score, fallback) FROM templates WHERE kind = 'admin' ORDER BY score;
         END;
         INSERT INTO templates VALUES ('admin', 3, 10);
         INSERT INTO templates VALUES ('guest', 99, 0);
         INSERT INTO templates VALUES ('admin', 5, 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(3)], vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY rowid;");
    assert_eq!(cli_rows, "3\n1");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_max_multi_arg_as_scalar_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-max-multi-arg-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER, fallback INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT max(score, fallback) FROM templates WHERE kind = 'admin' ORDER BY score;
         END;
         INSERT INTO templates VALUES ('admin', 3, 10);
         INSERT INTO templates VALUES ('guest', 99, 0);
         INSERT INTO templates VALUES ('admin', 5, 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Integer(10)], vec![Value::Integer(5)]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY rowid;");
    assert_eq!(cli_rows, "10\n5");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_min_max_distinct_multi_arg_as_scalar_like_sqlite()
{
    let fixture =
        writable_sqlite_fixture("execute-trigger-insert-select-min-max-distinct-multi-arg.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER, fallback INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT min(DISTINCT score, fallback) FROM templates WHERE kind = 'admin' ORDER BY score;
             INSERT INTO audit SELECT max(DISTINCT score, fallback) FROM templates WHERE kind = 'admin' ORDER BY score;
         END;
         INSERT INTO templates VALUES ('admin', 3, 10);
         INSERT INTO templates VALUES ('guest', 99, 0);
         INSERT INTO templates VALUES ('admin', 5, 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(3)],
            vec![Value::Integer(1)],
            vec![Value::Integer(10)],
            vec![Value::Integer(5)],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY rowid;");
    assert_eq!(cli_rows, "3\n1\n10\n5");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_star_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-star-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, label TEXT, score INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT * FROM templates WHERE kind = 'admin' ORDER BY score;
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2);
         INSERT INTO templates VALUES ('guest', 'G', 1);
         INSERT INTO templates VALUES ('admin', 'B', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT kind, label, score FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::from("B"), Value::Integer(1)],
            vec![Value::from("admin"), Value::from("A"), Value::Integer(2)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || label || '|' || score FROM audit;",
    );
    assert_eq!(cli_rows, "admin|B|1\nadmin|A|2");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_rowid_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-rowid-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(id INTEGER PRIMARY KEY, kind TEXT, label TEXT);
         CREATE TABLE audit(rid INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT rowid FROM templates WHERE kind = 'admin' ORDER BY rowid;
         END;
         INSERT INTO templates VALUES (5, 'admin', 'A');
         INSERT INTO templates VALUES (7, 'guest', 'G');
         INSERT INTO templates VALUES (9, 'admin', 'B');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT rid FROM audit ORDER BY rid;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(5)], vec![Value::Integer(9)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT rid FROM audit ORDER BY rid;");
    assert_eq!(cli_rows, "5\n9");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_qualified_star_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-qualified-star-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, score INTEGER);
         CREATE TABLE audit(kind TEXT, label TEXT, score INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT t.* FROM templates AS t WHERE t.kind = 'admin' ORDER BY score;
         END;
         INSERT INTO templates VALUES ('admin', 'A', 2);
         INSERT INTO templates VALUES ('guest', 'G', 1);
         INSERT INTO templates VALUES ('admin', 'B', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT kind, label, score FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("admin"), Value::from("B"), Value::Integer(1)],
            vec![Value::from("admin"), Value::from("A"), Value::Integer(2)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT kind || '|' || label || '|' || score FROM audit;",
    );
    assert_eq!(cli_rows, "admin|B|1\nadmin|A|2");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_order_by_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-order-by.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = 'admin' ORDER BY priority DESC;
         END;
         INSERT INTO templates VALUES ('admin', 'low', 1);
         INSERT INTO templates VALUES ('guest', 'guest', 9);
         INSERT INTO templates VALUES ('admin', 'high', 3);
         INSERT INTO templates VALUES ('admin', 'mid', 2);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("high")],
            vec![Value::from("mid")],
            vec![Value::from("low")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "high\nmid\nlow");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_order_by_alias_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-order-by-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label AS display FROM templates WHERE kind = 'admin' ORDER BY display DESC;
         END;
         INSERT INTO templates VALUES ('admin', 'b');
         INSERT INTO templates VALUES ('guest', 'z');
         INSERT INTO templates VALUES ('admin', 'a');
         INSERT INTO templates VALUES ('admin', 'c');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("c")],
            vec![Value::from("b")],
            vec![Value::from("a")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "c\nb\na");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_order_by_bare_alias_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-order-by-bare-alias.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label display FROM templates WHERE kind = 'admin' ORDER BY display DESC;
         END;
         INSERT INTO templates VALUES ('admin', 'b');
         INSERT INTO templates VALUES ('guest', 'z');
         INSERT INTO templates VALUES ('admin', 'a');
         INSERT INTO templates VALUES ('admin', 'c');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("c")],
            vec![Value::from("b")],
            vec![Value::from("a")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "c\nb\na");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_order_by_position_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-order-by-position.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = 'admin' ORDER BY 1 DESC;
         END;
         INSERT INTO templates VALUES ('admin', 'b');
         INSERT INTO templates VALUES ('guest', 'z');
         INSERT INTO templates VALUES ('admin', 'a');
         INSERT INTO templates VALUES ('admin', 'c');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("c")],
            vec![Value::from("b")],
            vec![Value::from("a")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "c\nb\na");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_order_by_multiple_terms_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-order-by-multiple.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = 'admin' ORDER BY priority DESC, label ASC;
         END;
         INSERT INTO templates VALUES ('admin', 'b', 2);
         INSERT INTO templates VALUES ('admin', 'a', 2);
         INSERT INTO templates VALUES ('admin', 'c', 3);
         INSERT INTO templates VALUES ('guest', 'z', 9);
         INSERT INTO templates VALUES ('admin', 'd', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("c")],
            vec![Value::from("a")],
            vec![Value::from("b")],
            vec![Value::from("d")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "c\na\nb\nd");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_order_by_nulls_like_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-order-by-nulls.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = 'admin' ORDER BY priority ASC, label ASC;
         END;
         INSERT INTO templates VALUES ('admin', 'middle', 2);
         INSERT INTO templates VALUES ('admin', 'nullish', NULL);
         INSERT INTO templates VALUES ('admin', 'first', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("nullish")],
            vec![Value::from("first")],
            vec![Value::from("middle")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "nullish\nfirst\nmiddle");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_order_by_nulls_last_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-order-by-nulls-last.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = 'admin' ORDER BY priority ASC NULLS LAST, label ASC;
         END;
         INSERT INTO templates VALUES ('admin', 'middle', 2);
         INSERT INTO templates VALUES ('admin', 'nullish', NULL);
         INSERT INTO templates VALUES ('admin', 'first', 1);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("first")],
            vec![Value::from("middle")],
            vec![Value::from("nullish")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "first\nmiddle\nnullish");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_order_by_collate_nocase_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-order-by-nocase.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = 'admin' ORDER BY label COLLATE NOCASE ASC;
         END;
         INSERT INTO templates VALUES ('admin', 'b');
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('admin', 'a');
         INSERT INTO templates VALUES ('admin', 'B');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("A")],
            vec![Value::from("a")],
            vec![Value::from("b")],
            vec![Value::from("B")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "A\na\nb\nB");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_distinct_from_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-distinct-from.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT DISTINCT label FROM templates WHERE kind = 'admin' ORDER BY label;
         END;
         INSERT INTO templates VALUES ('admin', 'B');
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'A');
         INSERT INTO templates VALUES ('admin', 'B');
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("B")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "A\nB");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_order_by_limit_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-order-by-limit.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = 'admin' ORDER BY priority DESC LIMIT 2;
         END;
         INSERT INTO templates VALUES ('admin', 'low', 1);
         INSERT INTO templates VALUES ('guest', 'guest', 9);
         INSERT INTO templates VALUES ('admin', 'high', 3);
         INSERT INTO templates VALUES ('admin', 'mid', 2);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("high")], vec![Value::from("mid")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "high\nmid");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_limit_offset_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-limit-offset.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = 'admin' ORDER BY priority DESC LIMIT 1 OFFSET 1;
         END;
         INSERT INTO templates VALUES ('admin', 'low', 1);
         INSERT INTO templates VALUES ('guest', 'guest', 9);
         INSERT INTO templates VALUES ('admin', 'high', 3);
         INSERT INTO templates VALUES ('admin', 'mid', 2);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("mid")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "mid");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_limit_comma_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-limit-comma.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = 'admin' ORDER BY priority DESC LIMIT 1, 2;
         END;
         INSERT INTO templates VALUES ('admin', 'low', 1);
         INSERT INTO templates VALUES ('guest', 'guest', 9);
         INSERT INTO templates VALUES ('admin', 'high', 3);
         INSERT INTO templates VALUES ('admin', 'mid', 2);
         INSERT INTO templates VALUES ('admin', 'top', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("high")], vec![Value::from("mid")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "high\nmid");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_negative_limit_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-negative-limit.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, priority INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind = 'admin' ORDER BY priority DESC LIMIT -1 OFFSET 1;
         END;
         INSERT INTO templates VALUES ('admin', 'low', 1);
         INSERT INTO templates VALUES ('guest', 'guest', 9);
         INSERT INTO templates VALUES ('admin', 'high', 3);
         INSERT INTO templates VALUES ('admin', 'mid', 2);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("mid")], vec![Value::from("low")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit;");
    assert_eq!(cli_rows, "mid\nlow");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_qualified_columns_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-qualified-columns.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT templates.label FROM templates WHERE templates.kind = new.name;
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'AA');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("AA")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nAA");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_alias_columns_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-alias-columns.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT t.label FROM templates AS t WHERE t.kind = new.name;
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'AA');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("AA")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nAA");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_like_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-like-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE label LIKE 'A%';
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'AA');
         INSERT INTO templates VALUES ('other', 'B');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("AA")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nAA");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_glob_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-glob-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE label GLOB 'A*';
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'AA');
         INSERT INTO templates VALUES ('other', 'B');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("AA")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nAA");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_in_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-in-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE kind IN ('admin', 'owner');
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'AA');
         INSERT INTO templates VALUES ('owner', 'O');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("A")],
            vec![Value::from("AA")],
            vec![Value::from("O")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nAA\nO");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_between_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-between-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT, score INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE score BETWEEN 10 AND 20;
         END;
         INSERT INTO templates VALUES ('admin', 'A', 10);
         INSERT INTO templates VALUES ('guest', 'G', 5);
         INSERT INTO templates VALUES ('admin', 'AA', 20);
         INSERT INTO templates VALUES ('owner', 'O', 21);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("AA")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nAA");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_and_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-and-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE label LIKE 'A%' AND kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'AA');
         INSERT INTO templates VALUES ('other', 'B');
         INSERT INTO templates VALUES ('guest', 'AB');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("AA")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nAA");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_or_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-or-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE label LIKE 'A%' OR kind = 'other';
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'AA');
         INSERT INTO templates VALUES ('other', 'B');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("A")],
            vec![Value::from("AA")],
            vec![Value::from("B")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nAA\nB");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_function_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-function-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT upper(label) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'alpha');
         INSERT INTO templates VALUES ('guest', 'guest');
         INSERT INTO templates VALUES ('admin', 'beta');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("ALPHA")], vec![Value::from("BETA")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "ALPHA\nBETA");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_function_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-function-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE lower(kind) = 'admin';
         END;
         INSERT INTO templates VALUES ('ADMIN', 'A');
         INSERT INTO templates VALUES ('guest', 'G');
         INSERT INTO templates VALUES ('admin', 'AA');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("AA")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nAA");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_length_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-length-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT length(label) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'alpha');
         INSERT INTO templates VALUES ('guest', 'guest');
         INSERT INTO templates VALUES ('admin', 'bb');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(5)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "2\n5");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_substr_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-substr-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE substr(label, 1, 1) = 'a';
         END;
         INSERT INTO templates VALUES ('admin', 'alpha');
         INSERT INTO templates VALUES ('guest', 'guest');
         INSERT INTO templates VALUES ('admin', 'beta');
         INSERT INTO templates VALUES ('admin', 'arc');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("alpha")], vec![Value::from("arc")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "alpha\narc");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_concat_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-concat-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(value TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label || ':' || kind FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'alpha');
         INSERT INTO templates VALUES ('guest', 'guest');
         INSERT INTO templates VALUES ('admin', 'beta');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alpha:admin")],
            vec![Value::from("beta:admin")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "alpha:admin\nbeta:admin");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_add_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-add-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(label TEXT, score INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE score + 1 = 8;
         END;
         INSERT INTO templates VALUES ('low', 6);
         INSERT INTO templates VALUES ('hit', 7);
         INSERT INTO templates VALUES ('high', 8);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("hit")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "hit");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_multiply_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-multiply-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT score * 2 FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3);
         INSERT INTO templates VALUES ('guest', 4);
         INSERT INTO templates VALUES ('admin', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Integer(6)], vec![Value::Integer(10)]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "6\n10");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_subtract_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-subtract-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(label TEXT, score INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE score - 1 = 6;
         END;
         INSERT INTO templates VALUES ('low', 6);
         INSERT INTO templates VALUES ('hit', 7);
         INSERT INTO templates VALUES ('high', 8);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("hit")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "hit");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_modulo_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-modulo-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(label TEXT, score INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE score % 2 = 0;
         END;
         INSERT INTO templates VALUES ('one', 1);
         INSERT INTO templates VALUES ('two', 2);
         INSERT INTO templates VALUES ('four', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("four")], vec![Value::from("two")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "four\ntwo");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_divide_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-divide-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT score / 2 FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 8);
         INSERT INTO templates VALUES ('guest', 9);
         INSERT INTO templates VALUES ('admin', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(4)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "2\n4");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_bitand_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-bitand-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(label TEXT, flags INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE flags & 2 = 2;
         END;
         INSERT INTO templates VALUES ('one', 1);
         INSERT INTO templates VALUES ('two', 2);
         INSERT INTO templates VALUES ('three', 3);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("three")], vec![Value::from("two")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "three\ntwo");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_bitor_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-bitor-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(label TEXT, flags INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT flags | 2 FROM templates WHERE label != 'skip';
         END;
         INSERT INTO templates VALUES ('a', 1);
         INSERT INTO templates VALUES ('b', 4);
         INSERT INTO templates VALUES ('skip', 8);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(3)], vec![Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "3\n6");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_shift_left_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-shift-left.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT score << 1 FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 3);
         INSERT INTO templates VALUES ('admin', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Integer(4)], vec![Value::Integer(10)]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "4\n10");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_shift_right_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-shift-right.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, score INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT score >> 1 FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 3);
         INSERT INTO templates VALUES ('admin', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "1\n2");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_cast_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-cast-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT CAST(label AS INTEGER) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', '12');
         INSERT INTO templates VALUES ('guest', '99');
         INSERT INTO templates VALUES ('admin', '7x');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Integer(7)], vec![Value::Integer(12)]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "7\n12");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_case_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-case-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(label TEXT, score INTEGER);
         CREATE TABLE audit(value TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT CASE WHEN score >= 7 THEN label ELSE 'low' END FROM templates;
         END;
         INSERT INTO templates VALUES ('alice', 8);
         INSERT INTO templates VALUES ('bob', 5);
         INSERT INTO templates VALUES ('carol', 7);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice")],
            vec![Value::from("carol")],
            vec![Value::from("low")],
        ]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "alice\ncarol\nlow");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_is_true_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-is-true-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(label TEXT, active INTEGER);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE active IS TRUE;
         END;
         INSERT INTO templates VALUES ('yes', 1);
         INSERT INTO templates VALUES ('no', 0);
         INSERT INTO templates VALUES ('two', 2);
         INSERT INTO templates VALUES ('null', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("two")], vec![Value::from("yes")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "two\nyes");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_bitnot_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-bitnot-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, flags INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT ~flags FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 0);
         INSERT INTO templates VALUES ('guest', 1);
         INSERT INTO templates VALUES ('admin', 5);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Integer(-6)], vec![Value::Integer(-1)]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "-6\n-1");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_unicode_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-unicode-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT unicode(label) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'Alice');
         INSERT INTO templates VALUES ('guest', 'Guest');
         INSERT INTO templates VALUES ('admin', 'ßeta');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Integer(65)], vec![Value::Integer(223)]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "65\n223");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_char_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-char-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, code INTEGER);
         CREATE TABLE audit(value TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT char(code) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 65);
         INSERT INTO templates VALUES ('guest', 90);
         INSERT INTO templates VALUES ('admin', 66);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("B")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "A\nB");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_zeroblob_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-zeroblob-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, size INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT length(zeroblob(size)) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 2);
         INSERT INTO templates VALUES ('guest', 7);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(4)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "2\n4");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_coalesce_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-coalesce-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(value TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT coalesce(label, 'fallback') FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'alpha');
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO templates VALUES ('guest', 'guest');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("alpha")], vec![Value::from("fallback")],]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "alpha\nfallback");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_nullif_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-nullif-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE nullif(kind, 'skip') IS NOT NULL;
         END;
         INSERT INTO templates VALUES ('admin', 'A');
         INSERT INTO templates VALUES ('skip', 'S');
         INSERT INTO templates VALUES ('owner', 'O');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("A")], vec![Value::from("O")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "A\nO");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_trim_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-trim-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(value TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT trim(label) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', ' alpha ');
         INSERT INTO templates VALUES ('guest', ' guest ');
         INSERT INTO templates VALUES ('admin', ' beta ');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("alpha")], vec![Value::from("beta")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "alpha\nbeta");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_replace_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-replace-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE replace(label, '-', '') = 'ab';
         END;
         INSERT INTO templates VALUES ('admin', 'a-b');
         INSERT INTO templates VALUES ('guest', 'a-c');
         INSERT INTO templates VALUES ('admin', 'ab');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("a-b")], vec![Value::from("ab")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "a-b\nab");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_abs_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-abs-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, delta INTEGER);
         CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT abs(delta) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', -7);
         INSERT INTO templates VALUES ('guest', -3);
         INSERT INTO templates VALUES ('admin', 4);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(4)], vec![Value::Integer(7)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "4\n7");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_instr_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-instr-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(label TEXT);
         CREATE TABLE audit(label TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT label FROM templates WHERE instr(label, '-') > 0;
         END;
         INSERT INTO templates VALUES ('a-b');
         INSERT INTO templates VALUES ('ab');
         INSERT INTO templates VALUES ('c-d');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT label FROM audit ORDER BY label;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("a-b")], vec![Value::from("c-d")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT label FROM audit ORDER BY label;");
    assert_eq!(cli_rows, "a-b\nc-d");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_round_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-round-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, reading REAL);
         CREATE TABLE audit(value REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT round(reading, 1) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3.14);
         INSERT INTO templates VALUES ('guest', 2.22);
         INSERT INTO templates VALUES ('admin', 4.05);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(rows, vec![vec![Value::Real(3.1)], vec![Value::Real(4.0)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "3.1\n4.0");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_typeof_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-typeof-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, reading REAL);
         CREATE TABLE audit(value TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT typeof(reading) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 3.14);
         INSERT INTO templates VALUES ('guest', 2.0);
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("null")], vec![Value::from("real")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "null\nreal");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_quote_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-quote-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, label TEXT);
         CREATE TABLE audit(value TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT quote(label) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', 'a''b');
         INSERT INTO templates VALUES ('guest', 'g');
         INSERT INTO templates VALUES ('admin', NULL);
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("'a''b'")], vec![Value::from("NULL")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "'a''b'\nNULL");
}

#[test]
fn rustsql_executes_simple_trigger_insert_select_from_hex_projection_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-select-hex-projection.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE templates(kind TEXT, payload BLOB);
         CREATE TABLE audit(value TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit SELECT hex(payload) FROM templates WHERE kind = 'admin';
         END;
         INSERT INTO templates VALUES ('admin', X'0A0B');
         INSERT INTO templates VALUES ('guest', X'FF');
         INSERT INTO templates VALUES ('admin', X'6162');
         INSERT INTO users VALUES (1, 'admin');",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit ORDER BY value;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("0A0B")], vec![Value::from("6162")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit ORDER BY value;");
    assert_eq!(cli_rows, "0A0B\n6162");
}

#[test]
fn rustsql_executes_simple_trigger_insert_unary_minus_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-unary-minus-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, delta INTEGER);
         CREATE TABLE audit(delta INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (-new.delta);
         END;
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db.query("SELECT delta FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(-7)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT delta FROM audit;");
    assert_eq!(cli_rows, "-7");
}

#[test]
fn rustsql_executes_simple_trigger_insert_not_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-not-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, active INTEGER);
         CREATE TABLE audit(active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (NOT new.active);
         END;
         INSERT INTO users VALUES (1, 1);",
    )
    .unwrap();

    let rows = db
        .query("SELECT active, typeof(active) FROM audit;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(0), Value::from("integer")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT active || '|' || typeof(active) FROM audit;",
    );
    assert_eq!(cli_rows, "0|integer");
}

#[test]
fn rustsql_executes_simple_trigger_insert_compare_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-compare-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE audit(active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.score > 0);
         END;
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db
        .query("SELECT active, typeof(active) FROM audit;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("integer")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT active || '|' || typeof(active) FROM audit;",
    );
    assert_eq!(cli_rows, "1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_insert_is_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-is-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, v TEXT);
         CREATE TABLE audit(active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.v IS NULL);
         END;
         INSERT INTO users VALUES (1, NULL);",
    )
    .unwrap();

    let rows = db
        .query("SELECT active, typeof(active) FROM audit;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("integer")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT active || '|' || typeof(active) FROM audit;",
    );
    assert_eq!(cli_rows, "1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_insert_is_true_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-is-true-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, active INTEGER);
         CREATE TABLE audit(active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.active IS TRUE);
         END;
         INSERT INTO users VALUES (1, 1);",
    )
    .unwrap();

    let rows = db
        .query("SELECT active, typeof(active) FROM audit;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("integer")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT active || '|' || typeof(active) FROM audit;",
    );
    assert_eq!(cli_rows, "1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_insert_between_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-between-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE audit(active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.score BETWEEN 1 AND 10);
         END;
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db
        .query("SELECT active, typeof(active) FROM audit;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("integer")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT active || '|' || typeof(active) FROM audit;",
    );
    assert_eq!(cli_rows, "1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_insert_in_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-in-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.name IN ('alice', 'bob'));
         END;
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT active, typeof(active) FROM audit;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("integer")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT active || '|' || typeof(active) FROM audit;",
    );
    assert_eq!(cli_rows, "1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_insert_like_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-like-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.name LIKE 'a%');
         END;
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT active, typeof(active) FROM audit;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("integer")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT active || '|' || typeof(active) FROM audit;",
    );
    assert_eq!(cli_rows, "1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_insert_cast_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-cast-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(v INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (CAST(new.name AS INTEGER));
         END;
         INSERT INTO users VALUES (1, '42');",
    )
    .unwrap();

    let rows = db.query("SELECT v, typeof(v) FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(42), Value::from("integer")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT v || '|' || typeof(v) FROM audit;");
    assert_eq!(cli_rows, "42|integer");
}

#[test]
fn rustsql_executes_simple_trigger_insert_bitnot_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-bitnot-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, flags INTEGER);
         CREATE TABLE audit(flags INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (~new.flags);
         END;
         INSERT INTO users VALUES (1, 5);",
    )
    .unwrap();

    let rows = db.query("SELECT flags FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(-6)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT flags FROM audit;");
    assert_eq!(cli_rows, "-6");
}

#[test]
fn rustsql_executes_simple_trigger_insert_add_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-add-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE audit(score INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.score + 1);
         END;
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db.query("SELECT score FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(8)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT score FROM audit;");
    assert_eq!(cli_rows, "8");
}

#[test]
fn rustsql_executes_simple_trigger_insert_subtract_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-subtract-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE audit(score INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.score - 1);
         END;
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db.query("SELECT score FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT score FROM audit;");
    assert_eq!(cli_rows, "6");
}

#[test]
fn rustsql_executes_simple_trigger_insert_multiply_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-multiply-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE audit(score INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.score * 2);
         END;
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db.query("SELECT score FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(14)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT score FROM audit;");
    assert_eq!(cli_rows, "14");
}

#[test]
fn rustsql_executes_simple_trigger_insert_divide_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-divide-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE audit(score INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.score / 2);
         END;
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db.query("SELECT score FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(3)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT score FROM audit;");
    assert_eq!(cli_rows, "3");
}

#[test]
fn rustsql_executes_simple_trigger_insert_modulo_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-modulo-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE audit(score INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.score % 3);
         END;
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db.query("SELECT score FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT score FROM audit;");
    assert_eq!(cli_rows, "1");
}

#[test]
fn rustsql_executes_simple_trigger_insert_bitand_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-bitand-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, flags INTEGER);
         CREATE TABLE audit(flags INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.flags & 3);
         END;
         INSERT INTO users VALUES (1, 6);",
    )
    .unwrap();

    let rows = db.query("SELECT flags FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT flags FROM audit;");
    assert_eq!(cli_rows, "2");
}

#[test]
fn rustsql_executes_simple_trigger_insert_bitor_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-bitor-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, flags INTEGER);
         CREATE TABLE audit(flags INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.flags | 1);
         END;
         INSERT INTO users VALUES (1, 4);",
    )
    .unwrap();

    let rows = db.query("SELECT flags FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(5)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT flags FROM audit;");
    assert_eq!(cli_rows, "5");
}

#[test]
fn rustsql_executes_simple_trigger_insert_shift_left_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-shift-left-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, flags INTEGER);
         CREATE TABLE audit(flags INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.flags << 1);
         END;
         INSERT INTO users VALUES (1, 5);",
    )
    .unwrap();

    let rows = db.query("SELECT flags FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(10)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT flags FROM audit;");
    assert_eq!(cli_rows, "10");
}

#[test]
fn rustsql_executes_simple_trigger_insert_shift_right_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-shift-right-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, flags INTEGER);
         CREATE TABLE audit(flags INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (new.flags >> 1);
         END;
         INSERT INTO users VALUES (1, 5);",
    )
    .unwrap();

    let rows = db.query("SELECT flags FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT flags FROM audit;");
    assert_eq!(cli_rows, "2");
}

#[test]
fn rustsql_executes_simple_trigger_insert_coalesce_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-coalesce-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (coalesce(new.name, 'unknown'));
         END;
         INSERT INTO users VALUES (1, NULL);",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("unknown")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit;");
    assert_eq!(cli_rows, "unknown");
}

#[test]
fn rustsql_executes_simple_trigger_insert_ifnull_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-ifnull-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (ifnull(new.name, 'unknown'));
         END;
         INSERT INTO users VALUES (1, NULL);",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("unknown")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit;");
    assert_eq!(cli_rows, "unknown");
}

#[test]
fn rustsql_executes_simple_trigger_insert_abs_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-abs-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, delta INTEGER);
         CREATE TABLE audit(delta INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (abs(new.delta));
         END;
         INSERT INTO users VALUES (1, -7);",
    )
    .unwrap();

    let rows = db.query("SELECT delta FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(7)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT delta FROM audit;");
    assert_eq!(cli_rows, "7");
}

#[test]
fn rustsql_executes_simple_trigger_insert_length_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-length-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(len INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (length(new.name));
         END;
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT len FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(5)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT len FROM audit;");
    assert_eq!(cli_rows, "5");
}

#[test]
fn rustsql_executes_simple_trigger_insert_substr_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-substr-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(prefix TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (substr(new.name, 1, 3));
         END;
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT prefix FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("ali")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT prefix FROM audit;");
    assert_eq!(cli_rows, "ali");
}

#[test]
fn rustsql_executes_simple_trigger_insert_trim_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-trim-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (trim(new.name, '.'));
         END;
         INSERT INTO users VALUES (1, '..alice..');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit;");
    assert_eq!(cli_rows, "alice");
}

#[test]
fn rustsql_executes_simple_trigger_insert_replace_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-replace-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (replace(new.name, '-', '_'));
         END;
         INSERT INTO users VALUES (1, 'a-b-c');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("a_b_c")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit;");
    assert_eq!(cli_rows, "a_b_c");
}

#[test]
fn rustsql_executes_simple_trigger_insert_instr_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-instr-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(pos INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (instr(new.name, '-'));
         END;
         INSERT INTO users VALUES (1, 'ab-c');",
    )
    .unwrap();

    let rows = db.query("SELECT pos FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(3)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT pos FROM audit;");
    assert_eq!(cli_rows, "3");
}

#[test]
fn rustsql_executes_simple_trigger_insert_round_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-round-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, reading REAL);
         CREATE TABLE audit(value REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (round(new.reading, 1));
         END;
         INSERT INTO users VALUES (1, 3.14159);",
    )
    .unwrap();

    let rows = db.query("SELECT value FROM audit;").unwrap();
    assert_eq!(rows, vec![vec![Value::Real(3.1)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT value FROM audit;");
    assert_eq!(cli_rows, "3.1");
}

#[test]
fn rustsql_executes_simple_trigger_insert_misc_scalar_expressions_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-misc-scalar-expressions.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, reading REAL, name TEXT);
         CREATE TABLE audit(t TEXT, q TEXT, u INTEGER, c TEXT, h TEXT, n TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (
                 typeof(new.reading),
                 quote(new.name),
                 unicode(new.name),
                 char(65, 66),
                 hex(new.name),
                 nullif(new.name, 'skip')
             );
         END;
         INSERT INTO users VALUES (1, 3.14, 'Az');",
    )
    .unwrap();

    let rows = db.query("SELECT t, q, u, c, h, n FROM audit;").unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("real"),
            Value::from("'Az'"),
            Value::Integer(65),
            Value::from("AB"),
            Value::from("417A"),
            Value::from("Az"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT t || '|' || q || '|' || u || '|' || c || '|' || h || '|' || ifnull(n, 'NULL') FROM audit;",
    );
    assert_eq!(cli_rows, "real|'Az'|65|AB|417A|Az");
}

#[test]
fn rustsql_executes_simple_trigger_insert_zeroblob_expression_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-insert-zeroblob-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, n INTEGER);
         CREATE TABLE audit(body BLOB);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (zeroblob(new.n));
         END;
         INSERT INTO users VALUES (1, 3);",
    )
    .unwrap();

    let rows = db
        .query("SELECT length(body), quote(body) FROM audit;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Integer(3), Value::from("X'000000'")]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT length(body) || '|' || quote(body) FROM audit;",
    );
    assert_eq!(cli_rows, "3|X'000000'");
}

#[test]
fn rustsql_executes_simple_trigger_with_multiple_insert_body_statements_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-multiple-inserts.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(seq INTEGER PRIMARY KEY, user_id INTEGER, tag TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (NULL, new.id, 'first');
             INSERT INTO audit VALUES (NULL, new.id, 'second');
         END;
         INSERT INTO users VALUES (8, 'ivy');",
    )
    .unwrap();

    let rows = db
        .query("SELECT user_id, tag FROM audit ORDER BY seq;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(8), Value::from("first")],
            vec![Value::Integer(8), Value::from("second")],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT seq || '|' || user_id || '|' || tag FROM audit ORDER BY seq;",
    );
    assert_eq!(cli_rows, "1|8|first\n2|8|second");
}

#[test]
fn rustsql_executes_simple_trigger_insert_with_multi_row_values_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-multi-row-values.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(seq INTEGER PRIMARY KEY, user_id INTEGER, tag TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (NULL, new.id, 'first'), (NULL, new.id, 'second');
         END;
         INSERT INTO users VALUES (10, 'kate');",
    )
    .unwrap();

    let rows = db
        .query("SELECT user_id, tag FROM audit ORDER BY seq;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(10), Value::from("first")],
            vec![Value::Integer(10), Value::from("second")],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT seq || '|' || user_id || '|' || tag FROM audit ORDER BY seq;",
    );
    assert_eq!(cli_rows, "1|10|first\n2|10|second");
}

#[test]
fn rustsql_executes_simple_before_insert_trigger_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-before-insert-trigger.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(seq INTEGER PRIMARY KEY, user_id INTEGER, name TEXT, tag TEXT);
         CREATE TRIGGER trg_users_bi BEFORE INSERT ON users
         BEGIN
             INSERT INTO audit VALUES (NULL, new.id, new.name, 'before');
         END;
         INSERT INTO users VALUES (3, 'dina');",
    )
    .unwrap();

    let audit_rows = db
        .query("SELECT user_id, name, tag FROM audit ORDER BY seq;")
        .unwrap();
    assert_eq!(
        audit_rows,
        vec![vec![
            Value::Integer(3),
            Value::from("dina"),
            Value::from("before"),
        ]]
    );
    let user_rows = db.query("SELECT id, name FROM users;").unwrap();
    assert_eq!(
        user_rows,
        vec![vec![Value::Integer(3), Value::from("dina")]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT seq || '|' || user_id || '|' || name || '|' || tag FROM audit ORDER BY seq;",
    );
    assert_eq!(cli_rows, "1|3|dina|before");
}

#[test]
fn rustsql_executes_simple_after_update_trigger_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-update-trigger.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(old_name TEXT, new_name TEXT, tag TEXT);
         CREATE TRIGGER trg_users_au AFTER UPDATE ON users
         BEGIN
             INSERT INTO audit VALUES (old.name, new.name, 'updated');
         END;
         INSERT INTO users VALUES (1, 'alice');
         UPDATE users SET name = 'bob' WHERE id = 1;",
    )
    .unwrap();

    let rows = db
        .query("SELECT old_name, new_name, tag FROM audit;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("alice"),
            Value::from("bob"),
            Value::from("updated"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT old_name || '|' || new_name || '|' || tag FROM audit;",
    );
    assert_eq!(cli_rows, "alice|bob|updated");
}

#[test]
fn rustsql_executes_simple_after_update_of_trigger_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-update-of-trigger.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(seq INTEGER PRIMARY KEY, old_name TEXT, new_name TEXT, tag TEXT);
         CREATE TRIGGER trg_users_au_name AFTER UPDATE OF name ON users
         BEGIN
             INSERT INTO audit VALUES (NULL, old.name, new.name, 'name-updated');
         END;
         INSERT INTO users VALUES (1, 'alice');
         UPDATE users SET id = id WHERE id = 1;
         UPDATE users SET name = name WHERE id = 1;",
    )
    .unwrap();

    let audit_rows = db
        .query("SELECT old_name, new_name, tag FROM audit ORDER BY seq;")
        .unwrap();
    assert_eq!(
        audit_rows,
        vec![vec![
            Value::from("alice"),
            Value::from("alice"),
            Value::from("name-updated"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT seq || '|' || old_name || '|' || new_name || '|' || tag FROM audit ORDER BY seq;",
    );
    assert_eq!(cli_rows, "1|alice|alice|name-updated");
}

#[test]
fn rustsql_executes_simple_before_update_trigger_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-before-update-trigger.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(seq INTEGER PRIMARY KEY, old_name TEXT, new_name TEXT, tag TEXT);
         CREATE TRIGGER trg_users_bu BEFORE UPDATE ON users
         BEGIN
             INSERT INTO audit VALUES (NULL, old.name, new.name, 'before-update');
         END;
         INSERT INTO users VALUES (5, 'faye');
         UPDATE users SET name = 'gina' WHERE id = 5;",
    )
    .unwrap();

    let audit_rows = db
        .query("SELECT old_name, new_name, tag FROM audit ORDER BY seq;")
        .unwrap();
    assert_eq!(
        audit_rows,
        vec![vec![
            Value::from("faye"),
            Value::from("gina"),
            Value::from("before-update"),
        ]]
    );
    let user_rows = db.query("SELECT id, name FROM users;").unwrap();
    assert_eq!(
        user_rows,
        vec![vec![Value::Integer(5), Value::from("gina")]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT seq || '|' || old_name || '|' || new_name || '|' || tag FROM audit ORDER BY seq;",
    );
    assert_eq!(cli_rows, "1|faye|gina|before-update");
}

#[test]
fn rustsql_executes_simple_after_delete_trigger_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-after-delete-trigger.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(user_id INTEGER, name TEXT, tag TEXT);
         CREATE TRIGGER trg_users_ad AFTER DELETE ON users
         BEGIN
             INSERT INTO audit VALUES (old.id, old.name, 'deleted');
         END;
         INSERT INTO users VALUES (2, 'carol');
         DELETE FROM users WHERE id = 2;",
    )
    .unwrap();

    let rows = db
        .query("SELECT user_id, name, tag FROM audit ORDER BY user_id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(2),
            Value::from("carol"),
            Value::from("deleted"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || name || '|' || tag FROM audit ORDER BY user_id;",
    );
    assert_eq!(cli_rows, "2|carol|deleted");
}

#[test]
fn rustsql_executes_simple_trigger_delete_body_statement_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-delete-body.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE cache(user_id INTEGER, note TEXT);
         CREATE TRIGGER trg_users_ad AFTER DELETE ON users
         BEGIN
             DELETE FROM cache WHERE user_id = old.id;
         END;
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO cache VALUES (1, 'drop');
         INSERT INTO cache VALUES (2, 'keep');
         DELETE FROM users WHERE id = 1;",
    )
    .unwrap();

    let rows = db
        .query("SELECT user_id, note FROM cache ORDER BY user_id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2), Value::from("keep")]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || note FROM cache ORDER BY user_id;",
    );
    assert_eq!(cli_rows, "2|keep");
}

#[test]
fn rustsql_executes_simple_trigger_delete_body_expression_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-delete-body-expression-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(name TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             DELETE FROM audit WHERE length(name) = 3;
         END;
         INSERT INTO audit VALUES ('bob');
         INSERT INTO audit VALUES ('alice');
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM audit ORDER BY name;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name FROM audit ORDER BY name;");
    assert_eq!(cli_rows, "alice");
}

#[test]
fn rustsql_executes_simple_trigger_delete_body_without_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-delete-body-no-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE cache(user_id INTEGER, note TEXT);
         CREATE TRIGGER trg_users_ad AFTER DELETE ON users
         BEGIN
             DELETE FROM cache;
         END;
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO cache VALUES (1, 'drop');
         INSERT INTO cache VALUES (2, 'drop-too');
         DELETE FROM users WHERE id = 1;",
    )
    .unwrap();

    let rows = db.query("SELECT COUNT(*) FROM cache;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(0)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT COUNT(*) FROM cache;");
    assert_eq!(cli_rows, "0");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_statement_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 0);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, cnt FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || cnt FROM stats;");
    assert_eq!(cli_rows, "alice|1");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_without_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-no-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1;
         END;
         INSERT INTO stats VALUES ('a', 0);
         INSERT INTO stats VALUES ('b', 10);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("a"), Value::Integer(1)],
            vec![Value::from("b"), Value::Integer(11)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "a|1\nb|11");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_not_equal_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-not-equal.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name != new.name;
         END;
         INSERT INTO stats VALUES ('alice', 0);
         INSERT INTO stats VALUES ('bob', 10);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::Integer(0)],
            vec![Value::from("bob"), Value::Integer(11)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice|0\nbob|11");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_lte_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-lte-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE rules(threshold INTEGER PRIMARY KEY, hits INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE rules SET hits = hits + 1 WHERE threshold <= new.score;
         END;
         INSERT INTO rules VALUES (5, 0);
         INSERT INTO rules VALUES (10, 0);
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db
        .query("SELECT threshold, hits FROM rules ORDER BY threshold;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(5), Value::Integer(1)],
            vec![Value::Integer(10), Value::Integer(0)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT threshold || '|' || hits FROM rules ORDER BY threshold;",
    );
    assert_eq!(cli_rows, "5|1\n10|0");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_between_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-between-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE rules(threshold INTEGER, hits INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE rules SET hits = hits + 1 WHERE threshold BETWEEN 5 AND new.score;
         END;
         INSERT INTO rules VALUES (3, 0);
         INSERT INTO rules VALUES (5, 10);
         INSERT INTO rules VALUES (7, 20);
         INSERT INTO rules VALUES (10, 30);
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db
        .query("SELECT threshold, hits FROM rules ORDER BY threshold;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(3), Value::Integer(0)],
            vec![Value::Integer(5), Value::Integer(11)],
            vec![Value::Integer(7), Value::Integer(21)],
            vec![Value::Integer(10), Value::Integer(30)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT threshold || '|' || hits FROM rules ORDER BY threshold;",
    );
    assert_eq!(cli_rows, "3|0\n5|11\n7|21\n10|30");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_not_between_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-not-between-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE rules(threshold INTEGER, hits INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE rules SET hits = hits + 1 WHERE threshold NOT BETWEEN 5 AND new.score;
         END;
         INSERT INTO rules VALUES (3, 0);
         INSERT INTO rules VALUES (5, 10);
         INSERT INTO rules VALUES (7, 20);
         INSERT INTO rules VALUES (10, 30);
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db
        .query("SELECT threshold, hits FROM rules ORDER BY threshold;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(3), Value::Integer(1)],
            vec![Value::Integer(5), Value::Integer(10)],
            vec![Value::Integer(7), Value::Integer(20)],
            vec![Value::Integer(10), Value::Integer(31)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT threshold || '|' || hits FROM rules ORDER BY threshold;",
    );
    assert_eq!(cli_rows, "3|1\n5|10\n7|20\n10|31");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_expression_left_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-expression-left-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE length(name) = 5;
         END;
         INSERT INTO stats VALUES ('alice', 0);
         INSERT INTO stats VALUES ('bob', 10);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::Integer(1)],
            vec![Value::from("bob"), Value::Integer(10)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice|1\nbob|10");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_expression_right_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-expression-right-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name = lower(name);
         END;
         INSERT INTO stats VALUES ('alice', 0);
         INSERT INTO stats VALUES ('ALICE', 10);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("ALICE"), Value::Integer(10)],
            vec![Value::from("alice"), Value::Integer(1)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "ALICE|10\nalice|1");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_compare_operator_inside_expression_in_sqlite_file() {
    let fixture =
        writable_sqlite_fixture("execute-trigger-update-body-compare-operator-in-expression.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, marker TEXT, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE replace(marker, '=', '') = 'ab';
         END;
         INSERT INTO stats VALUES ('alice', 'a=b', 0);
         INSERT INTO stats VALUES ('bob', 'a', 10);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, marker, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::from("a=b"), Value::Integer(1)],
            vec![Value::from("bob"), Value::from("a"), Value::Integer(10)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || marker || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice|a=b|1\nbob|a|10");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_in_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-in-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name IN ('alice', new.name);
         END;
         INSERT INTO stats VALUES ('alice', 0);
         INSERT INTO stats VALUES ('bob', 10);
         INSERT INTO stats VALUES ('carol', 20);
         INSERT INTO users VALUES (1, 'bob');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::Integer(1)],
            vec![Value::from("bob"), Value::Integer(11)],
            vec![Value::from("carol"), Value::Integer(20)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice|1\nbob|11\ncarol|20");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_not_in_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-not-in-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name NOT IN ('alice', new.name);
         END;
         INSERT INTO stats VALUES ('alice', 0);
         INSERT INTO stats VALUES ('bob', 10);
         INSERT INTO stats VALUES ('carol', 20);
         INSERT INTO users VALUES (1, 'bob');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::Integer(0)],
            vec![Value::from("bob"), Value::Integer(10)],
            vec![Value::from("carol"), Value::Integer(21)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice|0\nbob|10\ncarol|21");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_is_null_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-is-null-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, marker TEXT, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE marker IS NULL;
         END;
         INSERT INTO stats VALUES ('a', NULL, 0);
         INSERT INTO stats VALUES ('b', 'x', 10);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, marker, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("a"), Value::Null, Value::Integer(1)],
            vec![Value::from("b"), Value::from("x"), Value::Integer(10)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || ifnull(marker, 'NULL') || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "a|NULL|1\nb|x|10");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_is_not_null_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-is-not-null-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, marker TEXT, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE marker IS NOT NULL;
         END;
         INSERT INTO stats VALUES ('a', NULL, 0);
         INSERT INTO stats VALUES ('b', 'x', 10);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, marker, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("a"), Value::Null, Value::Integer(0)],
            vec![Value::from("b"), Value::from("x"), Value::Integer(11)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || ifnull(marker, 'NULL') || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "a|NULL|0\nb|x|11");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_is_value_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-is-value-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, marker TEXT, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE marker IS 'x';
         END;
         INSERT INTO stats VALUES ('alice', 'x', 0);
         INSERT INTO stats VALUES ('bob', NULL, 10);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, marker, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::from("x"), Value::Integer(1)],
            vec![Value::from("bob"), Value::Null, Value::Integer(10)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || ifnull(marker, 'NULL') || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice|x|1\nbob|NULL|10");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_is_not_value_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-is-not-value-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, marker TEXT, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE marker IS NOT 'x';
         END;
         INSERT INTO stats VALUES ('alice', 'x', 0);
         INSERT INTO stats VALUES ('bob', NULL, 10);
         INSERT INTO stats VALUES ('carol', 'y', 20);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, marker, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::from("x"), Value::Integer(0)],
            vec![Value::from("bob"), Value::Null, Value::Integer(11)],
            vec![Value::from("carol"), Value::from("y"), Value::Integer(21)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || ifnull(marker, 'NULL') || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice|x|0\nbob|NULL|11\ncarol|y|21");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_and_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-and-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE rules(threshold INTEGER, marker TEXT, hits INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE rules SET hits = hits + 1 WHERE threshold <= new.score AND marker IS NOT NULL;
         END;
         INSERT INTO rules VALUES (5, 'x', 0);
         INSERT INTO rules VALUES (10, 'x', 0);
         INSERT INTO rules VALUES (3, NULL, 0);
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db
        .query("SELECT threshold, marker, hits FROM rules ORDER BY threshold;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(3), Value::Null, Value::Integer(0)],
            vec![Value::Integer(5), Value::from("x"), Value::Integer(1)],
            vec![Value::Integer(10), Value::from("x"), Value::Integer(0)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT threshold || '|' || ifnull(marker, 'NULL') || '|' || hits FROM rules ORDER BY threshold;",
    );
    assert_eq!(cli_rows, "3|NULL|0\n5|x|1\n10|x|0");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_or_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-or-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, marker TEXT, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name = new.name OR marker IS NULL;
         END;
         INSERT INTO stats VALUES ('alice', 'x', 0);
         INSERT INTO stats VALUES ('bob', NULL, 10);
         INSERT INTO stats VALUES ('carol', 'x', 20);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, marker, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::from("x"), Value::Integer(1)],
            vec![Value::from("bob"), Value::Null, Value::Integer(11)],
            vec![Value::from("carol"), Value::from("x"), Value::Integer(20)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || ifnull(marker, 'NULL') || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice|x|1\nbob|NULL|11\ncarol|x|20");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_like_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-like-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name LIKE new.name || '%';
         END;
         INSERT INTO stats VALUES ('alice-1', 0);
         INSERT INTO stats VALUES ('bob-1', 10);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice-1"), Value::Integer(1)],
            vec![Value::from("bob-1"), Value::Integer(10)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice-1|1\nbob-1|10");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_not_like_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-not-like-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name NOT LIKE new.name || '%';
         END;
         INSERT INTO stats VALUES ('alice-1', 0);
         INSERT INTO stats VALUES ('bob-1', 10);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice-1"), Value::Integer(0)],
            vec![Value::from("bob-1"), Value::Integer(11)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice-1|0\nbob-1|11");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_like_escape_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-like-escape-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name LIKE 'a!_%' ESCAPE '!';
         END;
         INSERT INTO stats VALUES ('a_foo', 0);
         INSERT INTO stats VALUES ('abfoo', 10);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("a_foo"), Value::Integer(1)],
            vec![Value::from("abfoo"), Value::Integer(10)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "a_foo|1\nabfoo|10");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_like_dynamic_escape_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-like-dynamic-escape.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name LIKE 'a!_%' ESCAPE ('!' || '');
         END;
         INSERT INTO stats VALUES ('a_foo', 0);
         INSERT INTO stats VALUES ('abfoo', 10);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("a_foo"), Value::Integer(1)],
            vec![Value::from("abfoo"), Value::Integer(10)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "a_foo|1\nabfoo|10");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_like_null_escape_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-like-null-escape.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name LIKE 'a%' ESCAPE NULL;
         END;
         INSERT INTO stats VALUES ('alice', 0);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(0)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice|0");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_like_dynamic_pattern_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-like-dynamic-pattern.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name LIKE ('a' || '%');
         END;
         INSERT INTO stats VALUES ('alice', 0);
         INSERT INTO stats VALUES ('bob', 10);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::Integer(1)],
            vec![Value::from("bob"), Value::Integer(10)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "alice|1\nbob|10");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_glob_where_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-glob-where.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name GLOB new.name || '*';
         END;
         INSERT INTO stats VALUES ('Alice-1', 0);
         INSERT INTO stats VALUES ('alice-1', 10);
         INSERT INTO users VALUES (1, 'Alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("Alice-1"), Value::Integer(1)],
            vec![Value::from("alice-1"), Value::Integer(10)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "Alice-1|1\nalice-1|10");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_glob_character_class_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-glob-class.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name GLOB '[Aa]*';
         END;
         INSERT INTO stats VALUES ('Alice-1', 0);
         INSERT INTO stats VALUES ('alice-1', 10);
         INSERT INTO stats VALUES ('Bob-1', 20);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("Alice-1"), Value::Integer(1)],
            vec![Value::from("Bob-1"), Value::Integer(20)],
            vec![Value::from("alice-1"), Value::Integer(11)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "Alice-1|1\nBob-1|20\nalice-1|11");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_glob_range_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-glob-range.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1 WHERE name GLOB '[A-C]*';
         END;
         INSERT INTO stats VALUES ('Alpha', 0);
         INSERT INTO stats VALUES ('Bravo', 10);
         INSERT INTO stats VALUES ('Delta', 20);
         INSERT INTO users VALUES (1, 'anything');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt FROM stats ORDER BY name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("Alpha"), Value::Integer(1)],
            vec![Value::from("Bravo"), Value::Integer(11)],
            vec![Value::from("Delta"), Value::Integer(20)],
        ]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt FROM stats ORDER BY name;",
    );
    assert_eq!(cli_rows, "Alpha|1\nBravo|11\nDelta|20");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_with_multiple_assignments_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-multi-assign.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER, last_user INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt + 1, last_user = new.id WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 0, NULL);
         INSERT INTO users VALUES (9, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, cnt, last_user FROM stats;").unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("alice"),
            Value::Integer(1),
            Value::Integer(9),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt || '|' || last_user FROM stats;",
    );
    assert_eq!(cli_rows, "alice|1|9");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_assignments_read_old_row_like_sqlite() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-assignment-old-row.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY);
         CREATE TABLE stats(name TEXT PRIMARY KEY, a INTEGER, b INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET a = a + 1, b = a WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 1, 0);
         INSERT INTO users VALUES (1);",
    )
    .unwrap();

    let rows = db.query("SELECT a, b FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2), Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT a || '|' || b FROM stats;");
    assert_eq!(cli_rows, "2|1");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_case_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-case-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, flag INTEGER);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats
             SET note = CASE new.flag WHEN 1 THEN 'yes' ELSE 'no' END
             WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, 1);",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::from("yes")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|yes");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_unary_minus_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-unary-minus-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, delta INTEGER);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = -new.delta WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db.query("SELECT name, cnt FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::Integer(-7)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || cnt FROM stats;");
    assert_eq!(cli_rows, "target|-7");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_not_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-not-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, active INTEGER);
         CREATE TABLE stats(name TEXT PRIMARY KEY, active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET active = NOT new.active WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 1);
         INSERT INTO users VALUES (1, 1);",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, active, typeof(active) FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::Integer(0),
            Value::from("integer"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || active || '|' || typeof(active) FROM stats;",
    );
    assert_eq!(cli_rows, "target|0|integer");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_is_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-is-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, v TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET active = new.v IS NULL WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, NULL);",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, active, typeof(active) FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::Integer(1),
            Value::from("integer"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || active || '|' || typeof(active) FROM stats;",
    );
    assert_eq!(cli_rows, "target|1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_is_true_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-is-true-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, active INTEGER);
         CREATE TABLE stats(name TEXT PRIMARY KEY, active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET active = new.active IS TRUE WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, 1);",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, active, typeof(active) FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::Integer(1),
            Value::from("integer"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || active || '|' || typeof(active) FROM stats;",
    );
    assert_eq!(cli_rows, "target|1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_compare_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-compare-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE stats(name TEXT PRIMARY KEY, active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET active = new.score > 0 WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, active, typeof(active) FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::Integer(1),
            Value::from("integer"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || active || '|' || typeof(active) FROM stats;",
    );
    assert_eq!(cli_rows, "target|1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_between_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-between-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, score INTEGER);
         CREATE TABLE stats(name TEXT PRIMARY KEY, active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET active = new.score BETWEEN 1 AND 10 WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, 7);",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, active, typeof(active) FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::Integer(1),
            Value::from("integer"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || active || '|' || typeof(active) FROM stats;",
    );
    assert_eq!(cli_rows, "target|1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_in_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-in-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET active = new.name IN ('alice', 'bob') WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, active, typeof(active) FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::Integer(1),
            Value::from("integer"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || active || '|' || typeof(active) FROM stats;",
    );
    assert_eq!(cli_rows, "target|1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_like_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-like-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET active = new.name LIKE 'a%' WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, active, typeof(active) FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::Integer(1),
            Value::from("integer"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || active || '|' || typeof(active) FROM stats;",
    );
    assert_eq!(cli_rows, "target|1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_glob_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-glob-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, active INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET active = new.name GLOB 'A*' WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, 'Alice');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, active, typeof(active) FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::Integer(1),
            Value::from("integer"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || active || '|' || typeof(active) FROM stats;",
    );
    assert_eq!(cli_rows, "target|1|integer");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_concat_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-concat.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = note || ':' || new.name WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 'seen');
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("alice"), Value::from("seen:alice")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "alice|seen:alice");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_upper_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-upper-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = upper(new.name) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("target"), Value::from("ALICE")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|ALICE");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_lower_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-lower-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = lower(new.name) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, 'ALICE');",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("target"), Value::from("alice")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|alice");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_cast_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-cast-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = CAST(new.name AS INTEGER) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, '42');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, cnt, typeof(cnt) FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::Integer(42),
            Value::from("integer"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || cnt || '|' || typeof(cnt) FROM stats;",
    );
    assert_eq!(cli_rows, "target|42|integer");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_abs_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-abs-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, delta INTEGER);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = abs(new.delta) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, -7);",
    )
    .unwrap();

    let rows = db.query("SELECT name, cnt FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::Integer(7)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || cnt FROM stats;");
    assert_eq!(cli_rows, "target|7");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_coalesce_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-coalesce-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = coalesce(new.name, 'unknown') WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, NULL);",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("target"), Value::from("unknown")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|unknown");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_ifnull_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-ifnull-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = ifnull(new.name, 'unknown') WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, NULL);",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("target"), Value::from("unknown")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|unknown");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_nullif_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-nullif-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = nullif(new.name, 'skip') WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 'x');
         INSERT INTO users VALUES (1, 'skip');",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::Null]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || ifnull(note, 'NULL') FROM stats;",
    );
    assert_eq!(cli_rows, "target|NULL");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_length_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-length-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = length(new.name) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, cnt FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::Integer(5)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || cnt FROM stats;");
    assert_eq!(cli_rows, "target|5");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_substr_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-substr-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = substr(new.name, 1, 3) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::from("ali")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|ali");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_trim_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-trim-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = trim(new.name, '.') WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, '..alice..');",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("target"), Value::from("alice")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|alice");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_ltrim_rtrim_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-ltrim-rtrim-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, left_note TEXT, right_note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats
             SET left_note = ltrim(new.name, '.'),
                 right_note = rtrim(new.name, '.')
             WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '', '');
         INSERT INTO users VALUES (1, '..alice..');",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, left_note, right_note FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::from("alice.."),
            Value::from("..alice"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || left_note || '|' || right_note FROM stats;",
    );
    assert_eq!(cli_rows, "target|alice..|..alice");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_replace_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-replace-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = replace(new.name, '-', '_') WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, 'a-b-c');",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("target"), Value::from("a_b_c")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|a_b_c");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_instr_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-instr-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, pos INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET pos = instr(new.name, '-') WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, 'ab-c');",
    )
    .unwrap();

    let rows = db.query("SELECT name, pos FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::Integer(3)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || pos FROM stats;");
    assert_eq!(cli_rows, "target|3");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_round_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-round-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, reading REAL);
         CREATE TABLE stats(name TEXT PRIMARY KEY, value REAL);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET value = round(new.reading, 1) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0.0);
         INSERT INTO users VALUES (1, 3.14159);",
    )
    .unwrap();

    let rows = db.query("SELECT name, value FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::Real(3.1)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || value FROM stats;");
    assert_eq!(cli_rows, "target|3.1");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_typeof_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-typeof-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, reading REAL);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = typeof(new.reading) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, 3.14);",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::from("real")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|real");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_quote_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-quote-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = quote(new.name) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, 'O''Reilly');",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("target"), Value::from("'O''Reilly'")]]
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|'O''Reilly'");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_unicode_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-unicode-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, code INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET code = unicode(new.name) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', 0);
         INSERT INTO users VALUES (1, 'Alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, code FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::Integer(65)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || code FROM stats;");
    assert_eq!(cli_rows, "target|65");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_char_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-char-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, code INTEGER);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = char(new.code, 66) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, 65);",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::from("AB")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|AB");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_zeroblob_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-zeroblob-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, n INTEGER);
         CREATE TABLE stats(name TEXT PRIMARY KEY, body BLOB);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET body = zeroblob(new.n) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', X'FF');
         INSERT INTO users VALUES (1, 3);",
    )
    .unwrap();

    let rows = db
        .query("SELECT name, length(body), quote(body) FROM stats;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from("target"),
            Value::Integer(3),
            Value::from("X'000000'"),
        ]]
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT name || '|' || length(body) || '|' || quote(body) FROM stats;",
    );
    assert_eq!(cli_rows, "target|3|X'000000'");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_hex_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-hex-assignment.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, note TEXT);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET note = hex(new.name) WHERE name = 'target';
         END;
         INSERT INTO stats VALUES ('target', '');
         INSERT INTO users VALUES (1, 'Az');",
    )
    .unwrap();

    let rows = db.query("SELECT name, note FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("target"), Value::from("417A")]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || note FROM stats;");
    assert_eq!(cli_rows, "target|417A");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_subtract_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-subtract.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ad AFTER DELETE ON users
         BEGIN
             UPDATE stats SET cnt = cnt - 1 WHERE name = old.name;
         END;
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO stats VALUES ('alice', 3);
         DELETE FROM users WHERE id = 1;",
    )
    .unwrap();

    let rows = db.query("SELECT name, cnt FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || cnt FROM stats;");
    assert_eq!(cli_rows, "alice|2");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_multiply_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-multiply.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt * 2 WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 3);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, cnt FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || cnt FROM stats;");
    assert_eq!(cli_rows, "alice|6");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_divide_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-divide.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt / 2 WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 8);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, cnt FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(4)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || cnt FROM stats;");
    assert_eq!(cli_rows, "alice|4");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_modulo_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-modulo.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, cnt INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET cnt = cnt % 3 WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 8);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, cnt FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || cnt FROM stats;");
    assert_eq!(cli_rows, "alice|2");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_bitand_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-bitand.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, flags INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET flags = flags & 3 WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 6);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, flags FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(2)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || flags FROM stats;");
    assert_eq!(cli_rows, "alice|2");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_bitor_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-bitor.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, flags INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET flags = flags | 4 WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 2);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, flags FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || flags FROM stats;");
    assert_eq!(cli_rows, "alice|6");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_shift_left_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-shift-left.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, flags INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET flags = flags << 1 WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 3);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, flags FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(6)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || flags FROM stats;");
    assert_eq!(cli_rows, "alice|6");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_shift_right_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-shift-right.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, flags INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET flags = flags >> 1 WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 6);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, flags FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(3)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || flags FROM stats;");
    assert_eq!(cli_rows, "alice|3");
}

#[test]
fn rustsql_executes_simple_trigger_update_body_bitnot_assignment_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-trigger-update-body-bitnot.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE stats(name TEXT PRIMARY KEY, flags INTEGER);
         CREATE TRIGGER trg_users_ai AFTER INSERT ON users
         BEGIN
             UPDATE stats SET flags = ~flags WHERE name = new.name;
         END;
         INSERT INTO stats VALUES ('alice', 6);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db.query("SELECT name, flags FROM stats;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(-7)]]);

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT name || '|' || flags FROM stats;");
    assert_eq!(cli_rows, "alice|-7");
}

#[test]
fn rustsql_executes_simple_before_delete_trigger_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("execute-before-delete-trigger.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE audit(seq INTEGER PRIMARY KEY, user_id INTEGER, name TEXT, tag TEXT);
         CREATE TRIGGER trg_users_bd BEFORE DELETE ON users
         BEGIN
             INSERT INTO audit VALUES (NULL, old.id, old.name, 'before-delete');
         END;
         INSERT INTO users VALUES (4, 'erin');
         DELETE FROM users WHERE id = 4;",
    )
    .unwrap();

    let audit_rows = db
        .query("SELECT user_id, name, tag FROM audit ORDER BY seq;")
        .unwrap();
    assert_eq!(
        audit_rows,
        vec![vec![
            Value::Integer(4),
            Value::from("erin"),
            Value::from("before-delete"),
        ]]
    );
    let user_count = db.query("SELECT COUNT(*) FROM users;").unwrap();
    assert_eq!(user_count, vec![vec![Value::Integer(0)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT seq || '|' || user_id || '|' || name || '|' || tag FROM audit ORDER BY seq;",
    );
    assert_eq!(cli_rows, "1|4|erin|before-delete");
}

#[test]
fn rustsql_executes_stored_generated_columns_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("generated-column-exec.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (
            base INTEGER,
            plus_one INTEGER GENERATED ALWAYS AS (base + 1) STORED
        );
         INSERT INTO metrics(base) VALUES (3);
         UPDATE metrics SET base = 5;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT base, plus_one FROM metrics;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "5|6\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT base, plus_one FROM metrics;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(5), Value::Integer(6)]]);
}

#[test]
fn rustsql_rejects_explicit_insert_into_generated_column_in_sqlite_file() {
    let fixture = writable_sqlite_fixture("generated-column-explicit-insert.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (
            base INTEGER,
            plus_one INTEGER GENERATED ALWAYS AS (base + 1) STORED
        );",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO metrics(base, plus_one) VALUES (7, 8);")
        .unwrap_err();
    assert!(
        error.to_string().contains("generated column"),
        "unexpected error: {error}"
    );
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_virtual_generated_columns() {
    let fixture = writable_sqlite_fixture("generated-column-virtual-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER GENERATED ALWAYS AS (base + 1) VIRTUAL
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let generated_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'metrics';",
    );
    assert_eq!(
        generated_schema,
        "CREATE TABLE metrics (base INTEGER, plus_one INTEGER GENERATED ALWAYS AS (base + 1) VIRTUAL)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_implicit_virtual_generated_columns() {
    let fixture = writable_sqlite_fixture("generated-column-implicit-virtual-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER GENERATED ALWAYS AS (base + 1)
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let generated_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'metrics';",
    );
    assert_eq!(
        generated_schema,
        "CREATE TABLE metrics (base INTEGER, plus_one INTEGER GENERATED ALWAYS AS (base + 1))"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_as_generated_columns() {
    let fixture = writable_sqlite_fixture("generated-column-as-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER AS (base + 1)
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let generated_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'metrics';",
    );
    assert_eq!(
        generated_schema,
        "CREATE TABLE metrics (base INTEGER, plus_one INTEGER AS (base + 1))"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_commits_mixed_database_changes_when_without_rowid_tables_are_present() {
    let fixture = writable_sqlite_fixture("without-rowid-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                role TEXT,
                PRIMARY KEY(user_id, group_id)
            ) WITHOUT ROWID;
             INSERT INTO memberships VALUES (1, 10, 'owner');
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");

    let membership_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || group_id || '|' || role FROM memberships;",
    );
    assert_eq!(membership_rows, "1|10|owner");

    let membership_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'memberships';",
    );
    assert_eq!(
        membership_schema,
        "CREATE TABLE memberships (user_id INTEGER, group_id INTEGER, role TEXT, PRIMARY KEY(user_id, group_id)) WITHOUT ROWID"
    );
}

#[test]
fn rustsql_inserts_into_existing_without_rowid_table_and_preserves_rows() {
    let fixture = writable_sqlite_fixture("without-rowid-insert.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                role TEXT,
                PRIMARY KEY(user_id, group_id)
            ) WITHOUT ROWID;
             INSERT INTO memberships VALUES (1, 10, 'owner');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO memberships VALUES (2, 10, 'member');")
        .unwrap();

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || group_id || '|' || role \
         FROM memberships ORDER BY user_id, group_id;",
    );
    assert_eq!(cli_rows, "1|10|owner\n2|10|member");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT user_id, group_id, role FROM memberships ORDER BY user_id, group_id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Integer(10), Value::from("owner"),],
            vec![Value::Integer(2), Value::Integer(10), Value::from("member"),],
        ]
    );
}

#[test]
fn rustsql_creates_without_rowid_table_via_sql_and_persists_it() {
    let fixture = writable_sqlite_fixture("without-rowid-create.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            role TEXT,
            PRIMARY KEY(user_id, group_id)
        ) WITHOUT ROWID;
         INSERT INTO memberships VALUES (1, 10, 'owner');
         INSERT INTO memberships VALUES (2, 10, 'member');",
    )
    .unwrap();

    let membership_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'memberships';",
    );
    assert_eq!(
        membership_schema,
        "CREATE TABLE memberships (user_id INTEGER, group_id INTEGER, role TEXT, PRIMARY KEY(user_id, group_id)) WITHOUT ROWID"
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || group_id || '|' || role \
         FROM memberships ORDER BY user_id, group_id;",
    );
    assert_eq!(cli_rows, "1|10|owner\n2|10|member");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT user_id, group_id, role FROM memberships ORDER BY user_id, group_id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Integer(10), Value::from("owner"),],
            vec![Value::Integer(2), Value::Integer(10), Value::from("member"),],
        ]
    );
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_named_composite_primary_key_constraints() {
    let fixture = writable_sqlite_fixture("named-composite-primary-key-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                role TEXT,
                CONSTRAINT pk_memberships PRIMARY KEY(user_id, group_id)
            );
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let membership_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'memberships';",
    );
    assert_eq!(
        membership_schema,
        "CREATE TABLE memberships (user_id INTEGER, group_id INTEGER, role TEXT, CONSTRAINT pk_memberships PRIMARY KEY(user_id, group_id))"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_does_not_reuse_deleted_rowids_for_autoincrement_primary_keys() {
    let fixture = writable_sqlite_fixture("autoincrement-no-reuse.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);
         INSERT INTO users(name) VALUES ('alice');
         INSERT INTO users(name) VALUES ('bob');
         DELETE FROM users WHERE id = 2;
         INSERT INTO users(name) VALUES ('carol');",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|alice\n3|carol\n"
    );

    let cli_sequence = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT name, seq FROM sqlite_sequence;")
        .output()
        .unwrap();
    assert!(cli_sequence.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_sequence.stdout), "users|3\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(3), Value::from("carol")],
        ]
    );
}

#[test]
fn rustsql_renames_sqlite_sequence_entry_with_autoincrement_table() {
    let fixture = writable_sqlite_fixture("autoincrement-rename.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);
         INSERT INTO users(name) VALUES ('alice');
         ALTER TABLE users RENAME TO customers;",
    )
    .unwrap();

    let cli_sequence = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT name, seq FROM sqlite_sequence;")
        .output()
        .unwrap();
    assert!(cli_sequence.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_sequence.stdout),
        "customers|1\n"
    );
}

#[test]
fn rustsql_drops_sqlite_sequence_entry_with_autoincrement_table() {
    let fixture = writable_sqlite_fixture("autoincrement-drop.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);
         INSERT INTO users(name) VALUES ('alice');
         DROP TABLE users;",
    )
    .unwrap();

    let cli_sequence = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT name, seq FROM sqlite_sequence;")
        .output()
        .unwrap();
    assert!(cli_sequence.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_sequence.stdout), "");
}

#[test]
fn rustsql_updates_integer_primary_key_as_rowid() {
    let fixture = writable_sqlite_fixture("update-rowid.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         UPDATE users SET id = 5 WHERE id = 1;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "5|alice\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(5), Value::from("alice")]]);
}

#[test]
fn rustsql_supports_alter_table_add_column_and_renames() {
    let fixture = writable_sqlite_fixture("alter-table.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         ALTER TABLE users ADD COLUMN age INTEGER DEFAULT 0;
         ALTER TABLE users RENAME COLUMN name TO full_name;
         ALTER TABLE users RENAME TO customers;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, full_name, age FROM customers ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "1|alice|0\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, full_name, age FROM customers ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("alice"),
            Value::Integer(0),
        ]]
    );
}

#[test]
fn rustsql_supports_alter_table_drop_column() {
    let fixture = writable_sqlite_fixture("alter-table-drop-column.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 30);
         ALTER TABLE users DROP COLUMN age;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "1|alice\n");

    let cli_schema = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users';")
        .output()
        .unwrap();

    assert!(cli_schema.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_schema.stdout).trim(),
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);
}

#[test]
fn rustsql_supports_insert_select() {
    let fixture = writable_sqlite_fixture("insert-select.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE archive_users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO archive_users SELECT id, name FROM users WHERE id >= 2;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM archive_users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "2|bob\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM archive_users ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2), Value::from("bob")]]);
}

#[test]
fn rustsql_supports_insert_select_with_explicit_column_list_and_defaults() {
    let fixture = writable_sqlite_fixture("insert-select-columns.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE archive_users (
             id INTEGER PRIMARY KEY,
             name TEXT,
             active INTEGER DEFAULT 1
         );
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO archive_users (id, name)
         SELECT id, name FROM users WHERE id >= 2;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name, active FROM archive_users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "2|bob|1\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name, active FROM archive_users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(2),
            Value::from("bob"),
            Value::Integer(1),
        ]]
    );
}

#[test]
fn rustsql_supports_replace_into_select() {
    let fixture = writable_sqlite_fixture("replace-into-select.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE archive_users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO archive_users VALUES (1, 'stale');
         REPLACE INTO archive_users
         SELECT id, name FROM users WHERE id = 1;",
    )
    .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM archive_users ORDER BY id;")
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "1|alice\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM archive_users ORDER BY id;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);
}

#[test]
fn rustsql_enforces_foreign_keys_on_parent_delete_and_update() {
    let fixture = writable_sqlite_fixture("foreign-keys.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));
         INSERT INTO users VALUES (1);
         INSERT INTO orders VALUES (10, 1);",
    )
    .unwrap();

    let delete_error = db.execute("DELETE FROM users WHERE id = 1;").unwrap_err();
    assert!(delete_error.to_string().contains("foreign key constraint"));

    let update_error = db
        .execute("UPDATE users SET id = 2 WHERE id = 1;")
        .unwrap_err();
    assert!(update_error.to_string().contains("foreign key constraint"));

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "SELECT \
                 (SELECT COUNT(*) FROM users WHERE id = 1), \
                 (SELECT COUNT(*) FROM orders WHERE user_id = 1);",
        )
        .output()
        .unwrap();

    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "1|1\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    assert_eq!(
        reopened.query("SELECT id FROM users ORDER BY id;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        reopened
            .query("SELECT user_id FROM orders ORDER BY id;")
            .unwrap(),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn rustsql_rewrites_sqlite_schema_preserving_parent_primary_key_foreign_key_shorthand() {
    let fixture = writable_sqlite_fixture("foreign-key-parent-primary-key-shorthand-rewrite.db");

    let status = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY);
             CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users);
             CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO logs VALUES (1, 'before');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    db.execute("INSERT INTO logs VALUES (2, 'after');").unwrap();

    let order_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'orders';",
    );
    assert_eq!(
        order_schema,
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users)"
    );

    let log_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || note FROM logs ORDER BY id;",
    );
    assert_eq!(log_rows, "1|before\n2|after");
}

#[test]
fn rustsql_writes_multi_page_table_and_index_btrees() {
    let fixture = writable_sqlite_fixture("multi-page.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_name ON users(name);",
    )
    .unwrap();

    let mut sql = String::new();
    for n in 1..=400 {
        sql.push_str(&format!(
            "INSERT INTO users VALUES ({n}, 'user-{n:04}-{}');",
            "x".repeat(64)
        ));
    }
    db.execute(&sql).unwrap();

    let index_root = sqlite_index_root_page(&fixture.path, "idx_users_name");
    assert_eq!(sqlite_page_type(&fixture.path, 2), 0x05);
    assert_eq!(sqlite_page_type(&fixture.path, index_root), 0x02);

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, name FROM users WHERE id IN (1, 400) ORDER BY id;")
        .output()
        .unwrap();
    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        format!(
            "1|user-0001-{}\n400|user-0400-{}\n",
            "x".repeat(64),
            "x".repeat(64)
        )
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, name FROM users WHERE id IN (1, 400) ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(1),
                Value::from(format!("user-0001-{}", "x".repeat(64))),
            ],
            vec![
                Value::Integer(400),
                Value::from(format!("user-0400-{}", "x".repeat(64))),
            ],
        ]
    );
}

#[test]
fn rustsql_persists_secondary_index_on_without_rowid_table() {
    let fixture = writable_sqlite_fixture("without-rowid-secondary-index.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            role TEXT,
            PRIMARY KEY(user_id, group_id)
        ) WITHOUT ROWID;
         CREATE INDEX idx_memberships_role ON memberships(role);
         INSERT INTO memberships VALUES (1, 10, 'owner');
         INSERT INTO memberships VALUES (2, 20, 'member');",
    )
    .unwrap();

    let cli_indexes = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name;")
        .output()
        .unwrap();
    assert!(cli_indexes.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_indexes.stdout),
        "idx_memberships_role\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let indexes = reopened.list_indexes("memberships").unwrap();
    assert_eq!(indexes.len(), 2);
    assert!(
        indexes
            .iter()
            .any(|index| index.name == "idx_memberships_role")
    );
    assert!(
        indexes
            .iter()
            .any(|index| index.name == "sqlite_autoindex_memberships_1")
    );
}

#[test]
fn rustsql_persists_partial_index_but_hides_it_from_usable_index_list() {
    let fixture = writable_sqlite_fixture("partial-index-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, active INTEGER);
         CREATE INDEX idx_users_email_active ON users(email) WHERE active = 1;
         INSERT INTO users VALUES (1, 'alice@example.com', 1);
         INSERT INTO users VALUES (2, 'bob@example.com', 0);",
    )
    .unwrap();

    let cli_schema = sqlite3_scalar(
        &fixture.path,
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_users_email_active';",
    );
    assert_eq!(
        cli_schema,
        "CREATE INDEX idx_users_email_active ON users (email) WHERE active = 1"
    );

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, email, active FROM users ORDER BY id;")
        .output()
        .unwrap();
    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|alice@example.com|1\n2|bob@example.com|0\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, email, active FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(1),
                Value::from("alice@example.com"),
                Value::Integer(1),
            ],
            vec![
                Value::Integer(2),
                Value::from("bob@example.com"),
                Value::Integer(0),
            ],
        ]
    );

    let indexes = reopened.list_indexes("users").unwrap();
    assert!(indexes.is_empty());
}

#[test]
fn rustsql_updates_without_rowid_rows_through_secondary_index_lookup() {
    let fixture = writable_sqlite_fixture("without-rowid-secondary-index-update.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            role TEXT,
            PRIMARY KEY(user_id, group_id)
        ) WITHOUT ROWID;
         CREATE INDEX idx_memberships_role ON memberships(role);
         INSERT INTO memberships VALUES (1, 10, 'owner');
         INSERT INTO memberships VALUES (2, 20, 'member');
         UPDATE memberships SET role = 'editor' WHERE role = 'member';",
    )
    .unwrap();

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || group_id || '|' || role \
         FROM memberships ORDER BY user_id, group_id;",
    );
    assert_eq!(cli_rows, "1|10|owner\n2|20|editor");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT user_id FROM memberships WHERE role = 'editor';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_deletes_without_rowid_rows_through_secondary_index_lookup() {
    let fixture = writable_sqlite_fixture("without-rowid-secondary-index-delete.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            role TEXT,
            PRIMARY KEY(user_id, group_id)
        ) WITHOUT ROWID;
         CREATE INDEX idx_memberships_role ON memberships(role);
         INSERT INTO memberships VALUES (1, 10, 'owner');
         INSERT INTO memberships VALUES (2, 20, 'member');
         DELETE FROM memberships WHERE role = 'member';",
    )
    .unwrap();

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || group_id || '|' || role \
         FROM memberships ORDER BY user_id, group_id;",
    );
    assert_eq!(cli_rows, "1|10|owner");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT user_id FROM memberships WHERE role = 'member';")
        .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn rustsql_enforces_unique_secondary_index_on_without_rowid_table() {
    let fixture = writable_sqlite_fixture("without-rowid-unique-secondary-index.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            email TEXT,
            PRIMARY KEY(user_id, group_id)
        ) WITHOUT ROWID;
         CREATE UNIQUE INDEX idx_memberships_email ON memberships(email);
         INSERT INTO memberships VALUES (1, 10, 'a@example.com');
         INSERT INTO memberships VALUES (2, 20, 'b@example.com');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO memberships VALUES (3, 30, 'a@example.com');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_memberships_email constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT user_id || '|' || group_id || '|' || email \
         FROM memberships ORDER BY user_id, group_id;",
    );
    assert_eq!(cli_rows, "1|10|a@example.com\n2|20|b@example.com");
}

#[test]
fn rustsql_enforces_unique_expression_index_on_insert() {
    let fixture = writable_sqlite_fixture("unique-expression-index-insert.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_lower_name ON users(lower(name));
         INSERT INTO users VALUES (1, 'Alice');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'alice');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_lower_name constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|Alice");
}

#[test]
fn rustsql_enforces_unique_expression_index_on_update() {
    let fixture = writable_sqlite_fixture("unique-expression-index-update.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_lower_name ON users(lower(name));
         INSERT INTO users VALUES (1, 'Alice');
         INSERT INTO users VALUES (2, 'Bob');",
    )
    .unwrap();

    let error = db
        .execute("UPDATE users SET name = 'alice' WHERE id = 2;")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_lower_name constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|Alice\n2|Bob");
}

#[test]
fn rustsql_insert_or_replace_uses_unique_expression_index_conflicts() {
    let fixture = writable_sqlite_fixture("unique-expression-index-replace.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_lower_name ON users(lower(name));
         INSERT INTO users VALUES (1, 'Alice');
         INSERT INTO users VALUES (2, 'Bob');",
    )
    .unwrap();

    db.execute("INSERT OR REPLACE INTO users VALUES (3, 'alice');")
        .unwrap();

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "2|Bob\n3|alice");
}

#[test]
fn rustsql_enforces_unique_substr_expression_index_on_insert() {
    let fixture = writable_sqlite_fixture("unique-expression-index-substr-insert.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_name_initial ON users(substr(name, 1, 1));
         INSERT INTO users VALUES (1, 'Alice');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'Andrew');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_name_initial constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|Alice");
}

#[test]
fn rustsql_substr_expression_index_truncates_text_at_nul_like_sqlite() {
    let fixture = writable_sqlite_fixture("unique-expression-index-substr-text-nul.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_substr_nul_name ON users(substr(name, 2, 5));
         INSERT INTO users VALUES (1, 'abc' || char(0) || 'def');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'abc' || char(0) || 'xyz');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_substr_nul_name constraint failed"),
        "unexpected error: {error}"
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(substr(name, 2, 5)) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'bc'");
}

#[test]
fn rustsql_substr_expression_index_handles_zero_start_like_sqlite() {
    let fixture = writable_sqlite_fixture("unique-expression-index-substr-zero-start.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_substr_zero_start ON users(substr(name, 0, 2));
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'adam');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_substr_zero_start constraint failed"),
        "unexpected error: {error}"
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(substr(name, 0, 2)) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'a'");
}

#[test]
fn rustsql_enforces_unique_trim_expression_index_on_insert() {
    let fixture = writable_sqlite_fixture("unique-expression-index-trim-insert.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_trimmed_name ON users(trim(name));
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, '  alice  ');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_trimmed_name constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(name) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'alice'");
}

#[test]
fn rustsql_enforces_unique_instr_expression_index_on_insert() {
    let fixture = writable_sqlite_fixture("unique-expression-index-instr-insert.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_li_position ON users(instr(name, 'li'));
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'bliss');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_li_position constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || name FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|alice");
}

#[test]
fn rustsql_enforces_blob_instr_expression_index_by_byte_like_sqlite() {
    let fixture = writable_sqlite_fixture("unique-expression-index-blob-instr-insert.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE packets (id INTEGER PRIMARY KEY, payload BLOB);
         CREATE UNIQUE INDEX idx_packets_marker_position ON packets(instr(payload, X'FF'));
         INSERT INTO packets VALUES (1, X'F09F9880FF');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO packets VALUES (2, X'01020304FF');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_packets_marker_position constraint failed"),
        "unexpected error: {error}"
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || instr(payload, X'FF') FROM packets ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|5");
}

#[test]
fn rustsql_enforces_unique_replace_expression_index_on_insert() {
    let fixture = writable_sqlite_fixture("unique-expression-index-replace-function-insert.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_dashless_name ON users(replace(name, '-', ''));
         INSERT INTO users VALUES (1, 'ab-c');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'a-bc');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_dashless_name constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(name) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'ab-c'");
}

#[test]
fn rustsql_replace_expression_index_truncates_text_at_nul_like_sqlite() {
    let fixture = writable_sqlite_fixture("unique-expression-index-replace-text-nul.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_replace_nul_name ON users(replace(name, 'd', 'X'));
         INSERT INTO users VALUES (1, 'abc' || char(0) || 'def');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'abc' || char(0) || 'xyz');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_replace_nul_name constraint failed"),
        "unexpected error: {error}"
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(replace(name, 'd', 'X')) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'abc'");
}

#[test]
fn rustsql_nullif_expression_index_allows_repeated_null_keys() {
    let fixture = writable_sqlite_fixture("expression-index-nullif-null-keys.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_non_unknown_name ON users(nullif(name, 'unknown'));
         INSERT INTO users VALUES (1, 'unknown');
         INSERT INTO users VALUES (2, 'unknown');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (3, 'alice'); INSERT INTO users VALUES (4, 'alice');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_non_unknown_name constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(name) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'unknown'\n2|'unknown'\n3|'alice'");
}

#[test]
fn rustsql_ifnull_expression_index_normalizes_null_keys() {
    let fixture = writable_sqlite_fixture("expression-index-ifnull-null-keys.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_name_or_unknown ON users(ifnull(name, 'unknown'));
         INSERT INTO users VALUES (1, NULL);",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, NULL);")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_name_or_unknown constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(name) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|NULL");
}

#[test]
fn rustsql_coalesce_expression_index_normalizes_null_keys() {
    let fixture = writable_sqlite_fixture("expression-index-coalesce-null-keys.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, nickname TEXT, name TEXT);
         CREATE UNIQUE INDEX idx_users_display_name ON users(coalesce(nickname, name, 'unknown'));
         INSERT INTO users VALUES (1, NULL, 'alice');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'alice', 'ally');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_display_name constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(nickname) || '|' || quote(name) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|NULL|'alice'");
}

#[test]
fn rustsql_unicode_expression_index_enforces_unique_codepoint() {
    let fixture = writable_sqlite_fixture("expression-index-unicode-codepoint.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_first_codepoint ON users(unicode(name));
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'adam');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_first_codepoint constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(name) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'alice'");
}

#[test]
fn rustsql_quote_expression_index_enforces_unique_quoted_text() {
    let fixture = writable_sqlite_fixture("expression-index-quote-text.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_quoted_name ON users(quote(name));
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'alice');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_quoted_name constraint failed")
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(name) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'alice'");
}

#[test]
fn rustsql_quote_expression_index_truncates_text_at_nul_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-quote-text-nul.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE UNIQUE INDEX idx_users_quoted_nul_name ON users(quote(name));
         INSERT INTO users VALUES (1, 'a' || char(0) || 'b');",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (2, 'a' || char(0) || 'c');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unique index idx_users_quoted_nul_name constraint failed"),
        "unexpected error: {error}"
    );

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(name) || '|' || length(name) FROM users ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|'a'|1");
}

#[test]
fn rustsql_quote_expression_index_formats_infinity_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-quote-infinity.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading REAL);
         CREATE INDEX idx_metrics_quote_reading ON metrics(quote(reading));
         INSERT INTO metrics VALUES (1, 1e999);
         INSERT INTO metrics VALUES (2, -1e999);",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM metrics WHERE quote(reading) = '9.0e+999';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));

    let rows = db
        .query("SELECT id FROM metrics WHERE quote(reading) = '9.0e+999';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let cli_rows = sqlite3_scalar(
        &fixture.path,
        "SELECT id || '|' || quote(reading) FROM metrics ORDER BY id;",
    );
    assert_eq!(cli_rows, "1|9.0e+999\n2|-9.0e+999");
}

#[test]
fn rustsql_json_array_expression_index_rejects_blob_values_like_sqlite() {
    let fixture = writable_sqlite_fixture("expression-index-json-array-blob.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE files (id INTEGER PRIMARY KEY, payload BLOB);
         CREATE INDEX idx_files_json_payload ON files(json_array(payload));",
    )
    .unwrap();

    let error = db
        .execute("INSERT INTO files VALUES (1, X'4142');")
        .unwrap_err();
    assert!(
        error.to_string().contains("JSON cannot hold BLOB values"),
        "unexpected error: {error}"
    );

    let cli_rows = sqlite3_scalar(&fixture.path, "SELECT COUNT(*) FROM files;");
    assert_eq!(cli_rows, "0");
}

#[test]
fn rustsql_persists_composite_secondary_index_on_without_rowid_table() {
    let fixture = writable_sqlite_fixture("without-rowid-composite-secondary-index.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            active INTEGER,
            role TEXT,
            PRIMARY KEY(user_id, group_id)
        ) WITHOUT ROWID;
         CREATE INDEX idx_memberships_active_role ON memberships(active, role);
         INSERT INTO memberships VALUES (1, 10, 1, 'alpha');
         INSERT INTO memberships VALUES (2, 20, 1, 'bravo');
         INSERT INTO memberships VALUES (3, 30, 0, 'charlie');",
    )
    .unwrap();

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query(
            "SELECT user_id FROM memberships \
             WHERE active = 1 AND role > 'alpha' AND role < 'charlie' \
             ORDER BY user_id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn rustsql_writes_overflowed_table_and_index_payloads() {
    let fixture = writable_sqlite_fixture("overflow-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT);
         CREATE INDEX idx_docs_body ON docs(body);",
    )
    .unwrap();

    let huge_body = "z".repeat(10_000);
    db.execute(&format!("INSERT INTO docs VALUES (1, '{}');", huge_body))
        .unwrap();

    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg("SELECT id, length(body) FROM docs;")
        .output()
        .unwrap();
    assert!(cli_rows.status.success());
    assert_eq!(String::from_utf8_lossy(&cli_rows.stdout), "1|10000\n");

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, length(body) FROM docs;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(10_000)]]);

    let storage = rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap();
    let txn = storage.begin().unwrap();
    let row_ids = storage
        .lookup_index(
            txn,
            "docs",
            "idx_docs_body",
            &[Value::Text(huge_body.clone())],
        )
        .unwrap();
    storage.rollback(txn).unwrap();

    assert_eq!(row_ids, vec![rustsql::common::types::RowId(1)]);
}

#[test]
fn rustsql_writes_overflowed_interior_index_pages() {
    let fixture = writable_sqlite_fixture("interior-index-overflow-write.db");
    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );

    db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT);")
        .unwrap();

    let mut sql = String::new();
    for n in 1..=120 {
        sql.push_str(&format!(
            "INSERT INTO docs VALUES ({n}, '{}');",
            format!("doc-{n:03}-{}", "x".repeat(2_000))
        ));
    }
    sql.push_str("CREATE INDEX idx_docs_body ON docs(body);");

    db.execute(&sql).unwrap();

    let root_page = sqlite_index_root_page(&fixture.path, "idx_docs_body");
    assert_eq!(sqlite_page_type(&fixture.path, root_page), 0x02);
    assert!(
        sqlite_file_has_overflowed_interior_index_cell(&fixture.path),
        "expected at least one overflowed interior index cell in the written sqlite file",
    );

    let first_body = format!("doc-001-{}", "x".repeat(2_000));
    let last_body = format!("doc-120-{}", "x".repeat(2_000));
    let cli_rows = Command::new("sqlite3")
        .arg(&fixture.path)
        .arg(format!(
            "SELECT id, length(body) FROM docs \
                 WHERE body IN ('{}', '{}') ORDER BY id;",
            first_body, last_body
        ))
        .output()
        .unwrap();
    assert!(cli_rows.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli_rows.stdout),
        "1|2008\n120|2008\n"
    );

    let reopened = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = reopened
        .query("SELECT id, length(body) FROM docs WHERE id IN (1, 120) ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Integer(2_008)],
            vec![Value::Integer(120), Value::Integer(2_008)],
        ]
    );
}
