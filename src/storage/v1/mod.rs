use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::common::error::{DbError, Result};
use crate::common::types::{ColumnDef, IndexMeta, Row, RowId, Schema, Value};
use crate::engine::traits::{
    CatalogStore, IndexStore, PlanningStorageEngine, TableStore, TransactionManager,
};
use crate::engine::txn::TransactionId;
use crate::sql::ast::CompareOp;
use crate::sql::planner::PlanningContext;

pub mod catalog;
pub mod index;
pub mod table;
pub mod txn;

#[derive(Debug, Default)]
pub struct FileStorage {
    base: PathBuf,
    inner: RefCell<FileStorageInner>,
}

#[derive(Debug)]
struct FileStorageInner {
    state: PersistedState,
    next_txn_id: u64,
    active_txn: Option<ActiveTransaction>,
}

#[derive(Debug, Clone, Default)]
struct PersistedState {
    schemas: BTreeMap<String, Schema>,
    rows: BTreeMap<String, BTreeMap<RowId, Row>>,
    indexes: BTreeMap<String, BTreeMap<String, IndexState>>,
    next_row_ids: BTreeMap<String, u64>,
}

#[derive(Debug, Clone)]
struct ActiveTransaction {
    id: TransactionId,
    snapshot: PersistedState,
}

#[derive(Debug, Clone)]
struct IndexState {
    meta: IndexMeta,
    entries: BTreeMap<Vec<Value>, BTreeSet<RowId>>,
}

impl Default for FileStorageInner {
    fn default() -> Self {
        Self {
            state: PersistedState::default(),
            next_txn_id: 1,
            active_txn: None,
        }
    }
}

