use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{DbError, Result};

use super::file_header::DatabaseHeader;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pager {
    path: PathBuf,
    header: DatabaseHeader,
}

impl Pager {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let file_len = file.metadata()?.len();

        if file_len < 100 {
            return Err(DbError::storage("sqlite database file is too short"));
        }

        let mut header_bytes = [0_u8; 100];
        file.read_exact(&mut header_bytes)?;
        let header = DatabaseHeader::decode(&header_bytes)?;
        let page_size = u64::from(header.page_size);

        if file_len < page_size {
            return Err(DbError::storage(
                "sqlite file is shorter than declared sqlite page size",
            ));
        }
        if file_len % page_size != 0 {
            return Err(DbError::storage(
                "sqlite file length is not aligned to declared sqlite page size",
            ));
        }

        Ok(Self { path, header })
    }

    pub fn header(&self) -> &DatabaseHeader {
        &self.header
    }

    pub fn usable_size(&self) -> Result<usize> {
        usize::try_from(self.header.usable_size()?)
            .map_err(|_| DbError::storage("sqlite usable page size does not fit in memory"))
    }

    pub fn page_count(&self) -> Result<u32> {
        let file_len = File::open(&self.path)?.metadata()?.len();
        let page_size = u64::from(self.header.page_size);
        let page_count = file_len / page_size;
        u32::try_from(page_count)
            .map_err(|_| DbError::storage("sqlite page count does not fit in u32"))
    }

    pub fn read_page(&self, page_no: u32) -> Result<Vec<u8>> {
        if page_no == 0 {
            return Err(DbError::storage("sqlite page numbers start at 1"));
        }

        let page_size = u64::from(self.header.page_size);
        let offset = u64::from(page_no - 1)
            .checked_mul(page_size)
            .ok_or_else(|| DbError::storage("sqlite page offset overflow"))?;
        let page_end = offset
            .checked_add(page_size)
            .ok_or_else(|| DbError::storage("sqlite page range overflow"))?;

        let mut file = File::open(&self.path)?;
        let file_len = file.metadata()?.len();
        if page_end > file_len {
            return Err(DbError::storage(format!(
                "sqlite page {page_no} is out of bounds"
            )));
        }

        let mut page = vec![
            0_u8;
            usize::try_from(page_size).map_err(|_| DbError::storage(
                "sqlite page size does not fit in memory"
            ))?
        ];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut page)?;

        Ok(page)
    }

    pub fn read_overflow_chain(
        &self,
        first_overflow_page: u32,
        remaining_bytes: usize,
    ) -> Result<Vec<u8>> {
        if first_overflow_page == 0 {
            return Err(DbError::storage(
                "sqlite overflow chains must start at a non-zero page number",
            ));
        }

        let usable_size = self.usable_size()?;
        let chunk_size = usable_size
            .checked_sub(4)
            .ok_or_else(|| DbError::storage("sqlite usable page size is too small for overflow"))?;
        if chunk_size == 0 {
            return Err(DbError::storage(
                "sqlite usable page size is too small for overflow payload data",
            ));
        }

        let mut remaining = remaining_bytes;
        let mut next_page = first_overflow_page;
        let mut payload = Vec::with_capacity(remaining_bytes);

        while remaining > 0 {
            let page = self.read_page(next_page)?;
            if page.len() < 4 {
                return Err(DbError::storage(format!(
                    "sqlite overflow page {next_page} is truncated",
                )));
            }

            let next_overflow =
                u32::from_be_bytes(page[..4].try_into().map_err(|_| {
                    DbError::storage("sqlite overflow next-page pointer is invalid")
                })?);
            let bytes_to_take = remaining.min(chunk_size);
            let data_end = 4_usize
                .checked_add(bytes_to_take)
                .ok_or_else(|| DbError::storage("sqlite overflow payload range overflow"))?;
            let data = page.get(4..data_end).ok_or_else(|| {
                DbError::storage(format!(
                    "sqlite overflow page {next_page} does not contain enough payload bytes",
                ))
            })?;
            payload.extend_from_slice(data);
            remaining -= bytes_to_take;

            if remaining == 0 {
                break;
            }
            if next_overflow == 0 {
                return Err(DbError::storage(
                    "sqlite overflow chain terminated before the payload was fully read",
                ));
            }
            next_page = next_overflow;
        }

        Ok(payload)
    }
}
