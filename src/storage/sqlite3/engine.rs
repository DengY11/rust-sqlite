use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::common::error::{DbError, Result};
use crate::common::types::{ColumnDef, ColumnType, IndexMeta, Row, RowId, Schema, Value};
use crate::engine::traits::{
    CatalogStore, IndexStore, PlanningStorageEngine, TableStore, TransactionManager,
};
use crate::engine::txn::TransactionId;
use crate::sql::ast::{CompareOp, IsolationLevel};
use crate::sql::parser::parse_check_constraint_expression;
use crate::sql::planner::PlanningContext;

use super::btree::{get_table_row, lookup_index_entries, scan_table_rows};
use super::index_expr::validate_index_term;
use super::pager::Pager;
use super::schema::{Catalog, load_catalog};
use super::writer::{WritableDatabase, WritableTable, write_database};

#[derive(Debug)]
pub struct FileStorage {
    path: Option<PathBuf>,
    pager: RefCell<Option<Pager>>,
    catalog: RefCell<Catalog>,
    writable: RefCell<WritableDatabase>,
    txn_state: RefCell<TxnState>,
    ignore_check_constraints: RefCell<bool>,
    case_sensitive_like: RefCell<bool>,
}

#[derive(Debug)]
struct TxnState {
    next_txn_id: u64,
    active_txn: Option<TransactionId>,
    pending_writable: Option<WritableDatabase>,
}

impl Default for FileStorage {
    fn default() -> Self {
        Self {
            path: None,
            pager: RefCell::new(None),
            catalog: RefCell::new(Catalog::default()),
            writable: RefCell::new(WritableDatabase::default()),
            txn_state: RefCell::new(TxnState {
                next_txn_id: 1,
                active_txn: None,
                pending_writable: None,
            }),
            ignore_check_constraints: RefCell::new(false),
            case_sensitive_like: RefCell::new(false),
        }
    }
}