impl FileStorage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let base = path.as_ref().to_path_buf();
        ensure_layout(&base)?;
        let _ = txn::clear_active_txn(&base);

        Ok(Self {
            inner: RefCell::new(FileStorageInner {
                state: load_state(&base)?,
                next_txn_id: 1,
                active_txn: None,
            }),
            base,
        })
    }

    fn planning_context(&self, transaction_id: Option<TransactionId>) -> Result<PlanningContext> {
        let inner = self.inner.borrow();
        inner.validate_snapshot_transaction(transaction_id)?;

        let schemas = inner
            .state
            .schemas
            .clone()
            .into_iter()
            .collect::<HashMap<_, _>>();
        let indexes = inner
            .state
            .indexes
            .iter()
            .map(|(table, entries)| {
                (
                    table.clone(),
                    entries
                        .values()
                        .map(|state| state.meta.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        Ok(PlanningContext::new(schemas, indexes))
    }
}

impl PlanningStorageEngine for FileStorage {
    fn planning_context_snapshot(
        &self,
        transaction_id: Option<TransactionId>,
    ) -> Result<PlanningContext> {
        self.planning_context(transaction_id)
    }
}

impl FileStorageInner {
    fn project_index_key(schema: &Schema, index: &IndexMeta, row: &Row) -> Result<Vec<Value>> {
        index
            .columns
            .iter()
            .map(|column| {
                let position = Self::column_position(schema, column)?;
                row.get(position).cloned().ok_or_else(|| {
                    DbError::storage(format!(
                        "row for table {} is missing column {column}",
                        schema.name
                    ))
                })
            })
            .collect()
    }

    fn validate_transaction(&self, transaction_id: TransactionId) -> Result<()> {
        match &self.active_txn {
            Some(active) if active.id == transaction_id => Ok(()),
            Some(active) => Err(DbError::txn(format!(
                "transaction {} is not active; current transaction is {}",
                transaction_id.0, active.id.0
            ))),
            None => Err(DbError::txn("no active transaction")),
        }
    }

    fn validate_snapshot_transaction(&self, transaction_id: Option<TransactionId>) -> Result<()> {
        match (transaction_id, &self.active_txn) {
            (Some(id), Some(active)) if id == active.id => Ok(()),
            (Some(id), Some(active)) => Err(DbError::txn(format!(
                "transaction {} is not active; current transaction is {}",
                id.0, active.id.0
            ))),
            (Some(id), None) => Err(DbError::txn(format!("transaction {} is not active", id.0))),
            (None, Some(_)) => Err(DbError::txn(
                "metadata snapshot requires the active transaction id while a transaction is open",
            )),
            (None, None) => Ok(()),
        }
    }

    fn require_schema(&self, schema_name: &str) -> Result<&Schema> {
        self.state
            .schemas
            .get(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))
    }

    fn next_row_id(&mut self, schema_name: &str) -> RowId {
        let next = self
            .state
            .next_row_ids
            .entry(schema_name.to_string())
            .or_insert(1);
        let row_id = RowId(*next);
        *next += 1;
        row_id
    }

    fn column_position(schema: &Schema, column: &str) -> Result<usize> {
        schema
            .columns
            .iter()
            .position(|entry| entry.name == column)
            .ok_or_else(|| {
                DbError::storage(format!("unknown column {column} on table {}", schema.name))
            })
    }

    fn add_row_to_indexes(&mut self, schema_name: &str, row_id: RowId, row: &Row) -> Result<()> {
        let schema = self.require_schema(schema_name)?.clone();
        let Some(indexes) = self.state.indexes.get_mut(schema_name) else {
            return Ok(());
        };

        for index in indexes.values_mut() {
            let key = Self::project_index_key(&schema, &index.meta, row)?;
            if index.meta.enforces_unique_key(&key)
                && index
                    .entries
                    .get(&key)
                    .is_some_and(|row_ids| !row_ids.is_empty())
            {
                return Err(DbError::storage(format!(
                    "unique index {} constraint failed",
                    index.meta.name
                )));
            }
            index.entries.entry(key).or_default().insert(row_id);
        }

        Ok(())
    }

    fn validate_unique_index_constraints(&self, schema_name: &str, row: &Row) -> Result<()> {
        let schema = self.require_schema(schema_name)?.clone();
        let Some(indexes) = self.state.indexes.get(schema_name) else {
            return Ok(());
        };

        for index in indexes.values() {
            let key = Self::project_index_key(&schema, &index.meta, row)?;
            if index.meta.enforces_unique_key(&key)
                && index
                    .entries
                    .get(&key)
                    .is_some_and(|row_ids| !row_ids.is_empty())
            {
                return Err(DbError::storage(format!(
                    "unique index {} constraint failed",
                    index.meta.name
                )));
            }
        }

        Ok(())
    }

    fn remove_row_from_indexes(
        &mut self,
        schema_name: &str,
        row_id: RowId,
        row: &Row,
    ) -> Result<()> {
        let schema = self.require_schema(schema_name)?.clone();
        let Some(indexes) = self.state.indexes.get_mut(schema_name) else {
            return Ok(());
        };

        for index in indexes.values_mut() {
            let key = Self::project_index_key(&schema, &index.meta, row)?;

            if let Some(row_ids) = index.entries.get_mut(&key) {
                row_ids.remove(&row_id);
                if row_ids.is_empty() {
                    index.entries.remove(&key);
                }
            }
        }

        Ok(())
    }

    fn rebuild_indexes_for_schema(
        &mut self,
        schema_name: &str,
        schema: &Schema,
        rows: &BTreeMap<RowId, Row>,
    ) -> Result<()> {
        let Some(indexes) = self.state.indexes.get_mut(schema_name) else {
            return Ok(());
        };

        for index in indexes.values_mut() {
            index.entries.clear();
            for (row_id, row) in rows {
                let key = Self::project_index_key(schema, &index.meta, row)?;
                index.entries.entry(key).or_default().insert(*row_id);
            }
        }

        Ok(())
    }
}

impl CatalogStore for FileStorage {
    fn create_schema(&self, transaction_id: TransactionId, schema: Schema) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;

        if inner.state.schemas.contains_key(&schema.name) {
            return Err(DbError::storage(format!(
                "table already exists: {}",
                schema.name
            )));
        }

        inner
            .state
            .schemas
            .insert(schema.name.clone(), schema.clone());
        inner.state.rows.entry(schema.name.clone()).or_default();
        inner.state.indexes.entry(schema.name.clone()).or_default();
        inner.state.next_row_ids.entry(schema.name).or_insert(1);
        Ok(())
    }

    fn drop_schema(&self, transaction_id: TransactionId, name: &str) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;

        if inner.state.schemas.remove(name).is_none() {
            return Err(DbError::storage(format!("unknown table: {name}")));
        }
        inner.state.rows.remove(name);
        inner.state.indexes.remove(name);
        inner.state.next_row_ids.remove(name);
        Ok(())
    }

    fn replace_schema(&self, transaction_id: TransactionId, schema: Schema) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;
        if !inner.state.schemas.contains_key(&schema.name) {
            return Err(DbError::storage(format!("unknown table: {}", schema.name)));
        }
        inner.state.schemas.insert(schema.name.clone(), schema);
        Ok(())
    }

    fn rename_schema(
        &self,
        transaction_id: TransactionId,
        old_name: &str,
        new_name: &str,
    ) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;
        if inner.state.schemas.contains_key(new_name) {
            return Err(DbError::storage(format!(
                "table already exists: {new_name}"
            )));
        }
        let mut schema = inner
            .state
            .schemas
            .remove(old_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {old_name}")))?;
        schema.name = new_name.to_string();
        inner.state.schemas.insert(new_name.to_string(), schema);
        if let Some(rows) = inner.state.rows.remove(old_name) {
            inner.state.rows.insert(new_name.to_string(), rows);
        }
        if let Some(indexes) = inner.state.indexes.remove(old_name) {
            inner.state.indexes.insert(new_name.to_string(), indexes);
        }
        if let Some(next_row_id) = inner.state.next_row_ids.remove(old_name) {
            inner
                .state
                .next_row_ids
                .insert(new_name.to_string(), next_row_id);
        }
        for schema in inner.state.schemas.values_mut() {
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
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;
        let schema = inner.require_schema(schema_name)?.clone();
        if schema.columns.iter().any(|entry| entry.name == column.name) {
            return Err(DbError::storage(format!(
                "column already exists on table {schema_name}: {}",
                column.name
            )));
        }
        let default_value = column
            .default_value
            .as_ref()
            .map_or(Ok(Value::Null), |default| default.evaluate())?;
        let mut updated_schema = schema;
        updated_schema.columns.push(column);
        updated_schema.validate_constraints_metadata()?;
        if let Some(rows) = inner.state.rows.get(schema_name) {
            for row in rows.values() {
                let mut candidate = row.clone();
                candidate.push(default_value.clone());
                updated_schema.validate_row_values(&candidate)?;
                updated_schema.validate_check_constraints(&candidate)?;
            }
        }
        inner
            .state
            .schemas
            .insert(schema_name.to_string(), updated_schema);
        if let Some(rows) = inner.state.rows.get_mut(schema_name) {
            for row in rows.values_mut() {
                row.push(default_value.clone());
            }
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
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;
        let schema = inner.require_schema(schema_name)?.clone();
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
        inner
            .state
            .schemas
            .insert(schema_name.to_string(), updated_schema);
        if let Some(indexes) = inner.state.indexes.get_mut(schema_name) {
            for index in indexes.values_mut() {
                for column in &mut index.meta.columns {
                    if column == old_name {
                        *column = new_name.to_string();
                    }
                }
            }
        }
        for (name, schema) in &mut inner.state.schemas {
            if name != schema_name {
                schema.rename_foreign_key_ref_column(schema_name, old_name, new_name);
            }
        }
        Ok(())
    }

    fn drop_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        old_name: &str,
    ) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;
        let schema = inner.require_schema(schema_name)?.clone();
        let (updated_schema, removed_index) = schema.drop_column(old_name)?;

        let updated_rows = inner
            .state
            .rows
            .get(schema_name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|(row_id, mut row)| {
                row.remove(removed_index);
                (row_id, row)
            })
            .collect::<BTreeMap<_, _>>();

        inner
            .state
            .schemas
            .insert(schema_name.to_string(), updated_schema.clone());
        inner
            .state
            .rows
            .insert(schema_name.to_string(), updated_rows.clone());
        inner.rebuild_indexes_for_schema(schema_name, &updated_schema, &updated_rows)?;
        Ok(())
    }

    fn get_schema(&self, transaction_id: TransactionId, name: &str) -> Result<Option<Schema>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;
        Ok(inner.state.schemas.get(name).cloned())
    }

    fn list_schemas(&self, transaction_id: TransactionId) -> Result<Vec<Schema>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;
        Ok(inner.state.schemas.values().cloned().collect())
    }
}

