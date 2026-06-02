use std::collections::HashMap;

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Schema, Value};
use crate::sql::ast::{
    AggregateArg, AggregateFunc, AlterTableAction, Assignment, Expr, OrderBy, OrderByExpr,
    ScalarExpr, SelectItem, SelectStatement, Statement,
};
use crate::sql::plan::{JoinPlan, Plan};

#[derive(Debug, Clone, Default)]
pub struct PlanningContext {
    schemas: HashMap<String, Schema>,
    indexes: HashMap<String, Vec<IndexMeta>>,
}

struct SingleTablePlanInput<'a> {
    table: &'a str,
    table_alias: Option<&'a str>,
    columns: &'a [SelectItem],
    filter: &'a Option<Expr>,
    order_by: &'a [OrderBy],
    limit: Option<usize>,
    distinct: bool,
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
pub struct Planner {
    allow_unresolved_outer_refs: bool,
}

impl Planner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            allow_unresolved_outer_refs: false,
        }
    }

    pub(crate) fn with_unresolved_outer_refs() -> Self {
        Self {
            allow_unresolved_outer_refs: true,
        }
    }

    pub fn plan_statement(&self, statement: &Statement, context: &PlanningContext) -> Result<Plan> {
        match statement {
            Statement::CreateTable {
                name,
                columns,
                constraints,
            } => Ok(Plan::CreateTable {
                name: name.clone(),
                columns: columns.clone(),
                constraints: constraints.clone(),
            }),
            Statement::CreateIndex {
                name,
                table,
                columns,
                unique,
            } => {
                let schema = self.require_schema(context, table)?;
                if columns.is_empty() {
                    return Err(DbError::plan("index must define at least one column"));
                }
                let mut seen = std::collections::BTreeSet::new();
                for column in columns {
                    self.require_column(schema, column)?;
                    if !seen.insert(column.clone()) {
                        return Err(DbError::plan(format!(
                            "duplicate index column name: {column}"
                        )));
                    }
                }

                Ok(Plan::CreateIndex {
                    name: name.clone(),
                    table: table.clone(),
                    columns: columns.clone(),
                    unique: *unique,
                })
            }
            Statement::DropTable { name } => {
                self.require_schema(context, name)?;
                Ok(Plan::DropTable { name: name.clone() })
            }
            Statement::DropIndex { name } => {
                let table = self.resolve_index_table(context, name)?;
                Ok(Plan::DropIndex {
                    table,
                    name: name.clone(),
                })
            }
            Statement::AlterTable { table, action } => {
                self.plan_alter_table(table, action, context)
            }
            Statement::Insert {
                table,
                columns,
                values,
            } => self.plan_insert(table, columns.as_deref(), values, context),
            Statement::Delete {
                table,
                table_alias,
                filter,
            } => self.plan_delete(table, table_alias.as_deref(), filter, context),
            Statement::Update {
                table,
                table_alias,
                assignments,
                filter,
            } => self.plan_update(table, table_alias.as_deref(), assignments, filter, context),
            Statement::Select(select) => self.plan_select(select, context),
            Statement::ExplainQueryPlan(statement) => Ok(Plan::ExplainQueryPlan {
                plan: Box::new(self.plan_statement(statement, context)?),
            }),
            Statement::Begin => Ok(Plan::BeginTxn),
            Statement::Commit => Ok(Plan::CommitTxn),
            Statement::Rollback => Ok(Plan::RollbackTxn),
        }
    }

    fn plan_alter_table(
        &self,
        table: &str,
        action: &AlterTableAction,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        match action {
            AlterTableAction::AddColumn(column) => {
                if schema.columns.iter().any(|entry| entry.name == column.name) {
                    return Err(DbError::plan(format!(
                        "column already exists on table {table}: {}",
                        column.name
                    )));
                }
            }
            AlterTableAction::RenameTable { new_name } => {
                if context.schema(new_name).is_some() {
                    return Err(DbError::plan(format!("table already exists: {new_name}")));
                }
            }
            AlterTableAction::RenameColumn { old_name, new_name } => {
                self.require_column(schema, old_name)?;
                if schema.columns.iter().any(|entry| entry.name == *new_name) {
                    return Err(DbError::plan(format!(
                        "column already exists on table {table}: {new_name}"
                    )));
                }
            }
        }

        Ok(Plan::AlterTable {
            table: table.to_string(),
            action: action.clone(),
        })
    }

    fn plan_insert(
        &self,
        table: &str,
        columns: Option<&[String]>,
        values: &[Value],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let row = self.build_insert_row(schema, table, columns, values)?;
        Ok(Plan::Insert {
            table: table.to_string(),
            values: row,
        })
    }

    fn resolve_index_table(&self, context: &PlanningContext, index_name: &str) -> Result<String> {
        let matches = context
            .indexes
            .iter()
            .filter(|(_, indexes)| indexes.iter().any(|index| index.name == index_name))
            .map(|(table, _)| table.clone())
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [table] => Ok(table.clone()),
            [] => Err(DbError::plan(format!("unknown index: {index_name}"))),
            _ => Err(DbError::plan(format!("ambiguous index name: {index_name}"))),
        }
    }

    fn plan_delete(
        &self,
        table: &str,
        table_alias: Option<&str>,
        filter: &Option<Expr>,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_filter = filter
            .as_ref()
            .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
            .transpose()?;

        if let Some(expr) = &normalized_filter {
            let scope = self.build_single_table_scope(table, table_alias, context)?;
            self.require_scope_columns_with_outer(&scope, None, expr)?;
            self.validate_subqueries(expr, context, &scope)?;
        }

        Ok(Plan::Delete {
            table: table.to_string(),
            filter: normalized_filter,
        })
    }

    fn plan_update(
        &self,
        table: &str,
        table_alias: Option<&str>,
        assignments: &[Assignment],
        filter: &Option<Expr>,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let mut seen = std::collections::BTreeSet::new();
        let mut normalized_assignments = Vec::with_capacity(assignments.len());
        for assignment in assignments {
            self.require_column(schema, &assignment.column)?;
            if !seen.insert(assignment.column.clone()) {
                return Err(DbError::plan(format!(
                    "duplicate assignment target: {}",
                    assignment.column
                )));
            }
            normalized_assignments.push(assignment.clone());
        }

        let normalized_filter = filter
            .as_ref()
            .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
            .transpose()?;
        if let Some(expr) = &normalized_filter {
            let scope = self.build_single_table_scope(table, table_alias, context)?;
            self.require_scope_columns_with_outer(&scope, None, expr)?;
            self.validate_subqueries(expr, context, &scope)?;
        }

        Ok(Plan::Update {
            table: table.to_string(),
            assignments: normalized_assignments,
            filter: normalized_filter,
        })
    }

    fn plan_select(&self, select: &SelectStatement, context: &PlanningContext) -> Result<Plan> {
        self.plan_select_with_outer(select, context, None)
    }

    fn plan_select_with_outer(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
        outer_scope: Option<&QueryScope>,
    ) -> Result<Plan> {
        let has_aggregates = self.select_has_aggregates(&select.columns);

        if !select.joins.is_empty() {
            let source = self.plan_join_source(select, context, outer_scope)?;
            if has_aggregates || !select.group_by.is_empty() {
                self.validate_aggregate_projection(select, context)?;
                let rewritten_having = select.having.as_ref().map(|expr| {
                    self.rewrite_aggregate_expr_group_references(expr, &select.group_by)
                });
                let rewritten_order_by = select
                    .order_by
                    .iter()
                    .map(|item| {
                        self.rewrite_aggregate_order_by_group_references(item, &select.group_by)
                    })
                    .collect();
                return Ok(Plan::Aggregate {
                    source: Box::new(source),
                    columns: select.columns.clone(),
                    group_by: select.group_by.clone(),
                    having: rewritten_having,
                    order_by: rewritten_order_by,
                    limit: select.limit,
                });
            }

            return Ok(Plan::NestedLoopJoin {
                table: select.table.clone(),
                table_alias: select.table_alias.clone(),
                joins: select
                    .joins
                    .iter()
                    .map(|join| JoinPlan {
                        kind: join.kind,
                        table: join.table.clone(),
                        table_alias: join.table_alias.clone(),
                        on: join.on.clone(),
                    })
                    .collect(),
                columns: select.columns.clone(),
                filter: select.filter.clone(),
                order_by: select.order_by.clone(),
                limit: select.limit,
                distinct: select.distinct,
            });
        }

        if has_aggregates || !select.group_by.is_empty() {
            self.validate_aggregate_projection(select, context)?;
            let schema = self.require_schema(context, &select.table)?;
            let columns = self.normalize_aggregate_select_items(
                &select.table,
                select.table_alias.as_deref(),
                &select.columns,
                context,
            )?;
            let group_by = self.normalize_group_by(
                &select.table,
                select.table_alias.as_deref(),
                &select.group_by,
                context,
            )?;
            let having = select
                .having
                .as_ref()
                .map(|expr| {
                    self.normalize_aggregate_expr(
                        schema,
                        &select.table,
                        select.table_alias.as_deref(),
                        &columns,
                        &group_by,
                        expr,
                    )
                    .map(|expr| self.rewrite_aggregate_expr_group_references(&expr, &group_by))
                })
                .transpose()?;
            let order_by = select
                .order_by
                .iter()
                .map(|item| {
                    self.normalize_aggregate_order_by(
                        schema,
                        &select.table,
                        select.table_alias.as_deref(),
                        &columns,
                        &group_by,
                        item,
                    )
                    .map(|item| self.rewrite_aggregate_order_by_group_references(&item, &group_by))
                })
                .collect::<Result<Vec<_>>>()?;
            let source = self.plan_single_table_source(
                SingleTablePlanInput {
                    table: &select.table,
                    table_alias: select.table_alias.as_deref(),
                    columns: &[SelectItem::Wildcard],
                    filter: &select.filter,
                    order_by: &[],
                    limit: None,
                    distinct: false,
                },
                context,
                outer_scope,
            )?;
            return Ok(Plan::Aggregate {
                source: Box::new(source),
                columns,
                group_by,
                having,
                order_by,
                limit: select.limit,
            });
        }

        self.plan_single_table_source(
            SingleTablePlanInput {
                table: &select.table,
                table_alias: select.table_alias.as_deref(),
                columns: &select.columns,
                filter: &select.filter,
                order_by: &select.order_by,
                limit: select.limit,
                distinct: select.distinct,
            },
            context,
            outer_scope,
        )
    }

    fn plan_single_table_source(
        &self,
        input: SingleTablePlanInput<'_>,
        context: &PlanningContext,
        outer_scope: Option<&QueryScope>,
    ) -> Result<Plan> {
        let SingleTablePlanInput {
            table,
            table_alias,
            columns,
            filter,
            order_by,
            limit,
            distinct,
        } = input;
        let schema = self.require_schema(context, table)?;

        let normalized_columns = columns
            .iter()
            .map(|column| self.normalize_select_item(schema, table, table_alias, column))
            .collect::<Result<Vec<_>>>()?;

        for column in &normalized_columns {
            self.require_select_item_columns(schema, column)?;
        }

        let normalized_filter = filter
            .as_ref()
            .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
            .transpose()?;

        if let Some(expr) = &normalized_filter {
            let scope = self.build_single_table_scope(table, table_alias, context)?;
            self.require_scope_columns_with_outer(&scope, outer_scope, expr)?;
            self.validate_subqueries(expr, context, &scope)?;
        }

        let normalized_order_by = order_by
            .iter()
            .map(|item| {
                self.normalize_order_by(schema, table, table_alias, &normalized_columns, item)
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Plan::SeqScan {
            table: table.to_string(),
            table_alias: table_alias.map(str::to_string),
            columns: normalized_columns,
            filter: normalized_filter,
            order_by: normalized_order_by,
            limit,
            distinct,
        })
    }

    fn plan_join_source(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
        outer_scope: Option<&QueryScope>,
    ) -> Result<Plan> {
        self.require_schema(context, &select.table)?;
        let scope = self.build_scope(select, context)?;

        for item in &select.columns {
            self.require_join_select_item(&scope, item)?;
        }
        if let Some(filter) = &select.filter {
            self.require_scope_columns_with_outer(&scope, outer_scope, filter)?;
            self.validate_subqueries(filter, context, &scope)?;
        }
        for (index, join) in select.joins.iter().enumerate() {
            self.require_schema(context, &join.table)?;
            let join_scope = self.build_join_scope(select, context, index)?;
            self.require_scope_columns_with_outer(&join_scope, outer_scope, &join.on)?;
            self.validate_subqueries(&join.on, context, &join_scope)?;
        }
        for item in &select.order_by {
            self.require_order_by_scope(&scope, &select.columns, item)?;
        }

        Ok(Plan::NestedLoopJoin {
            table: select.table.clone(),
            table_alias: select.table_alias.clone(),
            joins: select
                .joins
                .iter()
                .map(|join| JoinPlan {
                    kind: join.kind,
                    table: join.table.clone(),
                    table_alias: join.table_alias.clone(),
                    on: join.on.clone(),
                })
                .collect(),
            columns: vec![SelectItem::Wildcard],
            filter: select.filter.clone(),
            order_by: vec![],
            limit: None,
            distinct: false,
        })
    }

    fn build_insert_row(
        &self,
        schema: &Schema,
        table: &str,
        columns: Option<&[String]>,
        values: &[Value],
    ) -> Result<Vec<Value>> {
        match columns {
            None => {
                if values.len() != schema.columns.len() {
                    return Err(DbError::plan(format!(
                        "insert into {table} expected {} values but got {}",
                        schema.columns.len(),
                        values.len()
                    )));
                }
                Ok(values.to_vec())
            }
            Some(columns) => {
                if columns.is_empty() {
                    return Err(DbError::plan("insert column list cannot be empty"));
                }
                if columns.len() != values.len() {
                    return Err(DbError::plan(format!(
                        "insert into {table} specified {} columns but got {} values",
                        columns.len(),
                        values.len()
                    )));
                }
                let mut row = schema
                    .columns
                    .iter()
                    .map(|column| column.default_value.clone().unwrap_or(Value::Null))
                    .collect::<Vec<_>>();
                let mut seen = std::collections::BTreeSet::new();
                for (column, value) in columns.iter().zip(values.iter()) {
                    if !seen.insert(column.clone()) {
                        return Err(DbError::plan(format!("duplicate insert column: {column}")));
                    }
                    let position = schema
                        .columns
                        .iter()
                        .position(|entry| entry.name == *column)
                        .ok_or_else(|| {
                            DbError::plan(format!(
                                "unknown column {column} on table {}",
                                schema.name
                            ))
                        })?;
                    row[position] = value.clone();
                }
                Ok(row)
            }
        }
    }

    fn normalize_select_item(
        &self,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        item: &SelectItem,
    ) -> Result<SelectItem> {
        Ok(match item {
            SelectItem::Wildcard => SelectItem::Wildcard,
            SelectItem::Column(name) => SelectItem::Column(self.normalize_column_reference(
                schema,
                table,
                table_alias,
                name,
            )?),
            SelectItem::AliasedColumn { name, alias } => SelectItem::AliasedColumn {
                name: self.normalize_column_reference(schema, table, table_alias, name)?,
                alias: alias.clone(),
            },
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                alias: alias.clone(),
            },
            SelectItem::Aggregate { func, arg, alias } => SelectItem::Aggregate {
                func: *func,
                arg: match arg {
                    AggregateArg::Wildcard => AggregateArg::Wildcard,
                    AggregateArg::Expr { expr, distinct } => AggregateArg::Expr {
                        expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                        distinct: *distinct,
                    },
                },
                alias: alias.clone(),
            },
        })
    }

    fn normalize_order_by(
        &self,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        columns: &[SelectItem],
        item: &OrderBy,
    ) -> Result<OrderBy> {
        let OrderByExpr::Column(column) = &item.expr else {
            return match &item.expr {
                OrderByExpr::Expr(expr) => Ok(OrderBy {
                    expr: OrderByExpr::Expr(self.normalize_scalar_expr(
                        schema,
                        table,
                        table_alias,
                        expr,
                    )?),
                    descending: item.descending,
                    nulls: item.nulls,
                }),
                OrderByExpr::Position(_) => Ok(item.clone()),
                OrderByExpr::Column(_) => unreachable!(),
            };
        };
        if self
            .select_aliases(columns)
            .iter()
            .any(|alias| alias == column)
        {
            return Ok(item.clone());
        }
        Ok(OrderBy {
            expr: OrderByExpr::Column(self.normalize_column_reference(
                schema,
                table,
                table_alias,
                column,
            )?),
            descending: item.descending,
            nulls: item.nulls,
        })
    }

    fn normalize_aggregate_order_by(
        &self,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        columns: &[SelectItem],
        group_by: &[ScalarExpr],
        item: &OrderBy,
    ) -> Result<OrderBy> {
        Ok(OrderBy {
            expr: match &item.expr {
                OrderByExpr::Column(column) => {
                    OrderByExpr::Column(self.normalize_aggregate_column_reference(
                        schema,
                        table,
                        table_alias,
                        columns,
                        group_by,
                        column,
                    )?)
                }
                OrderByExpr::Expr(expr) => {
                    OrderByExpr::Expr(self.normalize_aggregate_scalar_expr(
                        schema,
                        table,
                        table_alias,
                        columns,
                        group_by,
                        expr,
                    )?)
                }
                OrderByExpr::Position(position) => OrderByExpr::Position(*position),
            },
            descending: item.descending,
            nulls: item.nulls,
        })
    }

    fn select_aliases(&self, columns: &[SelectItem]) -> Vec<String> {
        columns
            .iter()
            .filter_map(|column| match column {
                SelectItem::AliasedColumn { alias, .. } => Some(alias.clone()),
                SelectItem::Expr {
                    alias: Some(alias), ..
                } => Some(alias.clone()),
                SelectItem::Aggregate {
                    alias: Some(alias), ..
                } => Some(alias.clone()),
                _ => None,
            })
            .collect()
    }

    fn normalize_expr(
        &self,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        expr: &Expr,
    ) -> Result<Expr> {
        Ok(match expr {
            Expr::Compare { column, op, value } => Expr::Compare {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
                op: *op,
                value: value.clone(),
            },
            Expr::CompareColumns { left, op, right } => Expr::CompareColumns {
                left: self.normalize_column_reference(schema, table, table_alias, left)?,
                op: *op,
                right: self.normalize_column_reference(schema, table, table_alias, right)?,
            },
            Expr::CompareScalar { left, op, right } => Expr::CompareScalar {
                left: self.normalize_scalar_expr(schema, table, table_alias, left)?,
                op: *op,
                right: self.normalize_scalar_expr(schema, table, table_alias, right)?,
            },
            Expr::IsNull { column, negated } => Expr::IsNull {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
                negated: *negated,
            },
            Expr::IsNullScalar { expr, negated } => Expr::IsNullScalar {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                negated: *negated,
            },
            Expr::InSubquery {
                column,
                query,
                negated,
            } => Expr::InSubquery {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
                query: query.clone(),
                negated: *negated,
            },
            Expr::InSubqueryScalar {
                expr,
                query,
                negated,
            } => Expr::InSubqueryScalar {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                query: query.clone(),
                negated: *negated,
            },
            Expr::CompareSubquery { column, op, query } => Expr::CompareSubquery {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
                op: *op,
                query: query.clone(),
            },
            Expr::CompareSubqueryScalar { left, op, query } => Expr::CompareSubqueryScalar {
                left: self.normalize_scalar_expr(schema, table, table_alias, left)?,
                op: *op,
                query: query.clone(),
            },
            Expr::ExistsSubquery { query, negated } => Expr::ExistsSubquery {
                query: query.clone(),
                negated: *negated,
            },
            Expr::Like {
                column,
                pattern,
                negated,
            } => Expr::Like {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
                pattern: pattern.clone(),
                negated: *negated,
            },
            Expr::LikeScalar {
                expr,
                pattern,
                negated,
            } => Expr::LikeScalar {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                pattern: pattern.clone(),
                negated: *negated,
            },
            Expr::Between {
                column,
                low,
                high,
                negated,
            } => Expr::Between {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
                low: low.clone(),
                high: high.clone(),
                negated: *negated,
            },
            Expr::BetweenScalar {
                expr,
                low,
                high,
                negated,
            } => Expr::BetweenScalar {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                low: self.normalize_scalar_expr(schema, table, table_alias, low)?,
                high: self.normalize_scalar_expr(schema, table, table_alias, high)?,
                negated: *negated,
            },
            Expr::Not(expr) => Expr::Not(Box::new(self.normalize_expr(
                schema,
                table,
                table_alias,
                expr,
            )?)),
            Expr::And(left, right) => Expr::And(
                Box::new(self.normalize_expr(schema, table, table_alias, left)?),
                Box::new(self.normalize_expr(schema, table, table_alias, right)?),
            ),
            Expr::Or(left, right) => Expr::Or(
                Box::new(self.normalize_expr(schema, table, table_alias, left)?),
                Box::new(self.normalize_expr(schema, table, table_alias, right)?),
            ),
        })
    }

    fn normalize_aggregate_expr(
        &self,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        columns: &[SelectItem],
        group_by: &[ScalarExpr],
        expr: &Expr,
    ) -> Result<Expr> {
        Ok(match expr {
            Expr::Compare { column, op, value } => Expr::Compare {
                column: self.normalize_aggregate_column_reference(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    column,
                )?,
                op: *op,
                value: value.clone(),
            },
            Expr::CompareColumns { left, op, right } => Expr::CompareColumns {
                left: self.normalize_aggregate_column_reference(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    left,
                )?,
                op: *op,
                right: self.normalize_aggregate_column_reference(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    right,
                )?,
            },
            Expr::CompareScalar { left, op, right } => Expr::CompareScalar {
                left: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    left,
                )?,
                op: *op,
                right: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    right,
                )?,
            },
            Expr::IsNull { column, negated } => Expr::IsNull {
                column: self.normalize_aggregate_column_reference(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    column,
                )?,
                negated: *negated,
            },
            Expr::IsNullScalar { expr, negated } => Expr::IsNullScalar {
                expr: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?,
                negated: *negated,
            },
            Expr::InSubquery {
                column,
                query,
                negated,
            } => Expr::InSubquery {
                column: self.normalize_aggregate_column_reference(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    column,
                )?,
                query: query.clone(),
                negated: *negated,
            },
            Expr::InSubqueryScalar {
                expr,
                query,
                negated,
            } => Expr::InSubqueryScalar {
                expr: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?,
                query: query.clone(),
                negated: *negated,
            },
            Expr::CompareSubquery { column, op, query } => Expr::CompareSubquery {
                column: self.normalize_aggregate_column_reference(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    column,
                )?,
                op: *op,
                query: query.clone(),
            },
            Expr::CompareSubqueryScalar { left, op, query } => Expr::CompareSubqueryScalar {
                left: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    left,
                )?,
                op: *op,
                query: query.clone(),
            },
            Expr::ExistsSubquery { query, negated } => Expr::ExistsSubquery {
                query: query.clone(),
                negated: *negated,
            },
            Expr::Like {
                column,
                pattern,
                negated,
            } => Expr::Like {
                column: self.normalize_aggregate_column_reference(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    column,
                )?,
                pattern: pattern.clone(),
                negated: *negated,
            },
            Expr::LikeScalar {
                expr,
                pattern,
                negated,
            } => Expr::LikeScalar {
                expr: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?,
                pattern: pattern.clone(),
                negated: *negated,
            },
            Expr::Between {
                column,
                low,
                high,
                negated,
            } => Expr::Between {
                column: self.normalize_aggregate_column_reference(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    column,
                )?,
                low: low.clone(),
                high: high.clone(),
                negated: *negated,
            },
            Expr::BetweenScalar {
                expr,
                low,
                high,
                negated,
            } => Expr::BetweenScalar {
                expr: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?,
                low: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    low,
                )?,
                high: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    high,
                )?,
                negated: *negated,
            },
            Expr::Not(expr) => Expr::Not(Box::new(self.normalize_aggregate_expr(
                schema,
                table,
                table_alias,
                columns,
                group_by,
                expr,
            )?)),
            Expr::And(left, right) => Expr::And(
                Box::new(self.normalize_aggregate_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    left,
                )?),
                Box::new(self.normalize_aggregate_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    right,
                )?),
            ),
            Expr::Or(left, right) => Expr::Or(
                Box::new(self.normalize_aggregate_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    left,
                )?),
                Box::new(self.normalize_aggregate_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    right,
                )?),
            ),
        })
    }

    fn normalize_aggregate_column_reference(
        &self,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        columns: &[SelectItem],
        group_by: &[ScalarExpr],
        column: &str,
    ) -> Result<String> {
        if self
            .aggregate_reference_names(columns, group_by)
            .iter()
            .any(|name| name == column)
        {
            Ok(column.to_string())
        } else {
            self.normalize_column_reference(schema, table, table_alias, column)
        }
    }

    fn normalize_aggregate_scalar_expr(
        &self,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        columns: &[SelectItem],
        group_by: &[ScalarExpr],
        expr: &ScalarExpr,
    ) -> Result<ScalarExpr> {
        Ok(match expr {
            ScalarExpr::Literal(value) => ScalarExpr::Literal(value.clone()),
            ScalarExpr::Column(name) => {
                ScalarExpr::Column(self.normalize_aggregate_column_reference(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    name,
                )?)
            }
            ScalarExpr::UnaryMinus(expr) => {
                ScalarExpr::UnaryMinus(Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?))
            }
            ScalarExpr::Binary { left, op, right } => ScalarExpr::Binary {
                left: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    left,
                )?),
                op: *op,
                right: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    right,
                )?),
            },
            ScalarExpr::Function { func, args } => ScalarExpr::Function {
                func: *func,
                args: args
                    .iter()
                    .map(|arg| {
                        self.normalize_aggregate_scalar_expr(
                            schema,
                            table,
                            table_alias,
                            columns,
                            group_by,
                            arg,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
            },
        })
    }

    fn normalize_column_reference(
        &self,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        column: &str,
    ) -> Result<String> {
        if let Some((prefix, suffix)) = column.split_once('.')
            && (prefix == table || table_alias.is_some_and(|alias| alias == prefix))
        {
            self.require_column(schema, suffix)?;
            return Ok(suffix.to_string());
        }
        if column.contains('.') {
            return Ok(column.to_string());
        }
        self.require_column(schema, column)?;
        Ok(column.to_string())
    }

    fn normalize_scalar_expr(
        &self,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        expr: &ScalarExpr,
    ) -> Result<ScalarExpr> {
        Ok(match expr {
            ScalarExpr::Literal(value) => ScalarExpr::Literal(value.clone()),
            ScalarExpr::Column(name) => ScalarExpr::Column(self.normalize_column_reference(
                schema,
                table,
                table_alias,
                name,
            )?),
            ScalarExpr::UnaryMinus(expr) => ScalarExpr::UnaryMinus(Box::new(
                self.normalize_scalar_expr(schema, table, table_alias, expr)?,
            )),
            ScalarExpr::Binary { left, op, right } => ScalarExpr::Binary {
                left: Box::new(self.normalize_scalar_expr(schema, table, table_alias, left)?),
                op: *op,
                right: Box::new(self.normalize_scalar_expr(schema, table, table_alias, right)?),
            },
            ScalarExpr::Function { func, args } => ScalarExpr::Function {
                func: *func,
                args: args
                    .iter()
                    .map(|arg| self.normalize_scalar_expr(schema, table, table_alias, arg))
                    .collect::<Result<Vec<_>>>()?,
            },
        })
    }

    fn require_scalar_expr_columns(&self, schema: &Schema, expr: &ScalarExpr) -> Result<()> {
        match expr {
            ScalarExpr::Literal(_) => Ok(()),
            ScalarExpr::Column(name) => self.require_column(schema, name),
            ScalarExpr::UnaryMinus(expr) => self.require_scalar_expr_columns(schema, expr),
            ScalarExpr::Binary { left, right, .. } => {
                self.require_scalar_expr_columns(schema, left)?;
                self.require_scalar_expr_columns(schema, right)
            }
            ScalarExpr::Function { args, .. } => {
                for arg in args {
                    self.require_scalar_expr_columns(schema, arg)?;
                }
                Ok(())
            }
        }
    }

    fn require_scalar_expr_scope(&self, scope: &QueryScope, expr: &ScalarExpr) -> Result<()> {
        match expr {
            ScalarExpr::Literal(_) => Ok(()),
            ScalarExpr::Column(name) => self.resolve_column_in_scope(scope, name).map(|_| ()),
            ScalarExpr::UnaryMinus(expr) => self.require_scalar_expr_scope(scope, expr),
            ScalarExpr::Binary { left, right, .. } => {
                self.require_scalar_expr_scope(scope, left)?;
                self.require_scalar_expr_scope(scope, right)
            }
            ScalarExpr::Function { args, .. } => {
                for arg in args {
                    self.require_scalar_expr_scope(scope, arg)?;
                }
                Ok(())
            }
        }
    }

    fn select_has_aggregates(&self, columns: &[SelectItem]) -> bool {
        columns
            .iter()
            .any(|column| matches!(column, SelectItem::Aggregate { .. }))
    }

    fn require_select_item_columns(&self, schema: &Schema, item: &SelectItem) -> Result<()> {
        match item {
            SelectItem::Wildcard => Ok(()),
            SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
                self.require_column(schema, name)
            }
            SelectItem::Expr { expr, .. } => self.require_scalar_expr_columns(schema, expr),
            SelectItem::Aggregate { arg, func, .. } => match arg {
                AggregateArg::Wildcard => {
                    if *func == AggregateFunc::Count {
                        Ok(())
                    } else {
                        Err(DbError::plan(
                            "only COUNT supports wildcard aggregate argument",
                        ))
                    }
                }
                AggregateArg::Expr { expr, .. } => self.require_scalar_expr_columns(schema, expr),
            },
        }
    }

    fn normalize_aggregate_select_items(
        &self,
        table: &str,
        table_alias: Option<&str>,
        columns: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Vec<SelectItem>> {
        let schema = self.require_schema(context, table)?;
        columns
            .iter()
            .map(|item| self.normalize_select_item(schema, table, table_alias, item))
            .collect()
    }

    fn normalize_group_by(
        &self,
        table: &str,
        table_alias: Option<&str>,
        group_by: &[ScalarExpr],
        context: &PlanningContext,
    ) -> Result<Vec<ScalarExpr>> {
        let schema = self.require_schema(context, table)?;
        group_by
            .iter()
            .map(|expr| self.normalize_scalar_expr(schema, table, table_alias, expr))
            .collect()
    }

    fn validate_aggregate_projection(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
    ) -> Result<()> {
        let schema = self.require_schema(context, &select.table)?;
        let normalized_columns = self.normalize_aggregate_select_items(
            &select.table,
            select.table_alias.as_deref(),
            &select.columns,
            context,
        )?;
        let normalized_group_by = self.normalize_group_by(
            &select.table,
            select.table_alias.as_deref(),
            &select.group_by,
            context,
        )?;
        let normalized_having = select
            .having
            .as_ref()
            .map(|expr| {
                self.normalize_aggregate_expr(
                    schema,
                    &select.table,
                    select.table_alias.as_deref(),
                    &normalized_columns,
                    &normalized_group_by,
                    expr,
                )
            })
            .transpose()?;
        let normalized_order_by = select
            .order_by
            .iter()
            .map(|item| {
                self.normalize_aggregate_order_by(
                    schema,
                    &select.table,
                    select.table_alias.as_deref(),
                    &normalized_columns,
                    &normalized_group_by,
                    item,
                )
            })
            .collect::<Result<Vec<_>>>()?;

        if !select.joins.is_empty() {
            let scope = self.build_scope(select, context)?;
            for expr in &select.group_by {
                self.require_scalar_expr_scope(&scope, expr)?;
            }
            for item in &select.columns {
                self.require_aggregate_select_item_in_scope(&scope, &select.group_by, item)?;
            }
            if let Some(having) = &select.having {
                self.require_aggregate_expr_references(having, &select.columns, &select.group_by)?;
            }
            for item in &select.order_by {
                self.require_aggregate_order_by_references(
                    item,
                    &select.columns,
                    &select.group_by,
                )?;
            }
            return Ok(());
        }

        for item in &normalized_columns {
            self.require_select_item_columns(schema, item)?;
            match item {
                SelectItem::Wildcard => {
                    return Err(DbError::plan(
                        "wildcard cannot be used with GROUP BY or aggregate projections",
                    ));
                }
                SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
                    if !self.group_by_contains_expr(
                        &normalized_group_by,
                        &ScalarExpr::Column(name.clone()),
                    ) {
                        return Err(DbError::plan(format!(
                            "non-aggregate column {name} must appear in GROUP BY"
                        )));
                    }
                }
                SelectItem::Expr { expr, .. } => {
                    if !self.group_by_contains_expr(&normalized_group_by, expr) {
                        return Err(DbError::plan(format!(
                            "non-aggregate expression {} must appear in GROUP BY",
                            self.scalar_expr_display(expr)
                        )));
                    }
                }
                SelectItem::Aggregate { func, arg, .. } => {
                    if matches!(func, AggregateFunc::Sum | AggregateFunc::Avg)
                        && matches!(arg, AggregateArg::Expr { expr, .. } if !self.scalar_expr_returns_integer(schema, expr))
                    {
                        return Err(DbError::plan(format!(
                            "{} only supports INTEGER columns",
                            match func {
                                AggregateFunc::Sum => "SUM",
                                AggregateFunc::Avg => "AVG",
                                _ => unreachable!(),
                            }
                        )));
                    }
                }
            }
        }

        if let Some(having) = &normalized_having {
            self.require_aggregate_expr_references(
                having,
                &normalized_columns,
                &normalized_group_by,
            )?;
        }
        for item in &normalized_order_by {
            self.require_aggregate_order_by_references(
                item,
                &normalized_columns,
                &normalized_group_by,
            )?;
        }

        Ok(())
    }

    fn scalar_expr_returns_integer(&self, schema: &Schema, expr: &ScalarExpr) -> bool {
        match expr {
            ScalarExpr::Literal(Value::Integer(_)) => true,
            ScalarExpr::Literal(_) => false,
            ScalarExpr::Column(column) => schema.columns.iter().any(|entry| {
                entry.name == *column
                    && matches!(entry.column_type, crate::common::types::ColumnType::Integer)
            }),
            ScalarExpr::UnaryMinus(expr) => self.scalar_expr_returns_integer(schema, expr),
            ScalarExpr::Binary { left, op, right } => {
                !matches!(op, crate::sql::ast::ScalarBinaryOp::Concat)
                    && self.scalar_expr_returns_integer(schema, left)
                    && self.scalar_expr_returns_integer(schema, right)
            }
            ScalarExpr::Function { func, args } => match func {
                crate::sql::ast::ScalarFunc::Abs => args
                    .iter()
                    .all(|arg| self.scalar_expr_returns_integer(schema, arg)),
                crate::sql::ast::ScalarFunc::Length => true,
                crate::sql::ast::ScalarFunc::Coalesce | crate::sql::ast::ScalarFunc::IfNull => args
                    .iter()
                    .all(|arg| self.scalar_expr_returns_integer(schema, arg)),
                crate::sql::ast::ScalarFunc::Lower | crate::sql::ast::ScalarFunc::Upper => false,
            },
        }
    }

    fn build_scope(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
    ) -> Result<QueryScope> {
        let mut bindings = Vec::with_capacity(select.joins.len() + 1);
        bindings.push(TableBinding {
            table: select.table.clone(),
            alias: select.table_alias.clone(),
            schema: self.require_schema(context, &select.table)?.clone(),
        });
        for join in &select.joins {
            bindings.push(TableBinding {
                table: join.table.clone(),
                alias: join.table_alias.clone(),
                schema: self.require_schema(context, &join.table)?.clone(),
            });
        }
        Ok(QueryScope { bindings })
    }

    fn build_join_scope(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
        join_index: usize,
    ) -> Result<QueryScope> {
        let mut bindings = Vec::with_capacity(join_index + 2);
        bindings.push(TableBinding {
            table: select.table.clone(),
            alias: select.table_alias.clone(),
            schema: self.require_schema(context, &select.table)?.clone(),
        });
        for join in select.joins.iter().take(join_index + 1) {
            bindings.push(TableBinding {
                table: join.table.clone(),
                alias: join.table_alias.clone(),
                schema: self.require_schema(context, &join.table)?.clone(),
            });
        }
        Ok(QueryScope { bindings })
    }

    fn build_single_table_scope(
        &self,
        table: &str,
        table_alias: Option<&str>,
        context: &PlanningContext,
    ) -> Result<QueryScope> {
        Ok(QueryScope {
            bindings: vec![TableBinding {
                table: table.to_string(),
                alias: table_alias.map(str::to_string),
                schema: self.require_schema(context, table)?.clone(),
            }],
        })
    }

    fn require_join_select_item(&self, scope: &QueryScope, item: &SelectItem) -> Result<()> {
        match item {
            SelectItem::Wildcard => Ok(()),
            SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
                self.resolve_column_in_scope(scope, name).map(|_| ())
            }
            SelectItem::Expr { expr, .. } => self.require_scalar_expr_scope(scope, expr),
            SelectItem::Aggregate { arg, func, .. } => match arg {
                AggregateArg::Wildcard => {
                    if *func == AggregateFunc::Count {
                        Ok(())
                    } else {
                        Err(DbError::plan(
                            "only COUNT supports wildcard aggregate argument",
                        ))
                    }
                }
                AggregateArg::Expr { expr, .. } => self.require_scalar_expr_scope(scope, expr),
            },
        }
    }

    fn require_aggregate_select_item_in_scope(
        &self,
        scope: &QueryScope,
        group_by: &[ScalarExpr],
        item: &SelectItem,
    ) -> Result<()> {
        match item {
            SelectItem::Wildcard => Err(DbError::plan(
                "wildcard cannot be used with GROUP BY or aggregate projections",
            )),
            SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
                self.resolve_column_in_scope(scope, name)?;
                if !self.group_by_contains_expr(group_by, &ScalarExpr::Column(name.clone())) {
                    return Err(DbError::plan(format!(
                        "non-aggregate column {name} must appear in GROUP BY"
                    )));
                }
                Ok(())
            }
            SelectItem::Expr { expr, .. } => {
                self.require_scalar_expr_scope(scope, expr)?;
                if !self.group_by_contains_expr(group_by, expr) {
                    return Err(DbError::plan(format!(
                        "non-aggregate expression {} must appear in GROUP BY",
                        self.scalar_expr_display(expr)
                    )));
                }
                Ok(())
            }
            SelectItem::Aggregate { arg, func, .. } => match arg {
                AggregateArg::Wildcard => {
                    if *func == AggregateFunc::Count {
                        Ok(())
                    } else {
                        Err(DbError::plan(
                            "only COUNT supports wildcard aggregate argument",
                        ))
                    }
                }
                AggregateArg::Expr { expr, .. } => self.require_scalar_expr_scope(scope, expr),
            },
        }
    }

    fn group_by_contains_expr(&self, group_by: &[ScalarExpr], expr: &ScalarExpr) -> bool {
        group_by.iter().any(|group_expr| {
            group_expr == expr
                || matches!((group_expr, expr), (ScalarExpr::Column(left), ScalarExpr::Column(right)) if left == right)
        })
    }

    fn scalar_expr_display(&self, expr: &ScalarExpr) -> String {
        match expr {
            ScalarExpr::Literal(value) => value.to_string(),
            ScalarExpr::Column(name) => name.clone(),
            ScalarExpr::UnaryMinus(expr) => format!("-{}", self.scalar_expr_display(expr)),
            ScalarExpr::Binary { left, op, right } => format!(
                "{} {} {}",
                self.scalar_expr_display(left),
                match op {
                    crate::sql::ast::ScalarBinaryOp::Add => "+",
                    crate::sql::ast::ScalarBinaryOp::Subtract => "-",
                    crate::sql::ast::ScalarBinaryOp::Multiply => "*",
                    crate::sql::ast::ScalarBinaryOp::Divide => "/",
                    crate::sql::ast::ScalarBinaryOp::Concat => "||",
                },
                self.scalar_expr_display(right)
            ),
            ScalarExpr::Function { func, args } => format!(
                "{}({})",
                match func {
                    crate::sql::ast::ScalarFunc::Length => "LENGTH",
                    crate::sql::ast::ScalarFunc::Lower => "LOWER",
                    crate::sql::ast::ScalarFunc::Upper => "UPPER",
                    crate::sql::ast::ScalarFunc::Abs => "ABS",
                    crate::sql::ast::ScalarFunc::Coalesce => "COALESCE",
                    crate::sql::ast::ScalarFunc::IfNull => "IFNULL",
                },
                args.iter()
                    .map(|arg| self.scalar_expr_display(arg))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn require_order_by_scope(
        &self,
        scope: &QueryScope,
        columns: &[SelectItem],
        item: &OrderBy,
    ) -> Result<()> {
        match &item.expr {
            OrderByExpr::Column(column) => {
                if self
                    .select_aliases(columns)
                    .iter()
                    .any(|alias| alias == column)
                {
                    return Ok(());
                }
                self.resolve_column_in_scope(scope, column).map(|_| ())
            }
            OrderByExpr::Expr(expr) => self.require_scalar_expr_scope(scope, expr),
            OrderByExpr::Position(_) => Ok(()),
        }
    }

    fn require_aggregate_order_by_references(
        &self,
        item: &OrderBy,
        columns: &[SelectItem],
        group_by: &[ScalarExpr],
    ) -> Result<()> {
        match &item.expr {
            OrderByExpr::Column(column) => {
                self.require_aggregate_column_reference(column, columns, group_by)
            }
            OrderByExpr::Expr(expr) => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            OrderByExpr::Position(_) => Ok(()),
        }
    }

    fn require_aggregate_expr_references(
        &self,
        expr: &Expr,
        columns: &[SelectItem],
        group_by: &[ScalarExpr],
    ) -> Result<()> {
        match expr {
            Expr::Compare { column, .. }
            | Expr::IsNull { column, .. }
            | Expr::Like { column, .. }
            | Expr::Between { column, .. }
            | Expr::InSubquery { column, .. }
            | Expr::CompareSubquery { column, .. } => {
                self.require_aggregate_column_reference(column, columns, group_by)
            }
            Expr::CompareColumns { left, right, .. } => {
                self.require_aggregate_column_reference(left, columns, group_by)?;
                self.require_aggregate_column_reference(right, columns, group_by)
            }
            Expr::CompareScalar { left, right, .. } => {
                self.require_aggregate_scalar_reference(left, columns, group_by)?;
                self.require_aggregate_scalar_reference(right, columns, group_by)
            }
            Expr::IsNullScalar { expr, .. }
            | Expr::LikeScalar { expr, .. }
            | Expr::InSubqueryScalar { expr, .. } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            Expr::BetweenScalar {
                expr, low, high, ..
            } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)?;
                self.require_aggregate_scalar_reference(low, columns, group_by)?;
                self.require_aggregate_scalar_reference(high, columns, group_by)
            }
            Expr::CompareSubqueryScalar { left, .. } => {
                self.require_aggregate_scalar_reference(left, columns, group_by)
            }
            Expr::ExistsSubquery { .. } => Ok(()),
            Expr::Not(expr) => self.require_aggregate_expr_references(expr, columns, group_by),
            Expr::And(left, right) | Expr::Or(left, right) => {
                self.require_aggregate_expr_references(left, columns, group_by)?;
                self.require_aggregate_expr_references(right, columns, group_by)
            }
        }
    }

    fn require_aggregate_scalar_reference(
        &self,
        expr: &ScalarExpr,
        columns: &[SelectItem],
        group_by: &[ScalarExpr],
    ) -> Result<()> {
        if self.group_by_contains_expr(group_by, expr) {
            return Ok(());
        }

        match expr {
            ScalarExpr::Literal(_) => Ok(()),
            ScalarExpr::Column(name) => {
                self.require_aggregate_column_reference(name, columns, group_by)
            }
            ScalarExpr::UnaryMinus(expr) => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            ScalarExpr::Binary { left, right, .. } => {
                self.require_aggregate_scalar_reference(left, columns, group_by)?;
                self.require_aggregate_scalar_reference(right, columns, group_by)
            }
            ScalarExpr::Function { args, .. } => {
                for arg in args {
                    self.require_aggregate_scalar_reference(arg, columns, group_by)?;
                }
                Ok(())
            }
        }
    }

    fn require_aggregate_column_reference(
        &self,
        column: &str,
        columns: &[SelectItem],
        group_by: &[ScalarExpr],
    ) -> Result<()> {
        if self
            .aggregate_reference_names(columns, group_by)
            .iter()
            .any(|name| name == column)
        {
            Ok(())
        } else {
            Err(DbError::plan(format!("unknown column {column}")))
        }
    }

    fn aggregate_reference_names(
        &self,
        columns: &[SelectItem],
        group_by: &[ScalarExpr],
    ) -> Vec<String> {
        let mut names = self.select_aliases(columns);

        for item in columns {
            match item {
                SelectItem::Column(name) => names.push(name.clone()),
                SelectItem::AliasedColumn { name, alias } => {
                    names.push(name.clone());
                    names.push(alias.clone());
                }
                SelectItem::Expr { expr, alias } => {
                    names.push(
                        alias
                            .clone()
                            .unwrap_or_else(|| self.scalar_expr_display(expr)),
                    );
                }
                SelectItem::Aggregate { func, arg, alias } => {
                    names.push(
                        alias
                            .clone()
                            .unwrap_or_else(|| self.aggregate_output_name(*func, arg)),
                    );
                }
                SelectItem::Wildcard => {}
            }
        }

        for expr in group_by {
            names.push(self.scalar_expr_display(expr));
        }

        names.sort();
        names.dedup();
        names
    }

    fn rewrite_aggregate_order_by_group_references(
        &self,
        item: &OrderBy,
        group_by: &[ScalarExpr],
    ) -> OrderBy {
        OrderBy {
            expr: match &item.expr {
                OrderByExpr::Column(column) => OrderByExpr::Column(column.clone()),
                OrderByExpr::Expr(expr) => OrderByExpr::Expr(
                    self.rewrite_aggregate_scalar_group_references(expr, group_by),
                ),
                OrderByExpr::Position(position) => OrderByExpr::Position(*position),
            },
            descending: item.descending,
            nulls: item.nulls,
        }
    }

    fn rewrite_aggregate_expr_group_references(
        &self,
        expr: &Expr,
        group_by: &[ScalarExpr],
    ) -> Expr {
        match expr {
            Expr::Compare { .. }
            | Expr::CompareColumns { .. }
            | Expr::IsNull { .. }
            | Expr::InSubquery { .. }
            | Expr::CompareSubquery { .. }
            | Expr::ExistsSubquery { .. }
            | Expr::Like { .. }
            | Expr::Between { .. } => expr.clone(),
            Expr::CompareScalar { left, op, right } => Expr::CompareScalar {
                left: self.rewrite_aggregate_scalar_group_references(left, group_by),
                op: *op,
                right: self.rewrite_aggregate_scalar_group_references(right, group_by),
            },
            Expr::IsNullScalar { expr, negated } => Expr::IsNullScalar {
                expr: self.rewrite_aggregate_scalar_group_references(expr, group_by),
                negated: *negated,
            },
            Expr::InSubqueryScalar {
                expr,
                query,
                negated,
            } => Expr::InSubqueryScalar {
                expr: self.rewrite_aggregate_scalar_group_references(expr, group_by),
                query: query.clone(),
                negated: *negated,
            },
            Expr::CompareSubqueryScalar { left, op, query } => Expr::CompareSubqueryScalar {
                left: self.rewrite_aggregate_scalar_group_references(left, group_by),
                op: *op,
                query: query.clone(),
            },
            Expr::LikeScalar {
                expr,
                pattern,
                negated,
            } => Expr::LikeScalar {
                expr: self.rewrite_aggregate_scalar_group_references(expr, group_by),
                pattern: pattern.clone(),
                negated: *negated,
            },
            Expr::BetweenScalar {
                expr,
                low,
                high,
                negated,
            } => Expr::BetweenScalar {
                expr: self.rewrite_aggregate_scalar_group_references(expr, group_by),
                low: self.rewrite_aggregate_scalar_group_references(low, group_by),
                high: self.rewrite_aggregate_scalar_group_references(high, group_by),
                negated: *negated,
            },
            Expr::Not(expr) => Expr::Not(Box::new(
                self.rewrite_aggregate_expr_group_references(expr, group_by),
            )),
            Expr::And(left, right) => Expr::And(
                Box::new(self.rewrite_aggregate_expr_group_references(left, group_by)),
                Box::new(self.rewrite_aggregate_expr_group_references(right, group_by)),
            ),
            Expr::Or(left, right) => Expr::Or(
                Box::new(self.rewrite_aggregate_expr_group_references(left, group_by)),
                Box::new(self.rewrite_aggregate_expr_group_references(right, group_by)),
            ),
        }
    }

    fn rewrite_aggregate_scalar_group_references(
        &self,
        expr: &ScalarExpr,
        group_by: &[ScalarExpr],
    ) -> ScalarExpr {
        if let Some(label) = group_by
            .iter()
            .find(|group_expr| *group_expr == expr)
            .map(|group_expr| self.scalar_expr_display(group_expr))
        {
            return ScalarExpr::Column(label);
        }

        match expr {
            ScalarExpr::Literal(_) | ScalarExpr::Column(_) => expr.clone(),
            ScalarExpr::UnaryMinus(expr) => ScalarExpr::UnaryMinus(Box::new(
                self.rewrite_aggregate_scalar_group_references(expr, group_by),
            )),
            ScalarExpr::Binary { left, op, right } => ScalarExpr::Binary {
                left: Box::new(self.rewrite_aggregate_scalar_group_references(left, group_by)),
                op: *op,
                right: Box::new(self.rewrite_aggregate_scalar_group_references(right, group_by)),
            },
            ScalarExpr::Function { func, args } => ScalarExpr::Function {
                func: *func,
                args: args
                    .iter()
                    .map(|arg| self.rewrite_aggregate_scalar_group_references(arg, group_by))
                    .collect(),
            },
        }
    }

    fn aggregate_output_name(&self, func: AggregateFunc, arg: &AggregateArg) -> String {
        format!(
            "{}({})",
            match func {
                AggregateFunc::Count => "COUNT",
                AggregateFunc::Sum => "SUM",
                AggregateFunc::Avg => "AVG",
                AggregateFunc::Min => "MIN",
                AggregateFunc::Max => "MAX",
            },
            match arg {
                AggregateArg::Wildcard => "*".to_string(),
                AggregateArg::Expr { expr, distinct } => {
                    if *distinct {
                        format!("DISTINCT {}", self.scalar_expr_display(expr))
                    } else {
                        self.scalar_expr_display(expr)
                    }
                }
            }
        )
    }

    fn require_scope_columns_with_outer(
        &self,
        scope: &QueryScope,
        outer_scope: Option<&QueryScope>,
        filter: &Expr,
    ) -> Result<()> {
        match filter {
            Expr::Compare { column, .. }
            | Expr::IsNull { column, .. }
            | Expr::InSubquery { column, .. }
            | Expr::CompareSubquery { column, .. }
            | Expr::Like { column, .. }
            | Expr::Between { column, .. } => self
                .resolve_column_in_scope_chain(scope, outer_scope, column)
                .map(|_| ()),
            Expr::ExistsSubquery { .. } => Ok(()),
            Expr::InSubqueryScalar { expr, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            Expr::CompareSubqueryScalar { left, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, left)
            }
            Expr::CompareColumns { left, right, .. } => {
                self.resolve_column_in_scope_chain(scope, outer_scope, left)?;
                self.resolve_column_in_scope_chain(scope, outer_scope, right)
                    .map(|_| ())
            }
            Expr::IsNullScalar { expr, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            Expr::LikeScalar { expr, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            Expr::BetweenScalar {
                expr, low, high, ..
            } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, low)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, high)
            }
            Expr::CompareScalar { left, right, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, left)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, right)
            }
            Expr::Not(expr) => self.require_scope_columns_with_outer(scope, outer_scope, expr),
            Expr::And(left, right) | Expr::Or(left, right) => {
                self.require_scope_columns_with_outer(scope, outer_scope, left)?;
                self.require_scope_columns_with_outer(scope, outer_scope, right)
            }
        }
    }

    fn validate_subqueries(
        &self,
        filter: &Expr,
        context: &PlanningContext,
        outer_scope: &QueryScope,
    ) -> Result<()> {
        match filter {
            Expr::InSubquery { query, .. }
            | Expr::InSubqueryScalar { query, .. }
            | Expr::CompareSubquery { query, .. }
            | Expr::CompareSubqueryScalar { query, .. }
            | Expr::ExistsSubquery { query, .. } => {
                self.validate_select_subquery(query, context, outer_scope)
            }
            Expr::Not(expr) => self.validate_subqueries(expr, context, outer_scope),
            Expr::And(left, right) | Expr::Or(left, right) => {
                self.validate_subqueries(left, context, outer_scope)?;
                self.validate_subqueries(right, context, outer_scope)
            }
            Expr::Compare { .. }
            | Expr::CompareColumns { .. }
            | Expr::CompareScalar { .. }
            | Expr::IsNull { .. }
            | Expr::IsNullScalar { .. }
            | Expr::LikeScalar { .. }
            | Expr::Like { .. }
            | Expr::Between { .. } => Ok(()),
            Expr::BetweenScalar { .. } => Ok(()),
        }
    }

    fn validate_select_subquery(
        &self,
        query: &SelectStatement,
        context: &PlanningContext,
        outer_scope: &QueryScope,
    ) -> Result<()> {
        let _ = self.plan_select_with_outer(query, context, Some(outer_scope))?;
        if query.columns.len() != 1 {
            return Err(DbError::plan("subquery must return exactly one column"));
        }
        Ok(())
    }

    fn resolve_column_in_scope_chain(
        &self,
        scope: &QueryScope,
        outer_scope: Option<&QueryScope>,
        column: &str,
    ) -> Result<(String, String)> {
        match self.resolve_column_in_scope(scope, column) {
            Ok(resolved) => Ok(resolved),
            Err(_) => {
                if let Some(outer_scope) = outer_scope {
                    self.resolve_column_in_scope(outer_scope, column)
                } else if self.allow_unresolved_outer_refs && column.contains('.') {
                    Ok((String::new(), column.to_string()))
                } else {
                    Err(DbError::plan(format!("unknown column {column}")))
                }
            }
        }
    }

    fn require_scalar_expr_scope_chain(
        &self,
        scope: &QueryScope,
        outer_scope: Option<&QueryScope>,
        expr: &ScalarExpr,
    ) -> Result<()> {
        match expr {
            ScalarExpr::Literal(_) => Ok(()),
            ScalarExpr::Column(name) => self
                .resolve_column_in_scope_chain(scope, outer_scope, name)
                .map(|_| ()),
            ScalarExpr::UnaryMinus(expr) => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            ScalarExpr::Binary { left, right, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, left)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, right)
            }
            ScalarExpr::Function { args, .. } => {
                for arg in args {
                    self.require_scalar_expr_scope_chain(scope, outer_scope, arg)?;
                }
                Ok(())
            }
        }
    }

    fn resolve_column_in_scope(
        &self,
        scope: &QueryScope,
        column: &str,
    ) -> Result<(String, String)> {
        if let Some((prefix, suffix)) = column.split_once('.') {
            for binding in &scope.bindings {
                if (binding.table == prefix || binding.alias.as_deref() == Some(prefix))
                    && binding
                        .schema
                        .columns
                        .iter()
                        .any(|entry| entry.name == suffix)
                {
                    return Ok((binding.table.clone(), suffix.to_string()));
                }
            }
            return Err(DbError::plan(format!("unknown column {column}")));
        }

        let mut matches = scope.bindings.iter().filter(|binding| {
            binding
                .schema
                .columns
                .iter()
                .any(|entry| entry.name == column)
        });
        let Some(first) = matches.next() else {
            return Err(DbError::plan(format!("unknown column {column}")));
        };
        if matches.next().is_some() {
            return Err(DbError::plan(format!(
                "ambiguous column reference: {column}"
            )));
        }
        Ok((first.table.clone(), column.to_string()))
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
}

#[derive(Debug, Clone)]
struct TableBinding {
    table: String,
    alias: Option<String>,
    schema: Schema,
}

#[derive(Debug, Clone)]
struct QueryScope {
    bindings: Vec<TableBinding>,
}