impl FileStorage {
    fn without_rowid_synthetic_row_id(schema: &Schema, row: &Row) -> Result<RowId> {
        let primary_key = schema.primary_key_constraint.as_ref().ok_or_else(|| {
            DbError::storage(format!(
                "WITHOUT ROWID table {} is missing PRIMARY KEY metadata",
                schema.name
            ))
        })?;
        let key = primary_key
            .columns
            .iter()
            .map(|column| {
                let index = schema.column_index(column)?;
                row.get(index).cloned().ok_or_else(|| {
                    DbError::storage(format!(
                        "row for table {} is missing column {column}",
                        schema.name
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Self::hash_without_rowid_key(&key)
    }

    fn hash_without_rowid_key(key: &[Value]) -> Result<RowId> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let value = hasher.finish();
        if value == 0 {
            return Ok(RowId(1));
        }
        Ok(RowId(value))
    }

    fn without_rowid_primary_key_index_name(schema_name: &str) -> String {
        format!("sqlite_autoindex_{schema_name}_1")
    }

    fn is_without_rowid_primary_key_index(
        schema: &Schema,
        index: &IndexMeta,
        schema_name: &str,
    ) -> bool {
        schema.without_rowid
            && index.name == Self::without_rowid_primary_key_index_name(schema_name)
            && schema
                .primary_key_constraint
                .as_ref()
                .is_some_and(|primary_key| primary_key.columns == index.columns)
    }

    fn without_rowid_lookup_row_ids(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        key_prefix: &[Value],
        require_full_key: bool,
    ) -> Result<Vec<RowId>> {
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        let rows = if self.txn_state.borrow().pending_writable.is_some() {
            self.writable_view()
                .tables
                .get(schema_name)
                .map(|table| table.rows.clone())
                .unwrap_or_default()
        } else {
            let (_, root_page) = self.require_schema_and_root_page(schema_name)?;
            let pager = self.pager.borrow();
            let pager = pager.as_ref().ok_or_else(|| {
                DbError::storage("sqlite3 FileStorage is not backed by a database file")
            })?;
            scan_table_rows(pager, root_page, &schema)?
        };

        if let Some(primary_key) = &schema.primary_key_constraint {
            let expected = primary_key.columns.len();
            if require_full_key && key_prefix.len() != expected {
                return Err(DbError::storage(format!(
                    "index {} expected {} key values but got {}",
                    Self::without_rowid_primary_key_index_name(schema_name),
                    expected,
                    key_prefix.len()
                )));
            }
            if !require_full_key && key_prefix.len() > expected {
                return Err(DbError::storage(format!(
                    "index {} expected at most {} key values but got {}",
                    Self::without_rowid_primary_key_index_name(schema_name),
                    expected,
                    key_prefix.len()
                )));
            }
        }

        let primary_key_columns = schema
            .primary_key_constraint
            .as_ref()
            .ok_or_else(|| {
                DbError::storage(format!(
                    "WITHOUT ROWID table {schema_name} is missing PRIMARY KEY metadata"
                ))
            })?
            .columns
            .clone();

        let mut row_ids = Vec::new();
        for (_row_id, row) in rows {
            let key = primary_key_columns
                .iter()
                .map(|column| {
                    let index = schema.column_index(column)?;
                    row.get(index).cloned().ok_or_else(|| {
                        DbError::storage(format!(
                            "row for table {} is missing column {column}",
                            schema.name
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if key.starts_with(key_prefix) {
                row_ids.push(Self::without_rowid_synthetic_row_id(&schema, &row)?);
            }
        }
        Ok(row_ids)
    }

    fn without_rowid_scan_rows_internal(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        schema: &Schema,
    ) -> Result<Vec<(RowId, Row)>> {
        if self.txn_state.borrow().pending_writable.is_some() {
            let rows = self
                .writable_view()
                .tables
                .get(schema_name)
                .map(|table| table.rows.clone())
                .unwrap_or_default();
            return Self::materialize_without_rowid_rows(schema, rows);
        }

        let (_, root_page) = self.require_schema_and_root_page(schema_name)?;
        let pager = self.pager.borrow();
        let pager = pager.as_ref().ok_or_else(|| {
            DbError::storage("sqlite3 FileStorage is not backed by a database file")
        })?;
        let _ = transaction_id;
        let rows = scan_table_rows(pager, root_page, schema)?;
        Self::materialize_without_rowid_rows(schema, rows)
    }

    fn materialize_without_rowid_rows(
        schema: &Schema,
        rows: Vec<(RowId, Row)>,
    ) -> Result<Vec<(RowId, Row)>> {
        rows.into_iter()
            .map(|(_row_id, row)| Ok((Self::without_rowid_synthetic_row_id(schema, &row)?, row)))
            .collect()
    }

    fn without_rowid_row_position(table: &WritableTable, row_id: RowId) -> Result<Option<usize>> {
        for (index, (_stored_row_id, row)) in table.rows.iter().enumerate() {
            if Self::without_rowid_synthetic_row_id(&table.schema, row)? == row_id {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let (pager, catalog, writable) = match Pager::open(&path) {
            Ok(pager) => {
                let catalog = load_catalog(&pager)?;
                let writable = Self::load_writable_database(&pager, &catalog)?;
                (Some(pager), catalog, writable)
            }
            Err(DbError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                (None, Catalog::default(), WritableDatabase::default())
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            path: Some(path),
            pager: RefCell::new(pager),
            catalog: RefCell::new(catalog),
            writable: RefCell::new(writable),
            txn_state: RefCell::new(TxnState {
                next_txn_id: 1,
                active_txn: None,
                pending_writable: None,
            }),
            ignore_check_constraints: RefCell::new(false),
            case_sensitive_like: RefCell::new(false),
        })
    }

    fn load_writable_database(pager: &Pager, catalog: &Catalog) -> Result<WritableDatabase> {
        let mut database = WritableDatabase::default();
        database.schema_version = pager.header().schema_version;
        database.user_version = pager.header().user_version;
        database.application_id = pager.header().application_id;
        for (table_name, schema) in catalog.schemas() {
            let root_page = catalog.table_root_page(table_name).ok_or_else(|| {
                DbError::storage(format!(
                    "sqlite catalog is missing root page for table {table_name}",
                ))
            })?;
            let rows = scan_table_rows(pager, root_page, schema)?;
            if schema.without_rowid {
                database.contains_without_rowid_tables = true;
            }
            database.tables.insert(
                table_name.clone(),
                WritableTable {
                    schema: schema.clone(),
                    rows,
                },
            );
        }
        for (table_name, indexes) in catalog.indexes() {
            database.indexes.insert(table_name.clone(), indexes.clone());
        }
        if let Some(root_page) = catalog.sqlite_sequence_root_page() {
            database.sqlite_sequence_exists = true;
            let schema = Schema::new(
                "sqlite_sequence",
                vec![
                    ColumnDef::new("name", ColumnType::Text),
                    ColumnDef::new("seq", ColumnType::Integer),
                ],
            );
            let rows = scan_table_rows(pager, root_page, &schema)?;
            for (_, row) in rows {
                let [Value::Text(name), Value::Integer(seq)] = row.as_slice() else {
                    return Err(DbError::storage(
                        "sqlite_sequence row did not contain expected (name TEXT, seq INTEGER)",
                    ));
                };
                let seq = u64::try_from(*seq).map_err(|_| {
                    DbError::storage("sqlite_sequence seq must be a non-negative INTEGER")
                })?;
                database.sqlite_sequence.insert(name.clone(), seq);
            }
        }
        Ok(database)
    }

    fn validate_transaction(&self, transaction_id: TransactionId) -> Result<()> {
        match self.txn_state.borrow().active_txn {
            Some(active) if active == transaction_id => Ok(()),
            Some(active) => Err(DbError::txn(format!(
                "transaction {} is not active; current transaction is {}",
                transaction_id.0, active.0
            ))),
            None => Err(DbError::txn("no active transaction")),
        }
    }

    fn unsupported(&self, operation: &str) -> DbError {
        DbError::storage(format!(
            "sqlite3 FileStorage does not support {operation} in this phase"
        ))
    }

    fn require_schema_and_root_page(&self, schema_name: &str) -> Result<(Schema, u32)> {
        let catalog = self.catalog.borrow();
        let schema = catalog
            .schemas()
            .get(schema_name)
            .cloned()
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        let root_page = catalog.table_root_page(schema_name).ok_or_else(|| {
            DbError::storage(format!(
                "sqlite catalog is missing root page for table {schema_name}",
            ))
        })?;
        Ok((schema, root_page))
    }

    fn require_index_and_root_page(
        &self,
        schema_name: &str,
        index_name: &str,
    ) -> Result<(IndexMeta, u32)> {
        let catalog = self.catalog.borrow();
        let index = catalog
            .indexes()
            .get(schema_name)
            .and_then(|indexes| indexes.get(index_name))
            .cloned()
            .ok_or_else(|| {
                DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
            })?;
        let root_page = catalog
            .index_root_page(schema_name, index_name)
            .ok_or_else(|| {
                DbError::storage(format!(
                    "sqlite catalog is missing root page for index {index_name} on table {schema_name}",
                ))
            })?;
        Ok((index, root_page))
    }

    fn writable_view(&self) -> WritableDatabase {
        let txn_state = self.txn_state.borrow();
        txn_state
            .pending_writable
            .clone()
            .unwrap_or_else(|| self.writable.borrow().clone())
    }

    fn with_pending_writable_mut<T>(
        &self,
        transaction_id: TransactionId,
        f: impl FnOnce(&mut WritableDatabase) -> Result<T>,
    ) -> Result<T> {
        self.validate_transaction(transaction_id)?;
        let base = self.writable.borrow().clone();
        let mut txn_state = self.txn_state.borrow_mut();
        let pending = txn_state.pending_writable.get_or_insert(base);
        f(pending)
    }

    fn project_index_key(
        &self,
        schema: &Schema,
        index: &IndexMeta,
        row: &Row,
    ) -> Result<Vec<Value>> {
        index
            .columns
            .iter()
            .map(|column| {
                crate::storage::sqlite3::index_expr::evaluate_index_term_with_like_mode(
                    schema,
                    row,
                    column,
                    *self.case_sensitive_like.borrow(),
                )
            })
            .collect()
    }

    fn row_matches_partial_index(
        &self,
        schema: &Schema,
        index: &IndexMeta,
        row: &Row,
    ) -> Result<bool> {
        let Some(predicate_sql) = index.predicate.as_deref() else {
            return Ok(true);
        };
        let predicate = parse_check_constraint_expression(predicate_sql)?;
        schema.validate_check_expr_metadata(&predicate)?;
        schema.matches_check_expr_with_like_mode(
            &predicate,
            row,
            *self.case_sensitive_like.borrow(),
        )
    }

    fn integer_primary_key_column_index(schema: &Schema) -> Option<usize> {
        if schema.without_rowid {
            return None;
        }
        let primary_key_columns = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.primary_key)
            .collect::<Vec<_>>();
        let [(index, column)] = primary_key_columns.as_slice() else {
            return None;
        };
        (matches!(column.column_type, ColumnType::Integer)
            && !matches!(
                column.primary_key_sort_order,
                Some(crate::common::types::SortOrder::Desc)
            ))
        .then_some(*index)
    }

    fn next_row_id_for_insert(table: &WritableTable, sqlite_sequence: Option<u64>) -> u64 {
        if table
            .schema
            .columns
            .iter()
            .any(|column| column.primary_key && column.autoincrement)
        {
            sqlite_sequence.unwrap_or(0).saturating_add(1)
        } else {
            table
                .rows
                .iter()
                .map(|(row_id, _)| row_id.0)
                .max()
                .unwrap_or(0)
                .saturating_add(1)
        }
    }

    fn compare_values(left: &Value, right: &Value) -> Result<Option<Ordering>> {
        Ok(match (left, right) {
            (Value::Null, Value::Null) => Some(Ordering::Equal),
            (Value::Boolean(left), Value::Boolean(right)) => Some(left.cmp(right)),
            (Value::Integer(left), Value::Integer(right)) => Some(left.cmp(right)),
            (Value::Blob(left), Value::Blob(right)) => Some(left.cmp(right)),
            (Value::Text(left), Value::Text(right)) => Some(left.cmp(right)),
            (Value::Null, _) | (_, Value::Null) => None,
            _ => {
                return Err(DbError::storage(format!(
                    "cannot compare {} with {} in sqlite index range scan",
                    left.type_name(),
                    right.type_name()
                )));
            }
        })
    }

    fn compare_with_operator(left: &Value, op: CompareOp, right: &Value) -> Result<bool> {
        let Some(ordering) = Self::compare_values(left, right)? else {
            return Ok(false);
        };
        Ok(match op {
            CompareOp::Eq => ordering == Ordering::Equal,
            CompareOp::Ne => ordering != Ordering::Equal,
            CompareOp::Gt => ordering == Ordering::Greater,
            CompareOp::Gte => matches!(ordering, Ordering::Greater | Ordering::Equal),
            CompareOp::Lt => ordering == Ordering::Less,
            CompareOp::Lte => matches!(ordering, Ordering::Less | Ordering::Equal),
        })
    }

    fn row_matches_index_range(
        &self,
        schema: &Schema,
        index: &IndexMeta,
        row: &Row,
        key_prefix: &[Value],
        lower: Option<(CompareOp, &Value)>,
        upper: Option<(CompareOp, &Value)>,
    ) -> Result<bool> {
        let key = self.project_index_key(schema, index, row)?;
        if !key.starts_with(key_prefix) {
            return Ok(false);
        }

        let range_value = key.get(key_prefix.len()).ok_or_else(|| {
            DbError::storage(format!(
                "index {} has no range column after prefix of length {}",
                index.name,
                key_prefix.len()
            ))
        })?;
        if let Some((op, value)) = lower {
            if !Self::compare_with_operator(range_value, op, value)? {
                return Ok(false);
            }
        }
        if let Some((op, value)) = upper {
            if !Self::compare_with_operator(range_value, op, value)? {
                return Ok(false);
            }
        }

        Ok(true)
    }

    fn validate_unique_indexes_for_row(
        &self,
        schema: &Schema,
        indexes: Option<&std::collections::BTreeMap<String, IndexMeta>>,
        existing_rows: &[(RowId, Row)],
        candidate_row: &Row,
    ) -> Result<()> {
        let Some(indexes) = indexes else {
            return Ok(());
        };

        for index in indexes.values().filter(|index| {
            index.unique
                && !schema
                    .primary_key_constraint
                    .as_ref()
                    .is_some_and(|primary_key| {
                        index
                            .name
                            .starts_with(&format!("sqlite_autoindex_{}_", schema.name))
                            && primary_key.columns == index.columns
                    })
        }) {
            if !self.row_matches_partial_index(schema, index, candidate_row)? {
                continue;
            }
            let candidate_key = self.project_index_key(schema, index, candidate_row)?;
            if !index.enforces_unique_key(&candidate_key) {
                continue;
            }

            for (_, existing_row) in existing_rows {
                if !self.row_matches_partial_index(schema, index, existing_row)? {
                    continue;
                }
                let existing_key = self.project_index_key(schema, index, existing_row)?;
                if existing_key == candidate_key {
                    return Err(DbError::storage(format!(
                        "unique index {} constraint failed",
                        index.name
                    )));
                }
            }
        }

        Ok(())
    }
}

impl PlanningStorageEngine for FileStorage {
    fn planning_context_snapshot(
        &self,
        transaction_id: Option<TransactionId>,
    ) -> Result<PlanningContext> {
        if let Some(transaction_id) = transaction_id {
            self.validate_transaction(transaction_id)?;
        }

        let writable = self.writable_view();
        let schemas = writable
            .tables
            .iter()
            .map(|(name, table)| (name.clone(), table.schema.clone()))
            .collect::<HashMap<_, _>>();
        let indexes = self
            .writable_view()
            .indexes
            .iter()
            .map(|(table, entries)| {
                (
                    table.clone(),
                    entries.values().cloned().collect::<Vec<IndexMeta>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        Ok(PlanningContext::new(schemas, indexes))
    }

    fn database_path(&self) -> Option<PathBuf> {
        self.path
            .as_ref()
            .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
    }

    fn journal_mode(&self) -> &'static str {
        "delete"
    }

    fn ignore_check_constraints(&self) -> bool {
        *self.ignore_check_constraints.borrow()
    }

    fn set_ignore_check_constraints(&self, enabled: bool) -> Result<()> {
        *self.ignore_check_constraints.borrow_mut() = enabled;
        Ok(())
    }

    fn case_sensitive_like(&self) -> bool {
        *self.case_sensitive_like.borrow()
    }

    fn set_case_sensitive_like(&self, enabled: bool) -> Result<()> {
        *self.case_sensitive_like.borrow_mut() = enabled;
        Ok(())
    }

    fn database_page_size(&self) -> u32 {
        self.pager
            .borrow()
            .as_ref()
            .map(|pager| pager.header().page_size)
            .unwrap_or(4096)
    }

    fn database_page_count(&self) -> Result<u32> {
        self.pager
            .borrow()
            .as_ref()
            .map_or(Ok(0), |pager| pager.page_count())
    }

    fn database_freelist_count(&self) -> Result<u32> {
        Ok(self
            .pager
            .borrow()
            .as_ref()
            .map(|pager| pager.header().freelist_count)
            .unwrap_or(0))
    }

    fn user_version(&self) -> Result<u32> {
        Ok(self.writable_view().user_version)
    }

    fn set_user_version(&self, version: u32) -> Result<()> {
        let active_txn = self.txn_state.borrow().active_txn;
        if let Some(transaction_id) = active_txn {
            self.with_pending_writable_mut(transaction_id, |database| {
                database.user_version = version;
                Ok(())
            })
        } else {
            let mut database = self.writable.borrow().clone();
            database.user_version = version;
            let path = self.path.clone().ok_or_else(|| {
                DbError::storage("sqlite3 FileStorage is not backed by a database file")
            })?;
            write_database(&path, &database)?;
            let pager = Pager::open(&path)?;
            let catalog = load_catalog(&pager)?;
            *self.catalog.borrow_mut() = catalog;
            *self.pager.borrow_mut() = Some(pager);
            *self.writable.borrow_mut() = database;
            Ok(())
        }
    }

    fn application_id(&self) -> Result<u32> {
        Ok(self.writable_view().application_id)
    }

    fn set_application_id(&self, application_id: u32) -> Result<()> {
        let active_txn = self.txn_state.borrow().active_txn;
        if let Some(transaction_id) = active_txn {
            self.with_pending_writable_mut(transaction_id, |database| {
                database.application_id = application_id;
                Ok(())
            })
        } else {
            let mut database = self.writable.borrow().clone();
            database.application_id = application_id;
            let path = self.path.clone().ok_or_else(|| {
                DbError::storage("sqlite3 FileStorage is not backed by a database file")
            })?;
            write_database(&path, &database)?;
            let pager = Pager::open(&path)?;
            let catalog = load_catalog(&pager)?;
            *self.catalog.borrow_mut() = catalog;
            *self.pager.borrow_mut() = Some(pager);
            *self.writable.borrow_mut() = database;
            Ok(())
        }
    }

    fn schema_version(&self) -> Result<u32> {
        Ok(self.writable_view().schema_version)
    }

    fn set_schema_version(&self, schema_version: u32) -> Result<()> {
        let active_txn = self.txn_state.borrow().active_txn;
        if let Some(transaction_id) = active_txn {
            self.with_pending_writable_mut(transaction_id, |database| {
                database.schema_version = schema_version;
                Ok(())
            })
        } else {
            let mut database = self.writable.borrow().clone();
            database.schema_version = schema_version;
            let path = self.path.clone().ok_or_else(|| {
                DbError::storage("sqlite3 FileStorage is not backed by a database file")
            })?;
            write_database(&path, &database)?;
            let pager = Pager::open(&path)?;
            let catalog = load_catalog(&pager)?;
            *self.catalog.borrow_mut() = catalog;
            *self.pager.borrow_mut() = Some(pager);
            *self.writable.borrow_mut() = database;
            Ok(())
        }
    }
}

impl CatalogStore for FileStorage {
    fn create_schema(&self, transaction_id: TransactionId, schema: Schema) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            schema.validate_constraints_metadata()?;
            if database.tables.contains_key(&schema.name) {
                return Err(DbError::storage(format!(
                    "table {} already exists",
                    schema.name
                )));
            }
            let schema_name = schema.name.clone();
            let has_autoincrement = schema
                .columns
                .iter()
                .any(|column| column.primary_key && column.autoincrement);
            database.tables.insert(
                schema_name.clone(),
                WritableTable {
                    schema,
                    rows: Vec::new(),
                },
            );
            if has_autoincrement {
                database.sqlite_sequence_exists = true;
                database.sqlite_sequence.insert(schema_name, 0);
            }
            Ok(())
        })
    }

    fn drop_schema(&self, transaction_id: TransactionId, _name: &str) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let name = _name;
            if database.tables.remove(name).is_none() {
                return Err(DbError::storage(format!("unknown table: {name}")));
            }
            database.indexes.remove(name);
            database.sqlite_sequence.remove(name);
            Ok(())
        })
    }

    fn replace_schema(&self, transaction_id: TransactionId, _schema: Schema) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        Err(self.unsupported("ALTER TABLE"))
    }

    fn rename_schema(
        &self,
        transaction_id: TransactionId,
        old_name: &str,
        new_name: &str,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            if database.tables.contains_key(new_name) {
                return Err(DbError::storage(format!(
                    "table already exists: {new_name}"
                )));
            }

            let mut table = database
                .tables
                .remove(old_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {old_name}")))?;
            table.schema.name = new_name.to_string();
            for (name, other_table) in &mut database.tables {
                if name != new_name {
                    other_table
                        .schema
                        .rename_foreign_key_ref_table(old_name, new_name);
                }
            }
            table
                .schema
                .rename_foreign_key_ref_table(old_name, new_name);
            database.tables.insert(new_name.to_string(), table);

            if let Some(indexes) = database.indexes.remove(old_name) {
                database.indexes.insert(new_name.to_string(), indexes);
            }
            if let Some(seq) = database.sqlite_sequence.remove(old_name) {
                database.sqlite_sequence.insert(new_name.to_string(), seq);
            }
            Ok(())
        })
    }

    fn add_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        column: ColumnDef,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get_mut(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            if table
                .schema
                .columns
                .iter()
                .any(|entry| entry.name == column.name)
            {
                return Err(DbError::storage(format!(
                    "column already exists on table {schema_name}: {}",
                    column.name
                )));
            }

            let default_value = column
                .default_value
                .as_ref()
                .map_or(Ok(Value::Null), |default| default.evaluate())?;
            let mut updated_schema = table.schema.clone();
            updated_schema.columns.push(column);
            updated_schema.validate_constraints_metadata()?;

            let mut updated_rows = Vec::with_capacity(table.rows.len());
            for (row_id, row) in &table.rows {
                let mut candidate = row.clone();
                candidate.push(default_value.clone());
                updated_schema.validate_row_values(&candidate)?;
                updated_schema.validate_check_constraints(&candidate)?;
                updated_rows.push((*row_id, candidate));
            }

            table.schema = updated_schema;
            table.rows = updated_rows;
            Ok(())
        })
    }

    fn rename_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get_mut(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            if !table
                .schema
                .columns
                .iter()
                .any(|entry| entry.name == old_name)
            {
                return Err(DbError::storage(format!(
                    "unknown column {old_name} on table {schema_name}"
                )));
            }
            if table
                .schema
                .columns
                .iter()
                .any(|entry| entry.name == new_name)
            {
                return Err(DbError::storage(format!(
                    "column already exists on table {schema_name}: {new_name}"
                )));
            }

            table.schema.rename_column_references(old_name, new_name);
            table
                .schema
                .rename_foreign_key_ref_column(schema_name, old_name, new_name);
            table.schema.validate_constraints_metadata()?;

            if let Some(indexes) = database.indexes.get_mut(schema_name) {
                for index in indexes.values_mut() {
                    for column in &mut index.columns {
                        if column == old_name {
                            *column = new_name.to_string();
                        }
                    }
                }
            }
            for (name, other_table) in &mut database.tables {
                if name != schema_name {
                    other_table.schema.rename_foreign_key_ref_column(
                        schema_name,
                        old_name,
                        new_name,
                    );
                }
            }
            Ok(())
        })
    }

    fn drop_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        old_name: &str,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get_mut(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            let (updated_schema, removed_index) = table.schema.drop_column(old_name)?;
            table.schema = updated_schema;
            for (_, row) in &mut table.rows {
                row.remove(removed_index);
            }

            if let Some(indexes) = database.indexes.get_mut(schema_name) {
                indexes.retain(|_, index| !index.columns.iter().any(|column| column == old_name));
            }
            Ok(())
        })
    }

