use std::path::{Path, PathBuf};
use std::process::Command;

use rustsql::common::types::{RowId, Value};
use rustsql::db::Database;
use rustsql::engine::traits::{IndexStore, TableStore, TransactionManager};
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

fn overflow_fixture() -> SqliteDbFixture {
    sqlite_db(
        "PRAGMA page_size = 512; \
         VACUUM; \
         CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL); \
         CREATE INDEX idx_docs_body ON docs(body); \
         INSERT INTO docs VALUES (1, hex(zeroblob(1200)));",
        "overflow.db",
    )
}

#[test]
fn sqlite3_engine_reads_overflowed_table_payloads() {
    let fixture = overflow_fixture();
    let storage = sqlite_storage(&fixture.path);
    let txn = storage.begin().unwrap();

    let row = storage.get_row(txn, "docs", RowId(1)).unwrap().unwrap();

    assert_eq!(row[0], Value::Integer(1));
    match &row[1] {
        Value::Text(text) => {
            assert_eq!(text.len(), 2400);
            assert!(text.chars().all(|ch| ch == '0'));
        }
        value => panic!("expected text overflow payload, got {value:?}"),
    }

    storage.rollback(txn).unwrap();
}

#[test]
fn sqlite3_engine_queries_rows_with_overflowed_table_payloads() {
    let fixture = overflow_fixture();
    let db = Database::with_storage(sqlite_storage(&fixture.path));

    let rows = db
        .query("SELECT id, length(body) FROM docs WHERE id = 1;")
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(2400)]]);
}

#[test]
fn sqlite3_engine_looks_up_indexes_with_overflowed_index_payloads() {
    let fixture = overflow_fixture();
    let storage = sqlite_storage(&fixture.path);
    let txn = storage.begin().unwrap();

    let row_ids = storage
        .lookup_index(
            txn,
            "docs",
            "idx_docs_body",
            &[Value::Text("0".repeat(2400))],
        )
        .unwrap();

    assert_eq!(row_ids, vec![RowId(1)]);
    storage.rollback(txn).unwrap();
}
