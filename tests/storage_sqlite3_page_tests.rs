use rustsql::storage::sqlite3::page::{BtreePageHeader, PageType};

fn make_page(len: usize) -> Vec<u8> {
    vec![0_u8; len]
}

#[test]
fn sqlite_btree_page_header_parses_leaf_table_page() {
    let mut page = make_page(4096);
    page[0] = 0x0d;
    page[3..5].copy_from_slice(&2_u16.to_be_bytes());
    page[5..7].copy_from_slice(&4000_u16.to_be_bytes());

    let header = BtreePageHeader::decode(&page, false).unwrap();

    assert_eq!(header.page_type, PageType::LeafTable);
    assert_eq!(header.cell_count, 2);
    assert_eq!(header.cell_content_area_start, 4000);
}

#[test]
fn sqlite_btree_page_header_parses_first_page_header_at_100_byte_offset() {
    let mut page = make_page(4096);
    page[100] = 0x0d;
    page[103..105].copy_from_slice(&3_u16.to_be_bytes());
    page[105..107].copy_from_slice(&3900_u16.to_be_bytes());

    let header = BtreePageHeader::decode(&page, true).unwrap();

    assert_eq!(header.page_type, PageType::LeafTable);
    assert_eq!(header.cell_count, 3);
    assert_eq!(header.cell_content_area_start, 3900);
}

#[test]
fn sqlite_btree_page_header_parses_interior_page_rightmost_pointer() {
    let mut page = make_page(4096);
    page[0] = 0x05;
    page[8..12].copy_from_slice(&99_u32.to_be_bytes());

    let header = BtreePageHeader::decode(&page, false).unwrap();

    assert_eq!(header.page_type, PageType::InteriorTable);
    assert_eq!(header.rightmost_pointer, Some(99));
}

#[test]
fn sqlite_btree_page_header_rejects_short_page() {
    let page = make_page(7);
    let error = BtreePageHeader::decode(&page, false).unwrap_err();

    assert!(error.to_string().contains("sqlite btree page is too short"));
}

#[test]
fn sqlite_btree_page_header_rejects_unknown_page_type() {
    let mut page = make_page(4096);
    page[0] = 0xff;

    let error = BtreePageHeader::decode(&page, false).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("unknown sqlite btree page type 0xff")
    );
}

#[test]
fn sqlite_btree_page_header_decodes_64kib_cell_content_area_start_sentinel() {
    let mut page = make_page(4096);
    page[0] = 0x0d;
    page[5..7].copy_from_slice(&0_u16.to_be_bytes());

    let header = BtreePageHeader::decode(&page, false).unwrap();

    assert_eq!(header.cell_content_area_start as u32, 65_536);
}
