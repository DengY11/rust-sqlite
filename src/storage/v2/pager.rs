use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::common::error::{DbError, Result};
use crate::engine::txn::TransactionId;
use crate::sql::ast::IsolationLevel;

use super::page::{MetaPage, PAGE_SIZE, PageId, PageKind, empty_page, encode_payload_page};
use super::txn_manager::TxnManager;
use super::wal::{recover_frames, write_commit, write_frame};

#[derive(Debug)]
pub struct Pager {
    db_path: PathBuf,
    wal_path: PathBuf,
    meta: MetaPage,
    committed_page_count: u32,
    page_cache: Arc<Mutex<HashMap<PageId, Vec<u8>>>>,
    txn_page_states: HashMap<u64, TxnPageState>,
    txn_manager: Arc<Mutex<TxnManager>>,
}

#[derive(Debug, Clone)]
struct PageWrite {
    before_image: Option<Vec<u8>>,
    after_image: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageWriteSnapshot {
    pub page_id: PageId,
    pub before_image: Option<Vec<u8>>,
    pub after_image: Vec<u8>,
}

#[derive(Debug, Clone)]
struct TxnPageState {
    page_writes: BTreeMap<PageId, PageWrite>,
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

        let page_cache = {
            let mut caches = shared_page_caches().lock().unwrap();
            caches
                .entry(db_path.clone())
                .or_insert_with(|| Arc::new(Mutex::new(HashMap::new())))
                .clone()
        };

        Ok(Self {
            txn_manager: Arc::new(Mutex::new(TxnManager::with_next_txn_id(meta.next_txn_id))),
            db_path,
            wal_path,
            committed_page_count: meta.page_count,
            meta,
            page_cache,
            txn_page_states: HashMap::new(),
        })
    }

    pub fn meta(&self) -> Result<MetaPage> {
        Ok(self.meta.clone())
    }

    pub fn begin(&mut self) -> Result<u64> {
        let txn_id = self
            .txn_manager
            .lock()
            .unwrap()
            .begin(IsolationLevel::ReadCommitted);
        if let Err(error) = self.begin_with_txn_id(txn_id.0) {
            let _ = self.txn_manager.lock().unwrap().abort(txn_id);
            return Err(error);
        }
        Ok(txn_id.0)
    }

    pub fn begin_with_txn_id(&mut self, txn_id: u64) -> Result<()> {
        if self.txn_page_states.contains_key(&txn_id) {
            return Err(DbError::txn(format!(
                "transaction {txn_id} is already active"
            )));
        }

        self.meta.next_txn_id = self.meta.next_txn_id.max(txn_id.saturating_add(1));
        self.txn_page_states.insert(
            txn_id,
            TxnPageState {
                page_writes: BTreeMap::new(),
            },
        );
        Ok(())
    }

    pub fn attach_txn_manager(&mut self, txn_manager: Arc<Mutex<TxnManager>>) {
        self.txn_manager = txn_manager;
    }

    pub fn commit(&mut self, txn_id: u64) -> Result<()> {
        let txn_state = self
            .txn_page_states
            .remove(&txn_id)
            .ok_or_else(|| DbError::txn(format!("transaction {txn_id} is not active")))?;
        let mut pages_to_commit = txn_state
            .page_writes
            .into_iter()
            .map(|(page_id, page_write)| (page_id, page_write.after_image))
            .collect::<BTreeMap<_, _>>();

        let committed_page_count = self.committed_page_count_after_commit(&pages_to_commit);
        let committed_meta = MetaPage {
            page_size: self.meta.page_size,
            page_count: committed_page_count,
            catalog_page_id: self.meta.catalog_page_id,
            next_txn_id: self.meta.next_txn_id,
        };
        pages_to_commit.insert(PageId(0), committed_meta.encode());

        let mut wal_bytes = Vec::new();
        for (page_id, page) in &pages_to_commit {
            write_frame(&mut wal_bytes, txn_id, *page_id, page)?;
        }
        write_commit(&mut wal_bytes, txn_id, pages_to_commit.len() as u32)?;

        let mut wal = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.wal_path)?;
        wal.write_all(&wal_bytes)?;
        wal.sync_all()?;

