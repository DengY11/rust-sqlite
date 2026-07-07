pub mod memory;
pub mod sqlite3;
pub mod v1;
pub mod v2;

#[cfg(test)]
mod tests {
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
    fn storage_module_exposes_all_backends() {
        let _memory = crate::storage::memory::MemoryStorage::new();

        let dir = tempdir().unwrap();
        let v1 = crate::storage::v1::FileStorage::open(dir.path().join("v1")).unwrap();
        let v2 = crate::storage::v2::FileStorage::open(dir.path().join("v2")).unwrap();
        let sqlite3_path = dir.path().join("sqlite3.db");
        write_minimal_sqlite_file(&sqlite3_path);
        let sqlite3 = crate::storage::sqlite3::FileStorage::open(sqlite3_path).unwrap();

        let _ = (v1, v2, sqlite3);
    }
}
