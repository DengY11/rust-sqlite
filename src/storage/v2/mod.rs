use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use crate::common::error::{DbError, Result};
use crate::common::types::{ColumnDef, IndexMeta, Row, RowId, Schema, Value};
use crate::engine::traits::{
    CatalogStore, IndexStore, PlanningStorageEngine, TableStore, TransactionManager,
};
use crate::engine::txn::TransactionId;
use crate::sql::ast::{CompareOp, IsolationLevel};
use crate::sql::planner::PlanningContext;

use self::btree::BTree;
use self::catalog::{CatalogState, load_catalog, store_catalog};
use self::codec::{
    decode_index_key, decode_row, decode_row_ids, decode_versioned_row, encode_index_key,
    encode_row, encode_row_ids, encode_uncommitted_row_version, finalize_row_versions,
    mark_row_deleted, append_row_version, project_index_key, visible_row, VersionedRow,
};
use self::index_tree::IndexTree;
use self::pager::{PageWriteSnapshot, Pager};
use self::tx_types::{TxnStatus, UndoRecord};
use self::txn_manager::TxnManager as StorageTxnManager;

pub mod btree;
pub mod catalog;
pub mod codec;
pub mod index_tree;
pub mod page;
pub mod pager;
pub mod tx_types;
pub mod txn_manager;
pub mod wal;

#[derive(Debug)]
pub struct FileStorage {
    pager: RefCell<Pager>,
    catalog: RefCell<CatalogState>,
    txn_manager: Arc<Mutex<StorageTxnManager>>,
    session_catalog_snapshots: RefCell<HashMap<TransactionId, CatalogState>>,
}