    fn get_schema(&self, transaction_id: TransactionId, name: &str) -> Result<Option<Schema>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .writable_view()
            .tables
            .get(name)
            .map(|table| table.schema.clone()))
    }

    fn list_schemas(&self, transaction_id: TransactionId) -> Result<Vec<Schema>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .writable_view()
            .tables
            .values()
            .map(|table| table.schema.clone())
            .collect())
    }
}

impl TableStore for FileStorage {
    fn insert_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row: Row,
    ) -> Result<RowId> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get_mut(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;

            let mut row = row;
            let row_id_column_index = Self::integer_primary_key_column_index(&table.schema);
            let row_id = if let Some(index) = row_id_column_index {
                match row.get(index) {
                    Some(Value::Integer(value)) => RowId(u64::try_from(*value).map_err(|_| {
                        DbError::storage("sqlite rowid must be a non-negative INTEGER")
                    })?),
                    Some(Value::Null) => {
                        let next = Self::next_row_id_for_insert(
                            table,
                            database.sqlite_sequence.get(schema_name).copied(),
                        );
                        let row_id = RowId(next);
                        row[index] =
                            Value::Integer(i64::try_from(row_id.0).map_err(|_| {
                                DbError::storage("sqlite rowid does not fit in i64")
                            })?);
                        row_id
                    }
                    Some(value) => {
                        return Err(DbError::storage(format!(
                            "sqlite rowid column must be INTEGER, got {}",
                            value.type_name()
                        )));
                    }
                    None => return Err(DbError::storage("sqlite row is missing rowid column")),
                }
            } else {
                let next = table
                    .rows
                    .last()
                    .map(|(row_id, _)| row_id.0.saturating_add(1))
                    .unwrap_or(1);
                RowId(next)
            };

            table.schema.validate_row_values(&row)?;
            if !*self.ignore_check_constraints.borrow() {
                table.schema.validate_check_constraints_with_like_mode(
                    &row,
                    *self.case_sensitive_like.borrow(),
                )?;
            }
            let existing_rows = table.rows.iter().map(|(_, row)| row).collect::<Vec<_>>();
            table
                .schema
                .validate_primary_key_uniqueness(&row, &existing_rows)?;
            self.validate_unique_indexes_for_row(
                &table.schema,
                database.indexes.get(schema_name),
                &table.rows,
                &row,
            )?;

            table.rows.push((row_id, row));
            table.rows.sort_by_key(|(row_id, _)| row_id.0);
            if table
                .schema
                .columns
                .iter()
                .any(|column| column.primary_key && column.autoincrement)
            {
                let entry = database
                    .sqlite_sequence
                    .entry(schema_name.to_string())
                    .or_insert(0);
                *entry = (*entry).max(row_id.0);
            }
            Ok(row_id)
        })
    }

    fn get_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
    ) -> Result<Option<Row>> {
        self.validate_transaction(transaction_id)?;
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        if schema.without_rowid {
            return Ok(self
                .without_rowid_scan_rows_internal(transaction_id, schema_name, &schema)?
                .into_iter()
                .find(|(candidate, _)| *candidate == row_id)
                .map(|(_, row)| row));
        }
        if self.txn_state.borrow().pending_writable.is_some() {
            return Ok(self
                .writable_view()
                .tables
                .get(schema_name)
                .and_then(|table| {
                    table
                        .rows
                        .iter()
                        .find(|(candidate, _)| *candidate == row_id)
                        .map(|(_, row)| row.clone())
                }));
        }
        let (_, root_page) = self.require_schema_and_root_page(schema_name)?;
        let pager = self.pager.borrow();
        let pager = pager.as_ref().ok_or_else(|| {
            DbError::storage("sqlite3 FileStorage is not backed by a database file")
        })?;
        get_table_row(pager, root_page, &schema, row_id)
    }

    fn scan_rows(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<(RowId, Row)>> {
        self.validate_transaction(transaction_id)?;
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        if schema.without_rowid {
            return self.without_rowid_scan_rows_internal(transaction_id, schema_name, &schema);
        }
        if self.txn_state.borrow().pending_writable.is_some() {
            return Ok(self
                .writable_view()
                .tables
                .get(schema_name)
                .map(|table| table.rows.clone())
                .unwrap_or_default());
        }
        let (_, root_page) = self.require_schema_and_root_page(schema_name)?;
        let pager = self.pager.borrow();
        let pager = pager.as_ref().ok_or_else(|| {
            DbError::storage("sqlite3 FileStorage is not backed by a database file")
        })?;
        scan_table_rows(pager, root_page, &schema)
    }

    fn delete_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get_mut(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            if table.schema.without_rowid {
                let Some(position) = Self::without_rowid_row_position(table, row_id)? else {
                    return Err(DbError::storage(format!(
                        "unknown rowid {} on table {schema_name}",
                        row_id.0
                    )));
                };
                table.rows.remove(position);
                return Ok(());
            }
            let original_len = table.rows.len();
            table.rows.retain(|(candidate, _)| *candidate != row_id);
            if table.rows.len() == original_len {
                return Err(DbError::storage(format!(
                    "unknown rowid {} on table {schema_name}",
                    row_id.0
                )));
            }
            Ok(())
        })
    }

    fn update_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
        row: Row,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get_mut(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            table.schema.validate_row_values(&row)?;
            if !*self.ignore_check_constraints.borrow() {
                table.schema.validate_check_constraints_with_like_mode(
                    &row,
                    *self.case_sensitive_like.borrow(),
                )?;
            }

            let position = if table.schema.without_rowid {
                Self::without_rowid_row_position(table, row_id)?.ok_or_else(|| {
                    DbError::storage(format!("unknown rowid {} on table {schema_name}", row_id.0))
                })?
            } else {
                table
                    .rows
                    .iter()
                    .position(|(candidate, _)| *candidate == row_id)
                    .ok_or_else(|| {
                        DbError::storage(format!(
                            "unknown rowid {} on table {schema_name}",
                            row_id.0
                        ))
                    })?
            };

            let new_row_id = table
                .schema
                .columns
                .iter()
                .position(|column| {
                    column.primary_key
                        && matches!(
                            column.column_type,
                            crate::common::types::ColumnType::Integer
                        )
                })
                .map(|index| match row.get(index) {
                    Some(Value::Integer(value)) => u64::try_from(*value).map(RowId).map_err(|_| {
                        DbError::storage("sqlite rowid must be a non-negative INTEGER")
                    }),
                    Some(value) => Err(DbError::storage(format!(
                        "sqlite rowid column must be INTEGER, got {}",
                        value.type_name()
                    ))),
                    None => Err(DbError::storage("sqlite row is missing rowid column")),
                })
                .transpose()?
                .unwrap_or(row_id);

            table.rows[position] = (new_row_id, row);
            if !table.schema.without_rowid {
                table
                    .rows
                    .sort_by_key(|(candidate_row_id, _)| candidate_row_id.0);
            }
            Ok(())
        })
    }
}

