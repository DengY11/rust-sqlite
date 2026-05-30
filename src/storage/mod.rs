pub mod memory;
pub mod v1;
pub mod v2;

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    #[test]
    fn storage_module_exposes_all_backends() {
        let _memory = crate::storage::memory::MemoryStorage::new();

        let dir = tempdir().unwrap();
        let v1 = crate::storage::v1::FileStorage::open(dir.path().join("v1")).unwrap();
        let v2 = crate::storage::v2::FileStorage::open(dir.path().join("v2")).unwrap();

        let _ = (v1, v2);
    }
}
