use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use crate::common::error::{DbError, Result};
use crate::common::types::{ColumnDef, IndexMeta, Row, RowId, Schema, Value};
use crate::engine::traits::{
    CatalogStore, IndexStore, PlanningStorageEngine, TableStore, TransactionManager,
};
use crate::engine::txn::TransactionId;
use crate::sql::ast::CompareOp;
use crate::sql::planner::PlanningContext;

use self::btree::BTree;
use self::catalog::{CatalogState, load_catalog, store_catalog};
use self::codec::{
    decode_index_key, decode_row, decode_row_ids, encode_index_key, encode_row, encode_row_ids,
    project_index_key,
};
use self::index_tree::IndexTree;
use self::pager::Pager;

pub mod btree;
pub mod catalog;
pub mod codec;
pub mod index_tree;
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
            .get(&*pager, encoded_key)?
            .map(|bytes| decode_row_ids(&bytes))
            .transpose()?
            .unwrap_or_default();
        if !row_ids.contains(&row_id) {
            row_ids.push(row_id);
            row_ids.sort_by_key(|value| value.0);
        }
        tree.insert(pager, txn_id, encoded_key, &encode_row_ids(&row_ids)?)
    }

    fn remove_index_entry(
        tree: &mut IndexTree,
        pager: &mut Pager,
        txn_id: u64,
        encoded_key: &[u8],
        row_id: RowId,
    ) -> Result<()> {
        let Some(bytes) = tree.get(&*pager, encoded_key)? else {
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

        let catalog = self.catalog.borrow();
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
            tree.scan_all(&pager)?
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

        for (_, index_meta, root_page_id) in self.indexes_for_table(schema_name)? {
            let key_values = project_index_key(&schema, &index_meta, &row)?;
            if !index_meta.enforces_unique_key(&key_values) {
                continue;
            }

            let encoded_key = encode_index_key(&key_values)?;
            let pager = self.pager.borrow();
            let tree = IndexTree::from_root(root_page_id);
            if tree
                .get(&pager, &encoded_key)?
                .map(|bytes| decode_row_ids(&bytes))
                .transpose()?
                .is_some_and(|row_ids| !row_ids.is_empty())
            {
                return Err(DbError::storage(format!(
                    "unique index {} constraint failed",
                    index_meta.name
                )));
            }
        }

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

        for (index_name, index_meta, root_page_id) in self.indexes_for_table(schema_name)? {
            let key_values = project_index_key(&schema, &index_meta, &row)?;
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

        let existing_row = {
            let pager = self.pager.borrow();
            let tree = BTree::from_root(root_page_id);
            tree.get(&pager, row_id.0)?
                .map(|bytes| decode_row(&bytes))
                .transpose()?
        };

        let Some(existing_row) = existing_row else {
            return Ok(());
        };

        let (schema, _) = self.table_schema_and_root(schema_name)?;

        let mut pager = self.pager.borrow_mut();
        for (index_name, index_meta, index_root_page_id) in self.indexes_for_table(schema_name)? {
            let key_values = project_index_key(&schema, &index_meta, &existing_row)?;
            let encoded_key = encode_index_key(&key_values)?;
            let mut tree = IndexTree::from_root(index_root_page_id);
            Self::remove_index_entry(
                &mut tree,
                &mut pager,
                transaction_id.0,
                &encoded_key,
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
            tree.scan_all(&pager)?
        };

        let mut pager = self.pager.borrow_mut();
        let mut tree = IndexTree::create(&mut pager, transaction_id.0)?;

        for (row_id, bytes) in existing_rows {
            let row = decode_row(&bytes)?;
            let key_values = project_index_key(&schema, &index, &row)?;
            let encoded_key = encode_index_key(&key_values)?;
            if index.enforces_unique_key(&key_values)
                && tree
                    .get(&pager, &encoded_key)?
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
        let (index, root_page_id) = self.index_meta_and_root(schema_name, index_name)?;
        if key.len() != index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} expected {} key values but got {}",
                index.name,
                index.columns.len(),
                key.len()
            )));
        }
        let encoded_key = encode_index_key(key)?;
        let pager = self.pager.borrow();
        let tree = IndexTree::from_root(root_page_id);
        Ok(tree
            .get(&pager, &encoded_key)?
            .map(|bytes| decode_row_ids(&bytes))
            .transpose()?
            .unwrap_or_default())
    }

    fn scan_index_prefix(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key_prefix: &[Value],
    ) -> Result<Vec<RowId>> {
        self.validate_transaction(transaction_id)?;
        let (index, root_page_id) = self.index_meta_and_root(schema_name, index_name)?;
        if key_prefix.len() > index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} expected at most {} key values but got {}",
                index.name,
                index.columns.len(),
                key_prefix.len()
            )));
        }

        let pager = self.pager.borrow();
        let tree = IndexTree::from_root(root_page_id);
        let mut row_ids = BTreeSet::new();
        for (encoded_key, encoded_row_ids) in tree.scan_all(&pager)? {
            let decoded_key = decode_index_key(&encoded_key)?;
            if decoded_key.starts_with(key_prefix) {
                row_ids.extend(decode_row_ids(&encoded_row_ids)?);
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
        let (index, root_page_id) = self.index_meta_and_root(schema_name, index_name)?;
        if key_prefix.len() >= index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} has no range column after prefix of length {}",
                index.name,
                key_prefix.len()
            )));
        }

        let pager = self.pager.borrow();
        let tree = IndexTree::from_root(root_page_id);
        let mut row_ids = BTreeSet::new();
        for (encoded_key, encoded_row_ids) in tree.scan_all(&pager)? {
            let decoded_key = decode_index_key(&encoded_key)?;
            if !decoded_key.starts_with(key_prefix) {
                continue;
            }

            let Some(candidate) = decoded_key.get(key_prefix.len()) else {
                continue;
            };
            if matches_bounds(candidate, lower, upper) {
                row_ids.extend(decode_row_ids(&encoded_row_ids)?);
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
