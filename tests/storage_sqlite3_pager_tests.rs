use std::fs;

use rustsql::storage::sqlite3::file_header::DatabaseHeader;
use rustsql::storage::sqlite3::pager::Pager;
use tempfile::tempdir;

fn sqlite_file_bytes(page_size: u16, page_count: usize) -> Vec<u8> {
    let total_len = usize::from(page_size) * page_count;
    let mut bytes = vec![0_u8; total_len];
    bytes[..16].copy_from_slice(b"SQLite format 3\0");
    bytes[16..18].copy_from_slice(&page_size.to_be_bytes());
    bytes[28..32].copy_from_slice(&(page_count as u32).to_be_bytes());
    bytes[44..48].copy_from_slice(&4_u32.to_be_bytes());
    bytes
}

#[test]
fn sqlite_pager_reads_first_page_and_header() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("demo.db");

    let bytes = sqlite_file_bytes(4096, 1);
    fs::write(&path, &bytes).unwrap();

    let pager = Pager::open(&path).unwrap();
    let header = pager.header();
    let first_page = pager.read_page(1).unwrap();

    assert_eq!(header, &DatabaseHeader::decode(&bytes[..100]).unwrap());
    assert_eq!(first_page.len(), 4096);
}

#[test]
fn sqlite_pager_rejects_page_zero() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("demo.db");

    fs::write(&path, sqlite_file_bytes(4096, 1)).unwrap();

    let pager = Pager::open(&path).unwrap();
    let err = pager.read_page(0).unwrap_err();

    assert!(err.to_string().contains("sqlite page numbers start at 1"));
}

#[test]
fn sqlite_pager_rejects_out_of_bounds_page_reads() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("demo.db");

    fs::write(&path, sqlite_file_bytes(4096, 1)).unwrap();

    let pager = Pager::open(&path).unwrap();
    let err = pager.read_page(2).unwrap_err();

    assert!(err.to_string().contains("sqlite page 2 is out of bounds"));
}

#[test]
fn sqlite_pager_rejects_truncated_file_at_open_time() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("demo.db");

    fs::write(&path, sqlite_file_bytes(4096, 1)[..2048].to_vec()).unwrap();

    let err = Pager::open(&path).unwrap_err();

    assert!(
        err.to_string()
            .contains("shorter than declared sqlite page size")
    );
}

#[test]
fn sqlite_pager_rejects_non_page_aligned_file_at_open_time() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("demo.db");

    let mut bytes = sqlite_file_bytes(4096, 1);
    bytes.extend_from_slice(&[0_u8; 17]);
    fs::write(&path, bytes).unwrap();

    let err = Pager::open(&path).unwrap_err();

    assert!(
        err.to_string()
            .contains("not aligned to declared sqlite page size")
    );
}
