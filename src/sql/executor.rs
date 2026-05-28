use std::cell::Cell;
use std::cmp::Ordering;

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Row, Schema, Value};
use crate::engine::{StorageEngine, TransactionId};
use crate::sql::ast::{Expr, SelectItem};
use crate::sql::plan::Plan;

pub struct Executor<'a, S: StorageEngine> {
    storage: &'a S,
    current_txn: &'a Cell<Option<TransactionId>>,
}

impl<'a, S: StorageEngine> Executor<'a, S> {
    #[must_use]
    pub fn new(storage: &'a S, current_txn: &'a Cell<Option<TransactionId>>) -> Self {
        Self {
            storage,
            current_txn,
        }
    }

    pub fn execute(&self, plan: Plan) -> Result<Vec<Row>> {
        match plan {
            Plan::BeginTxn => {
                if self.current_txn.get().is_some() {
                    return Err(DbError::txn("transaction already active"));
                }
                let transaction_id = self.storage.begin()?;
                self.current_txn.set(Some(transaction_id));
                Ok(Vec::new())
            }
            Plan::CommitTxn => {
                let transaction_id = self
                    .current_txn
                    .get()
                    .ok_or_else(|| DbError::txn("no active transaction to commit"))?;
                self.storage.commit(transaction_id)?;
                self.current_txn.set(None);
                Ok(Vec::new())
            }
            Plan::RollbackTxn => {
                let transaction_id = self
                    .current_txn
                    .get()
                    .ok_or_else(|| DbError::txn("no active transaction to roll back"))?;
                self.storage.rollback(transaction_id)?;
                self.current_txn.set(None);
                Ok(Vec::new())
            }
            other => match self.current_txn.get() {
                Some(transaction_id) => self.execute_in_transaction(transaction_id, other),
                None => self.execute_autocommit(other),
            },
        }
    }

    fn execute_autocommit(&self, plan: Plan) -> Result<Vec<Row>> {
        let transaction_id = self.storage.begin()?;
        match self.execute_in_transaction(transaction_id, plan) {
            Ok(rows) => {
                self.storage.commit(transaction_id)?;
                Ok(rows)
            }
            Err(error) => {
                let _ = self.storage.rollback(transaction_id);
                Err(error)
            }
        }
    }

    fn execute_in_transaction(
        &self,
        transaction_id: TransactionId,
        plan: Plan,
    ) -> Result<Vec<Row>> {
        match plan {
            Plan::CreateTable { name, columns } => {
                self.storage
                    .create_schema(transaction_id, Schema::new(name, columns))?;
                Ok(Vec::new())
            }
            Plan::CreateIndex {
                name,
                table,
                column,
            } => {
                self.storage.create_index(
                    transaction_id,
                    &table,
                    IndexMeta {
                        name,
                        columns: vec![column],
                        unique: false,
                    },
                )?;
                Ok(Vec::new())
            }
            Plan::Insert { table, values } => {
                self.storage.insert_row(transaction_id, &table, values)?;
                Ok(Vec::new())
            }
            Plan::SeqScan {
                table,
                columns,
                filter,
            } => {
                let schema = self.require_schema(transaction_id, &table)?;
                let rows = self.storage.scan_rows(transaction_id, &table)?;

                rows.into_iter()
                    .filter_map(|(_, row)| {
                        match self.matches_filter(&schema, &row, filter.as_ref()) {
                            Ok(true) => Some(self.project_row(&schema, &row, &columns)),
                            Ok(false) => None,
                            Err(error) => Some(Err(error)),
                        }
                    })
                    .collect()
            }
            Plan::IndexScan {
                table,
                columns,
                index,
                value,
                ..
            } => {
                let schema = self.require_schema(transaction_id, &table)?;
                let row_ids =
                    self.storage
                        .lookup_index(transaction_id, &table, &index, &[value])?;

                let mut rows = Vec::with_capacity(row_ids.len());
                for row_id in row_ids {
                    if let Some(row) = self.storage.get_row(transaction_id, &table, row_id)? {
                        rows.push(self.project_row(&schema, &row, &columns)?);
                    }
                }
                Ok(rows)
            }
            Plan::BeginTxn | Plan::CommitTxn | Plan::RollbackTxn => Err(DbError::txn(
                "transaction control plan reached data execution path",
            )),
        }
    }

    fn require_schema(&self, transaction_id: TransactionId, table: &str) -> Result<Schema> {
        self.storage
            .get_schema(transaction_id, table)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {table}")))
    }

    fn project_row(&self, schema: &Schema, row: &Row, columns: &[SelectItem]) -> Result<Row> {
        if columns.len() == 1 && matches!(columns.first(), Some(SelectItem::Wildcard)) {
            return Ok(row.clone());
        }

        columns
            .iter()
            .map(|column| match column {
                SelectItem::Wildcard => Err(DbError::plan(
                    "wildcard cannot be mixed with explicit projections",
                )),
                SelectItem::Column(name) => self.lookup_value(schema, row, name).cloned(),
            })
            .collect()
    }

    fn matches_filter(&self, schema: &Schema, row: &Row, filter: Option<&Expr>) -> Result<bool> {
        let Some(filter) = filter else {
            return Ok(true);
        };

        match filter {
            Expr::Eq(column, value) => Ok(self.lookup_value(schema, row, column)? == value),
            Expr::Gt(column, value) => Ok(self
                .compare(self.lookup_value(schema, row, column)?, value)?
                == Some(Ordering::Greater)),
            Expr::Lt(column, value) => Ok(self
                .compare(self.lookup_value(schema, row, column)?, value)?
                == Some(Ordering::Less)),
        }
    }

    fn lookup_value<'b>(&self, schema: &Schema, row: &'b Row, column: &str) -> Result<&'b Value> {
        let position = schema
            .columns
            .iter()
            .position(|entry| entry.name == column)
            .ok_or_else(|| {
                DbError::plan(format!("unknown column {column} on table {}", schema.name))
            })?;

        row.get(position).ok_or_else(|| {
            DbError::storage(format!(
                "row for table {} is missing column {column}",
                schema.name
            ))
        })
    }

    fn compare(&self, left: &Value, right: &Value) -> Result<Option<Ordering>> {
        let ordering = match (left, right) {
            (Value::Null, Value::Null) => Some(Ordering::Equal),
            (Value::Boolean(left), Value::Boolean(right)) => Some(left.cmp(right)),
            (Value::Integer(left), Value::Integer(right)) => Some(left.cmp(right)),
            (Value::Text(left), Value::Text(right)) => Some(left.cmp(right)),
            _ => None,
        };

        Ok(ordering)
    }
}