        for (page_id, page) in &pages_to_commit {
            write_raw_page(&self.db_path, *page_id, page)?;
            self.page_cache
                .lock()
                .unwrap()
                .insert(*page_id, page.clone());
        }
        open_rw(&self.db_path)?.sync_all()?;

        File::create(&self.wal_path)?.sync_all()?;
        self.committed_page_count = committed_page_count;
        self.meta.page_count = self.effective_page_count(committed_page_count);
        self.txn_manager
            .lock()
            .unwrap()
            .release_transaction_resources(TransactionId(txn_id))?;
        Ok(())
    }

    pub fn rollback(&mut self, txn_id: u64) -> Result<()> {
        self.validate_transaction(txn_id)?;
        if let Some(txn_state) = self.txn_page_states.remove(&txn_id) {
            for (page_id, page_write) in txn_state.page_writes.into_iter().rev() {
                if let Some(before_image) = page_write.before_image {
                    self.page_cache
                        .lock()
                        .unwrap()
                        .insert(page_id, before_image);
                } else {
                    self.page_cache.lock().unwrap().remove(&page_id);
                }
            }
        }
        self.meta.page_count = self.effective_page_count(self.committed_page_count);
        self.txn_manager
            .lock()
            .unwrap()
            .release_transaction_resources(TransactionId(txn_id))?;
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

    pub fn acquire_page_write_lock(&mut self, txn_id: u64, page_id: PageId) -> Result<()> {
        self.validate_transaction(txn_id)?;
        self.wait_for_page_write_lock(txn_id, page_id)
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

        self.wait_for_page_write_lock(txn_id, page_id)?;

        let existing_write = self
            .txn_page_states
            .get_mut(&txn_id)
            .expect("validated transaction must have a page-write state")
            .page_writes
            .remove(&page_id);
        let before_image = match existing_write {
            Some(page_write) => page_write.before_image,
            None => self.current_before_image(page_id)?,
        };

        self.txn_page_states
            .get_mut(&txn_id)
            .expect("validated transaction must have a page-write state")
            .page_writes
            .insert(
                page_id,
                PageWrite {
                    before_image,
                    after_image: page,
                },
            );
        let page = self
            .txn_page_states
            .get(&txn_id)
            .and_then(|state| state.page_writes.get(&page_id))
            .map(|page_write| page_write.after_image.clone())
            .expect("page write was just inserted");
        self.page_cache.lock().unwrap().insert(page_id, page);
        Ok(())
    }

    pub fn read_page(&self, page_id: PageId) -> Result<Vec<u8>> {
        if let Some(page) = self.page_cache.lock().unwrap().get(&page_id).cloned() {
            return Ok(page.clone());
        }
        if page_id.0 >= self.committed_page_count {
            return Err(DbError::storage(format!(
                "page {} is out of bounds for database with {} pages",
                page_id.0, self.committed_page_count
            )));
        }

        self.read_committed_page(page_id)
    }

    pub fn pending_page_writes(&self, txn_id: u64) -> Result<Vec<PageWriteSnapshot>> {
        self.validate_transaction(txn_id)?;
        Ok(self
            .txn_page_states
            .get(&txn_id)
            .expect("validated transaction must have a page-write state")
            .page_writes
            .iter()
            .map(|(page_id, page_write)| PageWriteSnapshot {
                page_id: *page_id,
                before_image: page_write.before_image.clone(),
                after_image: page_write.after_image.clone(),
            })
            .collect())
    }

    fn allocate_page_with_kind(&mut self, txn_id: u64, kind: PageKind) -> Result<PageId> {
        self.validate_transaction(txn_id)?;
        let page_id = PageId(self.meta.page_count);
        self.meta.page_count += 1;
        if let Err(error) = self.wait_for_page_write_lock(txn_id, page_id) {
            self.meta.page_count = self.meta.page_count.saturating_sub(1);
            return Err(error);
        }
        let page = empty_page(kind);
        self.txn_page_states
            .get_mut(&txn_id)
            .expect("validated transaction must have a page-write state")
            .page_writes
            .insert(
                page_id,
                PageWrite {
                    before_image: None,
                    after_image: page.clone(),
                },
            );
        self.page_cache.lock().unwrap().insert(page_id, page);
        Ok(page_id)
    }

    fn validate_transaction(&self, txn_id: u64) -> Result<()> {
        if self.txn_page_states.contains_key(&txn_id) {
            Ok(())
        } else {
            Err(DbError::txn(format!("transaction {txn_id} is not active")))
        }
    }

    fn read_committed_page(&self, page_id: PageId) -> Result<Vec<u8>> {
        read_raw_page(&self.db_path, page_id)
    }

    fn current_before_image(&self, page_id: PageId) -> Result<Option<Vec<u8>>> {
        if let Some(page) = self.page_cache.lock().unwrap().get(&page_id).cloned() {
            return Ok(Some(page.clone()));
        }
        if page_id.0 < self.committed_page_count {
            return Ok(Some(read_raw_page(&self.db_path, page_id)?));
        }
        Ok(None)
    }

    fn cleanup_aborted_transaction_state(&mut self, txn_id: u64) -> Result<()> {
        let status = self
            .txn_manager
            .lock()
            .unwrap()
            .status(TransactionId(txn_id));
        if !matches!(status, Ok(super::tx_types::TxnStatus::Aborted)) {
            return Ok(());
        }
        if let Some(txn_state) = self.txn_page_states.remove(&txn_id) {
            for (page_id, page_write) in txn_state.page_writes.into_iter().rev() {
                if let Some(before_image) = page_write.before_image {
                    self.page_cache
                        .lock()
                        .unwrap()
                        .insert(page_id, before_image);
                } else {
                    self.page_cache.lock().unwrap().remove(&page_id);
                }
            }
        }
        self.meta.page_count = self.effective_page_count(self.committed_page_count);
        Ok(())
    }

    fn wait_for_page_write_lock(&mut self, txn_id: u64, page_id: PageId) -> Result<()> {
        let notifier = { self.txn_manager.lock().unwrap().lock_wait_notifier() };
        loop {
            let observed_epoch = TxnManager::current_lock_wait_epoch(&notifier);
            let acquire_result = {
                self.txn_manager
                    .lock()
                    .unwrap()
                    .acquire_page_write(TransactionId(txn_id), page_id)
            };
            match acquire_result {
                Ok(()) => return Ok(()),
                Err(error) if is_deadlock_error(&error) => {
                    self.cleanup_aborted_transaction_state(txn_id)?;
                    return Err(error);
                }
                Err(error) if is_waitable_lock_error(&error) => {
                    self.cleanup_aborted_transaction_state(txn_id)?;
                    TxnManager::wait_for_lock_epoch_change(&notifier, observed_epoch);
                }
                Err(error) => {
                    self.cleanup_aborted_transaction_state(txn_id)?;
                    return Err(error);
                }
            }
        }
    }

    fn committed_page_count_after_commit(
        &self,
        pages_to_commit: &BTreeMap<PageId, Vec<u8>>,
    ) -> u32 {
        pages_to_commit
            .keys()
            .map(|page_id| page_id.0.saturating_add(1))
            .max()
            .unwrap_or(self.committed_page_count)
            .max(self.committed_page_count)
    }

    fn effective_page_count(&self, committed_page_count: u32) -> u32 {
        self.txn_page_states
            .values()
            .flat_map(|state| state.page_writes.keys())
            .map(|page_id| page_id.0.saturating_add(1))
            .max()
            .unwrap_or(committed_page_count)
            .max(committed_page_count)
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

fn shared_page_caches() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<HashMap<PageId, Vec<u8>>>>>> {
    static CACHES: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<HashMap<PageId, Vec<u8>>>>>>> =
        OnceLock::new();
    CACHES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn open_rw(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
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

fn is_deadlock_error(error: &DbError) -> bool {
    error.to_string().contains("deadlock")
}

fn is_waitable_lock_error(error: &DbError) -> bool {
    let message = error.to_string();
    message.contains("page write conflict")
        || message.contains("page write wait")
        || message.contains("serializable conflict")
        || message.contains("predicate write wait")
}

#[cfg(test)]
mod tests {
    use crate::storage::v2::page::empty_page;
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
        pager
            .write_page(txn, page_id, vec![9_u8; PAGE_SIZE])
            .unwrap();
        pager.rollback(txn).unwrap();

        let reopened = Pager::open(&path).unwrap();
        assert!(reopened.read_page(page_id).is_err());
    }

    #[test]
    fn rollback_restores_page_count_after_uncommitted_allocation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();

        let txn = pager.begin().unwrap();
        let page_id = pager.allocate_page(txn).unwrap();
        assert_eq!(page_id, PageId(2));
        assert_eq!(pager.meta().unwrap().page_count, 3);

        pager.rollback(txn).unwrap();
        assert_eq!(pager.meta().unwrap().page_count, 2);

        let next_txn = pager.begin().unwrap();
        assert_eq!(pager.allocate_page(next_txn).unwrap(), PageId(2));
    }

    #[test]
    fn open_replays_committed_wal_frames() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();
        let page_id = pager.allocate_page(txn).unwrap();
        pager
            .write_page(txn, page_id, vec![5_u8; PAGE_SIZE])
            .unwrap();
        pager.commit(txn).unwrap();

        let reopened = Pager::open(&path).unwrap();
        assert_eq!(reopened.read_page(page_id).unwrap()[0], 5);
    }

    #[test]
    fn concurrent_transactions_share_dirty_pages_before_commit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();

        let setup_txn = pager.begin().unwrap();
        let page_id = pager.allocate_page(setup_txn).unwrap();
        let mut committed_page = empty_page(PageKind::Leaf);
        committed_page[16] = 7;
        pager
            .write_page(setup_txn, page_id, committed_page.clone())
            .unwrap();
        pager.commit(setup_txn).unwrap();

        let txn1 = pager.begin().unwrap();
        let _txn2 = pager.begin().unwrap();

        let mut updated_page = empty_page(PageKind::Leaf);
        updated_page[16] = 9;
        pager
            .write_page(txn1, page_id, updated_page.clone())
            .unwrap();

        assert_eq!(pager.read_page(page_id).unwrap()[16], 9);
    }

    #[test]
    fn rollback_restores_shared_page_after_uncommitted_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();

        let setup_txn = pager.begin().unwrap();
        let page_id = pager.allocate_page(setup_txn).unwrap();
        let mut committed_page = empty_page(PageKind::Leaf);
        committed_page[16] = 4;
        pager
            .write_page(setup_txn, page_id, committed_page.clone())
            .unwrap();
        pager.commit(setup_txn).unwrap();

        let txn = pager.begin().unwrap();
        let mut updated_page = empty_page(PageKind::Leaf);
        updated_page[16] = 8;
        pager.write_page(txn, page_id, updated_page).unwrap();
        assert_eq!(pager.read_page(page_id).unwrap()[16], 8);

        pager.rollback(txn).unwrap();
        assert_eq!(pager.read_page(page_id).unwrap()[16], 4);
    }

    #[test]
    fn commit_does_not_publish_uncommitted_pages_from_other_transactions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();

        let txn1 = pager.begin().unwrap();
        let txn2 = pager.begin().unwrap();

        let committed_page = pager.allocate_page(txn1).unwrap();
        pager
            .write_page(txn1, committed_page, empty_page(PageKind::Leaf))
            .unwrap();

        let uncommitted_page = pager.allocate_page(txn2).unwrap();
        pager
            .write_page(txn2, uncommitted_page, empty_page(PageKind::Leaf))
            .unwrap();

        pager.commit(txn1).unwrap();

        let reopened = Pager::open(&path).unwrap();
        assert!(reopened.read_page(committed_page).is_ok());
        assert!(reopened.read_page(uncommitted_page).is_ok());

        pager.rollback(txn2).unwrap();

        let reopened_after_rollback = Pager::open(&path).unwrap();
        assert!(reopened_after_rollback.read_page(uncommitted_page).is_err());
    }

    #[test]
    fn write_page_conflict_blocks_second_writer_on_same_page() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut setup_pager = Pager::open(&path).unwrap();
        let shared_txn_manager = setup_pager.txn_manager.clone();

        let setup_txn = setup_pager.begin().unwrap();
        let page_id = setup_pager.allocate_page(setup_txn).unwrap();
        setup_pager
            .write_page(setup_txn, page_id, empty_page(PageKind::Leaf))
            .unwrap();
        setup_pager.commit(setup_txn).unwrap();

        let (writer1_locked_tx, writer1_locked_rx) = std::sync::mpsc::channel();
        let (release_writer1_tx, release_writer1_rx) = std::sync::mpsc::channel();
        let (writer2_done_tx, writer2_done_rx) = std::sync::mpsc::channel();

        let writer1_manager = shared_txn_manager.clone();
        let writer1_path = path.clone();
        let writer1 = std::thread::spawn(move || {
            let mut pager = Pager::open(&writer1_path).unwrap();
            pager.attach_txn_manager(writer1_manager);
            let txn = pager.begin().unwrap();
            pager
                .write_page(txn, page_id, empty_page(PageKind::Leaf))
                .unwrap();
            writer1_locked_tx.send(()).unwrap();
            release_writer1_rx.recv().unwrap();
            pager.commit(txn).unwrap();
        });

        writer1_locked_rx.recv().unwrap();

        let writer2_manager = shared_txn_manager.clone();
        let writer2_path = path.clone();
        let writer2 = std::thread::spawn(move || {
            let mut pager = Pager::open(&writer2_path).unwrap();
            pager.attach_txn_manager(writer2_manager);
            let txn = pager.begin().unwrap();
            pager
                .write_page(txn, page_id, empty_page(PageKind::Leaf))
                .unwrap();
            pager.commit(txn).unwrap();
            writer2_done_tx.send(()).unwrap();
        });

        assert!(
            writer2_done_rx
                .recv_timeout(std::time::Duration::from_millis(150))
                .is_err()
        );
        release_writer1_tx.send(()).unwrap();
        writer2_done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();

        writer1.join().unwrap();
        writer2.join().unwrap();
    }

    #[test]
    fn detects_deadlock_between_page_writers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();

        let setup_txn = pager.begin().unwrap();
        let left_page = pager.allocate_page(setup_txn).unwrap();
        pager
            .write_page(setup_txn, left_page, empty_page(PageKind::Leaf))
            .unwrap();
        let right_page = pager.allocate_page(setup_txn).unwrap();
        pager
            .write_page(setup_txn, right_page, empty_page(PageKind::Leaf))
            .unwrap();
        pager.commit(setup_txn).unwrap();

        let txn1 = pager.begin().unwrap();
        let txn2 = pager.begin().unwrap();

        pager
            .write_page(txn1, left_page, empty_page(PageKind::Leaf))
            .unwrap();
        pager
            .write_page(txn2, right_page, empty_page(PageKind::Leaf))
            .unwrap();

        let first_wait = pager
            .txn_manager
            .lock()
            .unwrap()
            .acquire_page_write(TransactionId(txn1), right_page)
            .unwrap_err();
        assert!(first_wait.to_string().contains("page write conflict"));

        let deadlock = pager
            .txn_manager
            .lock()
            .unwrap()
            .acquire_page_write(TransactionId(txn2), left_page)
            .unwrap_err();
        assert!(deadlock.to_string().contains("deadlock"));
    }

    #[test]
    fn deadlock_victim_rollback_cleans_local_page_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();

        let setup_txn = pager.begin().unwrap();
        let left_page = pager.allocate_page(setup_txn).unwrap();
        pager
            .write_page(setup_txn, left_page, empty_page(PageKind::Leaf))
            .unwrap();
        let right_page = pager.allocate_page(setup_txn).unwrap();
        pager
            .write_page(setup_txn, right_page, empty_page(PageKind::Leaf))
            .unwrap();
        pager.commit(setup_txn).unwrap();

        let txn1 = pager.begin().unwrap();
        let txn2 = pager.begin().unwrap();

        let mut left_update = empty_page(PageKind::Leaf);
        left_update[24] = 1;
        pager.write_page(txn1, left_page, left_update).unwrap();

        let mut right_update = empty_page(PageKind::Leaf);
        right_update[24] = 2;
        pager.write_page(txn2, right_page, right_update).unwrap();

        pager
            .txn_manager
            .lock()
            .unwrap()
            .acquire_page_write(TransactionId(txn1), right_page)
            .unwrap_err();

        let deadlock = pager
            .txn_manager
            .lock()
            .unwrap()
            .acquire_page_write(TransactionId(txn2), left_page)
            .unwrap_err();
        assert!(deadlock.to_string().contains("deadlock"));

        pager.cleanup_aborted_transaction_state(txn2).unwrap();

        pager
            .write_page(txn1, right_page, empty_page(PageKind::Leaf))
            .unwrap();
        assert_eq!(pager.read_page(right_page).unwrap()[24], 0);
    }
}
