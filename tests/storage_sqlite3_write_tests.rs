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
         INSERT INTO metrics VALUES (2, 4.5);",
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
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, reading REAL);
         CREATE INDEX idx_metrics_sign_reading ON metrics(sign(reading));
         INSERT INTO metrics VALUES (1, 3.14);
         INSERT INTO metrics VALUES (2, -2.0);
         INSERT INTO metrics VALUES (3, 0.0);",
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
         INSERT INTO metrics VALUES (1, 3.14);
         INSERT INTO metrics VALUES (2, 2.71);",
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
         INSERT INTO metrics VALUES (2, '66');",
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
         INSERT INTO metrics VALUES (2, '4');",
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
