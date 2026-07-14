use std::cell::Cell;
use std::path::Path;

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Row, Schema};
use crate::engine::{PlanningStorageEngine, TransactionId};
use crate::sql::ast::Statement;
use crate::sql::executor::Executor;
use crate::sql::optimizer::Optimizer;
use crate::sql::parse_sql;
use crate::sql::planner::Planner;
use crate::storage::memory::MemoryStorage;
use crate::storage::v1::FileStorage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatementBatchKind {
    Query,
    Execute,
}

#[derive(Debug)]
pub struct Database<S = MemoryStorage> {
    storage: S,
    planner: Planner,
    optimizer: Optimizer,
    current_txn: Cell<Option<TransactionId>>,
    savepoint_transaction: Cell<bool>,
    savepoint_stack: std::cell::RefCell<Vec<String>>,
    last_insert_rowid: Cell<i64>,
    changes: Cell<i64>,
    total_changes: Cell<i64>,
    temp_database_used: Cell<bool>,
    deferred_foreign_keys_pending: Cell<bool>,
    defer_foreign_keys: Cell<bool>,
    foreign_keys: Cell<bool>,
    read_uncommitted: Cell<bool>,
    query_only: Cell<bool>,
    count_changes: Cell<bool>,
    recursive_triggers: Cell<bool>,
    trusted_schema: Cell<bool>,
    threads: Cell<u32>,
    synchronous: Cell<i64>,
    temp_synchronous: Cell<i64>,
    temp_store: Cell<i64>,
    journal_mode: std::cell::RefCell<String>,
    temp_journal_mode: std::cell::RefCell<String>,
    locking_mode: std::cell::RefCell<String>,
    temp_locking_mode: std::cell::RefCell<String>,
    cache_size: Cell<i64>,
    temp_cache_size: Cell<i64>,
    cache_spill: Cell<i64>,
    busy_timeout: Cell<i64>,
    secure_delete: Cell<i64>,
    temp_secure_delete: Cell<i64>,
    wal_autocheckpoint: Cell<i64>,
    auto_vacuum: Cell<i64>,
    max_page_count: Cell<i64>,
    temp_user_version: Cell<u32>,
    temp_application_id: Cell<u32>,
    temp_schema_version: Cell<u32>,
    mmap_size: Cell<i64>,
    analysis_limit: Cell<u32>,
    journal_size_limit: Cell<i64>,
    soft_heap_limit: Cell<i64>,
    automatic_index: Cell<bool>,
    cell_size_check: Cell<bool>,
    full_column_names: Cell<bool>,
    short_column_names: Cell<bool>,
    fullfsync: Cell<bool>,
    checkpoint_fullfsync: Cell<bool>,
    empty_result_callbacks: Cell<bool>,
    reverse_unordered_selects: Cell<bool>,
    temp_page_size: Cell<u32>,
}

