use crate::common::error::{DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    InteriorIndex,
    InteriorTable,
    LeafIndex,
    LeafTable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtreePageHeader {
    pub page_type: PageType,
    pub first_freeblock: u16,
    pub cell_count: u16,
    pub cell_content_area_start: u32,
    pub fragmented_free_bytes: u8,
    pub rightmost_pointer: Option<u32>,
}

impl BtreePageHeader {
    pub fn decode(page: &[u8], first_page: bool) -> Result<Self> {
        let offset = if first_page { 100 } else { 0 };
        let header_len = offset + 8;

        if page.len() < header_len {
            return Err(DbError::storage("sqlite btree page is too short"));
        }

        let page_type = match page[offset] {
            0x02 => PageType::InteriorIndex,
            0x05 => PageType::InteriorTable,
            0x0a => PageType::LeafIndex,
            0x0d => PageType::LeafTable,
            other => {
                return Err(DbError::storage(format!(
                    "unknown sqlite btree page type {other:#x}"
                )));
            }
        };

        let first_freeblock = u16::from_be_bytes(page[offset + 1..offset + 3].try_into().unwrap());
        let cell_count = u16::from_be_bytes(page[offset + 3..offset + 5].try_into().unwrap());
        let raw_cell_content_area_start =
            u16::from_be_bytes(page[offset + 5..offset + 7].try_into().unwrap());
        let cell_content_area_start = if raw_cell_content_area_start == 0 {
            65_536
        } else {
            u32::from(raw_cell_content_area_start)
        };
        let fragmented_free_bytes = page[offset + 7];

        let rightmost_pointer = match page_type {
            PageType::InteriorIndex | PageType::InteriorTable => {
                let pointer_end = offset + 12;
                if page.len() < pointer_end {
                    return Err(DbError::storage("sqlite interior btree page is too short"));
                }

                Some(u32::from_be_bytes(
                    page[offset + 8..pointer_end].try_into().unwrap(),
                ))
            }
            PageType::LeafIndex | PageType::LeafTable => None,
        };

        Ok(Self {
            page_type,
            first_freeblock,
            cell_count,
            cell_content_area_start,
            fragmented_free_bytes,
            rightmost_pointer,
        })
    }
}
