use std::cell::RefCell;
use std::cmp::{Ordering, max};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Row, RowId, Schema, Value};
use crate::engine::traits::{
    CatalogStore, IndexStore, PlanningStorageEngine, TableStore, TransactionManager,
};
use crate::engine::txn::TransactionId;
use crate::sql::ast::CompareOp;
use crate::sql::planner::PlanningContext;

#[derive(Debug, Default)]
pub struct MemoryStorage {
    inner: RefCell<MemoryStorageInner>,
}

#[derive(Debug)]
struct MemoryStorageInner {
    schemas: HashMap<String, Schema>,
    rows: HashMap<String, BTreeMap<RowId, Row>>,
    indexes: HashMap<String, BTreeMap<String, IndexState>>,
    next_row_ids: HashMap<String, u64>,
    next_txn_id: u64,
    active_txn: Option<ActiveTransaction>,
}

#[derive(Debug)]
struct ActiveTransaction {
    id: TransactionId,
    undo_log: Vec<UndoOp>,
}

#[derive(Debug, Clone)]
struct IndexState {
    meta: IndexMeta,
    entries: BTreeMap<Vec<Value>, BTreeSet<RowId>>,
}

#[derive(Debug)]
enum UndoOp {
    DropSchema {
        schema_name: String,
    },
    DropIndex {
        schema_name: String,
        index_name: String,
    },
    DeleteRow {
        schema_name: String,
        row_id: RowId,
    },
    ReinsertRow {
        schema_name: String,
        row_id: RowId,
        row: Row,
    },
}

impl Default for MemoryStorageInner {
    fn default() -> Self {
        Self {
            schemas: HashMap::new(),
            rows: HashMap::new(),
            indexes: HashMap::new(),
            next_row_ids: HashMap::new(),
            next_txn_id: 1,
            active_txn: None,
        }
    }
}

impl MemoryStorage {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn planning_context(
        &self,
        transaction_id: Option<TransactionId>,
    ) -> Result<PlanningContext> {
        let inner = self.inner.borrow();
        inner.validate_snapshot_transaction(transaction_id)?;

        let indexes = inner
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
            .collect();

        Ok(PlanningContext::new(inner.schemas.clone(), indexes))
    }
}

impl PlanningStorageEngine for MemoryStorage {
    fn planning_context_snapshot(
        &self,
        transaction_id: Option<TransactionId>,
    ) -> Result<PlanningContext> {
        self.planning_context(transaction_id)
    }
}

impl MemoryStorageInner {
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

    fn record_undo(&mut self, undo: UndoOp) {
        if let Some(active) = self.active_txn.as_mut() {
            active.undo_log.push(undo);
        }
    }

    fn require_schema(&self, schema_name: &str) -> Result<&Schema> {
        self.schemas
            .get(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))
    }

    fn next_row_id(&mut self, schema_name: &str) -> RowId {
        let entry = self
            .next_row_ids
            .entry(schema_name.to_string())
            .or_insert(1);
        let row_id = RowId(*entry);
        *entry += 1;
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
        let Some(indexes) = self.indexes.get_mut(schema_name) else {
            return Ok(());
        };

        for index in indexes.values_mut() {
            let key = Self::project_index_key(&schema, &index.meta, row)?;
            index.entries.entry(key).or_default().insert(row_id);
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
        let Some(indexes) = self.indexes.get_mut(schema_name) else {
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

    fn apply_undo(&mut self, undo: UndoOp) -> Result<()> {
        match undo {
            UndoOp::DropSchema { schema_name } => {
                self.schemas.remove(&schema_name);
                self.rows.remove(&schema_name);
                self.indexes.remove(&schema_name);
                self.next_row_ids.remove(&schema_name);
                Ok(())
            }
            UndoOp::DropIndex {
                schema_name,
                index_name,
            } => {
                if let Some(indexes) = self.indexes.get_mut(&schema_name) {
                    indexes.remove(&index_name);
                }
                Ok(())
            }
            UndoOp::DeleteRow {
                schema_name,
                row_id,
            } => {
                let removed_row = self
                    .rows
                    .get_mut(&schema_name)
                    .and_then(|rows| rows.remove(&row_id));

                if let Some(row) = removed_row {
                    self.remove_row_from_indexes(&schema_name, row_id, &row)?;
                }

                Ok(())
            }
            UndoOp::ReinsertRow {
                schema_name,
                row_id,
                row,
            } => {
                self.rows
                    .entry(schema_name.clone())
                    .or_default()
                    .insert(row_id, row.clone());
                self.next_row_ids
                    .entry(schema_name.clone())
                    .and_modify(|next| *next = max(*next, row_id.0 + 1))
                    .or_insert(row_id.0 + 1);
                self.add_row_to_indexes(&schema_name, row_id, &row)
            }
        }
    }
}

impl CatalogStore for MemoryStorage {
    fn create_schema(&self, transaction_id: TransactionId, schema: Schema) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;

        if inner.schemas.contains_key(&schema.name) {
            return Err(DbError::storage(format!(
                "table already exists: {}",
                schema.name
            )));
        }

        inner.record_undo(UndoOp::DropSchema {
            schema_name: schema.name.clone(),
        });
        inner.schemas.insert(schema.name.clone(), schema.clone());
        inner.rows.entry(schema.name.clone()).or_default();
        inner.indexes.entry(schema.name.clone()).or_default();
        inner.next_row_ids.entry(schema.name).or_insert(1);
        Ok(())
    }

    fn get_schema(&self, transaction_id: TransactionId, name: &str) -> Result<Option<Schema>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;
        Ok(inner.schemas.get(name).cloned())
    }

    fn list_schemas(&self, transaction_id: TransactionId) -> Result<Vec<Schema>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;
        Ok(inner.schemas.values().cloned().collect())
    }
}