impl<S: PlanningStorageEngine> Database<S> {
    #[must_use]
    pub fn with_storage(storage: S) -> Self {
        let journal_mode = storage.journal_mode().to_string();
        let temp_page_size = storage.database_page_size();
        Self {
            storage,
            planner: Planner::new(),
            optimizer: Optimizer::new(),
            current_txn: Cell::new(None),
            savepoint_transaction: Cell::new(false),
            savepoint_stack: std::cell::RefCell::new(Vec::new()),
            last_insert_rowid: Cell::new(0),
            changes: Cell::new(0),
            total_changes: Cell::new(0),
            temp_database_used: Cell::new(false),
            deferred_foreign_keys_pending: Cell::new(false),
            defer_foreign_keys: Cell::new(false),
            foreign_keys: Cell::new(false),
            read_uncommitted: Cell::new(false),
            query_only: Cell::new(false),
            count_changes: Cell::new(false),
            recursive_triggers: Cell::new(false),
            trusted_schema: Cell::new(false),
            threads: Cell::new(0),
            synchronous: Cell::new(2),
            temp_synchronous: Cell::new(0),
            temp_store: Cell::new(0),
            journal_mode: std::cell::RefCell::new(journal_mode),
            temp_journal_mode: std::cell::RefCell::new("delete".to_string()),
            locking_mode: std::cell::RefCell::new("normal".to_string()),
            temp_locking_mode: std::cell::RefCell::new("exclusive".to_string()),
            cache_size: Cell::new(2000),
            temp_cache_size: Cell::new(0),
            cache_spill: Cell::new(2000),
            busy_timeout: Cell::new(0),
            secure_delete: Cell::new(2),
            temp_secure_delete: Cell::new(2),
            wal_autocheckpoint: Cell::new(1000),
            auto_vacuum: Cell::new(0),
            max_page_count: Cell::new(1_073_741_823),
            temp_user_version: Cell::new(0),
            temp_application_id: Cell::new(0),
            temp_schema_version: Cell::new(0),
            mmap_size: Cell::new(0),
            analysis_limit: Cell::new(0),
            journal_size_limit: Cell::new(-1),
            soft_heap_limit: Cell::new(0),
            automatic_index: Cell::new(true),
            cell_size_check: Cell::new(false),
            full_column_names: Cell::new(false),
            short_column_names: Cell::new(true),
            fullfsync: Cell::new(false),
            checkpoint_fullfsync: Cell::new(true),
            empty_result_callbacks: Cell::new(false),
            reverse_unordered_selects: Cell::new(false),
            temp_page_size: Cell::new(temp_page_size),
        }
    }

    #[must_use]
    pub fn storage(&self) -> &S {
        &self.storage
    }

    pub fn execute(&self, sql: &str) -> Result<()> {
        let statements = parse_sql(sql)?;
        self.execute_parsed(&statements)
    }

    pub fn query(&self, sql: &str) -> Result<Vec<Row>> {
        let statements = parse_sql(sql)?;
        self.query_parsed(&statements)
    }

    pub fn list_schemas(&self) -> Result<Vec<Schema>> {
        self.with_metadata_transaction(|transaction_id| self.storage.list_schemas(transaction_id))
    }

    pub fn list_indexes(&self, table: &str) -> Result<Vec<IndexMeta>> {
        self.with_metadata_transaction(|transaction_id| {
            self.storage.list_indexes(transaction_id, table)
        })
    }

    pub(crate) fn execute_parsed(&self, statements: &[Statement]) -> Result<()> {
        self.validate_execute_batch(statements)?;

        let executor = Executor::new(
            &self.storage,
            &self.current_txn,
            &self.savepoint_transaction,
            &self.savepoint_stack,
            &self.last_insert_rowid,
            &self.changes,
            &self.total_changes,
            &self.temp_database_used,
            &self.deferred_foreign_keys_pending,
            &self.defer_foreign_keys,
            &self.foreign_keys,
            &self.read_uncommitted,
            &self.query_only,
            &self.count_changes,
            &self.recursive_triggers,
            &self.trusted_schema,
            &self.threads,
            &self.synchronous,
            &self.temp_synchronous,
            &self.temp_store,
            &self.journal_mode,
            &self.temp_journal_mode,
            &self.locking_mode,
            &self.temp_locking_mode,
            &self.cache_size,
            &self.temp_cache_size,
            &self.cache_spill,
            &self.busy_timeout,
            &self.secure_delete,
            &self.temp_secure_delete,
            &self.wal_autocheckpoint,
            &self.auto_vacuum,
            &self.max_page_count,
            &self.temp_user_version,
            &self.temp_application_id,
            &self.temp_schema_version,
            &self.mmap_size,
            &self.analysis_limit,
            &self.journal_size_limit,
            &self.soft_heap_limit,
            &self.automatic_index,
            &self.cell_size_check,
            &self.full_column_names,
            &self.short_column_names,
            &self.fullfsync,
            &self.checkpoint_fullfsync,
            &self.empty_result_callbacks,
            &self.reverse_unordered_selects,
            &self.temp_page_size,
        );
        for statement in statements {
            let plan = self.plan_statement(statement)?;
            executor.execute(plan)?;
        }

        Ok(())
    }

