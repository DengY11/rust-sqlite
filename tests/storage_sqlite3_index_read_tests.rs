use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustsql::common::types::Value;
use rustsql::db::Database;
use rustsql::engine::traits::{IndexStore, TransactionManager};
use tempfile::{TempDir, tempdir};

struct SqliteDbFixture {
    _dir: TempDir,
    path: PathBuf,
}

fn sqlite_db(sql: &str, file_name: &str) -> SqliteDbFixture {
    let dir = tempdir().unwrap();
    let path = dir.path().join(file_name);

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(sql)
        .status()
        .unwrap();
    assert!(status.success());

    SqliteDbFixture { _dir: dir, path }
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

fn sqlite_page_type(path: &Path, page_no: u32) -> u8 {
    let bytes = fs::read(path).unwrap();
    let page_size = sqlite_page_size(&bytes);
    bytes[sqlite_page_start(page_no, page_size)]
}

fn overflowed_interior_index_fixture() -> SqliteDbFixture {
    sqlite_db(
        "PRAGMA page_size = 512; \
         VACUUM; \
         CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL); \
         CREATE INDEX idx_docs_body ON docs(body); \
         WITH RECURSIVE seq(n) AS (
             SELECT 1
             UNION ALL
             SELECT n + 1 FROM seq WHERE n < 80
         )
         INSERT INTO docs(id, body)
         SELECT n, printf('%04d-', n) || hex(zeroblob(220))
         FROM seq;",
        "overflowed-interior-index.db",
    )
}

fn interior_index_fixture() -> SqliteDbFixture {
    sqlite_db(
        "PRAGMA page_size = 512; \
         CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL); \
         CREATE INDEX idx_users_name ON users(name); \
         WITH RECURSIVE seq(n) AS (
             SELECT 1
             UNION ALL
             SELECT n + 1 FROM seq WHERE n < 400
         )
         INSERT INTO users(id, name)
         SELECT n, printf('user-%04d-%s', n, hex(zeroblob(32)))
         FROM seq;",
        "interior-index.db",
    )
}

#[test]
fn sqlite3_engine_uses_secondary_index_for_lookup() {
    let fixture = sqlite_db(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); \
         CREATE INDEX idx_users_name ON users(name); \
         INSERT INTO users VALUES (1, 'alice'), (2, 'bob');",
        "index.db",
    );

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = db
        .query("SELECT id FROM users WHERE name = 'alice';")
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn sqlite3_engine_uses_blob_secondary_index_for_lookup() {
    let fixture = sqlite_db(
        "CREATE TABLE files (id INTEGER PRIMARY KEY, payload BLOB); \
         CREATE INDEX idx_files_payload ON files(payload); \
         INSERT INTO files VALUES (1, X'0001FEFF'), (2, X'ABCD');",
        "blob-index.db",
    );

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = db
        .query("SELECT id FROM files WHERE payload = X'0001FEFF';")
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn sqlite3_engine_lookup_index_traverses_real_interior_index_pages() {
    let fixture = interior_index_fixture();
    let root_page = sqlite_index_root_page(&fixture.path, "idx_users_name");
    assert_eq!(sqlite_page_type(&fixture.path, root_page), 0x02);

    let lookup_key = format!("user-{:04}-{}", 275, "00".repeat(32));
    let storage = rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap();
    let txn = storage.begin().unwrap();

    let row_ids = storage
        .lookup_index(
            txn,
            "users",
            "idx_users_name",
            &[Value::Text(lookup_key.clone())],
        )
        .unwrap();

    assert_eq!(row_ids, vec![rustsql::common::types::RowId(275)]);
    storage.rollback(txn).unwrap();
}

#[test]
fn sqlite3_engine_lookup_index_reads_blob_keys_from_real_sqlite_file() {
    let fixture = sqlite_db(
        "CREATE TABLE files (id INTEGER PRIMARY KEY, payload BLOB NOT NULL); \
         CREATE INDEX idx_files_payload ON files(payload); \
         INSERT INTO files VALUES (1, X'0001FEFF'), (2, X'ABCD'), (3, X'0001FE00');",
        "blob-index-lookup.db",
    );

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

    assert_eq!(row_ids, vec![rustsql::common::types::RowId(1)]);
    storage.rollback(txn).unwrap();
}

#[test]
fn sqlite3_engine_reads_overflowed_interior_index_cells_during_lookup() {
    let fixture = overflowed_interior_index_fixture();
    let root_page = sqlite_index_root_page(&fixture.path, "idx_docs_body");
    assert_eq!(sqlite_page_type(&fixture.path, root_page), 0x02);

    let storage = rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap();
    let txn = storage.begin().unwrap();
    let lookup_key = format!("{:04}-{}", 57, "00".repeat(220));

    let row_ids = storage
        .lookup_index(
            txn,
            "docs",
            "idx_docs_body",
            &[Value::Text(lookup_key.clone())],
        )
        .unwrap();

    assert_eq!(row_ids, vec![rustsql::common::types::RowId(57)]);
    storage.rollback(txn).unwrap();
}

#[test]
fn sqlite3_engine_scans_index_ranges_from_real_sqlite_file() {
    let fixture = sqlite_db(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active INTEGER); \
         CREATE INDEX idx_users_active_name ON users(active, name); \
         INSERT INTO users VALUES (1, 'alice', 1), (2, 'bob', 1), (3, 'carol', 1), (4, 'david', 0);",
        "index-range.db",
    );

    let storage = rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap();
    let txn = storage.begin().unwrap();

    let row_ids = storage
        .scan_index_range(
            txn,
            "users",
            "idx_users_active_name",
            &[Value::Integer(1)],
            Some((rustsql::sql::ast::CompareOp::Gt, &Value::from("alice"))),
            Some((rustsql::sql::ast::CompareOp::Lt, &Value::from("david"))),
        )
        .unwrap();

    assert_eq!(
        row_ids,
        vec![
            rustsql::common::types::RowId(2),
            rustsql::common::types::RowId(3),
        ]
    );
    storage.rollback(txn).unwrap();
}

#[test]
fn sqlite3_engine_lists_without_rowid_primary_key_as_usable_index() {
    let fixture = sqlite_db(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            name TEXT,
            PRIMARY KEY(user_id, group_id)
         ) WITHOUT ROWID;
         INSERT INTO memberships VALUES (1, 10, 'alpha'), (2, 20, 'beta');",
        "without-rowid-primary-key-index.db",
    );

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let indexes = db.list_indexes("memberships").unwrap();

    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "sqlite_autoindex_memberships_1");
    assert_eq!(
        indexes[0].columns,
        vec!["user_id".to_string(), "group_id".to_string()]
    );
    assert!(indexes[0].unique);
}

#[test]
fn sqlite3_engine_uses_without_rowid_primary_key_for_lookup() {
    let fixture = sqlite_db(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            name TEXT,
            PRIMARY KEY(user_id, group_id)
         ) WITHOUT ROWID;
         INSERT INTO memberships VALUES (1, 10, 'alpha'), (2, 20, 'beta');",
        "without-rowid-primary-key-lookup.db",
    );

    let db = Database::with_storage(
        rustsql::storage::sqlite3::FileStorage::open(&fixture.path).unwrap(),
    );
    let rows = db
        .query("SELECT name FROM memberships WHERE user_id = 2 AND group_id = 20;")
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("beta")]]);
}
