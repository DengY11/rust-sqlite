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
    last_insert_rowid: Cell<i64>,
    changes: Cell<i64>,
    total_changes: Cell<i64>,
    foreign_keys: Cell<bool>,
    read_uncommitted: Cell<bool>,
    query_only: Cell<bool>,
    recursive_triggers: Cell<bool>,
    trusted_schema: Cell<bool>,
    threads: Cell<u32>,
    cache_size: Cell<i64>,
    busy_timeout: Cell<i64>,
    reverse_unordered_selects: Cell<bool>,
}

impl<S: PlanningStorageEngine> Database<S> {
    #[must_use]
    pub fn with_storage(storage: S) -> Self {
        Self {
            storage,
            planner: Planner::new(),
            optimizer: Optimizer::new(),
            current_txn: Cell::new(None),
            last_insert_rowid: Cell::new(0),
            changes: Cell::new(0),
            total_changes: Cell::new(0),
            foreign_keys: Cell::new(false),
            read_uncommitted: Cell::new(false),
            query_only: Cell::new(false),
            recursive_triggers: Cell::new(false),
            trusted_schema: Cell::new(false),
            threads: Cell::new(0),
            cache_size: Cell::new(2000),
            busy_timeout: Cell::new(0),
            reverse_unordered_selects: Cell::new(false),
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
            &self.last_insert_rowid,
            &self.changes,
            &self.total_changes,
            &self.foreign_keys,
            &self.read_uncommitted,
            &self.query_only,
            &self.recursive_triggers,
            &self.trusted_schema,
            &self.threads,
            &self.cache_size,
            &self.busy_timeout,
            &self.reverse_unordered_selects,
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
            &self.last_insert_rowid,
            &self.changes,
            &self.total_changes,
            &self.foreign_keys,
            &self.read_uncommitted,
            &self.query_only,
            &self.recursive_triggers,
            &self.trusted_schema,
            &self.threads,
            &self.cache_size,
            &self.busy_timeout,
            &self.reverse_unordered_selects,
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
                | Statement::PragmaPageSize
                | Statement::PragmaPageCount
                | Statement::PragmaFreelistCount
                | Statement::PragmaUserVersion
                | Statement::PragmaApplicationId
                | Statement::PragmaSchemaVersion
                | Statement::PragmaForeignKeys
                | Statement::PragmaReadUncommitted
                | Statement::PragmaQueryOnly
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
                | Statement::PragmaJournalMode
                | Statement::PragmaSynchronous
                | Statement::PragmaCacheSize
                | Statement::PragmaTempStore
                | Statement::PragmaLockingMode
                | Statement::PragmaBusyTimeout
                | Statement::PragmaThreads
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
                    | Statement::PragmaPageSize
                    | Statement::PragmaPageCount
                    | Statement::PragmaFreelistCount
                    | Statement::PragmaUserVersion
                    | Statement::PragmaApplicationId
                    | Statement::PragmaSchemaVersion
                    | Statement::PragmaForeignKeys
                    | Statement::PragmaReadUncommitted
                    | Statement::PragmaQueryOnly
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
                    | Statement::PragmaJournalMode
                    | Statement::PragmaSynchronous
                    | Statement::PragmaCacheSize
                    | Statement::PragmaTempStore
                    | Statement::PragmaLockingMode
                    | Statement::PragmaBusyTimeout
                    | Statement::PragmaThreads
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
        if Self::classify_batch(statements) != StatementBatchKind::Query {
            return Err(DbError::sql(
                "Database::query only accepts SELECT statements",
            ));
        }

        Ok(())
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
