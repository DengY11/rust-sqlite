use std::cmp::Ordering;
use std::collections::HashMap;

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Schema, Value};
use crate::sql::ast::{
    AggregateArg, AggregateFunc, Assignment, CompareOp, Expr, OrderBy, OrderByExpr, SelectItem,
    SelectStatement, Statement,
};
use crate::sql::plan::{IndexBound, IndexRange, IndexScanSpec, JoinPlan, Plan};

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
                columns,
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
            Statement::Begin => Ok(Plan::BeginTxn),
            Statement::Commit => Ok(Plan::CommitTxn),
            Statement::Rollback => Ok(Plan::RollbackTxn),
        }
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
            self.require_filter_columns(schema, expr)?;
            self.validate_subqueries(expr, context)?;
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
            self.require_filter_columns(schema, expr)?;
            self.validate_subqueries(expr, context)?;
        }

        Ok(Plan::Update {
            table: table.to_string(),
            assignments: normalized_assignments,
            filter: normalized_filter,
        })
    }

    fn plan_select(&self, select: &SelectStatement, context: &PlanningContext) -> Result<Plan> {
        let has_aggregates = self.select_has_aggregates(&select.columns);

        if !select.joins.is_empty() {
            let source = self.plan_join_source(select, context)?;
            if has_aggregates || !select.group_by.is_empty() {
                self.validate_aggregate_projection(select, context)?;
                return Ok(Plan::Aggregate {
                    source: Box::new(source),
                    columns: select.columns.clone(),
                    group_by: select.group_by.clone(),
                    having: select.having.clone(),
                    order_by: select.order_by.clone(),
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
            )?;
            return Ok(Plan::Aggregate {
                source: Box::new(source),
                columns: self.normalize_aggregate_select_items(
                    &select.table,
                    select.table_alias.as_deref(),
                    &select.columns,
                    context,
                )?,
                group_by: self.normalize_group_by(
                    &select.table,
                    select.table_alias.as_deref(),
                    &select.group_by,
                    context,
                )?,
                having: select.having.clone(),
                order_by: select.order_by.clone(),
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
        )
    }

    fn plan_single_table_source(
        &self,
        input: SingleTablePlanInput<'_>,
        context: &PlanningContext,
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
            self.require_filter_columns(schema, expr)?;
            self.validate_subqueries(expr, context)?;
        }

        let normalized_order_by = order_by
            .iter()
            .map(|item| {
                self.normalize_order_by(schema, table, table_alias, &normalized_columns, item)
            })
            .collect::<Result<Vec<_>>>()?;

        if self.is_plain_indexable_filter(normalized_filter.as_ref())
            && let Some(expr) = normalized_filter.as_ref()
        {
            if let Some(scans) = self.find_index_union_scans(context, table, expr) {
                return Ok(Plan::IndexUnion {
                    table: table.to_string(),
                    table_alias: table_alias.map(str::to_string),
                    columns: normalized_columns,
                    scans,
                    filter: normalized_filter,
                    order_by: normalized_order_by,
                    limit,
                    distinct,
                });
            }

            if let Some(scan) = self.find_matching_index_scan(context, table, expr) {
                return Ok(Plan::IndexScan {
                    table: table.to_string(),
                    table_alias: table_alias.map(str::to_string),
                    columns: normalized_columns,
                    index: scan.index,
                    key_prefix: scan.key_prefix,
                    range: scan.range,
                    filter: normalized_filter,
                    order_by: normalized_order_by,
                    limit,
                    distinct,
                });
            }
        }

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
    ) -> Result<Plan> {
        self.require_schema(context, &select.table)?;
        let scope = self.build_scope(select, context)?;

        for item in &select.columns {
            self.require_join_select_item(&scope, item)?;
        }
        if let Some(filter) = &select.filter {
            self.require_scope_columns(&scope, filter)?;
            self.validate_subqueries(filter, context)?;
        }
        for join in &select.joins {
            self.require_schema(context, &join.table)?;
            self.require_scope_columns(&scope, &join.on)?;
            self.validate_subqueries(&join.on, context)?;
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
                let mut row = vec![Value::Null; schema.columns.len()];
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
            SelectItem::Aggregate { func, arg, alias } => SelectItem::Aggregate {
                func: *func,
                arg: match arg {
                    AggregateArg::Wildcard => AggregateArg::Wildcard,
                    AggregateArg::Column { name, distinct } => AggregateArg::Column {
                        name: self.normalize_column_reference(schema, table, table_alias, name)?,
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
            return Ok(item.clone());
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
        })
    }

    fn select_aliases(&self, columns: &[SelectItem]) -> Vec<String> {
        columns
            .iter()
            .filter_map(|column| match column {
                SelectItem::AliasedColumn { alias, .. } => Some(alias.clone()),
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
            Expr::IsNull { column, negated } => Expr::IsNull {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
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
            Expr::CompareSubquery { column, op, query } => Expr::CompareSubquery {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
                op: *op,
                query: query.clone(),
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
        self.require_column(schema, column)?;
        Ok(column.to_string())
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
                AggregateArg::Column { name, .. } => self.require_column(schema, name),
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
        group_by: &[String],
        context: &PlanningContext,
    ) -> Result<Vec<String>> {
        let schema = self.require_schema(context, table)?;
        group_by
            .iter()
            .map(|column| self.normalize_column_reference(schema, table, table_alias, column))
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

        if !select.joins.is_empty() {
            let scope = self.build_scope(select, context)?;
            for item in &select.columns {
                self.require_join_select_item(&scope, item)?;
            }
            for column in &select.group_by {
                self.resolve_column_in_scope(&scope, column)?;
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
                    if !normalized_group_by.iter().any(|column| column == name) {
                        return Err(DbError::plan(format!(
                            "non-aggregate column {name} must appear in GROUP BY"
                        )));
                    }
                }
                SelectItem::Aggregate { func, arg, .. } => {
                    if matches!(func, AggregateFunc::Sum | AggregateFunc::Avg)
                        && matches!(arg, AggregateArg::Column { name: column, .. } if !schema.columns.iter().any(|entry| entry.name == *column && matches!(entry.column_type, crate::common::types::ColumnType::Integer)))
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

        if !normalized_group_by.is_empty() && !self.select_has_aggregates(&normalized_columns) {
            return Ok(());
        }

        Ok(())
    }

    fn is_plain_indexable_filter(&self, filter: Option<&Expr>) -> bool {
        filter.is_some_and(|expr| self.expr_is_plain_indexable(expr))
    }

    fn expr_is_plain_indexable(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Compare { .. } | Expr::IsNull { .. } => true,
            Expr::Not(inner) => self.expr_is_plain_indexable(inner),
            Expr::And(left, right) | Expr::Or(left, right) => {
                self.expr_is_plain_indexable(left) && self.expr_is_plain_indexable(right)
            }
            Expr::CompareColumns { .. }
            | Expr::InSubquery { .. }
            | Expr::CompareSubquery { .. }
            | Expr::Like { .. }
            | Expr::Between { .. } => false,
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

    fn require_join_select_item(&self, scope: &QueryScope, item: &SelectItem) -> Result<()> {
        match item {
            SelectItem::Wildcard => Ok(()),
            SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
                self.resolve_column_in_scope(scope, name).map(|_| ())
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
                AggregateArg::Column { name, .. } => {
                    self.resolve_column_in_scope(scope, name).map(|_| ())
                }
            },
        }
    }

    fn require_order_by_scope(
        &self,
        scope: &QueryScope,
        columns: &[SelectItem],
        item: &OrderBy,
    ) -> Result<()> {
        let OrderByExpr::Column(column) = &item.expr else {
            return Ok(());
        };
        if self
            .select_aliases(columns)
            .iter()
            .any(|alias| alias == column)
        {
            return Ok(());
        }
        self.resolve_column_in_scope(scope, column).map(|_| ())
    }

    fn require_scope_columns(&self, scope: &QueryScope, filter: &Expr) -> Result<()> {
        match filter {
            Expr::Compare { column, .. }
            | Expr::IsNull { column, .. }
            | Expr::InSubquery { column, .. }
            | Expr::CompareSubquery { column, .. }
            | Expr::Like { column, .. }
            | Expr::Between { column, .. } => {
                self.resolve_column_in_scope(scope, column).map(|_| ())
            }
            Expr::CompareColumns { left, right, .. } => {
                self.resolve_column_in_scope(scope, left)?;
                self.resolve_column_in_scope(scope, right).map(|_| ())
            }
            Expr::Not(expr) => self.require_scope_columns(scope, expr),
            Expr::And(left, right) | Expr::Or(left, right) => {
                self.require_scope_columns(scope, left)?;
                self.require_scope_columns(scope, right)
            }
        }
    }

    fn validate_subqueries(&self, filter: &Expr, context: &PlanningContext) -> Result<()> {
        match filter {
            Expr::InSubquery { query, .. } | Expr::CompareSubquery { query, .. } => {
                self.validate_select_subquery(query, context)
            }
            Expr::Not(expr) => self.validate_subqueries(expr, context),
            Expr::And(left, right) | Expr::Or(left, right) => {
                self.validate_subqueries(left, context)?;
                self.validate_subqueries(right, context)
            }
            Expr::Compare { .. }
            | Expr::CompareColumns { .. }
            | Expr::IsNull { .. }
            | Expr::Like { .. }
            | Expr::Between { .. } => Ok(()),
        }
    }

    fn validate_select_subquery(
        &self,
        query: &SelectStatement,
        context: &PlanningContext,
    ) -> Result<()> {
        let _ = self.plan_select(query, context)?;
        if query.columns.len() != 1 {
            return Err(DbError::plan("subquery must return exactly one column"));
        }
        Ok(())
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

    fn find_matching_index_scan(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<IndexScanSpec> {
        let (index, key_prefix, range) = self.find_matching_index(context, table, filter)?;
        Some(IndexScanSpec {
            index: index.name.clone(),
            key_prefix,
            range,
        })
    }

    fn find_index_union_scans(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<Vec<IndexScanSpec>> {
        let mut branches = Vec::new();
        self.collect_or_branches(filter, &mut branches);
        if branches.len() < 2 {
            return None;
        }

        branches
            .into_iter()
            .map(|branch| self.find_matching_index_scan(context, table, branch))
            .collect()
    }

    fn collect_or_branches<'a>(&self, expr: &'a Expr, branches: &mut Vec<&'a Expr>) {
        match expr {
            Expr::Or(left, right) => {
                self.collect_or_branches(left, branches);
                self.collect_or_branches(right, branches);
            }
            _ => branches.push(expr),
        }
    }

    fn find_matching_index<'a>(
        &self,
        context: &'a PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<(&'a IndexMeta, Vec<Value>, Option<IndexRange>)> {
        let predicate_summary = self.extract_conjunctive_terms(filter)?;
        context
            .indexes_for(table)
            .iter()
            .filter_map(|index| {
                let key_prefix = index
                    .columns
                    .iter()
                    .map_while(|column| predicate_summary.equality_terms.get(column).cloned())
                    .collect::<Vec<_>>();
                let range = index.columns.get(key_prefix.len()).and_then(|column| {
                    let bounds = predicate_summary.range_terms.get(column)?;
                    (bounds.lower.is_some() || bounds.upper.is_some()).then(|| IndexRange {
                        column: column.clone(),
                        lower: bounds.lower.as_ref().map(|(op, value)| IndexBound {
                            op: *op,
                            value: value.clone(),
                        }),
                        upper: bounds.upper.as_ref().map(|(op, value)| IndexBound {
                            op: *op,
                            value: value.clone(),
                        }),
                    })
                });
                (!key_prefix.is_empty() || range.is_some()).then_some((index, key_prefix, range))
            })
            .max_by_key(|(_, key_prefix, range)| (key_prefix.len(), range.is_some()))
    }

    fn extract_conjunctive_terms(&self, expr: &Expr) -> Option<PredicateSummary> {
        let mut summary = PredicateSummary::default();
        self.collect_conjunctive_terms(expr, &mut summary)
            .then_some(summary)
    }

    fn collect_conjunctive_terms(&self, expr: &Expr, summary: &mut PredicateSummary) -> bool {
        match expr {
            Expr::Compare {
                column,
                op: CompareOp::Eq,
                value,
            } => {
                summary
                    .equality_terms
                    .entry(column.clone())
                    .or_insert_with(|| value.clone());
                true
            }
            Expr::Compare { column, op, value } => {
                let entry = summary.range_terms.entry(column.clone()).or_default();
                match op {
                    CompareOp::Gt | CompareOp::Gte => self.tighten_lower_bound(entry, *op, value),
                    CompareOp::Lt | CompareOp::Lte => self.tighten_upper_bound(entry, *op, value),
                    CompareOp::Ne => {}
                    CompareOp::Eq => unreachable!("equality branch handled above"),
                }
                true
            }
            Expr::CompareColumns { .. }
            | Expr::InSubquery { .. }
            | Expr::CompareSubquery { .. }
            | Expr::Like { .. }
            | Expr::Between { .. } => false,
            Expr::IsNull { .. } => true,
            Expr::Not(_) => false,
            Expr::Or(_, _) => false,
            Expr::And(left, right) => {
                self.collect_conjunctive_terms(left, summary)
                    && self.collect_conjunctive_terms(right, summary)
            }
        }
    }

    fn tighten_lower_bound(&self, entry: &mut RangeBounds, op: CompareOp, value: &Value) {
        match entry.lower.as_ref() {
            None => entry.lower = Some((op, value.clone())),
            Some((current_op, current)) => match self.compare_values(current, value) {
                Some(Ordering::Less) => entry.lower = Some((op, value.clone())),
                Some(Ordering::Equal)
                    if Self::lower_bound_strictness(op)
                        > Self::lower_bound_strictness(*current_op) =>
                {
                    entry.lower = Some((op, value.clone()));
                }
                _ => {}
            },
        }
    }

    fn tighten_upper_bound(&self, entry: &mut RangeBounds, op: CompareOp, value: &Value) {
        match entry.upper.as_ref() {
            None => entry.upper = Some((op, value.clone())),
            Some((current_op, current)) => match self.compare_values(current, value) {
                Some(Ordering::Greater) => entry.upper = Some((op, value.clone())),
                Some(Ordering::Equal)
                    if Self::upper_bound_strictness(op)
                        > Self::upper_bound_strictness(*current_op) =>
                {
                    entry.upper = Some((op, value.clone()));
                }
                _ => {}
            },
        }
    }

    fn lower_bound_strictness(op: CompareOp) -> u8 {
        match op {
            CompareOp::Gt => 2,
            CompareOp::Gte => 1,
            _ => 0,
        }
    }

    fn upper_bound_strictness(op: CompareOp) -> u8 {
        match op {
            CompareOp::Lt => 2,
            CompareOp::Lte => 1,
            _ => 0,
        }
    }

    fn compare_values(&self, left: &Value, right: &Value) -> Option<Ordering> {
        match (left, right) {
            (Value::Null, Value::Null) => Some(Ordering::Equal),
            (Value::Boolean(left), Value::Boolean(right)) => Some(left.cmp(right)),
            (Value::Integer(left), Value::Integer(right)) => Some(left.cmp(right)),
            (Value::Text(left), Value::Text(right)) => Some(left.cmp(right)),
            _ => None,
        }
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
            Expr::Compare { column, .. } => self.require_column(schema, column),
            Expr::CompareColumns { left, right, .. } => {
                self.require_column(schema, left)?;
                self.require_column(schema, right)
            }
            Expr::IsNull { column, .. } => self.require_column(schema, column),
            Expr::InSubquery { column, .. }
            | Expr::CompareSubquery { column, .. }
            | Expr::Like { column, .. }
            | Expr::Between { column, .. } => self.require_column(schema, column),
            Expr::Not(expr) => self.require_filter_columns(schema, expr),
            Expr::And(left, right) | Expr::Or(left, right) => {
                self.require_filter_columns(schema, left)?;
                self.require_filter_columns(schema, right)
            }
        }
    }
}

#[derive(Debug, Default)]
struct PredicateSummary {
    equality_terms: HashMap<String, Value>,
    range_terms: HashMap<String, RangeBounds>,
}

#[derive(Debug, Default)]
struct RangeBounds {
    lower: Option<(CompareOp, Value)>,
    upper: Option<(CompareOp, Value)>,
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
