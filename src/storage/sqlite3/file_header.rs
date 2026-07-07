use crate::common::error::{DbError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseHeader {
    pub page_size: u32,
    pub reserved_bytes: u8,
    pub page_count_hint: u32,
    pub freelist_count: u32,
    pub schema_version: u32,
    pub schema_format: u32,
    pub user_version: u32,
    pub application_id: u32,
}

impl DatabaseHeader {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 100 {
            return Err(DbError::storage("sqlite header must be exactly 100 bytes"));
        }
        if &bytes[..16] != b"SQLite format 3\0" {
            return Err(DbError::storage("invalid sqlite database header"));
        }

        let raw_page_size = u16::from_be_bytes(bytes[16..18].try_into().unwrap());
        let page_size = decode_page_size(raw_page_size)?;
        let reserved_bytes = bytes[20];
        let page_count_hint = u32::from_be_bytes(bytes[28..32].try_into().unwrap());
        let freelist_count = u32::from_be_bytes(bytes[36..40].try_into().unwrap());
        let schema_version = u32::from_be_bytes(bytes[40..44].try_into().unwrap());
        let schema_format = u32::from_be_bytes(bytes[44..48].try_into().unwrap());
        let user_version = u32::from_be_bytes(bytes[60..64].try_into().unwrap());
        let application_id = u32::from_be_bytes(bytes[68..72].try_into().unwrap());

        Ok(Self {
            page_size,
            reserved_bytes,
            page_count_hint,
            freelist_count,
            schema_version,
            schema_format,
            user_version,
            application_id,
        })
    }

    pub fn usable_size(&self) -> Result<u32> {
        let reserved = u32::from(self.reserved_bytes);
        if reserved >= self.page_size {
            return Err(DbError::storage(
                "sqlite reserved byte count must be smaller than page size",
            ));
        }
        self.page_size
            .checked_sub(reserved)
            .ok_or_else(|| DbError::storage("sqlite usable page size underflow"))
    }
}

fn decode_page_size(raw_page_size: u16) -> Result<u32> {
    let page_size = if raw_page_size == 1 {
        65_536
    } else {
        raw_page_size as u32
    };

    if page_size.is_power_of_two() && (512..=65_536).contains(&page_size) {
        Ok(page_size)
    } else {
        Err(DbError::storage("invalid sqlite page size"))
    }
}
