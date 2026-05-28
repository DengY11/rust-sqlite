use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Row, RowId, Schema, Value};
use crate::engine::traits::{
    CatalogStore, IndexStore, PlanningStorageEngine, TableStore, TransactionManager,
};
use crate::engine::txn::TransactionId;
use crate::sql::planner::PlanningContext;

use self::btree::BTree;
use self::catalog::{CatalogState, load_catalog, store_catalog};
use self::codec::{decode_row, encode_row};
use self::pager::Pager;

pub mod btree;
pub mod catalog;
pub mod codec;
pub mod page;
pub mod pager;
pub mod wal;

#[derive(Debug)]
pub struct FileStorage {
    pager: RefCell<Pager>,
    catalog: RefCell<CatalogState>,
    active_txn: Cell<Option<TransactionId>>,
    txn_snapshot: RefCell<Option<CatalogState>>,
}

impl FileStorage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let pager = Pager::open(path)?;
        let catalog = load_catalog(&pager)?;

        Ok(Self {
            pager: RefCell::new(pager),
            catalog: RefCell::new(catalog),
            active_txn: Cell::new(None),
            txn_snapshot: RefCell::new(None),
        })
    }

    fn validate_transaction(&self, transaction_id: TransactionId) -> Result<()> {
        match self.active_txn.get() {
            Some(active) if active == transaction_id => Ok(()),
            Some(active) => Err(DbError::txn(format!(
                "transaction {} is not active; current transaction is {}",
                transaction_id.0, active.0
            ))),
            None => Err(DbError::txn("no active transaction")),
        }
    }

    fn validate_snapshot_transaction(&self, transaction_id: Option<TransactionId>) -> Result<()> {
        match (transaction_id, self.active_txn.get()) {
            (Some(id), Some(active)) if id == active => Ok(()),
            (Some(id), Some(active)) => Err(DbError::txn(format!(
                "transaction {} is not active; current transaction is {}",
                id.0, active.0
            ))),
            (Some(id), None) => Err(DbError::txn(format!("transaction {} is not active", id.0))),
            (None, Some(_)) => Err(DbError::txn(
                "metadata snapshot requires the active transaction id while a transaction is open",
            )),
            (None, None) => Ok(()),
        }
    }

    fn table_schema_and_root(&self, table: &str) -> Result<(Schema, page::PageId)> {
        let catalog = self.catalog.borrow();
        let schema = catalog.schemas.get(table).cloned().ok_or_else(|| {
            DbError::storage(format!("unknown table: {table}"))
        })?;
        let root = catalog.table_roots.get(table).copied().ok_or_else(|| {
            DbError::storage(format!("missing table root for {table}"))
        })?;
        Ok((schema, root))
    }
}

impl PlanningStorageEngine for FileStorage {
    fn planning_context_snapshot(
        &self,
        transaction_id: Option<TransactionId>,
    ) -> Result<PlanningContext> {
        self.validate_snapshot_transaction(transaction_id)?;

        let catalog = self.catalog.borrow();
        let schemas = catalog.schemas.clone().into_iter().collect::<HashMap<_, _>>();
        let indexes = catalog
            .schemas
            .keys()
            .map(|table| (table.clone(), Vec::new()))
            .collect::<HashMap<_, _>>();

        Ok(PlanningContext::new(schemas, indexes))
    }
}

impl TransactionManager for FileStorage {
    fn begin(&self) -> Result<TransactionId> {
        if self.active_txn.get().is_some() {
            return Err(DbError::txn("transaction already active"));
        }

        let txn = TransactionId(self.pager.borrow_mut().begin()?);
        self.active_txn.set(Some(txn));
        *self.txn_snapshot.borrow_mut() = Some(self.catalog.borrow().clone());
        Ok(txn)
    }

