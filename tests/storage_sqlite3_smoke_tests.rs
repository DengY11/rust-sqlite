use std::fs;

use tempfile::tempdir;

fn write_minimal_sqlite_file(path: &std::path::Path) {
    let mut bytes = vec![0_u8; 4096];
    bytes[..16].copy_from_slice(b"SQLite format 3\0");
    bytes[16..18].copy_from_slice(&4096_u16.to_be_bytes());
    bytes[28..32].copy_from_slice(&1_u32.to_be_bytes());
    bytes[44..48].copy_from_slice(&4_u32.to_be_bytes());
    bytes[100] = 0x0d;
    bytes[105..107].copy_from_slice(&4096_u16.to_be_bytes());
    fs::write(path, bytes).unwrap();
}

#[test]
fn storage_module_exposes_sqlite3_backend() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("demo.db");
    write_minimal_sqlite_file(&path);

    let engine = rustsql::storage::sqlite3::FileStorage::open(path);
    assert!(engine.is_ok());
}