fn shared_txn_managers() -> &'static Mutex<HashMap<std::path::PathBuf, Arc<Mutex<StorageTxnManager>>>> {
    static MANAGERS: OnceLock<Mutex<HashMap<std::path::PathBuf, Arc<Mutex<StorageTxnManager>>>>> =
        OnceLock::new();
    MANAGERS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl FileStorage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut pager = Pager::open(&path)?;
        let catalog = load_catalog(&pager)?;
        let next_txn_id = pager.meta()?.next_txn_id;
        let txn_manager = {
            let mut managers = shared_txn_managers().lock().unwrap();
            managers
                .entry(path)
                .or_insert_with(|| Arc::new(Mutex::new(StorageTxnManager::with_next_txn_id(next_txn_id))))
                .clone()
        };
        txn_manager.lock().unwrap().sync_next_row_ids(&catalog.next_row_ids);
        pager.attach_txn_manager(txn_manager.clone());

        Ok(Self {
            pager: RefCell::new(pager),
            catalog: RefCell::new(catalog),
            txn_manager,
            session_catalog_snapshots: RefCell::new(HashMap::new()),
        })
    }

    fn validate_transaction(&self, transaction_id: TransactionId) -> Result<()> {
        match self.txn_manager.lock().unwrap().get(transaction_id) {
            Ok(txn) if txn.status == TxnStatus::Active => Ok(()),
            Ok(_) => Err(DbError::txn(format!(
                "transaction {} is not active",
                transaction_id.0
            ))),
            Err(error) => Err(error),
        }
    }

    fn validate_snapshot_transaction(&self, transaction_id: Option<TransactionId>) -> Result<()> {
        match transaction_id {
            Some(id) => self.validate_transaction(id),
            None if !self.session_catalog_snapshots.borrow().is_empty() => Err(DbError::txn(
                "metadata snapshot requires the active transaction id while a transaction is open",
            )),
            None => Ok(()),
        }
    }

    fn planning_catalog_snapshot(&self, transaction_id: Option<TransactionId>) -> Result<CatalogState> {
        if let Some(txn_id) = transaction_id {
            let isolation_level = {
                let manager = self.txn_manager.lock().unwrap();
                manager.get(txn_id)?.isolation_level
            };
            if isolation_level == IsolationLevel::ReadCommitted {
                self.txn_manager.lock().unwrap().refresh_snapshot(txn_id)?;

                let current_catalog = self.catalog.borrow().clone();
                let session_snapshot = self
                    .session_catalog_snapshots
                    .borrow()
                    .get(&txn_id)
                    .cloned();
                if session_snapshot.as_ref() == Some(&current_catalog) {
                    let catalog = load_catalog(&self.pager.borrow())?;
                    *self.catalog.borrow_mut() = catalog.clone();
                    return Ok(catalog);
                }

                return Ok(current_catalog);
            }

            return Ok(self.catalog.borrow().clone());
        }

        Ok(self.catalog.borrow().clone())
    }

    fn snapshot_for_transaction(
        &self,
        transaction_id: TransactionId,
    ) -> Result<self::tx_types::TxnSnapshot> {
        let mut manager = self.txn_manager.lock().unwrap();
        if manager.isolation_level(transaction_id)? == IsolationLevel::ReadCommitted {
            manager.refresh_snapshot(transaction_id)
        } else {
            manager.snapshot(transaction_id)
        }
    }

    fn finalize_transaction_row_versions(
        &self,
        transaction_id: TransactionId,
        commit_ts: u64,
    ) -> Result<()> {
        let catalog = self.catalog.borrow().clone();
        let mut pager = self.pager.borrow_mut();

        for root_page_id in catalog.table_roots.values() {
            let mut tree = BTree::from_root(*root_page_id);
            for (row_id, bytes) in tree.scan_all(&pager, transaction_id.0)? {
                let finalized = finalize_row_versions(&bytes, transaction_id.0, commit_ts)?;
                if finalized != bytes {
                    tree.insert(&mut pager, transaction_id.0, row_id, &finalized)?;
                }
            }
        }

        Ok(())
    }

    fn sync_transaction_page_writes(&self, transaction_id: TransactionId) -> Result<()> {
        let page_writes = self
            .pager
            .borrow()
            .pending_page_writes(transaction_id.0)?
            .into_iter()
            .map(
                |PageWriteSnapshot {
                     page_id,
                     before_image,
                     after_image,
                 }| self::tx_types::PageWriteSetEntry {
                    page_id,
                    before_image,
                    after_image,
                },
            )
            .collect();
        self.txn_manager
            .lock()
            .unwrap()
            .replace_page_write_set(transaction_id, page_writes)
    }

    fn record_undo(&self, transaction_id: TransactionId, record: UndoRecord) -> Result<()> {
        self.txn_manager
            .lock()
            .unwrap()
            .record_undo(transaction_id, record)
    }

    fn wait_for_write_conflicts(
        &self,
        transaction_id: TransactionId,
        table: &str,
        index_keys: &[(String, Vec<Value>)],
    ) -> Result<()> {
        let notifier = { self.txn_manager.lock().unwrap().lock_wait_notifier() };
        loop {
            let observed_epoch = StorageTxnManager::current_lock_wait_epoch(&notifier);
            let result = self
                .txn_manager
                .lock()
                .unwrap()
                .check_write_conflicts(transaction_id, table, index_keys);
            match result {
                Ok(()) => return Ok(()),
                Err(error) if is_deadlock_error(&error) => return Err(error),
                Err(error) if is_waitable_lock_error(&error) => {
                    StorageTxnManager::wait_for_lock_epoch_change(&notifier, observed_epoch);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn maybe_purge_garbage(&self) -> Result<()> {
        let (purge_horizon, planned_batch) = {
            let mut manager = self.txn_manager.lock().unwrap();
            let purge_horizon = manager.purge_horizon();
            manager.purge_finished_transactions_up_to(purge_horizon);

            let history_list_length = manager.history_list_length();
            if history_list_length == 0 {
                return Ok(());
            }

            let purge_batch_size = history_list_length
                .ilog2()
                .saturating_add(1)
                .clamp(1, 8) as usize;
            let planned_batch = manager.planned_purge_batch(purge_horizon, purge_batch_size);
            if planned_batch.is_empty() {
                manager.purge_finished_transactions_up_to(purge_horizon);
                return Ok(());
            }

            (purge_horizon, planned_batch)
        };

        let purge_txn = {
            let mut manager = self.txn_manager.lock().unwrap();
            manager.begin(IsolationLevel::ReadCommitted)
        };

        if let Err(error) = self.pager.borrow_mut().begin_with_txn_id(purge_txn.0) {
            let _ = self.txn_manager.lock().unwrap().abort(purge_txn);
            self.txn_manager
                .lock()
                .unwrap()
                .purge_finished_transactions_up_to(purge_horizon);
            return Err(error);
        }

        match self.purge_row_versions(purge_txn, purge_horizon, &planned_batch) {
            Ok(()) => {
                self.txn_manager.lock().unwrap().commit(purge_txn)?;
                self.pager.borrow_mut().commit(purge_txn.0)?;
                self.txn_manager
                    .lock()
                    .unwrap()
                    .complete_purge_batch(&planned_batch)?;
                self.txn_manager
                    .lock()
                    .unwrap()
                    .purge_finished_transactions_up_to(purge_horizon);
                Ok(())
            }
            Err(error) => {
                let _ = self.pager.borrow_mut().rollback(purge_txn.0);
                let _ = self.txn_manager.lock().unwrap().abort(purge_txn);
                self.txn_manager
                    .lock()
                    .unwrap()
                    .purge_finished_transactions_up_to(purge_horizon);
                Err(error)
            }
        }
    }

    fn purge_row_versions(
        &self,
        transaction_id: TransactionId,
        purge_horizon: u64,
        planned_batch: &[(TransactionId, UndoRecord)],
    ) -> Result<()> {
        for (_, record) in planned_batch {
            self.purge_record(transaction_id, purge_horizon, record)?;
        }
        Ok(())
    }

    fn purge_record(
        &self,
        transaction_id: TransactionId,
        purge_horizon: u64,
        record: &UndoRecord,
    ) -> Result<()> {
        match record {
            UndoRecord::DeleteRow { table, row_id, .. } => {
                self.purge_deleted_row(transaction_id, table, *row_id, purge_horizon)
            }
            _ => Ok(()),
        }
    }

    fn purge_deleted_row(
        &self,
        transaction_id: TransactionId,
        table: &str,
        row_id: RowId,
        purge_horizon: u64,
    ) -> Result<()> {
        let Ok((_, root_page_id)) = self.table_schema_and_root(table) else {
            return Ok(());
        };

        let mut pager = self.pager.borrow_mut();
        let mut tree = BTree::from_root(root_page_id);
        let Some(bytes) = tree.get(&pager, transaction_id.0, row_id.0)? else {
            return Ok(());
        };
        let versioned = decode_versioned_row(&bytes)?;
        let retained_versions = versioned
            .versions
            .into_iter()
            .filter(|version| {
                !version
                    .deleted_commit_ts
                    .is_some_and(|commit_ts| commit_ts <= purge_horizon)
            })
            .collect::<Vec<_>>();
        if retained_versions.is_empty() {
            tree.delete(&mut pager, transaction_id.0, row_id.0)?;
            return Ok(());
        }

        let retained_bytes = serde_json::to_vec(&VersionedRow {
            versions: retained_versions,
        })?;
        if retained_bytes != bytes {
            tree.insert(&mut pager, transaction_id.0, row_id.0, &retained_bytes)?;
        }
        Ok(())
    }

    fn replay_undo_records(&self, transaction_id: TransactionId) -> Result<()> {
        let undo_records = self.txn_manager.lock().unwrap().take_undo_records(transaction_id)?;
        if undo_records.is_empty() {
            return Ok(());
        }

        let mut pager = self.pager.borrow_mut();
        for record in undo_records {
            match record {
                UndoRecord::PageWrite { .. } => {}
                UndoRecord::InsertRow { table, row_id } => {
                    if let Ok((_, root_page_id)) = self.table_schema_and_root(&table) {
                        let mut tree = BTree::from_root(root_page_id);
                        tree.delete(&mut pager, transaction_id.0, row_id.0)?;
                    }
                }
                UndoRecord::DeleteRow {
                    table,
                    row_id,
                    previous_bytes,
                } => {
                    if let Ok((_, root_page_id)) = self.table_schema_and_root(&table) {
                        let mut tree = BTree::from_root(root_page_id);
                        tree.insert(&mut pager, transaction_id.0, row_id.0, &previous_bytes)?;
                    }
                }
                UndoRecord::IndexInsert {
                    table,
                    index,
                    row_id,
                    key,
                } => {
                    if let Ok((_, root_page_id)) = self.index_meta_and_root(&table, &index) {
                        let encoded_key = encode_index_key(&key)?;
                        let mut tree = IndexTree::from_root(root_page_id);
                        Self::remove_row_id_from_index_entry(
                            &mut tree,
                            &mut pager,
                            transaction_id.0,
                            &encoded_key,
                            row_id,
                        )?;
                    }
                }
                UndoRecord::IndexDelete {
                    table,
                    index,
                    row_id,
                    key,
                } => {
                    if let Ok((_, root_page_id)) = self.index_meta_and_root(&table, &index) {
                        let encoded_key = encode_index_key(&key)?;
                        let mut tree = IndexTree::from_root(root_page_id);
                        Self::upsert_index_entry(
                            &mut tree,
                            &mut pager,
                            transaction_id.0,
                            &encoded_key,
                            row_id,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn table_schema_and_root(&self, table: &str) -> Result<(Schema, page::PageId)> {
        let catalog = self.catalog.borrow();
        let schema = catalog
            .schemas
            .get(table)
            .cloned()
            .ok_or_else(|| DbError::storage(format!("unknown table: {table}")))?;
        let root = catalog
            .table_roots
            .get(table)
            .copied()
            .ok_or_else(|| DbError::storage(format!("missing table root for {table}")))?;
        Ok((schema, root))
    }

    fn index_meta_and_root(
        &self,
        table: &str,
        index_name: &str,
    ) -> Result<(IndexMeta, page::PageId)> {
        let catalog = self.catalog.borrow();
        let index = catalog
            .indexes
            .get(table)
            .and_then(|entry| entry.get(index_name))
            .cloned()
            .ok_or_else(|| {
                DbError::storage(format!("unknown index {index_name} on table {table}"))
            })?;
        let root = catalog
            .index_roots
            .get(table)
            .and_then(|entry| entry.get(index_name))
            .copied()
            .ok_or_else(|| {
                DbError::storage(format!(
                    "missing index root for {index_name} on table {table}"
                ))
            })?;
        Ok((index, root))
    }

    fn indexes_for_table(&self, table: &str) -> Result<Vec<(String, IndexMeta, page::PageId)>> {
        let catalog = self.catalog.borrow();
        let Some(indexes) = catalog.indexes.get(table) else {
            return Ok(Vec::new());
        };
        indexes
            .iter()
            .map(|(name, meta)| {
                let root = catalog
                    .index_roots
                    .get(table)
                    .and_then(|entry| entry.get(name))
                    .copied()
                    .ok_or_else(|| {
                        DbError::storage(format!("missing index root for {name} on table {table}"))
                    })?;
                Ok((name.clone(), meta.clone(), root))
            })
            .collect()
    }

    fn validate_new_index(
        schema: &Schema,
        indexes: Option<&BTreeMap<String, IndexMeta>>,
        index: &IndexMeta,
    ) -> Result<()> {
        if index.columns.is_empty() {
            return Err(DbError::storage("index must define at least one column"));
        }

        let mut seen = BTreeSet::new();
        for column in &index.columns {
            if !schema.columns.iter().any(|entry| entry.name == *column) {
                return Err(DbError::storage(format!(
                    "unknown column {column} on table {}",
                    schema.name
                )));
            }
            if !seen.insert(column.clone()) {
                return Err(DbError::storage(format!("duplicate index column {column}")));
            }
        }

        if indexes.is_some_and(|by_name| by_name.contains_key(&index.name)) {
            return Err(DbError::storage(format!(
                "index {} already exists",
                index.name
            )));
        }

        Ok(())
    }

    fn upsert_index_entry(
        tree: &mut IndexTree,
        pager: &mut Pager,
        txn_id: u64,
        encoded_key: &[u8],
        row_id: RowId,
    ) -> Result<()> {
        let mut row_ids = tree
            .get(&*pager, txn_id, encoded_key)?
            .map(|bytes| decode_row_ids(&bytes))
            .transpose()?
            .unwrap_or_default();
        if !row_ids.contains(&row_id) {
            row_ids.push(row_id);
            row_ids.sort_by_key(|value| value.0);
        }
        tree.insert(pager, txn_id, encoded_key, &encode_row_ids(&row_ids)?)
    }

    fn remove_row_id_from_index_entry(
        tree: &mut IndexTree,
        pager: &mut Pager,
        txn_id: u64,
        encoded_key: &[u8],
        row_id: RowId,
    ) -> Result<()> {
        let Some(bytes) = tree.get(&*pager, txn_id, encoded_key)? else {
            return Ok(());
        };
        let mut row_ids = decode_row_ids(&bytes)?;
        row_ids.retain(|candidate| *candidate != row_id);
        if row_ids.is_empty() {
            tree.delete(pager, txn_id, encoded_key)
        } else {
            tree.insert(pager, txn_id, encoded_key, &encode_row_ids(&row_ids)?)
        }
    }

}

impl PlanningStorageEngine for FileStorage {
    fn planning_context_snapshot(
        &self,
        transaction_id: Option<TransactionId>,
    ) -> Result<PlanningContext> {
        self.validate_snapshot_transaction(transaction_id)?;

        let catalog = self.planning_catalog_snapshot(transaction_id)?;
        let schemas = catalog
            .schemas
            .clone()
            .into_iter()
            .collect::<HashMap<_, _>>();
        let indexes = catalog
            .indexes
            .iter()
            .map(|(table, by_name)| (table.clone(), by_name.values().cloned().collect::<Vec<_>>()))
            .collect::<HashMap<_, _>>();

        Ok(PlanningContext::new(schemas, indexes))
    }
}

impl TransactionManager for FileStorage {
    fn begin(&self) -> Result<TransactionId> {
        self.begin_with_isolation(IsolationLevel::ReadCommitted)
    }

    fn begin_with_isolation(&self, isolation_level: IsolationLevel) -> Result<TransactionId> {
        let txn = {
            let mut manager = self.txn_manager.lock().unwrap();
            manager.begin(isolation_level)
        };
        if let Err(error) = self.pager.borrow_mut().begin_with_txn_id(txn.0) {
            let _ = self.txn_manager.lock().unwrap().abort(txn);
            return Err(error);
        }
        self.session_catalog_snapshots
            .borrow_mut()
            .insert(txn, self.catalog.borrow().clone());
        Ok(txn)
    }

    fn commit(&self, transaction_id: TransactionId) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        self.sync_transaction_page_writes(transaction_id)?;
        let commit_ts = self.txn_manager.lock().unwrap().reserve_commit_ts(transaction_id)?;
        self.finalize_transaction_row_versions(transaction_id, commit_ts)?;

        {
            let mut pager = self.pager.borrow_mut();
            let catalog = self.catalog.borrow().clone();
            store_catalog(&mut pager, transaction_id.0, &catalog)?;
            pager.commit(transaction_id.0)?;
        }

        self.txn_manager
            .lock()
            .unwrap()
            .finalize_commit(transaction_id, commit_ts)?;

        self.session_catalog_snapshots
            .borrow_mut()
            .remove(&transaction_id);
        self.maybe_purge_garbage()?;
        Ok(())
    }

    fn rollback(&self, transaction_id: TransactionId) -> Result<()> {
        let status = self.txn_manager.lock().unwrap().status(transaction_id)?;
        match status {
            TxnStatus::Active => {
                self.sync_transaction_page_writes(transaction_id)?;
                self.replay_undo_records(transaction_id)?;
                self.pager.borrow_mut().rollback(transaction_id.0)?;
                self.txn_manager.lock().unwrap().abort(transaction_id)?;
            }
            TxnStatus::Aborted => {
                let _ = self.txn_manager.lock().unwrap().take_undo_records(transaction_id)?;
                self.txn_manager
                    .lock()
                    .unwrap()
                    .clear_terminal_error(transaction_id)?;
            }
            TxnStatus::Committed => {
                return Err(DbError::txn(format!(
                    "transaction {} is not active",
                    transaction_id.0
                )));
            }
        }

        if let Some(snapshot) = self
            .session_catalog_snapshots
            .borrow_mut()
            .remove(&transaction_id)
        {
            *self.catalog.borrow_mut() = snapshot;
        }
        self.maybe_purge_garbage()?;

        Ok(())
    }
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

impl FileStorage {
    fn visible_row_at_root(
        &self,
        transaction_id: TransactionId,
        root_page_id: page::PageId,
        row_id: RowId,
    ) -> Result<Option<Row>> {
        let snapshot = self.snapshot_for_transaction(transaction_id)?;
        let pager = self.pager.borrow();
        let tree = BTree::from_root(root_page_id);
        Ok(tree
            .get(&pager, transaction_id.0, row_id.0)?
            .map(|bytes| visible_row(&bytes, transaction_id.0, &snapshot))
            .transpose()?
            .flatten())
    }

    fn filter_index_row_ids_for_key(
        &self,
        transaction_id: TransactionId,
        schema: &Schema,
        table_root_page_id: page::PageId,
        index: &IndexMeta,
        key_values: &[Value],
        row_ids: Vec<RowId>,
    ) -> Result<Vec<RowId>> {
        let mut visible = Vec::new();
        for row_id in row_ids {
            let Some(row) = self.visible_row_at_root(transaction_id, table_root_page_id, row_id)? else {
                continue;
            };
            if project_index_key(schema, index, &row)? == key_values {
                visible.push(row_id);
            }
        }
        Ok(visible)
    }
}

impl CatalogStore for FileStorage {
    fn create_schema(&self, transaction_id: TransactionId, schema: Schema) -> Result<()> {
        self.validate_transaction(transaction_id)?;

        let mut pager = self.pager.borrow_mut();
        let mut catalog = self.catalog.borrow_mut();
        if catalog.schemas.contains_key(&schema.name) {
            return Err(DbError::storage(format!(
                "table {} already exists",
                schema.name
            )));
        }

        let table_name = schema.name.clone();
        let tree = BTree::create(&mut pager, transaction_id.0)?;
        catalog.schemas.insert(table_name.clone(), schema);
        catalog
            .table_roots
            .insert(table_name.clone(), tree.root_page_id());
        catalog.next_row_ids.entry(table_name).or_insert(1);
        Ok(())
    }

    fn drop_schema(&self, transaction_id: TransactionId, name: &str) -> Result<()> {
        self.validate_transaction(transaction_id)?;

        let mut catalog = self.catalog.borrow_mut();
        if catalog.schemas.remove(name).is_none() {
            return Err(DbError::storage(format!("unknown table: {name}")));
        }
        catalog.table_roots.remove(name);
        catalog.indexes.remove(name);
        catalog.index_roots.remove(name);
        catalog.next_row_ids.remove(name);
        Ok(())
    }

    fn replace_schema(&self, transaction_id: TransactionId, schema: Schema) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let mut catalog = self.catalog.borrow_mut();
        if !catalog.schemas.contains_key(&schema.name) {
            return Err(DbError::storage(format!("unknown table: {}", schema.name)));
        }
        catalog.schemas.insert(schema.name.clone(), schema);
        Ok(())
    }

    fn rename_schema(
        &self,
        transaction_id: TransactionId,
        old_name: &str,
        new_name: &str,
    ) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let mut catalog = self.catalog.borrow_mut();
        if catalog.schemas.contains_key(new_name) {
            return Err(DbError::storage(format!(
                "table already exists: {new_name}"
            )));
        }
        let mut schema = catalog
            .schemas
            .remove(old_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {old_name}")))?;
        schema.name = new_name.to_string();
        catalog.schemas.insert(new_name.to_string(), schema);
        if let Some(root) = catalog.table_roots.remove(old_name) {
            catalog.table_roots.insert(new_name.to_string(), root);
        }
        if let Some(next_row_id) = catalog.next_row_ids.remove(old_name) {
            catalog
                .next_row_ids
                .insert(new_name.to_string(), next_row_id);
        }
        if let Some(indexes) = catalog.indexes.remove(old_name) {
            catalog.indexes.insert(new_name.to_string(), indexes);
        }
        if let Some(index_roots) = catalog.index_roots.remove(old_name) {
            catalog
                .index_roots
                .insert(new_name.to_string(), index_roots);
        }
        for schema in catalog.schemas.values_mut() {
            schema.rename_foreign_key_ref_table(old_name, new_name);
        }
        Ok(())
    }

    fn add_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        column: ColumnDef,
    ) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let (schema, root_page_id) = self.table_schema_and_root(schema_name)?;
        if schema.columns.iter().any(|entry| entry.name == column.name) {
            return Err(DbError::storage(format!(
                "column already exists on table {schema_name}: {}",
                column.name
            )));
        }
        let default_value = column.default_value.clone().unwrap_or(Value::Null);
        let rows = {
            let pager = self.pager.borrow();
            let tree = BTree::from_root(root_page_id);
            tree.scan_all(&pager, transaction_id.0)?
        };

        let mut updated_schema = schema;
        updated_schema.columns.push(column);
        updated_schema.validate_constraints_metadata()?;
        for (_, bytes) in &rows {
            let mut candidate = decode_row(bytes)?;
            candidate.push(default_value.clone());
            updated_schema.validate_row_values(&candidate)?;
            updated_schema.validate_check_constraints(&candidate)?;
        }
        {
            self.catalog
                .borrow_mut()
                .schemas
                .insert(schema_name.to_string(), updated_schema);
        }

        let mut pager = self.pager.borrow_mut();
        let mut tree = BTree::from_root(root_page_id);
        for (row_id, bytes) in rows {
            let mut row = decode_row(&bytes)?;
            row.push(default_value.clone());
            tree.insert(&mut pager, transaction_id.0, row_id, &encode_row(&row)?)?;
        }
        if tree.root_page_id() != root_page_id {
            self.catalog
                .borrow_mut()
                .table_roots
                .insert(schema_name.to_string(), tree.root_page_id());
        }
        Ok(())
    }

    fn rename_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let (schema, _) = self.table_schema_and_root(schema_name)?;
        if !schema.columns.iter().any(|entry| entry.name == old_name) {
            return Err(DbError::storage(format!(
                "unknown column {old_name} on table {schema_name}"
            )));
        }
        if schema.columns.iter().any(|entry| entry.name == new_name) {
            return Err(DbError::storage(format!(
                "column already exists on table {schema_name}: {new_name}"
            )));
        }

        let mut updated_schema = schema;
        updated_schema.rename_column_references(old_name, new_name);
        updated_schema.rename_foreign_key_ref_column(schema_name, old_name, new_name);
        updated_schema.validate_constraints_metadata()?;
        let mut catalog = self.catalog.borrow_mut();
        catalog
            .schemas
            .insert(schema_name.to_string(), updated_schema);
        if let Some(indexes) = catalog.indexes.get_mut(schema_name) {
            for index in indexes.values_mut() {
                for column in &mut index.columns {
                    if column == old_name {
                        *column = new_name.to_string();
                    }
                }
            }
        }
        for (name, schema) in &mut catalog.schemas {
            if name != schema_name {
                schema.rename_foreign_key_ref_column(schema_name, old_name, new_name);
            }
        }
        Ok(())
    }

    fn get_schema(&self, transaction_id: TransactionId, name: &str) -> Result<Option<Schema>> {
        self.validate_transaction(transaction_id)?;
        Ok(self.catalog.borrow().schemas.get(name).cloned())
    }

    fn list_schemas(&self, transaction_id: TransactionId) -> Result<Vec<Schema>> {
        self.validate_transaction(transaction_id)?;
        Ok(self.catalog.borrow().schemas.values().cloned().collect())
    }
}

impl TableStore for FileStorage {
    fn insert_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row: Row,
    ) -> Result<RowId> {
        self.validate_transaction(transaction_id)?;

        let (schema, root_page_id) = self.table_schema_and_root(schema_name)?;
        if row.len() != schema.columns.len() {
            return Err(DbError::storage(format!(
                "insert into {schema_name} expected {} values but got {}",
                schema.columns.len(),
                row.len()
            )));
        }

        schema.validate_row_values(&row)?;
        schema.validate_check_constraints(&row)?;

        let write_index_keys = self
            .indexes_for_table(schema_name)?
            .into_iter()
            .map(|(_, index_meta, _)| {
                project_index_key(&schema, &index_meta, &row)
                    .map(|key_values| (index_meta.name.clone(), key_values))
            })
            .collect::<Result<Vec<_>>>()?;
        self.wait_for_write_conflicts(transaction_id, schema_name, &write_index_keys)?;

        let snapshot = self.snapshot_for_transaction(transaction_id)?;
        let existing_rows = {
            let pager = self.pager.borrow();
            let tree = BTree::from_root(root_page_id);
            tree.scan_all(&pager, transaction_id.0)?
                .into_iter()
                .filter_map(|(_, bytes)| visible_row(&bytes, transaction_id.0, &snapshot).transpose())
                .collect::<Result<Vec<_>>>()?
        };
        let existing_refs = existing_rows.iter().collect::<Vec<_>>();
        schema.validate_primary_key_uniqueness(&row, &existing_refs)?;

        for (_, index_meta, index_root_page_id) in self.indexes_for_table(schema_name)? {
            let key_values = project_index_key(&schema, &index_meta, &row)?;
            if !index_meta.enforces_unique_key(&key_values) {
                continue;
            }

            let encoded_key = encode_index_key(&key_values)?;
            let pager = self.pager.borrow();
            let tree = IndexTree::from_root(index_root_page_id);
            let conflicting_rows = tree
                .get(&pager, transaction_id.0, &encoded_key)?
                .map(|bytes| decode_row_ids(&bytes))
                .transpose()?
                .map(|row_ids| {
                    self.filter_index_row_ids_for_key(
                        transaction_id,
                        &schema,
                        root_page_id,
                        &index_meta,
                        &key_values,
                        row_ids,
                    )
                })
                .transpose()?
                .unwrap_or_default();
            if !conflicting_rows.is_empty() {
                return Err(DbError::storage(format!(
                    "unique index {} constraint failed",
                    index_meta.name
                )));
            }
        }

        let fallback_next_row_id = self
            .catalog
            .borrow()
            .next_row_ids
            .get(schema_name)
            .copied()
            .unwrap_or(1);
        let row_id = self
            .txn_manager
            .lock()
            .unwrap()
            .allocate_row_id(transaction_id, schema_name, fallback_next_row_id)?;
        self.catalog
            .borrow_mut()
            .next_row_ids
            .insert(schema_name.to_string(), row_id.0.saturating_add(1));
        self.record_undo(
            transaction_id,
            UndoRecord::InsertRow {
                table: schema_name.to_string(),
                row_id,
            },
        )?;

        let bytes = encode_uncommitted_row_version(&row, transaction_id.0)?;
        let mut pager = self.pager.borrow_mut();
        let mut tree = BTree::from_root(root_page_id);
        tree.insert(&mut pager, transaction_id.0, row_id.0, &bytes)?;
        if tree.root_page_id() != root_page_id {
            self.catalog
                .borrow_mut()
                .table_roots
                .insert(schema_name.to_string(), tree.root_page_id());
        }

        for (index_name, index_meta, root_page_id) in self.indexes_for_table(schema_name)? {
            let key_values = project_index_key(&schema, &index_meta, &row)?;
            self.record_undo(
                transaction_id,
                UndoRecord::IndexInsert {
                    table: schema_name.to_string(),
                    index: index_name.clone(),
                    row_id,
                    key: key_values.clone(),
                },
            )?;
            let encoded_key = encode_index_key(&key_values)?;
            let mut tree = IndexTree::from_root(root_page_id);
            Self::upsert_index_entry(
                &mut tree,
                &mut pager,
                transaction_id.0,
                &encoded_key,
                row_id,
            )?;
            if tree.root_page_id() != root_page_id {
                self.catalog
                    .borrow_mut()
                    .index_roots
                    .entry(schema_name.to_string())
                    .or_default()
                    .insert(index_name, tree.root_page_id());
            }
        }

        Ok(row_id)
    }

    fn get_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
    ) -> Result<Option<Row>> {
        self.validate_transaction(transaction_id)?;

        let Ok((_, root_page_id)) = self.table_schema_and_root(schema_name) else {
            return Ok(None);
        };

        let snapshot = self.snapshot_for_transaction(transaction_id)?;
        let pager = self.pager.borrow();
        let tree = BTree::from_root(root_page_id);
        Ok(tree
            .get(&pager, transaction_id.0, row_id.0)?
            .map(|bytes| visible_row(&bytes, transaction_id.0, &snapshot))
            .transpose()?
            .flatten())
    }

    fn scan_rows(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<(RowId, Row)>> {
        self.validate_transaction(transaction_id)?;
        self.txn_manager
            .lock()
            .unwrap()
            .acquire_table_read_lock(transaction_id, schema_name)?;

        let Ok((_, root_page_id)) = self.table_schema_and_root(schema_name) else {
            return Ok(Vec::new());
        };

        let snapshot = self.snapshot_for_transaction(transaction_id)?;
        let pager = self.pager.borrow();
        let tree = BTree::from_root(root_page_id);
        tree.scan_all(&pager, transaction_id.0)?
            .into_iter()
            .filter_map(|(row_id, bytes)| {
                visible_row(&bytes, transaction_id.0, &snapshot)
                    .transpose()
                    .map(|result| result.map(|row| (RowId(row_id), row)))
            })
            .collect()
    }

    fn delete_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
    ) -> Result<()> {
        self.validate_transaction(transaction_id)?;

        let Ok((_, root_page_id)) = self.table_schema_and_root(schema_name) else {
            return Ok(());
        };

        let snapshot = self.snapshot_for_transaction(transaction_id)?;
        let existing_row = {
            let pager = self.pager.borrow();
            let tree = BTree::from_root(root_page_id);
            tree.get(&pager, transaction_id.0, row_id.0)?
                .map(|bytes| visible_row(&bytes, transaction_id.0, &snapshot))
                .transpose()?
                .flatten()
        };

        let Some(existing_row) = existing_row else {
            return Ok(());
        };

        let (schema, _) = self.table_schema_and_root(schema_name)?;
        let write_index_keys = self
            .indexes_for_table(schema_name)?
            .into_iter()
            .map(|(_, index_meta, _)| {
                project_index_key(&schema, &index_meta, &existing_row)
                    .map(|key_values| (index_meta.name.clone(), key_values))
            })
            .collect::<Result<Vec<_>>>()?;
        self.wait_for_write_conflicts(transaction_id, schema_name, &write_index_keys)?;
        let mut pager = self.pager.borrow_mut();
        let mut tree = BTree::from_root(root_page_id);
        let existing_bytes = tree
            .get(&pager, transaction_id.0, row_id.0)?
            .ok_or_else(|| DbError::storage(format!("missing row {} on table {schema_name}", row_id.0)))?;
        self.record_undo(
            transaction_id,
            UndoRecord::DeleteRow {
                table: schema_name.to_string(),
                row_id,
                previous_bytes: existing_bytes.clone(),
            },
        )?;
        for (index_name, key_values) in &write_index_keys {
            self.record_undo(
                transaction_id,
                UndoRecord::IndexDelete {
                    table: schema_name.to_string(),
                    index: index_name.clone(),
                    row_id,
                    key: key_values.clone(),
                },
            )?;
        }
        if let Some(updated_bytes) = mark_row_deleted(&existing_bytes, transaction_id.0, &snapshot)? {
            tree.insert(&mut pager, transaction_id.0, row_id.0, &updated_bytes)?;
        }
        if tree.root_page_id() != root_page_id {
            self.catalog
                .borrow_mut()
                .table_roots
                .insert(schema_name.to_string(), tree.root_page_id());
        }
        Ok(())
    }

    fn update_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
        row: Row,
    ) -> Result<()> {
        self.validate_transaction(transaction_id)?;

        let (schema, root_page_id) = self.table_schema_and_root(schema_name)?;
        if row.len() != schema.columns.len() {
            return Err(DbError::storage(format!(
                "update {schema_name} expected {} values but got {}",
                schema.columns.len(),
                row.len()
            )));
        }

        schema.validate_row_values(&row)?;
        schema.validate_check_constraints(&row)?;

        let snapshot = self.snapshot_for_transaction(transaction_id)?;
        let existing_row = {
            let pager = self.pager.borrow();
            let tree = BTree::from_root(root_page_id);
            tree.get(&pager, transaction_id.0, row_id.0)?
                .map(|bytes| visible_row(&bytes, transaction_id.0, &snapshot))
                .transpose()?
                .flatten()
        };
        let Some(existing_row) = existing_row else {
            return Ok(());
        };

        if existing_row == row {
            return Ok(());
        }

        let mut indexed_keys = Vec::new();
        let indexes = self.indexes_for_table(schema_name)?;
        let mut index_changes = Vec::with_capacity(indexes.len());
        for (index_name, index_meta, index_root_page_id) in indexes {
            let old_key = project_index_key(&schema, &index_meta, &existing_row)?;
            let new_key = project_index_key(&schema, &index_meta, &row)?;
            if !indexed_keys
                .iter()
                .any(|(existing_name, existing_key)| existing_name == &index_name && existing_key == &old_key)
            {
                indexed_keys.push((index_name.clone(), old_key.clone()));
            }
            if !indexed_keys
                .iter()
                .any(|(existing_name, existing_key)| existing_name == &index_name && existing_key == &new_key)
            {
                indexed_keys.push((index_name.clone(), new_key.clone()));
            }
            index_changes.push((index_name, index_meta, index_root_page_id, old_key, new_key));
        }
        self.wait_for_write_conflicts(transaction_id, schema_name, &indexed_keys)?;

        let existing_rows = {
            let pager = self.pager.borrow();
            let tree = BTree::from_root(root_page_id);
            tree.scan_all(&pager, transaction_id.0)?
                .into_iter()
                .filter(|(candidate_row_id, _)| *candidate_row_id != row_id.0)
                .filter_map(|(_, bytes)| visible_row(&bytes, transaction_id.0, &snapshot).transpose())
                .collect::<Result<Vec<_>>>()?
        };
        let existing_refs = existing_rows.iter().collect::<Vec<_>>();
        schema.validate_primary_key_uniqueness(&row, &existing_refs)?;

        for (_, index_meta, index_root_page_id, _, new_key) in &index_changes {
            if !index_meta.enforces_unique_key(new_key) {
                continue;
            }

            let encoded_key = encode_index_key(new_key)?;
            let pager = self.pager.borrow();
            let tree = IndexTree::from_root(*index_root_page_id);
            let conflicting_rows = tree
                .get(&pager, transaction_id.0, &encoded_key)?
                .map(|bytes| decode_row_ids(&bytes))
                .transpose()?
                .map(|row_ids| {
                    self.filter_index_row_ids_for_key(
                        transaction_id,
                        &schema,
                        root_page_id,
                        index_meta,
                        new_key,
                        row_ids,
                    )
                })
                .transpose()?
                .unwrap_or_default()
                .into_iter()
                .filter(|candidate| *candidate != row_id)
                .collect::<Vec<_>>();
            if !conflicting_rows.is_empty() {
                return Err(DbError::storage(format!(
                    "unique index {} constraint failed",
                    index_meta.name
                )));
            }
        }

        let mut pager = self.pager.borrow_mut();
        let mut tree = BTree::from_root(root_page_id);
        let existing_bytes = tree
            .get(&pager, transaction_id.0, row_id.0)?
            .ok_or_else(|| DbError::storage(format!("missing row {} on table {schema_name}", row_id.0)))?;
        self.record_undo(
            transaction_id,
            UndoRecord::DeleteRow {
                table: schema_name.to_string(),
                row_id,
                previous_bytes: existing_bytes.clone(),
            },
        )?;

        for (index_name, _, _, old_key, new_key) in &index_changes {
            if old_key != new_key {
                self.record_undo(
                    transaction_id,
                    UndoRecord::IndexDelete {
                        table: schema_name.to_string(),
                        index: index_name.clone(),
                        row_id,
                        key: old_key.clone(),
                    },
                )?;
                self.record_undo(
                    transaction_id,
                    UndoRecord::IndexInsert {
                        table: schema_name.to_string(),
                        index: index_name.clone(),
                        row_id,
                        key: new_key.clone(),
                    },
                )?;
            }
        }

        if let Some(updated_bytes) = append_row_version(&existing_bytes, &row, transaction_id.0, &snapshot)? {
            tree.insert(&mut pager, transaction_id.0, row_id.0, &updated_bytes)?;
        }
        if tree.root_page_id() != root_page_id {
            self.catalog
                .borrow_mut()
                .table_roots
                .insert(schema_name.to_string(), tree.root_page_id());
        }

        for (index_name, _, index_root_page_id, old_key, new_key) in index_changes {
            if old_key == new_key {
                continue;
            }
            let old_encoded_key = encode_index_key(&old_key)?;
            let new_encoded_key = encode_index_key(&new_key)?;
            let mut tree = IndexTree::from_root(index_root_page_id);
            Self::remove_row_id_from_index_entry(
                &mut tree,
                &mut pager,
                transaction_id.0,
                &old_encoded_key,
                row_id,
            )?;
            Self::upsert_index_entry(
                &mut tree,
                &mut pager,
                transaction_id.0,
                &new_encoded_key,
                row_id,
            )?;
            if tree.root_page_id() != index_root_page_id {
                self.catalog
                    .borrow_mut()
                    .index_roots
                    .entry(schema_name.to_string())
                    .or_default()
                    .insert(index_name, tree.root_page_id());
            }
        }

        Ok(())
    }
}

impl IndexStore for FileStorage {
    fn create_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index: IndexMeta,
    ) -> Result<()> {
        self.validate_transaction(transaction_id)?;

        let (schema, table_root_page_id) = self.table_schema_and_root(schema_name)?;
        {
            let catalog = self.catalog.borrow();
            Self::validate_new_index(&schema, catalog.indexes.get(schema_name), &index)?;
        }

        let existing_rows = {
            let pager = self.pager.borrow();
            let tree = BTree::from_root(table_root_page_id);
            tree.scan_all(&pager, transaction_id.0)?
        };

        let mut pager = self.pager.borrow_mut();
        let mut tree = IndexTree::create(&mut pager, transaction_id.0)?;

        for (row_id, bytes) in existing_rows {
            let row = decode_row(&bytes)?;
            let key_values = project_index_key(&schema, &index, &row)?;
            let encoded_key = encode_index_key(&key_values)?;
            if index.enforces_unique_key(&key_values)
                && tree
                    .get(&pager, transaction_id.0, &encoded_key)?
                    .map(|bytes| decode_row_ids(&bytes))
                    .transpose()?
                    .is_some_and(|row_ids| !row_ids.is_empty())
            {
                return Err(DbError::storage(format!(
                    "unique index {} constraint failed",
                    index.name
                )));
            }
            Self::upsert_index_entry(
                &mut tree,
                &mut pager,
                transaction_id.0,
                &encoded_key,
                RowId(row_id),
            )?;
        }

        {
            let mut catalog = self.catalog.borrow_mut();
            catalog
                .indexes
                .entry(schema_name.to_string())
                .or_default()
                .insert(index.name.clone(), index.clone());
            catalog
                .index_roots
                .entry(schema_name.to_string())
                .or_default()
                .insert(index.name.clone(), tree.root_page_id());
        }

        Ok(())
    }

    fn drop_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<()> {
        self.validate_transaction(transaction_id)?;

        let mut catalog = self.catalog.borrow_mut();
        let removed = catalog
            .indexes
            .get_mut(schema_name)
            .and_then(|indexes| indexes.remove(index_name));
        if removed.is_none() {
            return Err(DbError::storage(format!(
                "unknown index {index_name} on table {schema_name}"
            )));
        }
        if let Some(roots) = catalog.index_roots.get_mut(schema_name) {
            roots.remove(index_name);
        }
        Ok(())
    }

    fn get_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<Option<IndexMeta>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .catalog
            .borrow()
            .indexes
            .get(schema_name)
            .and_then(|entry| entry.get(index_name))
            .cloned())
    }

    fn list_indexes(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<IndexMeta>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .catalog
            .borrow()
            .indexes
            .get(schema_name)
            .map(|entry| entry.values().cloned().collect())
            .unwrap_or_default())
    }

    fn lookup_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key: &[Value],
    ) -> Result<Vec<RowId>> {
        self.validate_transaction(transaction_id)?;
        let (schema, table_root_page_id) = self.table_schema_and_root(schema_name)?;
        let (index, root_page_id) = self.index_meta_and_root(schema_name, index_name)?;
        if key.len() != index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} expected {} key values but got {}",
                index.name,
                index.columns.len(),
                key.len()
            )));
        }
        self.txn_manager
            .lock()
            .unwrap()
            .acquire_exact_key_lock(transaction_id, schema_name, index_name, key)?;
        let encoded_key = encode_index_key(key)?;
        let pager = self.pager.borrow();
        let tree = IndexTree::from_root(root_page_id);
        tree
            .get(&pager, transaction_id.0, &encoded_key)?
            .map(|bytes| decode_row_ids(&bytes))
            .transpose()?
            .map(|row_ids| {
                self.filter_index_row_ids_for_key(
                    transaction_id,
                    &schema,
                    table_root_page_id,
                    &index,
                    key,
                    row_ids,
                )
            })
            .transpose()?
            .map_or(Ok(Vec::new()), Ok)
    }

    fn scan_index_prefix(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key_prefix: &[Value],
    ) -> Result<Vec<RowId>> {
        self.validate_transaction(transaction_id)?;
        let (schema, table_root_page_id) = self.table_schema_and_root(schema_name)?;
        let (index, root_page_id) = self.index_meta_and_root(schema_name, index_name)?;
        if key_prefix.len() > index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} expected at most {} key values but got {}",
                index.name,
                index.columns.len(),
                key_prefix.len()
            )));
        }
        self.txn_manager
            .lock()
            .unwrap()
            .acquire_prefix_lock(transaction_id, schema_name, index_name, key_prefix)?;

        let pager = self.pager.borrow();
        let tree = IndexTree::from_root(root_page_id);
        let mut row_ids = BTreeSet::new();
        for (encoded_key, encoded_row_ids) in tree.scan_all(&pager, transaction_id.0)? {
            let decoded_key = decode_index_key(&encoded_key)?;
            if decoded_key.starts_with(key_prefix) {
                row_ids.extend(self.filter_index_row_ids_for_key(
                    transaction_id,
                    &schema,
                    table_root_page_id,
                    &index,
                    &decoded_key,
                    decode_row_ids(&encoded_row_ids)?,
                )?);
            }
        }

        Ok(row_ids.into_iter().collect())
    }

    fn scan_index_range(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key_prefix: &[Value],
        lower: Option<(CompareOp, &Value)>,
        upper: Option<(CompareOp, &Value)>,
    ) -> Result<Vec<RowId>> {
        self.validate_transaction(transaction_id)?;
        let (schema, table_root_page_id) = self.table_schema_and_root(schema_name)?;
        let (index, root_page_id) = self.index_meta_and_root(schema_name, index_name)?;
        if key_prefix.len() >= index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} has no range column after prefix of length {}",
                index.name,
                key_prefix.len()
            )));
        }
        self.txn_manager.lock().unwrap().acquire_range_lock(
            transaction_id,
            schema_name,
            index_name,
            key_prefix,
            lower,
            upper,
        )?;

        let pager = self.pager.borrow();
        let tree = IndexTree::from_root(root_page_id);
        let mut row_ids = BTreeSet::new();
        for (encoded_key, encoded_row_ids) in tree.scan_all(&pager, transaction_id.0)? {
            let decoded_key = decode_index_key(&encoded_key)?;
            if !decoded_key.starts_with(key_prefix) {
                continue;
            }

            let Some(candidate) = decoded_key.get(key_prefix.len()) else {
                continue;
            };
            if matches_bounds(candidate, lower, upper) {
                row_ids.extend(self.filter_index_row_ids_for_key(
                    transaction_id,
                    &schema,
                    table_root_page_id,
                    &index,
                    &decoded_key,
                    decode_row_ids(&encoded_row_ids)?,
                )?);
            }
        }

        Ok(row_ids.into_iter().collect())
    }
}