    pub(crate) fn query_parsed(&self, statements: &[Statement]) -> Result<Vec<Row>> {
        self.validate_query_batch(statements)?;

        let executor = Executor::new(
            &self.storage,
            &self.current_txn,
            &self.savepoint_transaction,
            &self.savepoint_stack,
            &self.last_insert_rowid,
            &self.changes,
            &self.total_changes,
            &self.temp_database_used,
            &self.deferred_foreign_keys_pending,
            &self.defer_foreign_keys,
            &self.foreign_keys,
            &self.read_uncommitted,
            &self.query_only,
            &self.count_changes,
            &self.recursive_triggers,
            &self.trusted_schema,
            &self.threads,
            &self.synchronous,
            &self.temp_synchronous,
            &self.temp_store,
            &self.journal_mode,
            &self.temp_journal_mode,
            &self.locking_mode,
            &self.temp_locking_mode,
            &self.cache_size,
            &self.temp_cache_size,
            &self.cache_spill,
            &self.busy_timeout,
            &self.secure_delete,
            &self.temp_secure_delete,
            &self.wal_autocheckpoint,
            &self.auto_vacuum,
            &self.max_page_count,
            &self.temp_user_version,
            &self.temp_application_id,
            &self.temp_schema_version,
            &self.mmap_size,
            &self.analysis_limit,
            &self.journal_size_limit,
            &self.soft_heap_limit,
            &self.automatic_index,
            &self.cell_size_check,
            &self.full_column_names,
            &self.short_column_names,
            &self.fullfsync,
            &self.checkpoint_fullfsync,
            &self.empty_result_callbacks,
            &self.reverse_unordered_selects,
            &self.temp_page_size,
        );
        let mut last_rows = Vec::new();

        for statement in statements {
            let plan = self.plan_statement(statement)?;
            last_rows = executor.execute(plan)?;
        }

        Ok(last_rows)
    }

    pub(crate) fn classify_batch(statements: &[Statement]) -> StatementBatchKind {
        if statements.iter().all(Self::is_query_statement) {
            StatementBatchKind::Query
        } else {
            StatementBatchKind::Execute
        }
    }

