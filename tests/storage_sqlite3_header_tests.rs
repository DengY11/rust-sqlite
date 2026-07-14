use rustsql::storage::sqlite3::file_header::DatabaseHeader;

#[test]
fn sqlite_header_parses_valid_database_header() {
    let mut bytes = [0_u8; 100];
    bytes[..16].copy_from_slice(b"SQLite format 3\0");
    bytes[16..18].copy_from_slice(&4096_u16.to_be_bytes());
    bytes[28..32].copy_from_slice(&1_u32.to_be_bytes());
    bytes[44..48].copy_from_slice(&4_u32.to_be_bytes());
    bytes[56..60].copy_from_slice(&1_u32.to_be_bytes());

    let header = DatabaseHeader::decode(&bytes).unwrap();

    assert_eq!(header.page_size, 4096);
    assert_eq!(header.reserved_bytes, 0);
    assert_eq!(header.schema_format, 4);
    assert_eq!(header.text_encoding, 1);
    assert_eq!(header.page_count_hint, 1);
    assert_eq!(header.usable_size().unwrap(), 4096);
}

#[test]
fn sqlite_header_decodes_64kib_page_size_from_special_case_encoding() {
    let mut bytes = [0_u8; 100];
    bytes[..16].copy_from_slice(b"SQLite format 3\0");
    bytes[16..18].copy_from_slice(&1_u16.to_be_bytes());
    bytes[28..32].copy_from_slice(&1_u32.to_be_bytes());
    bytes[44..48].copy_from_slice(&4_u32.to_be_bytes());
    bytes[56..60].copy_from_slice(&1_u32.to_be_bytes());

    let header = DatabaseHeader::decode(&bytes).unwrap();

    assert_eq!(header.page_size, 65_536);
    assert_eq!(header.reserved_bytes, 0);
    assert_eq!(header.usable_size().unwrap(), 65_536);
}

#[test]
fn sqlite_header_rejects_invalid_page_size() {
    let mut bytes = [0_u8; 100];
    bytes[..16].copy_from_slice(b"SQLite format 3\0");
    bytes[16..18].copy_from_slice(&500_u16.to_be_bytes());
    bytes[28..32].copy_from_slice(&1_u32.to_be_bytes());
    bytes[44..48].copy_from_slice(&4_u32.to_be_bytes());

    let error = DatabaseHeader::decode(&bytes).unwrap_err();

    assert!(error.to_string().contains("invalid sqlite page size"));
}

#[test]
fn sqlite_header_reports_usable_size_after_reserved_bytes() {
    let mut bytes = [0_u8; 100];
    bytes[..16].copy_from_slice(b"SQLite format 3\0");
    bytes[16..18].copy_from_slice(&512_u16.to_be_bytes());
    bytes[20] = 12;
    bytes[28..32].copy_from_slice(&1_u32.to_be_bytes());
    bytes[44..48].copy_from_slice(&4_u32.to_be_bytes());

    let header = DatabaseHeader::decode(&bytes).unwrap();

    assert_eq!(header.page_size, 512);
    assert_eq!(header.reserved_bytes, 12);
    assert_eq!(header.usable_size().unwrap(), 500);
}
