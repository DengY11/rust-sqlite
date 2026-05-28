use std::collections::HashMap;

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Schema};
use crate::sql::ast::{Expr, SelectItem, Statement};
use crate::sql::plan::Plan;

#[derive(Debug, Clone, Default)]
pub struct PlanningContext {
    schemas: HashMap<String, Schema>,
    indexes: HashMap<String, Vec<IndexMeta>>,
}

impl PlanningContext {
    #[must_use]
    pub fn new(schemas: HashMap<String, Schema>, indexes: HashMap<String, Vec<IndexMeta>>) -> Self {
        Self { schemas, indexes }
    }

    pub fn schema(&self, table: &str) -> Option<&Schema> {
        self.schemas.get(table)
    }

    pub fn indexes_for(&self, table: &str) -> &[IndexMeta] {
        self.indexes.get(table).map_or(&[], Vec::as_slice)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Planner;

impl Planner {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub fn plan_statement(&self, statement: &Statement, context: &PlanningContext) -> Result<Plan> {
        match statement {
            Statement::CreateTable { name, columns } => Ok(Plan::CreateTable {
                name: name.clone(),
                columns: columns.clone(),
            }),
            Statement::CreateIndex {
                name,
                table,
                column,
            } => {
                let schema = self.require_schema(context, table)?;
                self.require_column(schema, column)?;

                Ok(Plan::CreateIndex {
                    name: name.clone(),
                    table: table.clone(),
                    column: column.clone(),
                })
            }
            Statement::Insert { table, values } => {
                let schema = self.require_schema(context, table)?;

                if values.len() != schema.columns.len() {
                    return Err(DbError::plan(format!(
                        "insert into {table} expected {} values but got {}",
                        schema.columns.len(),
                        values.len()
                    )));
                }

                Ok(Plan::Insert {
                    table: table.clone(),
                    values: values.clone(),
                })
            }
            Statement::Select {
                columns,
                table,
                filter,
            } => self.plan_select(table, columns, filter, context),
            Statement::Begin => Ok(Plan::BeginTxn),
            Statement::Commit => Ok(Plan::CommitTxn),
            Statement::Rollback => Ok(Plan::RollbackTxn),
        }
    }

    fn plan_select(
        &self,
        table: &str,
        columns: &[SelectItem],
        filter: &Option<Expr>,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;

        for column in columns {
            if let SelectItem::Column(name) = column {
                self.require_column(schema, name)?;
            }
        }

        if let Some(expr) = filter {
            self.require_filter_columns(schema, expr)?;
        }

        if let Some(Expr::Eq(column, value)) = filter {
            if let Some(index) = self.find_single_column_index(context, table, column) {
                return Ok(Plan::IndexScan {
                    table: table.to_string(),
                    columns: columns.to_vec(),
                    index: index.name.clone(),
                    column: column.clone(),
                    value: value.clone(),
                });
            }
        }

        Ok(Plan::SeqScan {
            table: table.to_string(),
            columns: columns.to_vec(),
            filter: filter.clone(),
        })
    }

    fn find_single_column_index<'a>(
        &self,
        context: &'a PlanningContext,
        table: &str,
        column: &str,
    ) -> Option<&'a IndexMeta> {
        context.indexes_for(table).iter().find(|index| {
            index.columns.len() == 1 && index.columns.first().is_some_and(|value| value == column)
        })
    }

    fn require_schema<'a>(&self, context: &'a PlanningContext, table: &str) -> Result<&'a Schema> {
        context
            .schema(table)
            .ok_or_else(|| DbError::plan(format!("unknown table: {table}")))
    }

    fn require_column(&self, schema: &Schema, column: &str) -> Result<()> {
        if schema.columns.iter().any(|entry| entry.name == column) {
            Ok(())
        } else {
            Err(DbError::plan(format!(
                "unknown column {column} on table {}",
                schema.name
            )))
        }
    }

    fn require_filter_columns(&self, schema: &Schema, filter: &Expr) -> Result<()> {
        match filter {
            Expr::Eq(column, _) | Expr::Gt(column, _) | Expr::Lt(column, _) => {
                self.require_column(schema, column)
            }
        }
    }
}