    fn is_query_statement(statement: &Statement) -> bool {
        matches!(
            statement,
            Statement::Values(_)
                | Statement::ValuesWith { .. }
                | Statement::Select(_)
                | Statement::InsertReturning { .. }
                | Statement::InsertUpsertReturning { .. }
                | Statement::InsertManyReturning { .. }
                | Statement::InsertManyUpsertReturning { .. }
                | Statement::InsertDoNothingReturning { .. }
                | Statement::InsertManyDoNothingReturning { .. }
                | Statement::InsertExprReturning { .. }
                | Statement::InsertExprUpsertReturning { .. }
                | Statement::InsertManyExprReturning { .. }
                | Statement::InsertManyExprUpsertReturning { .. }
                | Statement::InsertExprDoNothingReturning { .. }
                | Statement::InsertManyExprDoNothingReturning { .. }
                | Statement::InsertSelectReturning { .. }
                | Statement::InsertSelectUpsertReturning { .. }
                | Statement::InsertSelectDoNothingReturning { .. }
                | Statement::DeleteReturning { .. }
                | Statement::DeleteReturningLimited { .. }
                | Statement::UpdateReturning { .. }
                | Statement::UpdateReturningLimited { .. }
                | Statement::ExplainQueryPlan(_)
                | Statement::PragmaTableInfo { .. }
                | Statement::PragmaTableXInfo { .. }
                | Statement::PragmaTableList { .. }
                | Statement::PragmaIndexList { .. }
                | Statement::PragmaIndexInfo { .. }
                | Statement::PragmaIndexXInfo { .. }
                | Statement::PragmaForeignKeyList { .. }
                | Statement::PragmaForeignKeyCheck { .. }
                | Statement::PragmaDatabaseList
                | Statement::PragmaPageSize { .. }
                | Statement::PragmaPageCount { .. }
                | Statement::PragmaMaxPageCount
                | Statement::SetPragmaMaxPageCount { .. }
                | Statement::PragmaFreelistCount { .. }
                | Statement::PragmaUserVersion { .. }
                | Statement::PragmaApplicationId { .. }
                | Statement::PragmaSchemaVersion { .. }
                | Statement::PragmaForeignKeys
                | Statement::PragmaDeferForeignKeys
                | Statement::PragmaReadUncommitted
                | Statement::PragmaQueryOnly
                | Statement::PragmaCountChanges
                | Statement::PragmaRecursiveTriggers
                | Statement::PragmaTrustedSchema
                | Statement::PragmaIgnoreCheckConstraints
                | Statement::PragmaEncoding
                | Statement::PragmaCollationList
                | Statement::PragmaDataVersion
                | Statement::PragmaQuickCheck
                | Statement::PragmaIntegrityCheck
                | Statement::PragmaFunctionList
                | Statement::PragmaCompileOptions
                | Statement::PragmaPragmaList
                | Statement::PragmaModuleList
                | Statement::PragmaStats
                | Statement::PragmaJournalMode { .. }
                | Statement::SetPragmaJournalMode { .. }
                | Statement::PragmaSynchronous { .. }
                | Statement::PragmaCacheSize { .. }
                | Statement::PragmaCacheSpill
                | Statement::SetPragmaCacheSpill { .. }
                | Statement::PragmaTempStore
                | Statement::PragmaLockingMode { .. }
                | Statement::SetPragmaLockingMode { .. }
                | Statement::PragmaSecureDelete { .. }
                | Statement::SetPragmaSecureDelete { .. }
                | Statement::PragmaWalAutocheckpoint
                | Statement::SetPragmaWalAutocheckpoint { .. }
                | Statement::PragmaWalCheckpoint
                | Statement::PragmaMmapSize
                | Statement::PragmaAutoVacuum
                | Statement::PragmaBusyTimeout
                | Statement::SetPragmaBusyTimeout { .. }
                | Statement::PragmaAnalysisLimit
                | Statement::SetPragmaAnalysisLimit { .. }
                | Statement::PragmaJournalSizeLimit
                | Statement::SetPragmaJournalSizeLimit { .. }
                | Statement::PragmaSoftHeapLimit
                | Statement::SetPragmaSoftHeapLimit { .. }
                | Statement::PragmaHardHeapLimit
                | Statement::SetPragmaHardHeapLimit { .. }
                | Statement::PragmaThreads
                | Statement::SetPragmaThreads { .. }
                | Statement::PragmaAutomaticIndex
                | Statement::PragmaCellSizeCheck
                | Statement::PragmaFullColumnNames
                | Statement::PragmaShortColumnNames
                | Statement::PragmaFullFsync
                | Statement::PragmaCheckpointFullFsync
                | Statement::PragmaEmptyResultCallbacks
                | Statement::PragmaCaseSensitiveLike
                | Statement::PragmaReverseUnorderedSelects
        ) || matches!(statement, Statement::WithDml { statement, .. } if Self::is_query_statement(statement))
    }