impl IndexStore for FileStorage {
    fn create_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index: IndexMeta,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            if index.columns.is_empty() {
                return Err(DbError::storage("index must define at least one column"));
            }
            for column in &index.columns {
                validate_index_term(&table.schema, column)?;
            }
            if let Some(predicate_sql) = index.predicate.as_deref() {
                let predicate = parse_check_constraint_expression(predicate_sql)?;
                table.schema.validate_check_expr_metadata(&predicate)?;
            }
            if index.unique {
                let mut seen = std::collections::BTreeSet::new();
                for (_, row) in &table.rows {
                    if !self.row_matches_partial_index(&table.schema, &index, row)? {
                        continue;
                    }
                    let key = self.project_index_key(&table.schema, &index, row)?;
                    if !index.enforces_unique_key(&key) {
                        continue;
                    }
                    if !seen.insert(key) {
                        return Err(DbError::storage(format!(
                            "unique index {} constraint failed",
                            index.name
                        )));
                    }
                }
            }

            let table_indexes = database.indexes.entry(schema_name.to_string()).or_default();
            if table_indexes.contains_key(&index.name) {
                return Err(DbError::storage(format!(
                    "index already exists on table {schema_name}: {}",
                    index.name
                )));
            }
            table_indexes.insert(index.name.clone(), index);
            Ok(())
        })
    }

    fn drop_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let indexes = database.indexes.get_mut(schema_name).ok_or_else(|| {
                DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
            })?;
            if indexes.remove(index_name).is_none() {
                return Err(DbError::storage(format!(
                    "unknown index {index_name} on table {schema_name}"
                )));
            }
            if indexes.is_empty() {
                database.indexes.remove(schema_name);
            }
            Ok(())
        })
    }

    fn get_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<Option<IndexMeta>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .writable_view()
            .indexes
            .get(schema_name)
            .and_then(|indexes| indexes.get(index_name))
            .cloned())
    }

    fn list_indexes(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<IndexMeta>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .writable_view()
            .indexes
            .get(schema_name)
            .map(|indexes| {
                indexes
                    .values()
                    .filter(|index| index.is_usable())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    fn list_all_indexes(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<IndexMeta>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .writable_view()
            .indexes
            .get(schema_name)
            .map(|indexes| indexes.values().cloned().collect())
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
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        let (index, root_page) = self.require_index_and_root_page(schema_name, index_name)?;
        if Self::is_without_rowid_primary_key_index(&schema, &index, schema_name) {
            return self.without_rowid_lookup_row_ids(transaction_id, schema_name, key, true);
        }
        if schema.without_rowid {
            let rows =
                self.without_rowid_scan_rows_internal(transaction_id, schema_name, &schema)?;
            let synthetic_ids = rows
                .into_iter()
                .filter_map(|(_row_id, row)| {
                    let index_key = self.project_index_key(&schema, &index, &row).ok()?;
                    if index_key != key {
                        return None;
                    }
                    Self::without_rowid_synthetic_row_id(&schema, &row).ok()
                })
                .collect::<Vec<_>>();
            return Ok(synthetic_ids);
        }
        if key.len() != index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} expected {} key values but got {}",
                index.name,
                index.columns.len(),
                key.len()
            )));
        }
        let pager = self.pager.borrow();
        let pager = pager.as_ref().ok_or_else(|| {
            DbError::storage("sqlite3 FileStorage is not backed by a database file")
        })?;
        lookup_index_entries(pager, root_page, &index, key)
    }

    fn scan_index_prefix(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key_prefix: &[Value],
    ) -> Result<Vec<RowId>> {
        self.validate_transaction(transaction_id)?;
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        let (index, root_page) = self.require_index_and_root_page(schema_name, index_name)?;
        if Self::is_without_rowid_primary_key_index(&schema, &index, schema_name) {
            return self.without_rowid_lookup_row_ids(
                transaction_id,
                schema_name,
                key_prefix,
                false,
            );
        }
        if schema.without_rowid {
            let rows =
                self.without_rowid_scan_rows_internal(transaction_id, schema_name, &schema)?;
            let synthetic_ids = rows
                .into_iter()
                .filter_map(|(_row_id, row)| {
                    let index_key = self.project_index_key(&schema, &index, &row).ok()?;
                    if !index_key.starts_with(key_prefix) {
                        return None;
                    }
                    Self::without_rowid_synthetic_row_id(&schema, &row).ok()
                })
                .collect::<Vec<_>>();
            return Ok(synthetic_ids);
        }
        if key_prefix.len() > index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} expected at most {} key values but got {}",
                index.name,
                index.columns.len(),
                key_prefix.len()
            )));
        }
        let pager = self.pager.borrow();
        let pager = pager.as_ref().ok_or_else(|| {
            DbError::storage("sqlite3 FileStorage is not backed by a database file")
        })?;
        lookup_index_entries(pager, root_page, &index, key_prefix)
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
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        if let Ok((index, _root_page)) = self.require_index_and_root_page(schema_name, index_name)
            && Self::is_without_rowid_primary_key_index(&schema, &index, schema_name)
        {
            if key_prefix.len() >= index.columns.len() {
                return Err(DbError::storage(format!(
                    "index {} has no range column after prefix of length {}",
                    index.name,
                    key_prefix.len()
                )));
            }

            let rows =
                self.without_rowid_scan_rows_internal(transaction_id, schema_name, &schema)?;
            let mut row_ids = BTreeSet::new();
            for (row_id, row) in rows {
                if self.row_matches_index_range(&schema, &index, &row, key_prefix, lower, upper)? {
                    row_ids.insert(row_id);
                }
            }
            return Ok(row_ids.into_iter().collect());
        }
        if schema.without_rowid {
            let database = self.writable_view();
            let index = database
                .indexes
                .get(schema_name)
                .and_then(|indexes| indexes.get(index_name))
                .ok_or_else(|| {
                    DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
                })?;

            if key_prefix.len() >= index.columns.len() {
                return Err(DbError::storage(format!(
                    "index {} has no range column after prefix of length {}",
                    index.name,
                    key_prefix.len()
                )));
            }

            let rows =
                self.without_rowid_scan_rows_internal(transaction_id, schema_name, &schema)?;
            let mut row_ids = BTreeSet::new();
            for (row_id, row) in rows {
                if self.row_matches_index_range(&schema, index, &row, key_prefix, lower, upper)? {
                    row_ids.insert(row_id);
                }
            }
            return Ok(row_ids.into_iter().collect());
        }
        let database = self.writable_view();
        let table = database
            .tables
            .get(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        let index = database
            .indexes
            .get(schema_name)
            .and_then(|indexes| indexes.get(index_name))
            .ok_or_else(|| {
                DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
            })?;

        if key_prefix.len() >= index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} has no range column after prefix of length {}",
                index.name,
                key_prefix.len()
            )));
        }

        let mut row_ids = BTreeSet::new();
        for (row_id, row) in &table.rows {
            if self.row_matches_index_range(&table.schema, index, row, key_prefix, lower, upper)? {
                row_ids.insert(*row_id);
            }
        }

        Ok(row_ids.into_iter().collect())
    }
}

