use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{DbError, Result};

use super::page::{
    MetaPage, PAGE_SIZE, PageId, PageKind, empty_page, encode_payload_page,
};
use super::wal::{recover_frames, write_commit, write_frame};

#[derive(Debug)]
pub struct Pager {
    db_path: PathBuf,
    wal_path: PathBuf,
    meta: MetaPage,
    active_txn: Option<u64>,
    dirty_pages: BTreeMap<PageId, Vec<u8>>,
    pre_txn_meta: Option<MetaPage>,
}

impl Pager {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db_path = path.as_ref().to_path_buf();
        let wal_path = db_path.with_extension("wal");
        let meta = if db_path.exists() {
            Self::load_meta_and_recover(&db_path, &wal_path)?
        } else {
            Self::bootstrap_new_file(&db_path, &wal_path)?
        };

        Ok(Self {
            db_path,
            wal_path,
            meta,
            active_txn: None,
            dirty_pages: BTreeMap::new(),
            pre_txn_meta: None,
        })
    }

    pub fn meta(&self) -> Result<MetaPage> {
        Ok(self.meta.clone())
    }

    pub fn begin(&mut self) -> Result<u64> {
        if self.active_txn.is_some() {
            return Err(DbError::txn("transaction already active"));
        }

        let snapshot = self.meta.clone();
        let txn_id = snapshot.next_txn_id;
        self.meta.next_txn_id += 1;
        self.pre_txn_meta = Some(snapshot);
        self.active_txn = Some(txn_id);
        Ok(txn_id)
    }

    pub fn commit(&mut self, txn_id: u64) -> Result<()> {
        self.validate_transaction(txn_id)?;
        self.dirty_pages.insert(PageId(0), self.meta.encode());

        let mut wal_bytes = Vec::new();
        for (page_id, page) in &self.dirty_pages {
            write_frame(&mut wal_bytes, txn_id, *page_id, page)?;
        }
        write_commit(&mut wal_bytes, txn_id, self.dirty_pages.len() as u32)?;

        let mut wal = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.wal_path)?;
        wal.write_all(&wal_bytes)?;
        wal.sync_all()?;

        for (page_id, page) in &self.dirty_pages {
            write_raw_page(&self.db_path, *page_id, page)?;
        }
        open_rw(&self.db_path)?.sync_all()?;

        File::create(&self.wal_path)?.sync_all()?;
        self.dirty_pages.clear();
        self.active_txn = None;
        self.pre_txn_meta = None;
        Ok(())
    }

    pub fn rollback(&mut self, txn_id: u64) -> Result<()> {
        self.validate_transaction(txn_id)?;
        self.dirty_pages.clear();
        if let Some(meta) = self.pre_txn_meta.take() {
            self.meta = meta;
        }
        self.active_txn = None;
        Ok(())
    }

    pub fn allocate_page(&mut self, txn_id: u64) -> Result<PageId> {
        self.allocate_page_with_kind(txn_id, PageKind::Leaf)
    }

    pub fn allocate_leaf_page(&mut self, txn_id: u64) -> Result<PageId> {
        self.allocate_page_with_kind(txn_id, PageKind::Leaf)
    }

    pub fn allocate_internal_page(&mut self, txn_id: u64) -> Result<PageId> {
        self.allocate_page_with_kind(txn_id, PageKind::Internal)
    }

    pub fn write_page(&mut self, txn_id: u64, page_id: PageId, page: Vec<u8>) -> Result<()> {
        self.validate_transaction(txn_id)?;
        if page.len() != PAGE_SIZE {
            return Err(DbError::storage(format!(
                "storage_v2 page writes require exactly {PAGE_SIZE} bytes"
            )));
        }
        if page_id.0 >= self.meta.page_count {
            return Err(DbError::storage(format!(
                "page {} is out of bounds for database with {} pages",
                page_id.0, self.meta.page_count
            )));
        }

        self.dirty_pages.insert(page_id, page);
        Ok(())
    }

    pub fn read_page(&self, page_id: PageId) -> Result<Vec<u8>> {
        if let Some(page) = self.dirty_pages.get(&page_id) {
            return Ok(page.clone());
        }
        if page_id.0 >= self.meta.page_count {
            return Err(DbError::storage(format!(
                "page {} is out of bounds for database with {} pages",
                page_id.0, self.meta.page_count
            )));
        }

        read_raw_page(&self.db_path, page_id)
    }

    fn allocate_page_with_kind(&mut self, txn_id: u64, kind: PageKind) -> Result<PageId> {
        self.validate_transaction(txn_id)?;
        let page_id = PageId(self.meta.page_count);
        self.meta.page_count += 1;
        self.dirty_pages.insert(page_id, empty_page(kind));
        Ok(page_id)
    }

    fn validate_transaction(&self, txn_id: u64) -> Result<()> {
        match self.active_txn {
            Some(active) if active == txn_id => Ok(()),
            Some(active) => Err(DbError::txn(format!(
                "transaction {txn_id} is not active; current transaction is {active}"
            ))),
            None => Err(DbError::txn("no active transaction")),
        }
    }

    fn bootstrap_new_file(db_path: &Path, wal_path: &Path) -> Result<MetaPage> {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let meta = MetaPage {
            page_size: PAGE_SIZE as u32,
            page_count: 2,
            catalog_page_id: PageId(1),
            next_txn_id: 1,
        };

        write_raw_page(db_path, PageId(0), &meta.encode())?;
        let catalog_page = encode_payload_page(PageKind::Catalog, &[])?;
        write_raw_page(db_path, PageId(1), &catalog_page)?;
        File::create(wal_path)?.sync_all()?;
        Ok(meta)
    }

    fn load_meta_and_recover(db_path: &Path, wal_path: &Path) -> Result<MetaPage> {
        if wal_path.exists() {
            let wal_bytes = fs::read(wal_path)?;
            let frames = recover_frames(&wal_bytes)?;
            if !frames.is_empty() {
                for frame in frames {
                    write_raw_page(db_path, frame.page_id, &frame.page_bytes)?;
                }
                open_rw(db_path)?.sync_all()?;
            }
            File::create(wal_path)?.sync_all()?;
        }

        let meta_page = read_raw_page(db_path, PageId(0))?;
        MetaPage::decode(&meta_page)
    }
}