    fn is_returning_statement(statement: &Statement) -> bool {
        matches!(
            statement,
            Statement::InsertReturning { .. }
                | Statement::InsertUpsertReturning { .. }
                | Statement::InsertManyReturning { .. }
                | Statement::InsertManyUpsertReturning { .. }
                | Statement::InsertDoNothingReturning { .. }
                | Statement::InsertManyDoNothingReturning { .. }
                | Statement::InsertExprReturning { .. }
                | Statement::InsertExprUpsertReturning { .. }
                | Statement::InsertManyExprReturning { .. }
                | Statement::InsertManyExprUpsertReturning { .. }
                | Statement::InsertExprDoNothingReturning { .. }
                | Statement::InsertManyExprDoNothingReturning { .. }
                | Statement::InsertSelectReturning { .. }
                | Statement::InsertSelectUpsertReturning { .. }
                | Statement::InsertSelectDoNothingReturning { .. }
                | Statement::DeleteReturning { .. }
                | Statement::DeleteReturningLimited { .. }
                | Statement::UpdateReturning { .. }
                | Statement::UpdateReturningLimited { .. }
        ) || matches!(statement, Statement::WithDml { statement, .. } if Self::is_returning_statement(statement))
    }

    fn validate_execute_batch(&self, statements: &[Statement]) -> Result<()> {
        if statements.iter().any(|statement| {
            matches!(
                statement,
                Statement::Select(_) | Statement::Values(_) | Statement::ValuesWith { .. }
            )
        }) {
            return Err(DbError::sql("SELECT statements must use Database::query"));
        }

        if statements.iter().any(Self::is_returning_statement) {
            return Err(DbError::sql(
                "RETURNING statements must use Database::query",
            ));
        }

        if statements
            .iter()
            .any(|statement| matches!(statement, Statement::ExplainQueryPlan(_)))
        {
            return Err(DbError::sql("EXPLAIN statements must use Database::query"));
        }

        if statements.iter().any(|statement| {
            matches!(
                statement,
                Statement::PragmaTableInfo { .. }
                    | Statement::PragmaTableXInfo { .. }
                    | Statement::PragmaTableList { .. }
                    | Statement::PragmaIndexList { .. }
                    | Statement::PragmaIndexInfo { .. }
                    | Statement::PragmaIndexXInfo { .. }
                    | Statement::PragmaForeignKeyList { .. }
                    | Statement::PragmaForeignKeyCheck { .. }
                    | Statement::PragmaDatabaseList
                    | Statement::PragmaPageSize { .. }
                    | Statement::PragmaPageCount { .. }
                    | Statement::PragmaMaxPageCount
                    | Statement::SetPragmaMaxPageCount { .. }
                    | Statement::PragmaFreelistCount { .. }
                    | Statement::PragmaUserVersion { .. }
                    | Statement::PragmaApplicationId { .. }
                    | Statement::PragmaSchemaVersion { .. }
                    | Statement::PragmaForeignKeys
                    | Statement::PragmaDeferForeignKeys
                    | Statement::PragmaReadUncommitted
                    | Statement::PragmaQueryOnly
                    | Statement::PragmaCountChanges
                    | Statement::PragmaRecursiveTriggers
                    | Statement::PragmaTrustedSchema
                    | Statement::PragmaIgnoreCheckConstraints
                    | Statement::PragmaEncoding
                    | Statement::PragmaCollationList
                    | Statement::PragmaDataVersion
                    | Statement::PragmaQuickCheck
                    | Statement::PragmaIntegrityCheck
                    | Statement::PragmaFunctionList
                    | Statement::PragmaCompileOptions
                    | Statement::PragmaPragmaList
                    | Statement::PragmaModuleList
                    | Statement::PragmaStats
                    | Statement::PragmaJournalMode { .. }
                    | Statement::SetPragmaJournalMode { .. }
                    | Statement::PragmaSynchronous { .. }
                    | Statement::PragmaCacheSize { .. }
                    | Statement::PragmaCacheSpill
                    | Statement::PragmaTempStore
                    | Statement::PragmaLockingMode { .. }
                    | Statement::SetPragmaLockingMode { .. }
                    | Statement::PragmaSecureDelete { .. }
                    | Statement::SetPragmaSecureDelete { .. }
                    | Statement::PragmaWalAutocheckpoint
                    | Statement::SetPragmaWalAutocheckpoint { .. }
                    | Statement::PragmaWalCheckpoint
                    | Statement::PragmaMmapSize
                    | Statement::PragmaAutoVacuum
                    | Statement::PragmaBusyTimeout
                    | Statement::PragmaAnalysisLimit
                    | Statement::PragmaJournalSizeLimit
                    | Statement::PragmaSoftHeapLimit
                    | Statement::PragmaHardHeapLimit
                    | Statement::PragmaThreads
                    | Statement::PragmaAutomaticIndex
                    | Statement::PragmaCellSizeCheck
                    | Statement::PragmaFullColumnNames
                    | Statement::PragmaShortColumnNames
                    | Statement::PragmaFullFsync
                    | Statement::PragmaCheckpointFullFsync
                    | Statement::PragmaEmptyResultCallbacks
                    | Statement::PragmaCaseSensitiveLike
                    | Statement::PragmaReverseUnorderedSelects
            )
        }) {
            return Err(DbError::sql(
                "PRAGMA statements that return rows must use Database::query",
            ));
        }

        Ok(())
    }

