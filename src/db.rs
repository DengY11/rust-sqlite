use std::cell::Cell;
use std::path::Path;

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Row, Schema};
use crate::engine::{PlanningStorageEngine, TransactionId};
use crate::sql::ast::Statement;
use crate::sql::executor::Executor;
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
    current_txn: Cell<Option<TransactionId>>,
}

impl<S: PlanningStorageEngine> Database<S> {
    #[must_use]
    pub fn with_storage(storage: S) -> Self {
        Self {
            storage,
            planner: Planner::new(),
            current_txn: Cell::new(None),
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

        let executor = Executor::new(&self.storage, &self.current_txn);
        for statement in statements {
            let plan = self.plan_statement(statement)?;
            executor.execute(plan)?;
        }

        Ok(())
    }

    pub(crate) fn query_parsed(&self, statements: &[Statement]) -> Result<Vec<Row>> {
        self.validate_query_batch(statements)?;

        let executor = Executor::new(&self.storage, &self.current_txn);
        let mut last_rows = Vec::new();

        for statement in statements {
            let plan = self.plan_statement(statement)?;
            last_rows = executor.execute(plan)?;
        }

        Ok(last_rows)
    }

    pub(crate) fn classify_batch(statements: &[Statement]) -> StatementBatchKind {
        if statements
            .iter()
            .all(|statement| matches!(statement, Statement::Select(_)))
        {
            StatementBatchKind::Query
        } else {
            StatementBatchKind::Execute
        }
    }

    fn validate_execute_batch(&self, statements: &[Statement]) -> Result<()> {
        if statements
            .iter()
            .any(|statement| matches!(statement, Statement::Select(_)))
        {
            return Err(DbError::sql("SELECT statements must use Database::query"));
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
        self.planner.plan_statement(statement, &context)
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