fn open_rw(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?)
}

fn write_raw_page(path: &Path, page_id: PageId, page: &[u8]) -> Result<()> {
    if page.len() != PAGE_SIZE {
        return Err(DbError::storage(format!(
            "storage_v2 page writes require exactly {PAGE_SIZE} bytes"
        )));
    }

    let mut file = open_rw(path)?;
    file.seek(SeekFrom::Start(page_id.0 as u64 * PAGE_SIZE as u64))?;
    file.write_all(page)?;
    Ok(())
}

fn read_raw_page(path: &Path, page_id: PageId) -> Result<Vec<u8>> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    file.seek(SeekFrom::Start(page_id.0 as u64 * PAGE_SIZE as u64))?;
    let mut page = vec![0_u8; PAGE_SIZE];
    file.read_exact(&mut page)?;
    Ok(page)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn pager_initializes_new_database_and_reopens_meta_page() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");

        {
            let pager = Pager::open(&path).unwrap();
            let meta = pager.meta().unwrap();
            assert_eq!(meta.page_count, 2);
            assert_eq!(meta.catalog_page_id, PageId(1));
        }

        let reopened = Pager::open(&path).unwrap();
        let meta = reopened.meta().unwrap();
        assert_eq!(meta.page_count, 2);
        assert_eq!(meta.catalog_page_id, PageId(1));
    }

    #[test]
    fn rollback_discards_dirty_pages() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();
        let page_id = pager.allocate_page(txn).unwrap();
        pager.write_page(txn, page_id, vec![9_u8; PAGE_SIZE]).unwrap();
        pager.rollback(txn).unwrap();

        let reopened = Pager::open(&path).unwrap();
        assert!(reopened.read_page(page_id).is_err());
    }

    #[test]
    fn open_replays_committed_wal_frames() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();
        let page_id = pager.allocate_page(txn).unwrap();
        pager.write_page(txn, page_id, vec![5_u8; PAGE_SIZE]).unwrap();
        pager.commit(txn).unwrap();

        let reopened = Pager::open(&path).unwrap();
        assert_eq!(reopened.read_page(page_id).unwrap()[0], 5);
    }
}
