use std::path::{Path, PathBuf};
use std::process::Command;

use rustsql::common::types::{RowId, Value};
use rustsql::db::Database;
use rustsql::engine::traits::{TableStore, TransactionManager};
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

fn sqlite_storage(path: &Path) -> rustsql::storage::sqlite3::FileStorage {
    rustsql::storage::sqlite3::FileStorage::open(path).unwrap()
}

#[test]
fn sqlite3_engine_database_query_reads_original_users_table() {
    let fixture = sqlite_db(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); \
         INSERT INTO users VALUES (1, 'alice'), (2, 'bob');",
        "rows.db",
    );

    let db = Database::with_storage(sqlite_storage(&fixture.path));
    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn sqlite3_engine_database_query_decodes_zero_and_one_integer_columns() {
    let fixture = sqlite_db(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, visits INTEGER, name TEXT); \
         INSERT INTO users VALUES (1, 0, 'alice'), (2, 1, 'bob');",
        "int-bools.db",
    );

    let db = Database::with_storage(sqlite_storage(&fixture.path));
    let rows = db
        .query("SELECT id, visits, name FROM users ORDER BY id;")
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Integer(0), Value::from("alice"),],
            vec![Value::Integer(2), Value::Integer(1), Value::from("bob"),],
        ]
    );
}

#[test]
fn sqlite3_engine_database_query_reads_blob_columns_from_real_sqlite_file() {
    let fixture = sqlite_db(
        "CREATE TABLE files (id INTEGER PRIMARY KEY, payload BLOB); \
         INSERT INTO files VALUES (1, X'0001FEFF');",
        "blob-values.db",
    );

    let db = Database::with_storage(sqlite_storage(&fixture.path));
    let rows = db
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
fn sqlite3_engine_database_query_reads_without_rowid_table_rows() {
    let fixture = sqlite_db(
        "CREATE TABLE memberships (
            user_id INTEGER,
            name TEXT,
            group_id INTEGER,
            PRIMARY KEY(user_id, group_id)
         ) WITHOUT ROWID;
         INSERT INTO memberships VALUES (1, 'alpha', 10), (2, 'beta', 20);",
        "without-rowid-read.db",
    );

    let db = Database::with_storage(sqlite_storage(&fixture.path));
    let rows = db
        .query("SELECT user_id, name, group_id FROM memberships ORDER BY user_id;")
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alpha"), Value::Integer(10),],
            vec![Value::Integer(2), Value::from("beta"), Value::Integer(20),],
        ]
    );
}

#[test]
fn sqlite3_engine_database_query_reads_without_rowid_rows_from_interior_index_pages() {
    let fixture = sqlite_db(
        "PRAGMA page_size = 512; \
         VACUUM; \
         CREATE TABLE memberships (
             user_id INTEGER,
             group_id INTEGER,
             note TEXT,
             PRIMARY KEY(user_id, group_id)
         ) WITHOUT ROWID; \
         WITH RECURSIVE seq(n) AS (
             SELECT 1
             UNION ALL
             SELECT n + 1 FROM seq WHERE n < 200
         )
         INSERT INTO memberships(user_id, group_id, note)
         SELECT n, n + 1000, printf('member-%03d-%s', n, hex(randomblob(24)))
         FROM seq;",
        "without-rowid-interior.db",
    );

    let db = Database::with_storage(sqlite_storage(&fixture.path));
    let rows = db
        .query(
            "SELECT user_id, group_id, note \
             FROM memberships \
             WHERE user_id >= 198 \
             ORDER BY user_id;",
        )
        .unwrap();

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Integer(198));
    assert_eq!(rows[0][1], Value::Integer(1198));
    assert_eq!(rows[1][0], Value::Integer(199));
    assert_eq!(rows[1][1], Value::Integer(1199));
    assert_eq!(rows[2][0], Value::Integer(200));
    assert_eq!(rows[2][1], Value::Integer(1200));
    for row in rows {
        match &row[2] {
            Value::Text(text) => assert!(text.starts_with("member-")),
            value => panic!("expected text value, got {value:?}"),
        }
    }
}

#[test]
fn sqlite3_engine_database_query_reads_without_rowid_overflow_payloads() {
    let fixture = sqlite_db(
        "PRAGMA page_size = 512; \
         VACUUM; \
         CREATE TABLE docs (
             category TEXT,
             doc_id INTEGER,
             body TEXT NOT NULL,
             PRIMARY KEY(category, doc_id)
         ) WITHOUT ROWID; \
         INSERT INTO docs VALUES ('guide', 1, hex(zeroblob(1200)));",
        "without-rowid-overflow.db",
    );

    let db = Database::with_storage(sqlite_storage(&fixture.path));
    let rows = db
        .query(
            "SELECT category, doc_id, length(body) \
             FROM docs \
             WHERE category = 'guide' AND doc_id = 1;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("guide"),
            Value::Integer(1),
            Value::Integer(2400),
        ]]
    );
}

#[test]
fn sqlite3_engine_get_row_decodes_table_record_values() {
    let fixture = sqlite_db(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, visits INTEGER, name TEXT); \
         INSERT INTO users VALUES (1, 0, 'alice'), (2, 1, 'bob');",
        "get-row.db",
    );
    let storage = sqlite_storage(&fixture.path);
    let txn = storage.begin().unwrap();

    let row = storage.get_row(txn, "users", RowId(2)).unwrap();

    assert_eq!(
        row,
        Some(vec![
            Value::Integer(2),
            Value::Integer(1),
            Value::from("bob"),
        ])
    );

    storage.rollback(txn).unwrap();
}

#[test]
fn sqlite3_engine_traverses_interior_table_pages_for_get_row() {
    let fixture = sqlite_db(
        "PRAGMA page_size = 512; \
         CREATE TABLE users (id INTEGER PRIMARY KEY, visits INTEGER, name TEXT); \
         WITH RECURSIVE seq(n) AS (
             SELECT 1
             UNION ALL
             SELECT n + 1 FROM seq WHERE n < 200
         )
         INSERT INTO users(id, visits, name)
         SELECT n, n % 2, printf('user-%03d-%s', n, hex(randomblob(32)))
         FROM seq;",
        "interior-pages.db",
    );
    let storage = sqlite_storage(&fixture.path);
    let txn = storage.begin().unwrap();

    let row = storage.get_row(txn, "users", RowId(200)).unwrap();

    let row = row.unwrap();
    assert_eq!(row[0], Value::Integer(200));
    assert_eq!(row[1], Value::Integer(0));
    match &row[2] {
        Value::Text(text) => {
            assert!(text.starts_with("user-200-"));
            assert!(text.len() > "user-200-".len());
        }
        value => panic!("expected text value, got {value:?}"),
    }

    storage.rollback(txn).unwrap();
}