    fn validate_query_batch(&self, statements: &[Statement]) -> Result<()> {
        if statements.iter().all(Self::is_query_statement) {
            return Ok(());
        }

        if self.count_changes.get()
            && statements
                .iter()
                .all(Self::is_query_or_count_changes_dml_statement)
        {
            return Ok(());
        }

        if Self::classify_batch(statements) != StatementBatchKind::Query {
            return Err(DbError::sql(
                "Database::query only accepts SELECT statements",
            ));
        }

        Ok(())
    }

    fn is_query_or_count_changes_dml_statement(statement: &Statement) -> bool {
        Self::is_query_statement(statement)
            || matches!(
                statement,
                Statement::Insert { .. }
                    | Statement::InsertUpsert { .. }
                    | Statement::InsertMany { .. }
                    | Statement::InsertManyUpsert { .. }
                    | Statement::InsertDoNothing { .. }
                    | Statement::InsertManyDoNothing { .. }
                    | Statement::InsertExpr { .. }
                    | Statement::InsertExprUpsert { .. }
                    | Statement::InsertManyExpr { .. }
                    | Statement::InsertManyExprUpsert { .. }
                    | Statement::InsertExprDoNothing { .. }
                    | Statement::InsertManyExprDoNothing { .. }
                    | Statement::InsertSelect { .. }
                    | Statement::InsertSelectUpsert { .. }
                    | Statement::InsertSelectDoNothing { .. }
                    | Statement::Delete { .. }
                    | Statement::DeleteLimited { .. }
                    | Statement::Update { .. }
                    | Statement::UpdateLimited { .. }
            )
            || matches!(statement, Statement::WithDml { statement, .. } if Self::is_query_or_count_changes_dml_statement(statement))
    }

    fn plan_statement(&self, statement: &Statement) -> Result<crate::sql::plan::Plan> {
        let context = self
            .storage
            .planning_context_snapshot(self.current_txn.get())?;
        let plan = self.planner.plan_statement(statement, &context)?;
        self.optimizer.optimize_with_context(plan, &context)
    }

    fn with_metadata_transaction<T>(
        &self,
        f: impl FnOnce(TransactionId) -> Result<T>,
    ) -> Result<T> {
        if let Some(transaction_id) = self.current_txn.get() {
            return f(transaction_id);
        }

        let transaction_id = self.storage.begin()?;
        let result = f(transaction_id);
        let rollback_result = self.storage.rollback(transaction_id);

        match (result, rollback_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }
}

impl Database<MemoryStorage> {
    #[must_use]
    pub fn new() -> Self {
        Self::memory()
    }

    #[must_use]
    pub fn memory() -> Self {
        Self::with_storage(MemoryStorage::new())
    }
}

impl Database<FileStorage> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::with_storage(FileStorage::open(path)?))
    }
}

impl<S> Default for Database<S>
where
    S: PlanningStorageEngine + Default,
{
    fn default() -> Self {
        Self::with_storage(S::default())
    }
}