impl TransactionManager for FileStorage {
    fn begin(&self) -> Result<TransactionId> {
        let mut txn_state = self.txn_state.borrow_mut();
        if let Some(active) = txn_state.active_txn {
            return Err(DbError::txn(format!(
                "transaction {} is already active",
                active.0
            )));
        }

        let transaction_id = TransactionId(txn_state.next_txn_id);
        txn_state.next_txn_id += 1;
        txn_state.active_txn = Some(transaction_id);
        Ok(transaction_id)
    }

    fn begin_with_isolation(&self, _isolation_level: IsolationLevel) -> Result<TransactionId> {
        self.begin()
    }

    fn commit(&self, transaction_id: TransactionId) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let pending = {
            let mut txn_state = self.txn_state.borrow_mut();
            let pending = txn_state.pending_writable.take();
            txn_state.active_txn = None;
            pending
        };
        if let Some(database) = pending {
            let path = self.path.clone().ok_or_else(|| {
                DbError::storage("sqlite3 FileStorage is not backed by a database file")
            })?;
            write_database(&path, &database)?;
            let pager = Pager::open(&path)?;
            let catalog = load_catalog(&pager)?;
            *self.catalog.borrow_mut() = catalog;
            *self.pager.borrow_mut() = Some(pager);
            *self.writable.borrow_mut() = database;
        }
        Ok(())
    }

    fn rollback(&self, transaction_id: TransactionId) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let mut txn_state = self.txn_state.borrow_mut();
        txn_state.pending_writable = None;
        txn_state.active_txn = None;
        Ok(())
    }
}
