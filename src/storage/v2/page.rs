use serde::{Deserialize, Serialize};

use crate::common::error::{DbError, Result};

pub const PAGE_SIZE: usize = 4096;
pub const STORAGE_MAGIC: &[u8; 4] = b"RSV2";

const KIND_OFFSET: usize = 4;
const PAYLOAD_LEN_OFFSET: usize = 8;
const PAYLOAD_OFFSET: usize = 16;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
pub struct PageId(pub u32);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Meta = 1,
    Catalog = 2,
    Leaf = 3,
    Internal = 4,
}

impl TryFrom<u8> for PageKind {
    type Error = DbError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Meta),
            2 => Ok(Self::Catalog),
            3 => Ok(Self::Leaf),
            4 => Ok(Self::Internal),
            other => Err(DbError::storage(format!(
                "unknown storage_v2 page kind: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaPage {
    pub page_size: u32,
    pub page_count: u32,
    pub catalog_page_id: PageId,
    pub next_txn_id: u64,
}

impl MetaPage {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut page = empty_page(PageKind::Meta);
        page[PAYLOAD_OFFSET..PAYLOAD_OFFSET + 4].copy_from_slice(&self.page_size.to_le_bytes());
        page[PAYLOAD_OFFSET + 4..PAYLOAD_OFFSET + 8]
            .copy_from_slice(&self.page_count.to_le_bytes());
        page[PAYLOAD_OFFSET + 8..PAYLOAD_OFFSET + 12]
            .copy_from_slice(&self.catalog_page_id.0.to_le_bytes());
        page[PAYLOAD_OFFSET + 12..PAYLOAD_OFFSET + 20]
            .copy_from_slice(&self.next_txn_id.to_le_bytes());
        page
    }

    pub fn decode(page: &[u8]) -> Result<Self> {
        validate_page_buffer(page)?;
        if page_kind(page)? != PageKind::Meta {
            return Err(DbError::storage("storage_v2 meta page has wrong page kind"));
        }

        Ok(Self {
            page_size: u32::from_le_bytes(
                page[PAYLOAD_OFFSET..PAYLOAD_OFFSET + 4].try_into().unwrap(),
            ),
            page_count: u32::from_le_bytes(
                page[PAYLOAD_OFFSET + 4..PAYLOAD_OFFSET + 8]
                    .try_into()
                    .unwrap(),
            ),
            catalog_page_id: PageId(u32::from_le_bytes(
                page[PAYLOAD_OFFSET + 8..PAYLOAD_OFFSET + 12]
                    .try_into()
                    .unwrap(),
            )),
            next_txn_id: u64::from_le_bytes(
                page[PAYLOAD_OFFSET + 12..PAYLOAD_OFFSET + 20]
                    .try_into()
                    .unwrap(),
            ),
        })
    }
}

pub fn empty_page(kind: PageKind) -> Vec<u8> {
    let mut page = vec![0_u8; PAGE_SIZE];
    page[..4].copy_from_slice(STORAGE_MAGIC);
    page[KIND_OFFSET] = kind as u8;
    page
}

pub fn encode_payload_page(kind: PageKind, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > PAGE_SIZE - PAYLOAD_OFFSET {
        return Err(DbError::storage(format!(
            "payload of {} bytes exceeds page capacity {}",
            payload.len(),
            PAGE_SIZE - PAYLOAD_OFFSET
        )));
    }

    let mut page = empty_page(kind);
    page[PAYLOAD_LEN_OFFSET..PAYLOAD_LEN_OFFSET + 4]
        .copy_from_slice(&(payload.len() as u32).to_le_bytes());
    page[PAYLOAD_OFFSET..PAYLOAD_OFFSET + payload.len()].copy_from_slice(payload);
    Ok(page)
}

pub fn decode_payload(page: &[u8], expected_kind: PageKind) -> Result<Vec<u8>> {
    validate_page_buffer(page)?;
    let kind = page_kind(page)?;
    if kind != expected_kind {
        return Err(DbError::storage(format!(
            "expected {:?} page but found {:?}",
            expected_kind, kind
        )));
    }

    let payload_len = u32::from_le_bytes(
        page[PAYLOAD_LEN_OFFSET..PAYLOAD_LEN_OFFSET + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    if payload_len > PAGE_SIZE - PAYLOAD_OFFSET {
        return Err(DbError::storage(format!(
            "page payload length {payload_len} exceeds capacity"
        )));
    }

    Ok(page[PAYLOAD_OFFSET..PAYLOAD_OFFSET + payload_len].to_vec())
}

pub fn page_kind(page: &[u8]) -> Result<PageKind> {
    validate_page_buffer(page)?;
    PageKind::try_from(page[KIND_OFFSET])
}

fn validate_page_buffer(page: &[u8]) -> Result<()> {
    if page.len() != PAGE_SIZE {
        return Err(DbError::storage(format!(
            "storage_v2 page buffer must be exactly {PAGE_SIZE} bytes"
        )));
    }
    if &page[..4] != STORAGE_MAGIC {
        return Err(DbError::storage("invalid storage_v2 page magic"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_page_roundtrips_through_bytes() {
        let meta = MetaPage {
            page_size: PAGE_SIZE as u32,
            page_count: 3,
            catalog_page_id: PageId(1),
            next_txn_id: 9,
        };

        let encoded = meta.encode();
        let decoded = MetaPage::decode(&encoded).unwrap();
        assert_eq!(decoded, meta);
    }

    #[test]
    fn rejects_page_buffer_with_wrong_magic() {
        let mut bytes = [0_u8; PAGE_SIZE];
        bytes[..4].copy_from_slice(b"nope");
        let error = MetaPage::decode(&bytes).unwrap_err();
        assert!(error.to_string().contains("invalid storage_v2 page magic"));
    }
}