fn matches_bounds(
    candidate: &Value,
    lower: Option<(CompareOp, &Value)>,
    upper: Option<(CompareOp, &Value)>,
) -> bool {
    lower.is_none_or(|(op, value)| matches_compare(candidate, op, value))
        && upper.is_none_or(|(op, value)| matches_compare(candidate, op, value))
}

fn matches_compare(left: &Value, op: CompareOp, right: &Value) -> bool {
    match op {
        CompareOp::Eq => left == right,
        CompareOp::Ne => left != right,
        CompareOp::Gt => compare_values(left, right) == Some(Ordering::Greater),
        CompareOp::Gte => matches!(
            compare_values(left, right),
            Some(Ordering::Greater | Ordering::Equal)
        ),
        CompareOp::Lt => compare_values(left, right) == Some(Ordering::Less),
        CompareOp::Lte => matches!(
            compare_values(left, right),
            Some(Ordering::Less | Ordering::Equal)
        ),
    }
}

fn compare_values(left: &Value, right: &Value) -> Option<Ordering> {
    match (left, right) {
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (Value::Boolean(left), Value::Boolean(right)) => Some(left.cmp(right)),
        (Value::Integer(left), Value::Integer(right)) => Some(left.cmp(right)),
        (Value::Text(left), Value::Text(right)) => Some(left.cmp(right)),
        _ => None,
    }
}