impl TableStore for FileStorage {
    fn insert_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row: Row,
    ) -> Result<RowId> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;

        let schema = inner.require_schema(schema_name)?.clone();
        if row.len() != schema.columns.len() {
            return Err(DbError::storage(format!(
                "insert into {schema_name} expected {} values but got {}",
                schema.columns.len(),
                row.len()
            )));
        }

        {
            let existing_rows = inner
                .state
                .rows
                .get(schema_name)
                .map(|rows| rows.values().collect::<Vec<_>>())
                .unwrap_or_default();
            schema.validate_row_values(&row)?;
            schema.validate_check_constraints(&row)?;
            schema.validate_primary_key_uniqueness(&row, &existing_rows)?;
        }
        inner.validate_unique_index_constraints(schema_name, &row)?;

        let row_id = inner.next_row_id(schema_name);
        inner
            .state
            .rows
            .entry(schema_name.to_string())
            .or_default()
            .insert(row_id, row.clone());
        inner.add_row_to_indexes(schema_name, row_id, &row)?;
        Ok(row_id)
    }

    fn get_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
    ) -> Result<Option<Row>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;
        Ok(inner
            .state
            .rows
            .get(schema_name)
            .and_then(|rows| rows.get(&row_id))
            .cloned())
    }

    fn scan_rows(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<(RowId, Row)>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;
        Ok(inner
            .state
            .rows
            .get(schema_name)
            .map(|rows| {
                rows.iter()
                    .map(|(row_id, row)| (*row_id, row.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn delete_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
    ) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;

        let removed = inner
            .state
            .rows
            .get_mut(schema_name)
            .and_then(|rows| rows.remove(&row_id));

        if let Some(row) = removed {
            inner.remove_row_from_indexes(schema_name, row_id, &row)?;
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
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;

        let schema = inner.require_schema(schema_name)?.clone();
        if row.len() != schema.columns.len() {
            return Err(DbError::storage(format!(
                "update {schema_name} expected {} values but got {}",
                schema.columns.len(),
                row.len()
            )));
        }

        let Some(existing_row) = inner
            .state
            .rows
            .get(schema_name)
            .and_then(|rows| rows.get(&row_id))
            .cloned()
        else {
            return Ok(());
        };

        if existing_row == row {
            return Ok(());
        }

        schema.validate_row_values(&row)?;
        schema.validate_check_constraints(&row)?;
        {
            let existing_rows = inner
                .state
                .rows
                .get(schema_name)
                .map(|rows| {
                    rows.iter()
                        .filter(|(candidate_row_id, _)| **candidate_row_id != row_id)
                        .map(|(_, existing_row)| existing_row)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            schema.validate_primary_key_uniqueness(&row, &existing_rows)?;
        }

        if let Some(indexes) = inner.state.indexes.get(schema_name) {
            for index in indexes.values() {
                let key = FileStorageInner::project_index_key(&schema, &index.meta, &row)?;
                if index.meta.enforces_unique_key(&key)
                    && index.entries.get(&key).is_some_and(|row_ids| {
                        row_ids
                            .iter()
                            .any(|candidate_row_id| *candidate_row_id != row_id)
                    })
                {
                    return Err(DbError::storage(format!(
                        "unique index {} constraint failed",
                        index.meta.name
                    )));
                }
            }
        }

        inner.remove_row_from_indexes(schema_name, row_id, &existing_row)?;
        inner
            .state
            .rows
            .entry(schema_name.to_string())
            .or_default()
            .insert(row_id, row.clone());
        inner.add_row_to_indexes(schema_name, row_id, &row)?;
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
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;

        let schema = inner.require_schema(schema_name)?.clone();
        if inner
            .state
            .indexes
            .get(schema_name)
            .is_some_and(|schema_indexes| schema_indexes.contains_key(&index.name))
        {
            return Err(DbError::storage(format!(
                "index already exists: {}",
                index.name
            )));
        }

        if index.columns.is_empty() {
            return Err(DbError::storage(format!(
                "index {} has no columns",
                index.name
            )));
        }

        let mut entries = BTreeMap::new();
        if let Some(rows) = inner.state.rows.get(schema_name) {
            for (row_id, row) in rows {
                let key = FileStorageInner::project_index_key(&schema, &index, row)?;
                if index.enforces_unique_key(&key)
                    && entries
                        .get(&key)
                        .is_some_and(|row_ids: &BTreeSet<RowId>| !row_ids.is_empty())
                {
                    return Err(DbError::storage(format!(
                        "unique index {} constraint failed",
                        index.name
                    )));
                }
                entries
                    .entry(key)
                    .or_insert_with(BTreeSet::new)
                    .insert(*row_id);
            }
        }

        inner
            .state
            .indexes
            .entry(schema_name.to_string())
            .or_default()
            .insert(
                index.name.clone(),
                IndexState {
                    meta: index,
                    entries,
                },
            );
        Ok(())
    }

    fn drop_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;

        let removed = inner
            .state
            .indexes
            .get_mut(schema_name)
            .and_then(|indexes| indexes.remove(index_name));
        if removed.is_none() {
            return Err(DbError::storage(format!(
                "unknown index {index_name} on table {schema_name}"
            )));
        }
        Ok(())
    }

    fn get_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<Option<IndexMeta>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;
        Ok(inner
            .state
            .indexes
            .get(schema_name)
            .and_then(|indexes| indexes.get(index_name))
            .map(|state| state.meta.clone()))
    }

    fn list_indexes(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<IndexMeta>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;
        Ok(inner
            .state
            .indexes
            .get(schema_name)
            .map(|indexes| indexes.values().map(|state| state.meta.clone()).collect())
            .unwrap_or_default())
    }

    fn lookup_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key: &[Value],
    ) -> Result<Vec<RowId>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;

        let index = inner
            .state
            .indexes
            .get(schema_name)
            .and_then(|indexes| indexes.get(index_name))
            .ok_or_else(|| {
                DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
            })?;

        Ok(index
            .entries
            .get(key)
            .map(|row_ids| row_ids.iter().copied().collect())
            .unwrap_or_default())
    }

    fn scan_index_prefix(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key_prefix: &[Value],
    ) -> Result<Vec<RowId>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;

        let index = inner
            .state
            .indexes
            .get(schema_name)
            .and_then(|indexes| indexes.get(index_name))
            .ok_or_else(|| {
                DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
            })?;

        if key_prefix.len() > index.meta.columns.len() {
            return Err(DbError::storage(format!(
                "index {} expected at most {} key values but got {}",
                index.meta.name,
                index.meta.columns.len(),
                key_prefix.len()
            )));
        }

        let mut row_ids = BTreeSet::new();
        for (key, matches) in &index.entries {
            if key.starts_with(key_prefix) {
                row_ids.extend(matches.iter().copied());
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
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;

        let index = inner
            .state
            .indexes
            .get(schema_name)
            .and_then(|indexes| indexes.get(index_name))
            .ok_or_else(|| {
                DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
            })?;

        if key_prefix.len() >= index.meta.columns.len() {
            return Err(DbError::storage(format!(
                "index {} has no range column after prefix of length {}",
                index.meta.name,
                key_prefix.len()
            )));
        }

        let mut row_ids = BTreeSet::new();
        for (key, matches) in &index.entries {
            if !key.starts_with(key_prefix) {
                continue;
            }

            let Some(candidate) = key.get(key_prefix.len()) else {
                continue;
            };
            if matches_bounds(candidate, lower, upper) {
                row_ids.extend(matches.iter().copied());
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

impl TransactionManager for FileStorage {
    fn begin(&self) -> Result<TransactionId> {
        let mut inner = self.inner.borrow_mut();
        if let Some(active) = &inner.active_txn {
            return Err(DbError::txn(format!(
                "transaction {} is already active",
                active.id.0
            )));
        }

        let transaction_id = TransactionId(inner.next_txn_id);
        txn::write_active_txn(&self.base, transaction_id)?;
        inner.next_txn_id += 1;
        inner.active_txn = Some(ActiveTransaction {
            id: transaction_id,
            snapshot: inner.state.clone(),
        });
        Ok(transaction_id)
    }

    fn commit(&self, transaction_id: TransactionId) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;
        persist_state(&self.base, &inner.state)?;
        inner.active_txn = None;
        let _ = txn::clear_active_txn(&self.base);
        Ok(())
    }

    fn rollback(&self, transaction_id: TransactionId) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let active = inner
            .active_txn
            .take()
            .ok_or_else(|| DbError::txn("no active transaction"))?;

        if active.id != transaction_id {
            inner.active_txn = Some(active);
            return Err(DbError::txn(format!(
                "transaction {} is not active",
                transaction_id.0
            )));
        }

        inner.state = active.snapshot;
        let _ = txn::clear_active_txn(&self.base);
        Ok(())
    }
}

fn load_state(base: &Path) -> Result<PersistedState> {
    let catalog = catalog::load_catalog(base)?;
    let mut state = PersistedState {
        schemas: catalog.schemas,
        rows: BTreeMap::new(),
        indexes: BTreeMap::new(),
        next_row_ids: BTreeMap::new(),
    };

    for table_name in state.schemas.keys().cloned().collect::<Vec<_>>() {
        let table_file = table::load_table(base, &table_name)?;
        state
            .next_row_ids
            .insert(table_name.clone(), table_file.next_row_id);
        state.rows.insert(
            table_name.clone(),
            table_file
                .rows
                .into_iter()
                .map(|record| (record.row_id, record.row))
                .collect(),
        );

        let table_index_dir = index::table_indexes_dir(base, &table_name);
        let mut schema_indexes = BTreeMap::new();
        if table_index_dir.is_dir() {
            for entry in fs::read_dir(&table_index_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                let Some(index_name) = path.file_stem().and_then(|name| name.to_str()) else {
                    continue;
                };
                if let Some(index_file) = index::load_index(base, &table_name, index_name)? {
                    schema_indexes.insert(
                        index_file.meta.name.clone(),
                        IndexState {
                            meta: index_file.meta,
                            entries: index_file
                                .entries
                                .into_iter()
                                .map(|entry| (entry.key, entry.row_ids.into_iter().collect()))
                                .collect(),
                        },
                    );
                }
            }
        }
        state.indexes.insert(table_name, schema_indexes);
    }

    Ok(state)
}

fn persist_state(base: &Path, state: &PersistedState) -> Result<()> {
    let tables_dir = table::tables_dir(base);
    if tables_dir.exists() {
        fs::remove_dir_all(&tables_dir)?;
    }
    fs::create_dir_all(&tables_dir)?;

    let indexes_dir = index::indexes_dir(base);
    if indexes_dir.exists() {
        fs::remove_dir_all(&indexes_dir)?;
    }
    fs::create_dir_all(&indexes_dir)?;

    catalog::save_catalog(
        base,
        &catalog::CatalogFile {
            schemas: state.schemas.clone(),
        },
    )?;

    for table_name in state.schemas.keys() {
        let rows = state
            .rows
            .get(table_name)
            .into_iter()
            .flat_map(|rows| rows.iter())
            .map(|(row_id, row)| table::TableRowRecord {
                row_id: *row_id,
                row: row.clone(),
            })
            .collect();
        table::save_table(
            base,
            table_name,
            &table::TableFile {
                next_row_id: *state.next_row_ids.get(table_name).unwrap_or(&1),
                rows,
            },
        )?;

        for index_state in state
            .indexes
            .get(table_name)
            .into_iter()
            .flat_map(|indexes| indexes.values())
        {
            index::save_index(
                base,
                table_name,
                &index_state.meta.name,
                &index::IndexFile {
                    meta: index_state.meta.clone(),
                    entries: index_state
                        .entries
                        .iter()
                        .map(|(key, row_ids)| index::IndexEntryFile {
                            key: key.clone(),
                            row_ids: row_ids.iter().copied().collect(),
                        })
                        .collect(),
                },
            )?;
        }
    }

    Ok(())
}

fn ensure_layout(base: &Path) -> Result<()> {
    if base.exists() && !base.is_dir() {
        return Err(DbError::storage(format!(
            "database path must be a directory: {}",
            base.display()
        )));
    }

    fs::create_dir_all(base)?;
    fs::create_dir_all(table::tables_dir(base))?;
    fs::create_dir_all(index::indexes_dir(base))?;
    fs::create_dir_all(txn::txn_dir(base))?;
    Ok(())
}

pub(crate) fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("tmp");
    fs::write(&temp_path, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temp_path, path)?;
    Ok(())
}

pub(crate) fn read_json_if_exists<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

pub(crate) fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