    fn commit(&self, transaction_id: TransactionId) -> Result<()> {
        self.validate_transaction(transaction_id)?;

        {
            let mut pager = self.pager.borrow_mut();
            let catalog = self.catalog.borrow().clone();
            store_catalog(&mut pager, transaction_id.0, &catalog)?;
            pager.commit(transaction_id.0)?;
        }

        self.active_txn.set(None);
        *self.txn_snapshot.borrow_mut() = None;
        Ok(())
    }

    fn rollback(&self, transaction_id: TransactionId) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        self.pager.borrow_mut().rollback(transaction_id.0)?;

        if let Some(snapshot) = self.txn_snapshot.borrow_mut().take() {
            *self.catalog.borrow_mut() = snapshot;
        }

        self.active_txn.set(None);
        Ok(())
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
        catalog.table_roots.insert(table_name.clone(), tree.root_page_id());
        catalog.next_row_ids.entry(table_name).or_insert(1);
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

        let existing_rows = {
            let pager = self.pager.borrow();
            let tree = BTree::from_root(root_page_id);
            tree.scan_all(&pager)?
                .into_iter()
                .map(|(_, bytes)| decode_row(&bytes))
                .collect::<Result<Vec<_>>>()?
        };
        let existing_refs = existing_rows.iter().collect::<Vec<_>>();
        schema.validate_primary_key_uniqueness(&row, &existing_refs)?;

        let row_id = {
            let mut catalog = self.catalog.borrow_mut();
            catalog.allocate_row_id(schema_name)
        };

        let bytes = encode_row(&row)?;
        let mut pager = self.pager.borrow_mut();
        let mut tree = BTree::from_root(root_page_id);
        tree.insert(&mut pager, transaction_id.0, row_id.0, &bytes)?;
        if tree.root_page_id() != root_page_id {
            self.catalog
                .borrow_mut()
                .table_roots
                .insert(schema_name.to_string(), tree.root_page_id());
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

        let pager = self.pager.borrow();
        let tree = BTree::from_root(root_page_id);
        tree.get(&pager, row_id.0)?
            .map(|bytes| decode_row(&bytes))
            .transpose()
    }

    fn scan_rows(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<(RowId, Row)>> {
        self.validate_transaction(transaction_id)?;

        let Ok((_, root_page_id)) = self.table_schema_and_root(schema_name) else {
            return Ok(Vec::new());
        };

        let pager = self.pager.borrow();
        let tree = BTree::from_root(root_page_id);
        tree.scan_all(&pager)?
            .into_iter()
            .map(|(row_id, bytes)| Ok((RowId(row_id), decode_row(&bytes)?)))
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

        let mut pager = self.pager.borrow_mut();
        let mut tree = BTree::from_root(root_page_id);
        tree.delete(&mut pager, transaction_id.0, row_id.0)?;
        if tree.root_page_id() != root_page_id {
            self.catalog
                .borrow_mut()
                .table_roots
                .insert(schema_name.to_string(), tree.root_page_id());
        }
        Ok(())
    }
}

impl IndexStore for FileStorage {
    fn create_index(
        &self,
        transaction_id: TransactionId,
        _schema_name: &str,
        _index: IndexMeta,
    ) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        Err(DbError::storage(
            "storage_v2 does not implement secondary indexes yet",
        ))
    }

    fn get_index(
        &self,
        transaction_id: TransactionId,
        _schema_name: &str,
        _index_name: &str,
    ) -> Result<Option<IndexMeta>> {
        self.validate_transaction(transaction_id)?;
        Ok(None)
    }

    fn list_indexes(
        &self,
        transaction_id: TransactionId,
        _schema_name: &str,
    ) -> Result<Vec<IndexMeta>> {
        self.validate_transaction(transaction_id)?;
        Ok(Vec::new())
    }

    fn lookup_index(
        &self,
        transaction_id: TransactionId,
        _schema_name: &str,
        _index_name: &str,
        _key: &[Value],
    ) -> Result<Vec<RowId>> {
        self.validate_transaction(transaction_id)?;
        Err(DbError::storage(
            "storage_v2 does not implement secondary indexes yet",
        ))
    }
}
