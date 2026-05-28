use std::collections::HashMap;

use crate::common::error::{DbError, Result};

use super::page::{PAGE_SIZE, PageId};

const RECORD_FRAME: u8 = 1;
const RECORD_COMMIT: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalFrame {
    pub txn_id: u64,
    pub page_id: PageId,
    pub page_bytes: Vec<u8>,
}

pub fn write_frame(out: &mut Vec<u8>, txn_id: u64, page_id: PageId, page: &[u8]) -> Result<()> {
    if page.len() != PAGE_SIZE {
        return Err(DbError::storage(format!(
            "wal frames must contain exactly {PAGE_SIZE} bytes"
        )));
    }

    out.push(RECORD_FRAME);
    out.extend_from_slice(&txn_id.to_le_bytes());
    out.extend_from_slice(&page_id.0.to_le_bytes());
    out.extend_from_slice(&(page.len() as u32).to_le_bytes());
    out.extend_from_slice(page);
    Ok(())
}

pub fn write_commit(out: &mut Vec<u8>, txn_id: u64, frame_count: u32) -> Result<()> {
    out.push(RECORD_COMMIT);
    out.extend_from_slice(&txn_id.to_le_bytes());
    out.extend_from_slice(&frame_count.to_le_bytes());
    Ok(())
}

pub fn recover_frames(bytes: &[u8]) -> Result<Vec<WalFrame>> {
    let mut offset = 0;
    let mut pending: HashMap<u64, Vec<WalFrame>> = HashMap::new();
    let mut committed = Vec::new();

    while offset < bytes.len() {
        let Some(&record_type) = bytes.get(offset) else {
            break;
        };
        offset += 1;

        match record_type {
            RECORD_FRAME => {
                if bytes.len().saturating_sub(offset) < 8 + 4 + 4 {
                    break;
                }

                let txn_id = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
                offset += 8;
                let page_id = PageId(u32::from_le_bytes(
                    bytes[offset..offset + 4].try_into().unwrap(),
                ));
                offset += 4;
                let page_len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
                    as usize;
                offset += 4;

                if bytes.len().saturating_sub(offset) < page_len {
                    break;
                }
                if page_len != PAGE_SIZE {
                    return Err(DbError::storage(format!(
                        "wal frame for page {} had {} bytes instead of {}",
                        page_id.0, page_len, PAGE_SIZE
                    )));
                }

                let page_bytes = bytes[offset..offset + page_len].to_vec();
                offset += page_len;
                pending.entry(txn_id).or_default().push(WalFrame {
                    txn_id,
                    page_id,
                    page_bytes,
                });
            }
            RECORD_COMMIT => {
                if bytes.len().saturating_sub(offset) < 8 + 4 {
                    break;
                }

                let txn_id = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
                offset += 8;
                let frame_count =
                    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
                offset += 4;

                if let Some(frames) = pending.remove(&txn_id) {
                    if frames.len() == frame_count {
                        committed.extend(frames);
                    }
                }
            }
            other => {
                return Err(DbError::storage(format!(
                    "unknown storage_v2 wal record type: {other}"
                )));
            }
        }
    }

    Ok(committed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_recovery_replays_only_committed_transactions() {
        let mut wal = Vec::new();
        write_frame(&mut wal, 7, PageId(2), &[1_u8; PAGE_SIZE]).unwrap();
        write_commit(&mut wal, 7, 1).unwrap();
        write_frame(&mut wal, 8, PageId(3), &[2_u8; PAGE_SIZE]).unwrap();

        let recovered = recover_frames(&wal).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].txn_id, 7);
        assert_eq!(recovered[0].page_id, PageId(2));
    }
}