impl TableStore for MemoryStorage {
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
                .rows
                .get(schema_name)
                .map(|rows| rows.values().collect::<Vec<_>>())
                .unwrap_or_default();
            schema.validate_row_values(&row)?;
            schema.validate_primary_key_uniqueness(&row, &existing_rows)?;
        }

        let row_id = inner.next_row_id(schema_name);
        inner
            .rows
            .entry(schema_name.to_string())
            .or_default()
            .insert(row_id, row.clone());
        inner.add_row_to_indexes(schema_name, row_id, &row)?;
        inner.record_undo(UndoOp::DeleteRow {
            schema_name: schema_name.to_string(),
            row_id,
        });
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
            .rows
            .get_mut(schema_name)
            .and_then(|rows| rows.remove(&row_id));

        if let Some(row) = removed {
            inner.remove_row_from_indexes(schema_name, row_id, &row)?;
            inner.record_undo(UndoOp::ReinsertRow {
                schema_name: schema_name.to_string(),
                row_id,
                row,
            });
        }

        Ok(())
    }
}

impl IndexStore for MemoryStorage {
    fn create_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index: IndexMeta,
    ) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;

        let schema = inner.require_schema(schema_name)?.clone();
        let already_exists = inner
            .indexes
            .get(schema_name)
            .is_some_and(|schema_indexes| schema_indexes.contains_key(&index.name));
        if already_exists {
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
        if let Some(rows) = inner.rows.get(schema_name) {
            for (row_id, row) in rows {
                let key = MemoryStorageInner::project_index_key(&schema, &index, row)?;
                entries
                    .entry(key)
                    .or_insert_with(BTreeSet::new)
                    .insert(*row_id);
            }
        }

        inner.record_undo(UndoOp::DropIndex {
            schema_name: schema_name.to_string(),
            index_name: index.name.clone(),
        });
        inner
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

    fn get_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<Option<IndexMeta>> {
        let inner = self.inner.borrow();
        inner.validate_transaction(transaction_id)?;
        Ok(inner
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

impl TransactionManager for MemoryStorage {
    fn begin(&self) -> Result<TransactionId> {
        let mut inner = self.inner.borrow_mut();
        if let Some(active) = &inner.active_txn {
            return Err(DbError::txn(format!(
                "transaction {} is already active",
                active.id.0
            )));
        }

        let transaction_id = TransactionId(inner.next_txn_id);
        inner.next_txn_id += 1;
        inner.active_txn = Some(ActiveTransaction {
            id: transaction_id,
            undo_log: Vec::new(),
        });
        Ok(transaction_id)
    }

    fn commit(&self, transaction_id: TransactionId) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.validate_transaction(transaction_id)?;
        inner.active_txn = None;
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

        for undo in active.undo_log.into_iter().rev() {
            inner.apply_undo(undo)?;
        }

        Ok(())
    }
}
