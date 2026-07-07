use std::collections::HashMap;

use crate::common::error::{DbError, Result};
use crate::common::types::{ColumnDef, ColumnType, IndexMeta, Schema, Value};
use crate::sql::ast::{
    AggregateArg, AggregateFunc, AlterTableAction, Assignment, CompareOp, CompoundOperator,
    CteBody, Expr, FromItem, IsolationLevel, NullOrder, OrderBy, OrderByExpr,
    SINGLE_ROW_SOURCE_TABLE, ScalarExpr, SelectItem, SelectStatement, Statement, UpsertClause,
    WithClause,
};
use crate::sql::parser::parse_scalar_sql_expression;
use crate::sql::plan::{JoinPlan, Plan};

const VALUES_SOURCE_TABLE: &str = "__rustsql_values__";

#[derive(Debug, Clone, Default)]
pub struct PlanningContext {
    schemas: HashMap<String, Schema>,
    indexes: HashMap<String, Vec<IndexMeta>>,
}

struct SingleTablePlanInput<'a> {
    table: &'a str,
    table_alias: Option<&'a str>,
    index_hint: TableIndexHintRef<'a>,
    columns: &'a [SelectItem],
    filter: &'a Option<Expr>,
    order_by: &'a [OrderBy],
    limit: Option<usize>,
    offset: Option<usize>,
    distinct: bool,
}

#[derive(Debug, Clone, Copy)]
enum TableIndexHintRef<'a> {
    None,
    IndexedBy(&'a str),
    NotIndexed,
}

struct DerivedSourcePlanInput<'a> {
    alias: &'a str,
    source: Plan,
    output_columns: Vec<String>,
    columns: &'a [SelectItem],
    filter: &'a Option<Expr>,
    order_by: &'a [OrderBy],
    limit: Option<usize>,
    offset: Option<usize>,
    distinct: bool,
}

#[derive(Debug, Clone)]
struct LoweredCte {
    columns: Option<Vec<String>>,
    query: CteBody,
}

type CteRegistry = HashMap<String, LoweredCte>;

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

    fn sqlite_catalog_schema(table: &str) -> Option<Schema> {
        if !matches!(table, "sqlite_master" | "sqlite_schema") {
            return None;
        }
        Some(Schema::new(
            table,
            vec![
                ColumnDef::new("type", ColumnType::Text),
                ColumnDef::new("name", ColumnType::Text),
                ColumnDef::new("tbl_name", ColumnType::Text),
                ColumnDef::new("rootpage", ColumnType::Integer),
                ColumnDef::new("sql", ColumnType::Text),
            ],
        ))
    }

    fn single_row_source_schema(table: &str) -> Option<Schema> {
        (table == SINGLE_ROW_SOURCE_TABLE).then(|| Schema::new(table, vec![]))
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
            Statement::WithDml { with, statement } => {
                self.plan_with_dml(with, statement.as_ref(), context)
            }
            Statement::CreateTable {
                name,
                columns,
                constraints,
                strict,
                without_rowid,
                if_not_exists,
            } => Ok(Plan::CreateTable {
                name: name.clone(),
                columns: columns.clone(),
                constraints: constraints.clone(),
                strict: *strict,
                without_rowid: *without_rowid,
                if_not_exists: *if_not_exists,
            }),
            Statement::CreateTableAs {
                name,
                if_not_exists,
                select,
            } => {
                if context.schema(name).is_some() {
                    if *if_not_exists {
                        return Ok(Plan::NoOp);
                    }
                    return Err(DbError::plan(format!("table {name} already exists")));
                }
                Ok(Plan::CreateTableAs {
                    name: name.clone(),
                    if_not_exists: *if_not_exists,
                    source: Box::new(self.plan_select(select, context)?),
                })
            }
            Statement::CreateIndex {
                name,
                table,
                columns,
                decorated_columns,
                unique,
                predicate,
                if_not_exists,
            } => {
                let schema = self.require_schema(context, table)?;
                if columns.is_empty() {
                    return Err(DbError::plan("index must define at least one column"));
                }
                let mut seen = std::collections::BTreeSet::new();
                for column in columns {
                    self.require_index_term(schema, table, column)?;
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
                    decorated_columns: decorated_columns.clone(),
                    unique: *unique,
                    predicate: predicate.clone(),
                    if_not_exists: *if_not_exists,
                })
            }
            Statement::DropTable { name, if_exists } => {
                if context.schema(name).is_none() {
                    return if *if_exists {
                        Ok(Plan::NoOp)
                    } else {
                        Err(DbError::plan(format!("unknown table: {name}")))
                    };
                }
                Ok(Plan::DropTable {
                    name: name.clone(),
                    if_exists: *if_exists,
                })
            }
            Statement::DropIndex { name, if_exists } => {
                let table = match self.resolve_index_table(context, name) {
                    Ok(table) => table,
                    Err(error)
                        if *if_exists
                            && error.to_string()
                                == format!("plan error: unknown index: {name}") =>
                    {
                        return Ok(Plan::NoOp);
                    }
                    Err(error) => return Err(error),
                };
                Ok(Plan::DropIndex {
                    table,
                    name: name.clone(),
                    if_exists: *if_exists,
                })
            }
            Statement::AlterTable { table, action } => {
                self.plan_alter_table(table, action, context)
            }
            Statement::Insert {
                table,
                columns,
                or_conflict,
                values,
            } => self.plan_insert(
                table,
                columns.as_deref(),
                or_conflict.as_deref(),
                values,
                context,
            ),
            Statement::InsertReturning {
                table,
                columns,
                or_conflict,
                values,
                returning,
            } => self.plan_insert_returning(
                table,
                columns.as_deref(),
                or_conflict.as_deref(),
                values,
                returning,
                context,
            ),
            Statement::InsertUpsert {
                table,
                columns,
                values,
                upsert,
            } => self.plan_insert_upsert(table, columns.as_deref(), values, upsert, context),
            Statement::InsertUpsertReturning {
                table,
                columns,
                values,
                upsert,
                returning,
            } => self.plan_insert_upsert_returning(
                table,
                columns.as_deref(),
                values,
                upsert,
                returning,
                context,
            ),
            Statement::InsertMany {
                table,
                columns,
                or_conflict,
                rows,
            } => self.plan_insert_many(
                table,
                columns.as_deref(),
                or_conflict.as_deref(),
                rows,
                context,
            ),
            Statement::InsertManyReturning {
                table,
                columns,
                or_conflict,
                rows,
                returning,
            } => self.plan_insert_many_returning(
                table,
                columns.as_deref(),
                or_conflict.as_deref(),
                rows,
                returning,
                context,
            ),
            Statement::InsertManyUpsert {
                table,
                columns,
                rows,
                upsert,
            } => self.plan_insert_many_upsert(table, columns.as_deref(), rows, upsert, context),
            Statement::InsertManyUpsertReturning {
                table,
                columns,
                rows,
                upsert,
                returning,
            } => self.plan_insert_many_upsert_returning(
                table,
                columns.as_deref(),
                rows,
                upsert,
                returning,
                context,
            ),
            Statement::InsertDoNothing {
                table,
                columns,
                target,
                values,
            } => self.plan_insert_do_nothing(
                table,
                columns.as_deref(),
                target.as_deref(),
                values,
                context,
            ),
            Statement::InsertDoNothingReturning {
                table,
                columns,
                target,
                values,
                returning,
            } => self.plan_insert_do_nothing_returning(
                table,
                columns.as_deref(),
                target.as_deref(),
                values,
                returning,
                context,
            ),
            Statement::InsertManyDoNothing {
                table,
                columns,
                target,
                rows,
            } => self.plan_insert_many_do_nothing(
                table,
                columns.as_deref(),
                target.as_deref(),
                rows,
                context,
            ),
            Statement::InsertManyDoNothingReturning {
                table,
                columns,
                target,
                rows,
                returning,
            } => self.plan_insert_many_do_nothing_returning(
                table,
                columns.as_deref(),
                target.as_deref(),
                rows,
                returning,
                context,
            ),
            Statement::InsertExpr {
                table,
                columns,
                or_conflict,
                values,
            } => self.plan_insert_expr(
                table,
                columns.as_deref(),
                or_conflict.as_deref(),
                values,
                context,
            ),
            Statement::InsertExprReturning {
                table,
                columns,
                or_conflict,
                values,
                returning,
            } => self.plan_insert_expr_returning(
                table,
                columns.as_deref(),
                or_conflict.as_deref(),
                values,
                returning,
                context,
            ),
            Statement::InsertExprUpsert {
                table,
                columns,
                values,
                upsert,
            } => self.plan_insert_expr_upsert(table, columns.as_deref(), values, upsert, context),
            Statement::InsertExprUpsertReturning {
                table,
                columns,
                values,
                upsert,
                returning,
            } => self.plan_insert_expr_upsert_returning(
                table,
                columns.as_deref(),
                values,
                upsert,
                returning,
                context,
            ),
            Statement::InsertManyExpr {
                table,
                columns,
                or_conflict,
                rows,
            } => self.plan_insert_many_expr(
                table,
                columns.as_deref(),
                or_conflict.as_deref(),
                rows,
                context,
            ),
            Statement::InsertManyExprReturning {
                table,
                columns,
                or_conflict,
                rows,
                returning,
            } => self.plan_insert_many_expr_returning(
                table,
                columns.as_deref(),
                or_conflict.as_deref(),
                rows,
                returning,
                context,
            ),
            Statement::InsertManyExprUpsert {
                table,
                columns,
                rows,
                upsert,
            } => {
                self.plan_insert_many_expr_upsert(table, columns.as_deref(), rows, upsert, context)
            }
            Statement::InsertManyExprUpsertReturning {
                table,
                columns,
                rows,
                upsert,
                returning,
            } => self.plan_insert_many_expr_upsert_returning(
                table,
                columns.as_deref(),
                rows,
                upsert,
                returning,
                context,
            ),
            Statement::InsertExprDoNothing {
                table,
                columns,
                target,
                values,
            } => self.plan_insert_expr_do_nothing(
                table,
                columns.as_deref(),
                target.as_deref(),
                values,
                context,
            ),
            Statement::InsertExprDoNothingReturning {
                table,
                columns,
                target,
                values,
                returning,
            } => self.plan_insert_expr_do_nothing_returning(
                table,
                columns.as_deref(),
                target.as_deref(),
                values,
                returning,
                context,
            ),
            Statement::InsertManyExprDoNothing {
                table,
                columns,
                target,
                rows,
            } => self.plan_insert_many_expr_do_nothing(
                table,
                columns.as_deref(),
                target.as_deref(),
                rows,
                context,
            ),
            Statement::InsertManyExprDoNothingReturning {
                table,
                columns,
                target,
                rows,
                returning,
            } => self.plan_insert_many_expr_do_nothing_returning(
                table,
                columns.as_deref(),
                target.as_deref(),
                rows,
                returning,
                context,
            ),
            Statement::InsertSelect {
                table,
                columns,
                or_conflict,
                select,
            } => self.plan_insert_select(
                table,
                columns.as_deref(),
                or_conflict.as_deref(),
                select,
                context,
            ),
            Statement::InsertSelectReturning {
                table,
                columns,
                or_conflict,
                select,
                returning,
            } => self.plan_insert_select_returning(
                table,
                columns.as_deref(),
                or_conflict.as_deref(),
                select,
                returning,
                context,
            ),
            Statement::InsertSelectUpsert {
                table,
                columns,
                select,
                upsert,
            } => self.plan_insert_select_upsert(table, columns.as_deref(), select, upsert, context),
            Statement::InsertSelectUpsertReturning {
                table,
                columns,
                select,
                upsert,
                returning,
            } => self.plan_insert_select_upsert_returning(
                table,
                columns.as_deref(),
                select,
                upsert,
                returning,
                context,
            ),
            Statement::InsertSelectDoNothing {
                table,
                columns,
                target,
                select,
            } => self.plan_insert_select_do_nothing(
                table,
                columns.as_deref(),
                target.as_deref(),
                select,
                context,
            ),
            Statement::InsertSelectDoNothingReturning {
                table,
                columns,
                target,
                select,
                returning,
            } => self.plan_insert_select_do_nothing_returning(
                table,
                columns.as_deref(),
                target.as_deref(),
                select,
                returning,
                context,
            ),
            Statement::Delete {
                table,
                table_alias,
                filter,
            } => self.plan_delete(table, table_alias.as_deref(), filter, context),
            Statement::DeleteLimited {
                table,
                table_alias,
                filter,
                order_by,
                limit,
                offset,
            } => self.plan_delete_limited(
                table,
                table_alias.as_deref(),
                filter,
                order_by,
                *limit,
                *offset,
                context,
            ),
            Statement::DeleteReturning {
                table,
                table_alias,
                filter,
                returning,
            } => self.plan_delete_returning(
                table,
                table_alias.as_deref(),
                filter,
                returning,
                &[],
                None,
                None,
                context,
            ),
            Statement::DeleteReturningLimited {
                table,
                table_alias,
                filter,
                returning,
                order_by,
                limit,
                offset,
            } => self.plan_delete_returning(
                table,
                table_alias.as_deref(),
                filter,
                returning,
                order_by,
                *limit,
                *offset,
                context,
            ),
            Statement::Update {
                table,
                table_alias,
                assignments,
                filter,
            } => self.plan_update(table, table_alias.as_deref(), assignments, filter, context),
            Statement::UpdateLimited {
                table,
                table_alias,
                assignments,
                filter,
                order_by,
                limit,
                offset,
            } => self.plan_update_limited(
                table,
                table_alias.as_deref(),
                assignments,
                filter,
                order_by,
                *limit,
                *offset,
                context,
            ),
            Statement::UpdateReturning {
                table,
                table_alias,
                assignments,
                filter,
                returning,
            } => self.plan_update_returning(
                table,
                table_alias.as_deref(),
                assignments,
                filter,
                returning,
                &[],
                None,
                None,
                context,
            ),
            Statement::UpdateReturningLimited {
                table,
                table_alias,
                assignments,
                filter,
                returning,
                order_by,
                limit,
                offset,
            } => self.plan_update_returning(
                table,
                table_alias.as_deref(),
                assignments,
                filter,
                returning,
                order_by,
                *limit,
                *offset,
                context,
            ),
            Statement::Values(rows) => Ok(Plan::Values { rows: rows.clone() }),
            Statement::ValuesWith { with, rows } => {
                let ctes = self.cte_registry_from_with(with)?;
                Ok(Plan::Values {
                    rows: self.lower_cte_value_rows(rows, &ctes)?,
                })
            }
            Statement::Select(select) => self.plan_select(select, context),
            Statement::ExplainQueryPlan(statement) => Ok(Plan::ExplainQueryPlan {
                plan: Box::new(self.plan_statement(statement, context)?),
            }),
            Statement::Analyze | Statement::Reindex | Statement::Vacuum => Ok(Plan::NoOp),
            Statement::PragmaTableInfo { table } => Ok(Plan::PragmaTableInfo {
                table: table.clone(),
            }),
            Statement::PragmaTableXInfo { table } => Ok(Plan::PragmaTableXInfo {
                table: table.clone(),
            }),
            Statement::PragmaTableList { table, schema } => Ok(Plan::PragmaTableList {
                table: table.clone(),
                schema: schema.clone(),
            }),
            Statement::PragmaIndexList { table } => Ok(Plan::PragmaIndexList {
                table: table.clone(),
            }),
            Statement::PragmaIndexInfo { index } => Ok(Plan::PragmaIndexInfo {
                index: index.clone(),
            }),
            Statement::PragmaIndexXInfo { index } => Ok(Plan::PragmaIndexXInfo {
                index: index.clone(),
            }),
            Statement::PragmaForeignKeyList { table } => Ok(Plan::PragmaForeignKeyList {
                table: table.clone(),
            }),
            Statement::PragmaForeignKeyCheck { table } => Ok(Plan::PragmaForeignKeyCheck {
                table: table.clone(),
            }),
            Statement::PragmaForeignKeys => Ok(Plan::PragmaForeignKeys),
            Statement::SetPragmaForeignKeys { enabled } => {
                Ok(Plan::SetPragmaForeignKeys { enabled: *enabled })
            }
            Statement::PragmaReadUncommitted => Ok(Plan::PragmaReadUncommitted),
            Statement::SetPragmaReadUncommitted { enabled } => {
                Ok(Plan::SetPragmaReadUncommitted { enabled: *enabled })
            }
            Statement::PragmaQueryOnly => Ok(Plan::PragmaQueryOnly),
            Statement::SetPragmaQueryOnly { enabled } => {
                Ok(Plan::SetPragmaQueryOnly { enabled: *enabled })
            }
            Statement::PragmaRecursiveTriggers => Ok(Plan::PragmaRecursiveTriggers),
            Statement::SetPragmaRecursiveTriggers { enabled } => {
                Ok(Plan::SetPragmaRecursiveTriggers { enabled: *enabled })
            }
            Statement::PragmaTrustedSchema => Ok(Plan::PragmaTrustedSchema),
            Statement::SetPragmaTrustedSchema { enabled } => {
                Ok(Plan::SetPragmaTrustedSchema { enabled: *enabled })
            }
            Statement::PragmaIgnoreCheckConstraints => Ok(Plan::PragmaIgnoreCheckConstraints),
            Statement::SetPragmaIgnoreCheckConstraints { enabled } => {
                Ok(Plan::SetPragmaIgnoreCheckConstraints { enabled: *enabled })
            }
            Statement::PragmaEncoding => Ok(Plan::PragmaEncoding),
            Statement::PragmaCollationList => Ok(Plan::PragmaCollationList),
            Statement::PragmaDataVersion => Ok(Plan::PragmaDataVersion),
            Statement::PragmaQuickCheck => Ok(Plan::PragmaQuickCheck),
            Statement::PragmaIntegrityCheck => Ok(Plan::PragmaIntegrityCheck),
            Statement::PragmaFunctionList => Ok(Plan::PragmaFunctionList),
            Statement::PragmaCompileOptions => Ok(Plan::PragmaCompileOptions),
            Statement::PragmaJournalMode => Ok(Plan::PragmaJournalMode),
            Statement::PragmaSynchronous => Ok(Plan::PragmaSynchronous),
            Statement::PragmaCacheSize => Ok(Plan::PragmaCacheSize),
            Statement::SetPragmaCacheSize { value } => {
                Ok(Plan::SetPragmaCacheSize { value: *value })
            }
            Statement::PragmaTempStore => Ok(Plan::PragmaTempStore),
            Statement::PragmaLockingMode => Ok(Plan::PragmaLockingMode),
            Statement::PragmaBusyTimeout => Ok(Plan::PragmaBusyTimeout),
            Statement::SetPragmaBusyTimeout { value } => {
                Ok(Plan::SetPragmaBusyTimeout { value: *value })
            }
            Statement::PragmaThreads => Ok(Plan::PragmaThreads),
            Statement::SetPragmaThreads { value } => Ok(Plan::SetPragmaThreads { value: *value }),
            Statement::PragmaCaseSensitiveLike => Ok(Plan::PragmaCaseSensitiveLike),
            Statement::SetPragmaCaseSensitiveLike { enabled } => {
                Ok(Plan::SetPragmaCaseSensitiveLike { enabled: *enabled })
            }
            Statement::PragmaReverseUnorderedSelects => Ok(Plan::PragmaReverseUnorderedSelects),
            Statement::SetPragmaReverseUnorderedSelects { enabled } => {
                Ok(Plan::SetPragmaReverseUnorderedSelects { enabled: *enabled })
            }
            Statement::PragmaOptimize => Ok(Plan::NoOp),
            Statement::PragmaDatabaseList => Ok(Plan::PragmaDatabaseList),
            Statement::PragmaPageSize => Ok(Plan::PragmaPageSize),
            Statement::PragmaPageCount => Ok(Plan::PragmaPageCount),
            Statement::PragmaFreelistCount => Ok(Plan::PragmaFreelistCount),
            Statement::PragmaUserVersion => Ok(Plan::PragmaUserVersion),
            Statement::SetPragmaUserVersion { value } => {
                Ok(Plan::SetPragmaUserVersion { value: *value })
            }
            Statement::PragmaApplicationId => Ok(Plan::PragmaApplicationId),
            Statement::SetPragmaApplicationId { value } => {
                Ok(Plan::SetPragmaApplicationId { value: *value })
            }
            Statement::PragmaSchemaVersion => Ok(Plan::PragmaSchemaVersion),
            Statement::SetPragmaSchemaVersion { value } => {
                Ok(Plan::SetPragmaSchemaVersion { value: *value })
            }
            Statement::Begin { isolation_level } => Ok(Plan::BeginTxn {
                isolation_level: isolation_level.unwrap_or(IsolationLevel::ReadCommitted),
            }),
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
            AlterTableAction::DropColumn { old_name } => {
                self.require_column(schema, old_name)?;
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
        or_conflict: Option<&str>,
        values: &[Value],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let row = self.build_insert_row(schema, table, columns, values)?;
        Ok(Plan::Insert {
            table: table.to_string(),
            or_conflict: or_conflict.map(str::to_string),
            values: row,
        })
    }

    fn plan_insert_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        or_conflict: Option<&str>,
        values: &[Value],
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let row = self.build_insert_row(schema, table, columns, values)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertReturning {
            table: table.to_string(),
            or_conflict: or_conflict.map(str::to_string),
            values: row,
            returning,
        })
    }

    fn plan_insert_many(
        &self,
        table: &str,
        columns: Option<&[String]>,
        or_conflict: Option<&str>,
        rows: &[Vec<Value>],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let planned_rows = rows
            .iter()
            .map(|values| self.build_insert_row(schema, table, columns, values))
            .collect::<Result<Vec<_>>>()?;
        Ok(Plan::InsertMany {
            table: table.to_string(),
            or_conflict: or_conflict.map(str::to_string),
            rows: planned_rows,
        })
    }

    fn plan_insert_many_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        or_conflict: Option<&str>,
        rows: &[Vec<Value>],
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let planned_rows = rows
            .iter()
            .map(|values| self.build_insert_row(schema, table, columns, values))
            .collect::<Result<Vec<_>>>()?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertManyReturning {
            table: table.to_string(),
            or_conflict: or_conflict.map(str::to_string),
            rows: planned_rows,
            returning,
        })
    }

    fn plan_insert_upsert(
        &self,
        table: &str,
        columns: Option<&[String]>,
        values: &[Value],
        upsert: &UpsertClause,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let row = self.build_insert_row(schema, table, columns, values)?;
        let upsert = self.plan_upsert_clause(schema, table, upsert, context)?;
        Ok(Plan::InsertUpsert {
            table: table.to_string(),
            values: row,
            upsert,
        })
    }

    fn plan_insert_upsert_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        values: &[Value],
        upsert: &UpsertClause,
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let row = self.build_insert_row(schema, table, columns, values)?;
        let upsert = self.plan_upsert_clause(schema, table, upsert, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertUpsertReturning {
            table: table.to_string(),
            values: row,
            upsert,
            returning,
        })
    }

    fn plan_insert_many_upsert(
        &self,
        table: &str,
        columns: Option<&[String]>,
        rows: &[Vec<Value>],
        upsert: &UpsertClause,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let planned_rows = rows
            .iter()
            .map(|values| self.build_insert_row(schema, table, columns, values))
            .collect::<Result<Vec<_>>>()?;
        let upsert = self.plan_upsert_clause(schema, table, upsert, context)?;
        Ok(Plan::InsertManyUpsert {
            table: table.to_string(),
            rows: planned_rows,
            upsert,
        })
    }

    fn plan_insert_many_upsert_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        rows: &[Vec<Value>],
        upsert: &UpsertClause,
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let planned_rows = rows
            .iter()
            .map(|values| self.build_insert_row(schema, table, columns, values))
            .collect::<Result<Vec<_>>>()?;
        let upsert = self.plan_upsert_clause(schema, table, upsert, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertManyUpsertReturning {
            table: table.to_string(),
            rows: planned_rows,
            upsert,
            returning,
        })
    }

    fn plan_insert_do_nothing(
        &self,
        table: &str,
        columns: Option<&[String]>,
        target: Option<&[String]>,
        values: &[Value],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let row = self.build_insert_row(schema, table, columns, values)?;
        self.validate_do_nothing_target(schema, table, target, context)?;
        Ok(Plan::InsertDoNothing {
            table: table.to_string(),
            target: target.map(|columns| columns.to_vec()),
            values: row,
        })
    }

    fn plan_insert_do_nothing_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        target: Option<&[String]>,
        values: &[Value],
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let row = self.build_insert_row(schema, table, columns, values)?;
        self.validate_do_nothing_target(schema, table, target, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertDoNothingReturning {
            table: table.to_string(),
            target: target.map(|columns| columns.to_vec()),
            values: row,
            returning,
        })
    }

    fn plan_insert_many_do_nothing(
        &self,
        table: &str,
        columns: Option<&[String]>,
        target: Option<&[String]>,
        rows: &[Vec<Value>],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let planned_rows = rows
            .iter()
            .map(|values| self.build_insert_row(schema, table, columns, values))
            .collect::<Result<Vec<_>>>()?;
        self.validate_do_nothing_target(schema, table, target, context)?;
        Ok(Plan::InsertManyDoNothing {
            table: table.to_string(),
            target: target.map(|columns| columns.to_vec()),
            rows: planned_rows,
        })
    }

    fn plan_insert_many_do_nothing_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        target: Option<&[String]>,
        rows: &[Vec<Value>],
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let planned_rows = rows
            .iter()
            .map(|values| self.build_insert_row(schema, table, columns, values))
            .collect::<Result<Vec<_>>>()?;
        self.validate_do_nothing_target(schema, table, target, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertManyDoNothingReturning {
            table: table.to_string(),
            target: target.map(|columns| columns.to_vec()),
            rows: planned_rows,
            returning,
        })
    }

    fn plan_insert_expr(
        &self,
        table: &str,
        columns: Option<&[String]>,
        or_conflict: Option<&str>,
        values: &[ScalarExpr],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_values = self.plan_insert_value_exprs(schema, table, columns, values)?;
        Ok(Plan::InsertExpr {
            table: table.to_string(),
            or_conflict: or_conflict.map(str::to_string),
            values: normalized_values,
        })
    }

    fn plan_insert_expr_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        or_conflict: Option<&str>,
        values: &[ScalarExpr],
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_values = self.plan_insert_value_exprs(schema, table, columns, values)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertExprReturning {
            table: table.to_string(),
            or_conflict: or_conflict.map(str::to_string),
            values: normalized_values,
            returning,
        })
    }

    fn plan_insert_expr_do_nothing(
        &self,
        table: &str,
        columns: Option<&[String]>,
        target: Option<&[String]>,
        values: &[ScalarExpr],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_values = self.plan_insert_value_exprs(schema, table, columns, values)?;
        self.validate_do_nothing_target(schema, table, target, context)?;
        Ok(Plan::InsertExprDoNothing {
            table: table.to_string(),
            target: target.map(|columns| columns.to_vec()),
            values: normalized_values,
        })
    }

    fn plan_insert_expr_do_nothing_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        target: Option<&[String]>,
        values: &[ScalarExpr],
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_values = self.plan_insert_value_exprs(schema, table, columns, values)?;
        self.validate_do_nothing_target(schema, table, target, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertExprDoNothingReturning {
            table: table.to_string(),
            target: target.map(|columns| columns.to_vec()),
            values: normalized_values,
            returning,
        })
    }

    fn plan_insert_many_expr_do_nothing(
        &self,
        table: &str,
        columns: Option<&[String]>,
        target: Option<&[String]>,
        rows: &[Vec<ScalarExpr>],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let single_row_schema = PlanningContext::single_row_source_schema(SINGLE_ROW_SOURCE_TABLE)
            .expect("single row source schema");
        let normalized_rows = rows
            .iter()
            .map(|values| {
                self.require_insert_value_exprs(values)?;
                self.validate_insert_value_arity(schema, table, columns, values.len())?;
                values
                    .iter()
                    .map(|expr| {
                        self.normalize_scalar_expr(
                            &single_row_schema,
                            SINGLE_ROW_SOURCE_TABLE,
                            None,
                            expr,
                        )
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        self.validate_do_nothing_target(schema, table, target, context)?;
        Ok(Plan::InsertManyExprDoNothing {
            table: table.to_string(),
            target: target.map(|columns| columns.to_vec()),
            rows: normalized_rows,
        })
    }

    fn plan_insert_many_expr_do_nothing_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        target: Option<&[String]>,
        rows: &[Vec<ScalarExpr>],
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let single_row_schema = PlanningContext::single_row_source_schema(SINGLE_ROW_SOURCE_TABLE)
            .expect("single row source schema");
        let normalized_rows = rows
            .iter()
            .map(|values| {
                self.require_insert_value_exprs(values)?;
                self.validate_insert_value_arity(schema, table, columns, values.len())?;
                values
                    .iter()
                    .map(|expr| {
                        self.normalize_scalar_expr(
                            &single_row_schema,
                            SINGLE_ROW_SOURCE_TABLE,
                            None,
                            expr,
                        )
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        self.validate_do_nothing_target(schema, table, target, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertManyExprDoNothingReturning {
            table: table.to_string(),
            target: target.map(|columns| columns.to_vec()),
            rows: normalized_rows,
            returning,
        })
    }

    fn plan_insert_expr_upsert(
        &self,
        table: &str,
        columns: Option<&[String]>,
        values: &[ScalarExpr],
        upsert: &UpsertClause,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_values = self.plan_insert_value_exprs(schema, table, columns, values)?;
        let upsert = self.plan_upsert_clause(schema, table, upsert, context)?;
        Ok(Plan::InsertExprUpsert {
            table: table.to_string(),
            values: normalized_values,
            upsert,
        })
    }

    fn plan_insert_expr_upsert_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        values: &[ScalarExpr],
        upsert: &UpsertClause,
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_values = self.plan_insert_value_exprs(schema, table, columns, values)?;
        let upsert = self.plan_upsert_clause(schema, table, upsert, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertExprUpsertReturning {
            table: table.to_string(),
            values: normalized_values,
            upsert,
            returning,
        })
    }

    fn plan_insert_many_expr_upsert(
        &self,
        table: &str,
        columns: Option<&[String]>,
        rows: &[Vec<ScalarExpr>],
        upsert: &UpsertClause,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_rows =
            self.plan_insert_many_value_expr_rows(schema, table, columns, rows)?;
        let upsert = self.plan_upsert_clause(schema, table, upsert, context)?;
        Ok(Plan::InsertManyExprUpsert {
            table: table.to_string(),
            rows: normalized_rows,
            upsert,
        })
    }

    fn plan_insert_many_expr_upsert_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        rows: &[Vec<ScalarExpr>],
        upsert: &UpsertClause,
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_rows =
            self.plan_insert_many_value_expr_rows(schema, table, columns, rows)?;
        let upsert = self.plan_upsert_clause(schema, table, upsert, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertManyExprUpsertReturning {
            table: table.to_string(),
            rows: normalized_rows,
            upsert,
            returning,
        })
    }

    fn plan_insert_many_expr(
        &self,
        table: &str,
        columns: Option<&[String]>,
        or_conflict: Option<&str>,
        rows: &[Vec<ScalarExpr>],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let single_row_schema = PlanningContext::single_row_source_schema(SINGLE_ROW_SOURCE_TABLE)
            .expect("single row source schema");
        let normalized_rows = rows
            .iter()
            .map(|values| {
                self.require_insert_value_exprs(values)?;
                self.validate_insert_value_arity(schema, table, columns, values.len())?;
                values
                    .iter()
                    .map(|expr| {
                        self.normalize_scalar_expr(
                            &single_row_schema,
                            SINGLE_ROW_SOURCE_TABLE,
                            None,
                            expr,
                        )
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Plan::InsertManyExpr {
            table: table.to_string(),
            or_conflict: or_conflict.map(str::to_string),
            rows: normalized_rows,
        })
    }

    fn plan_insert_many_expr_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        or_conflict: Option<&str>,
        rows: &[Vec<ScalarExpr>],
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let single_row_schema = PlanningContext::single_row_source_schema(SINGLE_ROW_SOURCE_TABLE)
            .expect("single row source schema");
        let normalized_rows = rows
            .iter()
            .map(|values| {
                self.require_insert_value_exprs(values)?;
                self.validate_insert_value_arity(schema, table, columns, values.len())?;
                values
                    .iter()
                    .map(|expr| {
                        self.normalize_scalar_expr(
                            &single_row_schema,
                            SINGLE_ROW_SOURCE_TABLE,
                            None,
                            expr,
                        )
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertManyExprReturning {
            table: table.to_string(),
            or_conflict: or_conflict.map(str::to_string),
            rows: normalized_rows,
            returning,
        })
    }

    fn plan_insert_select(
        &self,
        table: &str,
        columns: Option<&[String]>,
        or_conflict: Option<&str>,
        select: &SelectStatement,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let (_schema, source) = self.plan_insert_select_source(table, columns, select, context)?;
        Ok(Plan::InsertSelect {
            table: table.to_string(),
            columns: columns.map(|columns| columns.to_vec()),
            or_conflict: or_conflict.map(str::to_string),
            source: Box::new(source),
        })
    }

    fn plan_insert_select_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        or_conflict: Option<&str>,
        select: &SelectStatement,
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let (schema, source) = self.plan_insert_select_source(table, columns, select, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertSelectReturning {
            table: table.to_string(),
            columns: columns.map(|columns| columns.to_vec()),
            or_conflict: or_conflict.map(str::to_string),
            source: Box::new(source),
            returning,
        })
    }

    fn plan_insert_select_upsert(
        &self,
        table: &str,
        columns: Option<&[String]>,
        select: &SelectStatement,
        upsert: &UpsertClause,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let (schema, source) = self.plan_insert_select_source(table, columns, select, context)?;
        let upsert = self.plan_upsert_clause(schema, table, upsert, context)?;
        Ok(Plan::InsertSelectUpsert {
            table: table.to_string(),
            columns: columns.map(|columns| columns.to_vec()),
            source: Box::new(source),
            upsert,
        })
    }

    fn plan_insert_select_upsert_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        select: &SelectStatement,
        upsert: &UpsertClause,
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let (schema, source) = self.plan_insert_select_source(table, columns, select, context)?;
        let upsert = self.plan_upsert_clause(schema, table, upsert, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertSelectUpsertReturning {
            table: table.to_string(),
            columns: columns.map(|columns| columns.to_vec()),
            source: Box::new(source),
            upsert,
            returning,
        })
    }

    fn plan_insert_select_do_nothing(
        &self,
        table: &str,
        columns: Option<&[String]>,
        target: Option<&[String]>,
        select: &SelectStatement,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let (schema, source) = self.plan_insert_select_source(table, columns, select, context)?;
        self.validate_do_nothing_target(schema, table, target, context)?;
        Ok(Plan::InsertSelectDoNothing {
            table: table.to_string(),
            columns: columns.map(|columns| columns.to_vec()),
            target: target.map(|columns| columns.to_vec()),
            source: Box::new(source),
        })
    }

    fn plan_insert_select_do_nothing_returning(
        &self,
        table: &str,
        columns: Option<&[String]>,
        target: Option<&[String]>,
        select: &SelectStatement,
        returning: &[SelectItem],
        context: &PlanningContext,
    ) -> Result<Plan> {
        let (schema, source) = self.plan_insert_select_source(table, columns, select, context)?;
        self.validate_do_nothing_target(schema, table, target, context)?;
        let returning = self.plan_returning_items(schema, table, returning)?;
        Ok(Plan::InsertSelectDoNothingReturning {
            table: table.to_string(),
            columns: columns.map(|columns| columns.to_vec()),
            target: target.map(|columns| columns.to_vec()),
            source: Box::new(source),
            returning,
        })
    }

    fn plan_insert_select_source<'a>(
        &self,
        table: &str,
        columns: Option<&[String]>,
        select: &SelectStatement,
        context: &'a PlanningContext,
    ) -> Result<(&'a Schema, Plan)> {
        let schema = self.require_schema(context, table)?;
        let target_width = match columns {
            Some(columns) => columns.len(),
            None => schema
                .columns
                .iter()
                .filter(|column| column.generated_expr.is_none())
                .count(),
        };
        let source = self.plan_select(select, context)?;
        let source_width = self.plan_output_width(&source)?;
        if source_width != target_width {
            return Err(DbError::plan(format!(
                "insert into {table} expected {target_width} values but got {source_width}"
            )));
        }
        Ok((schema, source))
    }

    fn plan_output_width(&self, plan: &Plan) -> Result<usize> {
        match plan {
            Plan::SeqScan { columns, .. }
            | Plan::IndexScan { columns, .. }
            | Plan::IndexUnion { columns, .. }
            | Plan::NestedLoopJoin { columns, .. }
            | Plan::Aggregate { columns, .. }
            | Plan::DerivedSource { columns, .. } => Ok(columns.len()),
            Plan::Values { rows } => rows.first().map_or(Ok(0), |row| Ok(row.len())),
            Plan::Union { left, right, .. } => {
                let left_width = self.plan_output_width(left)?;
                let right_width = self.plan_output_width(right)?;
                if left_width != right_width {
                    return Err(DbError::plan("compound query output width mismatch"));
                }
                Ok(left_width)
            }
            Plan::ExplainQueryPlan { .. } => Ok(2),
            Plan::PragmaTableInfo { .. } => Ok(6),
            Plan::PragmaTableXInfo { .. } => Ok(7),
            Plan::PragmaTableList { .. } => Ok(6),
            Plan::PragmaIndexList { .. } => Ok(5),
            Plan::PragmaIndexInfo { .. } => Ok(3),
            Plan::PragmaIndexXInfo { .. } => Ok(6),
            Plan::PragmaForeignKeyList { .. } => Ok(8),
            Plan::PragmaForeignKeyCheck { .. } => Ok(4),
            Plan::PragmaForeignKeys => Ok(1),
            Plan::PragmaReadUncommitted => Ok(1),
            Plan::PragmaQueryOnly => Ok(1),
            Plan::PragmaRecursiveTriggers => Ok(1),
            Plan::PragmaTrustedSchema => Ok(1),
            Plan::PragmaIgnoreCheckConstraints => Ok(1),
            Plan::PragmaEncoding => Ok(1),
            Plan::PragmaCollationList => Ok(2),
            Plan::PragmaDataVersion => Ok(1),
            Plan::PragmaQuickCheck => Ok(1),
            Plan::PragmaIntegrityCheck => Ok(1),
            Plan::PragmaFunctionList => Ok(6),
            Plan::PragmaCompileOptions => Ok(1),
            Plan::PragmaJournalMode => Ok(1),
            Plan::PragmaSynchronous => Ok(1),
            Plan::PragmaCacheSize => Ok(1),
            Plan::PragmaTempStore => Ok(1),
            Plan::PragmaLockingMode => Ok(1),
            Plan::PragmaBusyTimeout => Ok(1),
            Plan::PragmaThreads => Ok(1),
            Plan::PragmaCaseSensitiveLike => Ok(1),
            Plan::PragmaReverseUnorderedSelects => Ok(1),
            Plan::PragmaDatabaseList => Ok(3),
            Plan::PragmaPageSize => Ok(1),
            Plan::PragmaPageCount => Ok(1),
            Plan::PragmaFreelistCount => Ok(1),
            Plan::PragmaUserVersion => Ok(1),
            Plan::PragmaApplicationId => Ok(1),
            Plan::PragmaSchemaVersion => Ok(1),
            other => Err(DbError::plan(format!(
                "unexpected insert-select source plan: {other:?}"
            ))),
        }
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

    fn plan_delete_limited(
        &self,
        table: &str,
        table_alias: Option<&str>,
        filter: &Option<Expr>,
        order_by: &[OrderBy],
        limit: Option<usize>,
        offset: Option<usize>,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_filter = filter
            .as_ref()
            .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
            .transpose()?;

        let scope = self.build_single_table_scope(table, table_alias, context)?;
        if let Some(expr) = &normalized_filter {
            self.require_scope_columns_with_outer(&scope, None, expr)?;
            self.validate_subqueries(expr, context, &scope)?;
        }
        for item in order_by {
            self.require_order_by_scope(&scope, &[], item)?;
        }
        let order_by = order_by
            .iter()
            .map(|item| self.normalize_order_by(schema, table, table_alias, &[], item))
            .collect::<Result<Vec<_>>>()?;

        Ok(Plan::DeleteLimited {
            table: table.to_string(),
            filter: normalized_filter,
            order_by,
            limit,
            offset,
        })
    }

    fn plan_delete_returning(
        &self,
        table: &str,
        table_alias: Option<&str>,
        filter: &Option<Expr>,
        returning: &[SelectItem],
        order_by: &[OrderBy],
        limit: Option<usize>,
        offset: Option<usize>,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let normalized_filter = filter
            .as_ref()
            .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
            .transpose()?;

        let scope = self.build_single_table_scope(table, table_alias, context)?;
        if let Some(expr) = &normalized_filter {
            self.require_scope_columns_with_outer(&scope, None, expr)?;
            self.validate_subqueries(expr, context, &scope)?;
        }
        for item in order_by {
            self.require_order_by_scope(&scope, &[], item)?;
        }
        let order_by = order_by
            .iter()
            .map(|item| self.normalize_order_by(schema, table, table_alias, &[], item))
            .collect::<Result<Vec<_>>>()?;

        let returning = self.plan_returning_items(schema, table, returning)?;
        if !order_by.is_empty() || limit.is_some() || offset.is_some() {
            Ok(Plan::DeleteReturningLimited {
                table: table.to_string(),
                filter: normalized_filter,
                returning,
                order_by,
                limit,
                offset,
            })
        } else {
            Ok(Plan::DeleteReturning {
                table: table.to_string(),
                filter: normalized_filter,
                returning,
            })
        }
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
        let scope = self.build_single_table_scope(table, table_alias, context)?;
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
            let value =
                self.normalize_scalar_expr(schema, table, table_alias, &assignment.value)?;
            self.require_scalar_expr_scope(&scope, &value)?;
            normalized_assignments.push(Assignment {
                column: assignment.column.clone(),
                value,
            });
        }

        let normalized_filter = filter
            .as_ref()
            .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
            .transpose()?;
        if let Some(expr) = &normalized_filter {
            self.require_scope_columns_with_outer(&scope, None, expr)?;
            self.validate_subqueries(expr, context, &scope)?;
        }

        Ok(Plan::Update {
            table: table.to_string(),
            assignments: normalized_assignments,
            filter: normalized_filter,
        })
    }

    fn plan_update_limited(
        &self,
        table: &str,
        table_alias: Option<&str>,
        assignments: &[Assignment],
        filter: &Option<Expr>,
        order_by: &[OrderBy],
        limit: Option<usize>,
        offset: Option<usize>,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let scope = self.build_single_table_scope(table, table_alias, context)?;
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
            let value =
                self.normalize_scalar_expr(schema, table, table_alias, &assignment.value)?;
            self.require_scalar_expr_scope(&scope, &value)?;
            normalized_assignments.push(Assignment {
                column: assignment.column.clone(),
                value,
            });
        }

        let normalized_filter = filter
            .as_ref()
            .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
            .transpose()?;
        if let Some(expr) = &normalized_filter {
            self.require_scope_columns_with_outer(&scope, None, expr)?;
            self.validate_subqueries(expr, context, &scope)?;
        }
        for item in order_by {
            self.require_order_by_scope(&scope, &[], item)?;
        }
        let order_by = order_by
            .iter()
            .map(|item| self.normalize_order_by(schema, table, table_alias, &[], item))
            .collect::<Result<Vec<_>>>()?;

        Ok(Plan::UpdateLimited {
            table: table.to_string(),
            assignments: normalized_assignments,
            filter: normalized_filter,
            order_by,
            limit,
            offset,
        })
    }

    fn plan_update_returning(
        &self,
        table: &str,
        table_alias: Option<&str>,
        assignments: &[Assignment],
        filter: &Option<Expr>,
        returning: &[SelectItem],
        order_by: &[OrderBy],
        limit: Option<usize>,
        offset: Option<usize>,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let schema = self.require_schema(context, table)?;
        let scope = self.build_single_table_scope(table, table_alias, context)?;
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
            let value =
                self.normalize_scalar_expr(schema, table, table_alias, &assignment.value)?;
            self.require_scalar_expr_scope(&scope, &value)?;
            normalized_assignments.push(Assignment {
                column: assignment.column.clone(),
                value,
            });
        }

        let normalized_filter = filter
            .as_ref()
            .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
            .transpose()?;
        if let Some(expr) = &normalized_filter {
            self.require_scope_columns_with_outer(&scope, None, expr)?;
            self.validate_subqueries(expr, context, &scope)?;
        }
        for item in order_by {
            self.require_order_by_scope(&scope, &[], item)?;
        }
        let order_by = order_by
            .iter()
            .map(|item| self.normalize_order_by(schema, table, table_alias, &[], item))
            .collect::<Result<Vec<_>>>()?;

        let returning = self.plan_returning_items(schema, table, returning)?;
        if !order_by.is_empty() || limit.is_some() || offset.is_some() {
            Ok(Plan::UpdateReturningLimited {
                table: table.to_string(),
                assignments: normalized_assignments,
                filter: normalized_filter,
                returning,
                order_by,
                limit,
                offset,
            })
        } else {
            Ok(Plan::UpdateReturning {
                table: table.to_string(),
                assignments: normalized_assignments,
                filter: normalized_filter,
                returning,
            })
        }
    }

    fn plan_select(&self, select: &SelectStatement, context: &PlanningContext) -> Result<Plan> {
        let lowered = self.lower_top_level_ctes(select)?;
        self.plan_select_with_outer(&lowered, context, None)
    }

    fn plan_with_dml(
        &self,
        with: &WithClause,
        statement: &Statement,
        context: &PlanningContext,
    ) -> Result<Plan> {
        let ctes = self.cte_registry_from_with(with)?;
        let lowered = self.lower_statement_ctes(statement, &ctes)?;
        self.plan_statement(&lowered, context)
    }

    fn lower_top_level_ctes(&self, select: &SelectStatement) -> Result<SelectStatement> {
        let ctes = select
            .with
            .as_ref()
            .map(|with| self.cte_registry_from_with(with))
            .transpose()?
            .unwrap_or_default();

        let mut lowered = self.lower_cte_references(select, &ctes)?;
        lowered.with = None;
        Ok(lowered)
    }

    fn cte_registry_from_with(&self, with: &WithClause) -> Result<CteRegistry> {
        let mut ctes = CteRegistry::new();
        for cte in &with.ctes {
            if ctes.contains_key(&cte.name) {
                return Err(DbError::plan(format!("duplicate CTE name: {}", cte.name)));
            }
            let lowered = match &cte.query {
                CteBody::Select(query) => {
                    CteBody::Select(Box::new(self.lower_cte_references(query, &ctes)?))
                }
                CteBody::Values(rows) => CteBody::Values(self.lower_cte_value_rows(rows, &ctes)?),
            };
            ctes.insert(
                cte.name.clone(),
                LoweredCte {
                    columns: cte.columns.clone(),
                    query: lowered,
                },
            );
        }
        Ok(ctes)
    }

    fn lower_statement_ctes(&self, statement: &Statement, ctes: &CteRegistry) -> Result<Statement> {
        Ok(match statement {
            Statement::Select(select) => {
                Statement::Select(self.lower_cte_references(select, ctes)?)
            }
            Statement::Delete {
                table,
                table_alias,
                filter,
            } => Statement::Delete {
                table: table.clone(),
                table_alias: table_alias.clone(),
                filter: filter
                    .as_ref()
                    .map(|expr| self.lower_cte_expr(expr, ctes))
                    .transpose()?,
            },
            Statement::DeleteLimited {
                table,
                table_alias,
                filter,
                order_by,
                limit,
                offset,
            } => Statement::DeleteLimited {
                table: table.clone(),
                table_alias: table_alias.clone(),
                filter: filter
                    .as_ref()
                    .map(|expr| self.lower_cte_expr(expr, ctes))
                    .transpose()?,
                order_by: order_by.clone(),
                limit: *limit,
                offset: *offset,
            },
            Statement::DeleteReturning {
                table,
                table_alias,
                filter,
                returning,
            } => Statement::DeleteReturning {
                table: table.clone(),
                table_alias: table_alias.clone(),
                filter: filter
                    .as_ref()
                    .map(|expr| self.lower_cte_expr(expr, ctes))
                    .transpose()?,
                returning: returning.clone(),
            },
            Statement::DeleteReturningLimited {
                table,
                table_alias,
                filter,
                returning,
                order_by,
                limit,
                offset,
            } => Statement::DeleteReturningLimited {
                table: table.clone(),
                table_alias: table_alias.clone(),
                filter: filter
                    .as_ref()
                    .map(|expr| self.lower_cte_expr(expr, ctes))
                    .transpose()?,
                returning: returning.clone(),
                order_by: order_by
                    .iter()
                    .map(|item| self.lower_cte_order_by(item, ctes))
                    .collect::<Result<Vec<_>>>()?,
                limit: *limit,
                offset: *offset,
            },
            Statement::Update {
                table,
                table_alias,
                assignments,
                filter,
            } => Statement::Update {
                table: table.clone(),
                table_alias: table_alias.clone(),
                assignments: assignments.clone(),
                filter: filter
                    .as_ref()
                    .map(|expr| self.lower_cte_expr(expr, ctes))
                    .transpose()?,
            },
            Statement::UpdateLimited {
                table,
                table_alias,
                assignments,
                filter,
                order_by,
                limit,
                offset,
            } => Statement::UpdateLimited {
                table: table.clone(),
                table_alias: table_alias.clone(),
                assignments: assignments.clone(),
                filter: filter
                    .as_ref()
                    .map(|expr| self.lower_cte_expr(expr, ctes))
                    .transpose()?,
                order_by: order_by.clone(),
                limit: *limit,
                offset: *offset,
            },
            Statement::UpdateReturning {
                table,
                table_alias,
                assignments,
                filter,
                returning,
            } => Statement::UpdateReturning {
                table: table.clone(),
                table_alias: table_alias.clone(),
                assignments: assignments.clone(),
                filter: filter
                    .as_ref()
                    .map(|expr| self.lower_cte_expr(expr, ctes))
                    .transpose()?,
                returning: returning.clone(),
            },
            Statement::UpdateReturningLimited {
                table,
                table_alias,
                assignments,
                filter,
                returning,
                order_by,
                limit,
                offset,
            } => Statement::UpdateReturningLimited {
                table: table.clone(),
                table_alias: table_alias.clone(),
                assignments: assignments.clone(),
                filter: filter
                    .as_ref()
                    .map(|expr| self.lower_cte_expr(expr, ctes))
                    .transpose()?,
                returning: returning.clone(),
                order_by: order_by
                    .iter()
                    .map(|item| self.lower_cte_order_by(item, ctes))
                    .collect::<Result<Vec<_>>>()?,
                limit: *limit,
                offset: *offset,
            },
            Statement::WithDml { .. } => {
                return Err(DbError::plan("nested WITH DML is not supported"));
            }
            statement => statement.clone(),
        })
    }

    fn lower_cte_references(
        &self,
        select: &SelectStatement,
        ctes: &CteRegistry,
    ) -> Result<SelectStatement> {
        Ok(SelectStatement {
            with: None,
            distinct: select.distinct,
            columns: select
                .columns
                .iter()
                .map(|item| self.lower_cte_select_item(item, ctes))
                .collect::<Result<Vec<_>>>()?,
            from: self.lower_cte_from_item(&select.from, ctes)?,
            joins: select
                .joins
                .iter()
                .map(|join| {
                    Ok(crate::sql::ast::JoinClause {
                        kind: join.kind,
                        source: self.lower_cte_from_item(&join.source, ctes)?,
                        on: self.lower_cte_expr(&join.on, ctes)?,
                        using_columns: join.using_columns.clone(),
                        natural: join.natural,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            filter: select
                .filter
                .as_ref()
                .map(|expr| self.lower_cte_expr(expr, ctes))
                .transpose()?,
            group_by: select
                .group_by
                .iter()
                .map(|expr| self.lower_cte_scalar_expr(expr, ctes))
                .collect::<Result<Vec<_>>>()?,
            having: select
                .having
                .as_ref()
                .map(|expr| self.lower_cte_expr(expr, ctes))
                .transpose()?,
            compounds: select
                .compounds
                .iter()
                .map(|compound| {
                    Ok(crate::sql::ast::CompoundSelect {
                        operator: compound.operator,
                        select: Box::new(
                            self.lower_cte_references(compound.select.as_ref(), ctes)?,
                        ),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            order_by: select
                .order_by
                .iter()
                .map(|item| self.lower_cte_order_by(item, ctes))
                .collect::<Result<Vec<_>>>()?,
            limit: select.limit,
            offset: select.offset,
        })
    }

    fn lower_cte_from_item(&self, from: &FromItem, ctes: &CteRegistry) -> Result<FromItem> {
        match from {
            FromItem::Table { name, alias }
            | FromItem::TableIndexed { name, alias, .. }
            | FromItem::TableNotIndexed { name, alias } => {
                let Some(cte) = ctes.get(name) else {
                    return Ok(from.clone());
                };
                match &cte.query {
                    CteBody::Select(query) => Ok(FromItem::Subquery {
                        query: query.clone(),
                        alias: alias.clone().unwrap_or_else(|| name.clone()),
                        columns: cte.columns.clone(),
                    }),
                    CteBody::Values(rows) => Ok(FromItem::Values {
                        rows: rows.clone(),
                        alias: Some(alias.clone().unwrap_or_else(|| name.clone())),
                        columns: cte.columns.clone(),
                    }),
                }
            }
            FromItem::Subquery {
                query,
                alias,
                columns,
            } => Ok(FromItem::Subquery {
                query: Box::new(self.lower_cte_references(query, ctes)?),
                alias: alias.clone(),
                columns: columns.clone(),
            }),
            FromItem::Values {
                rows,
                alias,
                columns,
            } => Ok(FromItem::Values {
                rows: self.lower_cte_value_rows(rows, ctes)?,
                alias: alias.clone(),
                columns: columns.clone(),
            }),
        }
    }

    fn lower_cte_expr(&self, expr: &Expr, ctes: &CteRegistry) -> Result<Expr> {
        match expr {
            Expr::InSubquery {
                column,
                query,
                negated,
            } => Ok(Expr::InSubquery {
                column: column.clone(),
                query: Box::new(self.lower_cte_references(query, ctes)?),
                negated: *negated,
            }),
            Expr::InList {
                column,
                values,
                negated,
            } => Ok(Expr::InList {
                column: column.clone(),
                values: values.clone(),
                negated: *negated,
            }),
            Expr::InSubqueryScalar {
                expr,
                query,
                negated,
            } => Ok(Expr::InSubqueryScalar {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                query: Box::new(self.lower_cte_references(query, ctes)?),
                negated: *negated,
            }),
            Expr::InListScalar {
                expr,
                values,
                negated,
            } => Ok(Expr::InListScalar {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                values: values
                    .iter()
                    .map(|value| self.lower_cte_scalar_expr(value, ctes))
                    .collect::<Result<Vec<_>>>()?,
                negated: *negated,
            }),
            Expr::CompareSubquery { column, op, query } => Ok(Expr::CompareSubquery {
                column: column.clone(),
                op: *op,
                query: Box::new(self.lower_cte_references(query, ctes)?),
            }),
            Expr::CompareSubqueryScalar { left, op, query } => Ok(Expr::CompareSubqueryScalar {
                left: self.lower_cte_scalar_expr(left, ctes)?,
                op: *op,
                query: Box::new(self.lower_cte_references(query, ctes)?),
            }),
            Expr::ExistsSubquery { query, negated } => Ok(Expr::ExistsSubquery {
                query: Box::new(self.lower_cte_references(query, ctes)?),
                negated: *negated,
            }),
            Expr::CompareScalar { left, op, right } => Ok(Expr::CompareScalar {
                left: self.lower_cte_scalar_expr(left, ctes)?,
                op: *op,
                right: self.lower_cte_scalar_expr(right, ctes)?,
            }),
            Expr::IsNullScalar { expr, negated } => Ok(Expr::IsNullScalar {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                negated: *negated,
            }),
            Expr::Is {
                left,
                right,
                negated,
            } => Ok(Expr::Is {
                left: self.lower_cte_scalar_expr(left, ctes)?,
                right: self.lower_cte_scalar_expr(right, ctes)?,
                negated: *negated,
            }),
            Expr::IsBool {
                expr,
                value,
                negated,
                explicit,
            } => Ok(Expr::IsBool {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                value: *value,
                negated: *negated,
                explicit: *explicit,
            }),
            Expr::LikeScalar {
                expr,
                pattern,
                escape,
                negated,
            } => Ok(Expr::LikeScalar {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            }),
            Expr::GlobScalar {
                expr,
                pattern,
                negated,
            } => Ok(Expr::GlobScalar {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                pattern: pattern.clone(),
                negated: *negated,
            }),
            Expr::Between {
                column,
                low,
                high,
                negated,
            } => Ok(Expr::Between {
                column: column.clone(),
                low: low.clone(),
                high: high.clone(),
                negated: *negated,
            }),
            Expr::BetweenScalar {
                expr,
                low,
                high,
                negated,
            } => Ok(Expr::BetweenScalar {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                low: self.lower_cte_scalar_expr(low, ctes)?,
                high: self.lower_cte_scalar_expr(high, ctes)?,
                negated: *negated,
            }),
            Expr::Not(inner) => Ok(Expr::Not(Box::new(self.lower_cte_expr(inner, ctes)?))),
            Expr::And(left, right) => Ok(Expr::And(
                Box::new(self.lower_cte_expr(left, ctes)?),
                Box::new(self.lower_cte_expr(right, ctes)?),
            )),
            Expr::Or(left, right) => Ok(Expr::Or(
                Box::new(self.lower_cte_expr(left, ctes)?),
                Box::new(self.lower_cte_expr(right, ctes)?),
            )),
            _ => Ok(expr.clone()),
        }
    }

    fn lower_cte_select_item(&self, item: &SelectItem, ctes: &CteRegistry) -> Result<SelectItem> {
        Ok(match item {
            SelectItem::Wildcard | SelectItem::Column(_) | SelectItem::AliasedColumn { .. } => {
                item.clone()
            }
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                alias: alias.clone(),
            },
            SelectItem::Aggregate {
                func,
                arg,
                filter,
                alias,
            } => SelectItem::Aggregate {
                func: *func,
                arg: self.lower_cte_aggregate_arg(arg, ctes)?,
                filter: filter
                    .as_ref()
                    .map(|expr| self.lower_cte_expr(expr, ctes))
                    .transpose()?,
                alias: alias.clone(),
            },
        })
    }

    fn lower_cte_aggregate_arg(
        &self,
        arg: &AggregateArg,
        ctes: &CteRegistry,
    ) -> Result<AggregateArg> {
        Ok(match arg {
            AggregateArg::Wildcard => AggregateArg::Wildcard,
            AggregateArg::Expr {
                expr,
                distinct,
                order_by,
            } => AggregateArg::Expr {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                distinct: *distinct,
                order_by: order_by
                    .iter()
                    .map(|item| self.lower_cte_order_by(item, ctes))
                    .collect::<Result<Vec<_>>>()?,
            },
            AggregateArg::GroupConcat {
                expr,
                separator,
                distinct,
                order_by,
            } => AggregateArg::GroupConcat {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                separator: separator
                    .as_ref()
                    .map(|expr| self.lower_cte_scalar_expr(expr, ctes))
                    .transpose()?,
                distinct: *distinct,
                order_by: order_by
                    .iter()
                    .map(|item| self.lower_cte_order_by(item, ctes))
                    .collect::<Result<Vec<_>>>()?,
            },
            AggregateArg::JsonGroupObject {
                key,
                value,
                order_by,
            } => AggregateArg::JsonGroupObject {
                key: self.lower_cte_scalar_expr(key, ctes)?,
                value: self.lower_cte_scalar_expr(value, ctes)?,
                order_by: order_by
                    .iter()
                    .map(|item| self.lower_cte_order_by(item, ctes))
                    .collect::<Result<Vec<_>>>()?,
            },
            AggregateArg::Percentile {
                expr,
                fraction,
                order_by,
            } => AggregateArg::Percentile {
                expr: self.lower_cte_scalar_expr(expr, ctes)?,
                fraction: self.lower_cte_scalar_expr(fraction, ctes)?,
                order_by: order_by
                    .iter()
                    .map(|item| self.lower_cte_order_by(item, ctes))
                    .collect::<Result<Vec<_>>>()?,
            },
        })
    }

    fn lower_cte_order_by(&self, item: &OrderBy, ctes: &CteRegistry) -> Result<OrderBy> {
        Ok(OrderBy {
            expr: match &item.expr {
                OrderByExpr::Column(_) | OrderByExpr::Position(_) => item.expr.clone(),
                OrderByExpr::Expr(expr) => {
                    OrderByExpr::Expr(self.lower_cte_scalar_expr(expr, ctes)?)
                }
            },
            descending: item.descending,
            collation: item.collation.clone(),
            nulls: item.nulls,
        })
    }

    fn lower_cte_value_rows(
        &self,
        rows: &[Vec<ScalarExpr>],
        ctes: &CteRegistry,
    ) -> Result<Vec<Vec<ScalarExpr>>> {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|expr| self.lower_cte_scalar_expr(expr, ctes))
                    .collect::<Result<Vec<_>>>()
            })
            .collect()
    }

    fn lower_cte_scalar_expr(&self, expr: &ScalarExpr, ctes: &CteRegistry) -> Result<ScalarExpr> {
        Ok(match expr {
            ScalarExpr::Literal(_) | ScalarExpr::Column(_) => expr.clone(),
            ScalarExpr::Tuple(values) => ScalarExpr::Tuple(
                values
                    .iter()
                    .map(|value| self.lower_cte_scalar_expr(value, ctes))
                    .collect::<Result<Vec<_>>>()?,
            ),
            ScalarExpr::UnaryMinus(expr) => {
                ScalarExpr::UnaryMinus(Box::new(self.lower_cte_scalar_expr(expr, ctes)?))
            }
            ScalarExpr::BitNot(expr) => {
                ScalarExpr::BitNot(Box::new(self.lower_cte_scalar_expr(expr, ctes)?))
            }
            ScalarExpr::Not(expr) => {
                ScalarExpr::Not(Box::new(self.lower_cte_scalar_expr(expr, ctes)?))
            }
            ScalarExpr::Cast { expr, ty } => ScalarExpr::Cast {
                expr: Box::new(self.lower_cte_scalar_expr(expr, ctes)?),
                ty: *ty,
            },
            ScalarExpr::Collate { expr, collation } => ScalarExpr::Collate {
                expr: Box::new(self.lower_cte_scalar_expr(expr, ctes)?),
                collation: collation.clone(),
            },
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => ScalarExpr::Is {
                left: Box::new(self.lower_cte_scalar_expr(left, ctes)?),
                right: Box::new(self.lower_cte_scalar_expr(right, ctes)?),
                negated: *negated,
            },
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => ScalarExpr::IsBool {
                expr: Box::new(self.lower_cte_scalar_expr(expr, ctes)?),
                value: *value,
                negated: *negated,
            },
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => ScalarExpr::InList {
                expr: Box::new(self.lower_cte_scalar_expr(expr, ctes)?),
                values: values
                    .iter()
                    .map(|value| self.lower_cte_scalar_expr(value, ctes))
                    .collect::<Result<Vec<_>>>()?,
                negated: *negated,
            },
            ScalarExpr::InSubquery {
                expr,
                query,
                negated,
            } => ScalarExpr::InSubquery {
                expr: Box::new(self.lower_cte_scalar_expr(expr, ctes)?),
                query: Box::new(self.lower_cte_references(query, ctes)?),
                negated: *negated,
            },
            ScalarExpr::Subquery { query } => ScalarExpr::Subquery {
                query: Box::new(self.lower_cte_references(query, ctes)?),
            },
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => ScalarExpr::Like {
                expr: Box::new(self.lower_cte_scalar_expr(expr, ctes)?),
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            },
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => ScalarExpr::Glob {
                expr: Box::new(self.lower_cte_scalar_expr(expr, ctes)?),
                pattern: pattern.clone(),
                negated: *negated,
            },
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => ScalarExpr::Between {
                expr: Box::new(self.lower_cte_scalar_expr(expr, ctes)?),
                low: Box::new(self.lower_cte_scalar_expr(low, ctes)?),
                high: Box::new(self.lower_cte_scalar_expr(high, ctes)?),
                negated: *negated,
            },
            ScalarExpr::Compare { left, op, right } => ScalarExpr::Compare {
                left: Box::new(self.lower_cte_scalar_expr(left, ctes)?),
                op: *op,
                right: Box::new(self.lower_cte_scalar_expr(right, ctes)?),
            },
            ScalarExpr::CompareSubquery { left, op, query } => ScalarExpr::CompareSubquery {
                left: Box::new(self.lower_cte_scalar_expr(left, ctes)?),
                op: *op,
                query: Box::new(self.lower_cte_references(query, ctes)?),
            },
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => ScalarExpr::Case {
                base: base
                    .as_deref()
                    .map(|expr| self.lower_cte_scalar_expr(expr, ctes))
                    .transpose()?
                    .map(Box::new),
                when_then_clauses: when_then_clauses
                    .iter()
                    .map(|(when_expr, then_expr)| {
                        Ok((
                            self.lower_cte_scalar_expr(when_expr, ctes)?,
                            self.lower_cte_scalar_expr(then_expr, ctes)?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
                else_expr: else_expr
                    .as_deref()
                    .map(|expr| self.lower_cte_scalar_expr(expr, ctes))
                    .transpose()?
                    .map(Box::new),
            },
            ScalarExpr::Binary { left, op, right } => ScalarExpr::Binary {
                left: Box::new(self.lower_cte_scalar_expr(left, ctes)?),
                op: *op,
                right: Box::new(self.lower_cte_scalar_expr(right, ctes)?),
            },
            ScalarExpr::Function { func, args } => ScalarExpr::Function {
                func: *func,
                args: args
                    .iter()
                    .map(|arg| self.lower_cte_scalar_expr(arg, ctes))
                    .collect::<Result<Vec<_>>>()?,
            },
            ScalarExpr::Aggregate { func, arg, filter } => ScalarExpr::Aggregate {
                func: *func,
                arg: Box::new(self.lower_cte_aggregate_arg(arg, ctes)?),
                filter: filter
                    .as_deref()
                    .map(|expr| self.lower_cte_expr(expr, ctes))
                    .transpose()?
                    .map(Box::new),
            },
        })
    }

    fn plan_select_with_outer(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
        outer_scope: Option<&QueryScope>,
    ) -> Result<Plan> {
        if !select.compounds.is_empty() {
            return self.plan_compound_select(select, context, outer_scope);
        }

        let has_aggregates = self.select_has_aggregates(&select.columns);

        if has_aggregates || !select.group_by.is_empty() {
            self.validate_aggregate_projection(select, context)?;
            let rewritten_group_by =
                self.rewrite_group_by_positions(&select.group_by, &select.columns);

            if self.select_is_simple_base_table_source(select) {
                let (table, table_alias) = select
                    .base_table()
                    .ok_or_else(|| DbError::plan("subquery sources are not supported yet"))?;
                let schema = self.require_schema(context, table)?;
                let columns = self.normalize_aggregate_select_items(
                    table,
                    table_alias,
                    &select.columns,
                    context,
                )?;
                let group_by =
                    self.normalize_group_by(table, table_alias, &rewritten_group_by, context)?;
                let having = select
                    .having
                    .as_ref()
                    .map(|expr| {
                        self.normalize_aggregate_expr(
                            schema,
                            table,
                            table_alias,
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
                            table,
                            table_alias,
                            &columns,
                            &group_by,
                            item,
                        )
                        .map(|item| {
                            self.rewrite_aggregate_order_by_group_references(&item, &group_by)
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let source = self.plan_single_table_source(
                    SingleTablePlanInput {
                        table,
                        table_alias,
                        index_hint: TableIndexHintRef::None,
                        columns: &[SelectItem::Wildcard],
                        filter: &select.filter,
                        order_by: &[],
                        limit: None,
                        offset: None,
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
                    offset: select.offset,
                });
            }

            let source = self.plan_aggregate_source(select, context, outer_scope)?;
            let rewritten_having = select.having.as_ref().map(|expr| {
                self.rewrite_aggregate_expr_group_references(expr, &rewritten_group_by)
            });
            let rewritten_order_by = select
                .order_by
                .iter()
                .map(|item| {
                    self.rewrite_aggregate_order_by_group_references(item, &rewritten_group_by)
                })
                .collect();
            return Ok(Plan::Aggregate {
                source: Box::new(source),
                columns: select.columns.clone(),
                group_by: rewritten_group_by,
                having: rewritten_having,
                order_by: rewritten_order_by,
                limit: select.limit,
                offset: select.offset,
            });
        }

        if !select.joins.is_empty() {
            let scope = self.build_scope(select, context)?;
            for item in &select.columns {
                self.require_join_select_item(&scope, item)?;
            }
            if let Some(filter) = &select.filter {
                self.require_scope_columns_with_outer(&scope, outer_scope, filter)?;
                self.validate_subqueries(filter, context, &scope)?;
            }
            for (index, join) in select.joins.iter().enumerate() {
                let join_scope = self.build_join_scope(select, context, index)?;
                self.require_scope_columns_with_outer(&join_scope, outer_scope, &join.on)?;
                self.validate_subqueries(&join.on, context, &join_scope)?;
            }
            for item in &select.order_by {
                self.require_order_by_scope(&scope, &select.columns, item)?;
            }

            return Ok(Plan::NestedLoopJoin {
                source: Box::new(self.plan_source_item(&select.from, context, outer_scope)?),
                joins: self.plan_join_clauses(&select.from, &select.joins, context, outer_scope)?,
                columns: select.columns.clone(),
                filter: select.filter.clone(),
                order_by: select.order_by.clone(),
                limit: select.limit,
                offset: select.offset,
                distinct: select.distinct,
            });
        }

        if let Some((source, alias, output_columns)) =
            self.plan_inline_source_item(&select.from, context, outer_scope)?
        {
            let derived_scope = self.build_derived_scope(&alias, &output_columns);

            for item in &select.columns {
                self.require_join_select_item(&derived_scope, item)?;
            }

            if let Some(filter) = &select.filter {
                self.require_scope_columns_with_outer(&derived_scope, outer_scope, filter)?;
                self.validate_subqueries(filter, context, &derived_scope)?;
            }

            for item in &select.order_by {
                self.require_order_by_scope(&derived_scope, &select.columns, item)?;
            }

            return self.plan_derived_source(DerivedSourcePlanInput {
                alias: &alias,
                source,
                output_columns,
                columns: &select.columns,
                filter: &select.filter,
                order_by: &select.order_by,
                limit: select.limit,
                offset: select.offset,
                distinct: select.distinct,
            });
        }

        let (table, table_alias) = select
            .base_table()
            .ok_or_else(|| DbError::plan("subquery sources are not supported yet"))?;

        self.plan_single_table_source(
            SingleTablePlanInput {
                table,
                table_alias,
                index_hint: table_index_hint(&select.from),
                columns: &select.columns,
                filter: &select.filter,
                order_by: &select.order_by,
                limit: select.limit,
                offset: select.offset,
                distinct: select.distinct,
            },
            context,
            outer_scope,
        )
    }

    fn plan_compound_select(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
        outer_scope: Option<&QueryScope>,
    ) -> Result<Plan> {
        let output_columns = self.derived_output_columns(select, context)?;
        let expected_width = output_columns.len();
        let compound_scope = self.build_derived_scope("__compound__", &output_columns);

        for item in &select.order_by {
            self.require_order_by_scope(&compound_scope, &select.columns, item)?;
        }

        let mut left_branch = select.clone();
        left_branch.compounds.clear();
        left_branch.order_by.clear();
        left_branch.limit = None;
        left_branch.offset = None;
        let mut plan = self.plan_select_with_outer(&left_branch, context, outer_scope)?;

        for (index, compound) in select.compounds.iter().enumerate() {
            let rhs_width = self
                .derived_output_columns(compound.select.as_ref(), context)?
                .len();
            if rhs_width != expected_width {
                return Err(DbError::plan(
                    "UNION branches must return the same number of columns",
                ));
            }

            let is_last = index + 1 == select.compounds.len();
            plan = Plan::Union {
                left: Box::new(plan),
                right: Box::new(self.plan_select_with_outer(
                    compound.select.as_ref(),
                    context,
                    outer_scope,
                )?),
                operator: compound.operator,
                all: matches!(compound.operator, CompoundOperator::UnionAll),
                order_by: if is_last {
                    select.order_by.clone()
                } else {
                    vec![]
                },
                limit: if is_last { select.limit } else { None },
                offset: if is_last { select.offset } else { None },
            };
        }

        Ok(plan)
    }

    fn plan_aggregate_source(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
        outer_scope: Option<&QueryScope>,
    ) -> Result<Plan> {
        if !select.joins.is_empty() {
            return self.plan_join_source(select, context, outer_scope);
        }

        match &select.from {
            FromItem::Table { .. }
            | FromItem::TableIndexed { .. }
            | FromItem::TableNotIndexed { .. } => {
                let (name, alias, index_hint) = table_source_parts(&select.from)
                    .expect("table source pattern must expose table parts");
                self.plan_single_table_source(
                    SingleTablePlanInput {
                        table: name,
                        table_alias: alias,
                        index_hint,
                        columns: &[SelectItem::Wildcard],
                        filter: &select.filter,
                        order_by: &[],
                        limit: None,
                        offset: None,
                        distinct: false,
                    },
                    context,
                    outer_scope,
                )
            }
            FromItem::Subquery { .. } | FromItem::Values { .. } => {
                let (source, alias, output_columns) = self
                    .plan_inline_source_item(&select.from, context, outer_scope)?
                    .expect("inline source item should produce a plan");
                let derived_scope = self.build_derived_scope(&alias, &output_columns);

                if let Some(filter) = &select.filter {
                    self.require_scope_columns_with_outer(&derived_scope, outer_scope, filter)?;
                    self.validate_subqueries(filter, context, &derived_scope)?;
                }

                self.plan_derived_source(DerivedSourcePlanInput {
                    alias: &alias,
                    source,
                    output_columns,
                    columns: &[SelectItem::Wildcard],
                    filter: &select.filter,
                    order_by: &[],
                    limit: None,
                    offset: None,
                    distinct: false,
                })
            }
        }
    }

    fn plan_inline_source_item(
        &self,
        from: &FromItem,
        context: &PlanningContext,
        outer_scope: Option<&QueryScope>,
    ) -> Result<Option<(Plan, String, Vec<String>)>> {
        match from {
            FromItem::Subquery {
                query,
                alias,
                columns,
            } => {
                let output_columns = self.apply_output_columns_override(
                    self.derived_output_columns(query, context)?,
                    columns.as_ref(),
                )?;
                Ok(Some((
                    self.plan_select_with_outer(query, context, outer_scope)?,
                    alias.clone(),
                    output_columns,
                )))
            }
            FromItem::Values {
                rows,
                alias,
                columns,
            } => {
                let output_columns = self.apply_output_columns_override(
                    self.values_output_columns(rows)?,
                    columns.as_ref(),
                )?;
                Ok(Some((
                    Plan::Values { rows: rows.clone() },
                    alias
                        .clone()
                        .unwrap_or_else(|| VALUES_SOURCE_TABLE.to_string()),
                    output_columns,
                )))
            }
            FromItem::Table { .. }
            | FromItem::TableIndexed { .. }
            | FromItem::TableNotIndexed { .. } => Ok(None),
        }
    }

    fn select_is_simple_base_table_source(&self, select: &SelectStatement) -> bool {
        select.joins.is_empty()
            && matches!(
                select.from,
                FromItem::Table { .. }
                    | FromItem::TableIndexed { .. }
                    | FromItem::TableNotIndexed { .. }
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
            index_hint,
            columns,
            filter,
            order_by,
            limit,
            offset,
            distinct,
        } = input;
        let schema = self.require_schema(context, table)?;
        self.validate_table_index_hint(context, table, index_hint)?;

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

        if matches!(index_hint, TableIndexHintRef::NotIndexed) {
            Ok(Plan::ForcedSeqScan {
                table: table.to_string(),
                table_alias: table_alias.map(str::to_string),
                columns: normalized_columns,
                filter: normalized_filter,
                order_by: normalized_order_by,
                limit,
                offset,
                distinct,
            })
        } else {
            Ok(Plan::SeqScan {
                table: table.to_string(),
                table_alias: table_alias.map(str::to_string),
                columns: normalized_columns,
                filter: normalized_filter,
                order_by: normalized_order_by,
                limit,
                offset,
                distinct,
            })
        }
    }

    fn validate_table_index_hint(
        &self,
        context: &PlanningContext,
        table: &str,
        index_hint: TableIndexHintRef<'_>,
    ) -> Result<()> {
        let TableIndexHintRef::IndexedBy(index_name) = index_hint else {
            return Ok(());
        };
        if context
            .indexes_for(table)
            .iter()
            .any(|index| index.name == *index_name)
        {
            Ok(())
        } else {
            Err(DbError::plan(format!("no such index: {index_name}")))
        }
    }

    fn plan_derived_source(&self, input: DerivedSourcePlanInput<'_>) -> Result<Plan> {
        let DerivedSourcePlanInput {
            alias,
            source,
            output_columns,
            columns,
            filter,
            order_by,
            limit,
            offset,
            distinct,
        } = input;

        Ok(Plan::DerivedSource {
            source: Box::new(source),
            alias: alias.to_string(),
            output_columns,
            columns: columns.to_vec(),
            filter: filter.clone(),
            order_by: order_by.to_vec(),
            limit,
            offset,
            distinct,
        })
    }

    fn plan_join_source(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
        outer_scope: Option<&QueryScope>,
    ) -> Result<Plan> {
        let scope = self.build_scope(select, context)?;

        for item in &select.columns {
            self.require_join_select_item(&scope, item)?;
        }
        if let Some(filter) = &select.filter {
            self.require_scope_columns_with_outer(&scope, outer_scope, filter)?;
            self.validate_subqueries(filter, context, &scope)?;
        }
        for (index, join) in select.joins.iter().enumerate() {
            let join_scope = self.build_join_scope(select, context, index)?;
            self.require_scope_columns_with_outer(&join_scope, outer_scope, &join.on)?;
            self.validate_subqueries(&join.on, context, &join_scope)?;
        }
        for item in &select.order_by {
            self.require_order_by_scope(&scope, &select.columns, item)?;
        }

        Ok(Plan::NestedLoopJoin {
            source: Box::new(self.plan_source_item(&select.from, context, outer_scope)?),
            joins: self.plan_join_clauses(&select.from, &select.joins, context, outer_scope)?,
            columns: vec![SelectItem::Wildcard],
            filter: select.filter.clone(),
            order_by: vec![],
            limit: None,
            offset: None,
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
        let generated_column_count = schema
            .columns
            .iter()
            .filter(|column| column.generated_expr.is_some())
            .count();
        match columns {
            None => {
                let expected_values = schema.columns.len().saturating_sub(generated_column_count);
                if values.is_empty() {
                    return schema
                        .columns
                        .iter()
                        .map(|column| {
                            if column.generated_expr.is_some() {
                                Ok(Value::Null)
                            } else {
                                column
                                    .default_value
                                    .as_ref()
                                    .map_or(Ok(Value::Null), |default| default.evaluate())
                            }
                        })
                        .collect();
                }
                if values.len() != expected_values {
                    return Err(DbError::plan(format!(
                        "insert into {table} expected {} values but got {}",
                        expected_values,
                        values.len()
                    )));
                }
                let mut row = Vec::with_capacity(schema.columns.len());
                let mut input_values = values.iter();
                for column in &schema.columns {
                    if column.generated_expr.is_some() {
                        row.push(Value::Null);
                    } else {
                        row.push(input_values.next().cloned().ok_or_else(|| {
                            DbError::plan(format!(
                                "insert into {table} expected {} values but got {}",
                                expected_values,
                                values.len()
                            ))
                        })?);
                    }
                }
                Ok(row)
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
                    .map(|column| {
                        column
                            .default_value
                            .as_ref()
                            .map_or(Ok(Value::Null), |default| default.evaluate())
                    })
                    .collect::<Result<Vec<_>>>()?;
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
                    if schema.columns[position].generated_expr.is_some() {
                        return Err(DbError::plan(format!(
                            "cannot INSERT into generated column {column}"
                        )));
                    }
                    row[position] = value.clone();
                }
                Ok(row)
            }
        }
    }

    fn validate_insert_value_arity(
        &self,
        schema: &Schema,
        table: &str,
        columns: Option<&[String]>,
        value_count: usize,
    ) -> Result<()> {
        let generated_column_count = schema
            .columns
            .iter()
            .filter(|column| column.generated_expr.is_some())
            .count();
        match columns {
            None => {
                let expected_values = schema.columns.len().saturating_sub(generated_column_count);
                if value_count != expected_values {
                    return Err(DbError::plan(format!(
                        "insert into {table} expected {} values but got {}",
                        expected_values, value_count
                    )));
                }
            }
            Some(columns) => {
                if value_count != columns.len() {
                    return Err(DbError::plan(format!(
                        "insert into {table} expected {} values but got {}",
                        columns.len(),
                        value_count
                    )));
                }
                for column in columns {
                    self.require_column(schema, column)?;
                }
            }
        }
        Ok(())
    }

    fn plan_insert_value_exprs(
        &self,
        schema: &Schema,
        table: &str,
        columns: Option<&[String]>,
        values: &[ScalarExpr],
    ) -> Result<Vec<ScalarExpr>> {
        self.require_insert_value_exprs(values)?;
        let single_row_schema = PlanningContext::single_row_source_schema(SINGLE_ROW_SOURCE_TABLE)
            .expect("single row source schema");
        let normalized_values = values
            .iter()
            .map(|expr| {
                self.normalize_scalar_expr(&single_row_schema, SINGLE_ROW_SOURCE_TABLE, None, expr)
            })
            .collect::<Result<Vec<_>>>()?;
        self.validate_insert_value_arity(schema, table, columns, normalized_values.len())?;
        Ok(normalized_values)
    }

    fn plan_insert_many_value_expr_rows(
        &self,
        schema: &Schema,
        table: &str,
        columns: Option<&[String]>,
        rows: &[Vec<ScalarExpr>],
    ) -> Result<Vec<Vec<ScalarExpr>>> {
        rows.iter()
            .map(|values| self.plan_insert_value_exprs(schema, table, columns, values))
            .collect()
    }

    fn plan_upsert_clause(
        &self,
        schema: &Schema,
        table: &str,
        upsert: &UpsertClause,
        context: &PlanningContext,
    ) -> Result<UpsertClause> {
        self.validate_do_nothing_target(schema, table, upsert.target.as_deref(), context)?;
        let scope = self.build_upsert_scope(schema, table);
        let mut seen = std::collections::BTreeSet::new();
        let mut assignments = Vec::with_capacity(upsert.assignments.len());
        for assignment in &upsert.assignments {
            self.require_column(schema, &assignment.column)?;
            if !seen.insert(assignment.column.clone()) {
                return Err(DbError::plan(format!(
                    "duplicate assignment target: {}",
                    assignment.column
                )));
            }
            let value = self.normalize_upsert_scalar_expr(schema, table, &assignment.value)?;
            self.require_scalar_expr_scope(&scope, &value)?;
            assignments.push(Assignment {
                column: assignment.column.clone(),
                value,
            });
        }
        let filter = upsert
            .filter
            .as_ref()
            .map(|expr| self.normalize_upsert_filter_expr(schema, table, expr))
            .transpose()?;
        if let Some(expr) = &filter {
            self.require_scope_columns_with_outer(&scope, None, expr)?;
        }
        Ok(UpsertClause {
            target: upsert.target.clone(),
            assignments,
            filter,
        })
    }

    fn build_upsert_scope(&self, schema: &Schema, table: &str) -> QueryScope {
        QueryScope {
            bindings: vec![
                TableBinding {
                    table: table.to_string(),
                    alias: None,
                    schema: schema.clone(),
                    exposes_rowid: self.schema_exposes_rowid(schema),
                    hidden_columns: Vec::new(),
                },
                TableBinding {
                    table: "excluded".to_string(),
                    alias: None,
                    schema: schema.clone(),
                    exposes_rowid: false,
                    hidden_columns: schema
                        .columns
                        .iter()
                        .map(|column| column.name.clone())
                        .collect(),
                },
            ],
        }
    }

    fn normalize_upsert_scalar_expr(
        &self,
        schema: &Schema,
        table: &str,
        expr: &ScalarExpr,
    ) -> Result<ScalarExpr> {
        match expr {
            ScalarExpr::Column(name) => {
                if let Some((prefix, suffix)) = name.split_once('.') {
                    if prefix == "excluded" {
                        self.require_column(schema, suffix)?;
                        return Ok(ScalarExpr::Column(name.clone()));
                    }
                }
                self.normalize_scalar_expr(schema, table, None, expr)
            }
            ScalarExpr::Literal(_) => self.normalize_scalar_expr(schema, table, None, expr),
            ScalarExpr::Tuple(values) => Ok(ScalarExpr::Tuple(
                values
                    .iter()
                    .map(|value| self.normalize_upsert_scalar_expr(schema, table, value))
                    .collect::<Result<Vec<_>>>()?,
            )),
            ScalarExpr::UnaryMinus(value) => Ok(ScalarExpr::UnaryMinus(Box::new(
                self.normalize_upsert_scalar_expr(schema, table, value)?,
            ))),
            ScalarExpr::BitNot(value) => Ok(ScalarExpr::BitNot(Box::new(
                self.normalize_upsert_scalar_expr(schema, table, value)?,
            ))),
            ScalarExpr::Not(value) => Ok(ScalarExpr::Not(Box::new(
                self.normalize_upsert_scalar_expr(schema, table, value)?,
            ))),
            ScalarExpr::Collate { expr, collation } => Ok(ScalarExpr::Collate {
                expr: Box::new(self.normalize_upsert_scalar_expr(schema, table, expr)?),
                collation: collation.clone(),
            }),
            ScalarExpr::Cast { expr, ty } => Ok(ScalarExpr::Cast {
                expr: Box::new(self.normalize_upsert_scalar_expr(schema, table, expr)?),
                ty: *ty,
            }),
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => Ok(ScalarExpr::Is {
                left: Box::new(self.normalize_upsert_scalar_expr(schema, table, left)?),
                right: Box::new(self.normalize_upsert_scalar_expr(schema, table, right)?),
                negated: *negated,
            }),
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => Ok(ScalarExpr::IsBool {
                expr: Box::new(self.normalize_upsert_scalar_expr(schema, table, expr)?),
                value: *value,
                negated: *negated,
            }),
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => Ok(ScalarExpr::InList {
                expr: Box::new(self.normalize_upsert_scalar_expr(schema, table, expr)?),
                values: values
                    .iter()
                    .map(|value| self.normalize_upsert_scalar_expr(schema, table, value))
                    .collect::<Result<Vec<_>>>()?,
                negated: *negated,
            }),
            ScalarExpr::InSubquery {
                expr,
                query,
                negated,
            } => Ok(ScalarExpr::InSubquery {
                expr: Box::new(self.normalize_upsert_scalar_expr(schema, table, expr)?),
                query: query.clone(),
                negated: *negated,
            }),
            ScalarExpr::Subquery { query } => Ok(ScalarExpr::Subquery {
                query: query.clone(),
            }),
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => Ok(ScalarExpr::Like {
                expr: Box::new(self.normalize_upsert_scalar_expr(schema, table, expr)?),
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            }),
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => Ok(ScalarExpr::Glob {
                expr: Box::new(self.normalize_upsert_scalar_expr(schema, table, expr)?),
                pattern: pattern.clone(),
                negated: *negated,
            }),
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => Ok(ScalarExpr::Between {
                expr: Box::new(self.normalize_upsert_scalar_expr(schema, table, expr)?),
                low: Box::new(self.normalize_upsert_scalar_expr(schema, table, low)?),
                high: Box::new(self.normalize_upsert_scalar_expr(schema, table, high)?),
                negated: *negated,
            }),
            ScalarExpr::Compare { left, op, right } => Ok(ScalarExpr::Compare {
                left: Box::new(self.normalize_upsert_scalar_expr(schema, table, left)?),
                op: *op,
                right: Box::new(self.normalize_upsert_scalar_expr(schema, table, right)?),
            }),
            ScalarExpr::CompareSubquery { left, op, query } => Ok(ScalarExpr::CompareSubquery {
                left: Box::new(self.normalize_upsert_scalar_expr(schema, table, left)?),
                op: *op,
                query: query.clone(),
            }),
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => Ok(ScalarExpr::Case {
                base: base
                    .as_ref()
                    .map(|expr| self.normalize_upsert_scalar_expr(schema, table, expr))
                    .transpose()?
                    .map(Box::new),
                when_then_clauses: when_then_clauses
                    .iter()
                    .map(|(when_expr, then_expr)| {
                        Ok((
                            self.normalize_upsert_scalar_expr(schema, table, when_expr)?,
                            self.normalize_upsert_scalar_expr(schema, table, then_expr)?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
                else_expr: else_expr
                    .as_ref()
                    .map(|expr| self.normalize_upsert_scalar_expr(schema, table, expr))
                    .transpose()?
                    .map(Box::new),
            }),
            ScalarExpr::Binary { left, op, right } => Ok(ScalarExpr::Binary {
                left: Box::new(self.normalize_upsert_scalar_expr(schema, table, left)?),
                op: *op,
                right: Box::new(self.normalize_upsert_scalar_expr(schema, table, right)?),
            }),
            ScalarExpr::Function { func, args } => Ok(ScalarExpr::Function {
                func: *func,
                args: args
                    .iter()
                    .map(|arg| self.normalize_upsert_scalar_expr(schema, table, arg))
                    .collect::<Result<Vec<_>>>()?,
            }),
            ScalarExpr::Aggregate { .. } => Err(DbError::plan(
                "aggregate functions are not allowed in ON CONFLICT DO UPDATE",
            )),
        }
    }

    fn normalize_upsert_filter_expr(
        &self,
        schema: &Schema,
        table: &str,
        expr: &Expr,
    ) -> Result<Expr> {
        Ok(match expr {
            Expr::Compare { column, op, value } => Expr::Compare {
                column: scalar_column_name(self.normalize_upsert_scalar_expr(
                    schema,
                    table,
                    &ScalarExpr::Column(column.clone()),
                )?)?,
                op: *op,
                value: value.clone(),
            },
            Expr::CompareColumns { left, op, right } => Expr::CompareColumns {
                left: scalar_column_name(self.normalize_upsert_scalar_expr(
                    schema,
                    table,
                    &ScalarExpr::Column(left.clone()),
                )?)?,
                op: *op,
                right: scalar_column_name(self.normalize_upsert_scalar_expr(
                    schema,
                    table,
                    &ScalarExpr::Column(right.clone()),
                )?)?,
            },
            Expr::CompareScalar { left, op, right } => Expr::CompareScalar {
                left: self.normalize_upsert_scalar_expr(schema, table, left)?,
                op: *op,
                right: self.normalize_upsert_scalar_expr(schema, table, right)?,
            },
            Expr::IsNull { column, negated } => Expr::IsNull {
                column: scalar_column_name(self.normalize_upsert_scalar_expr(
                    schema,
                    table,
                    &ScalarExpr::Column(column.clone()),
                )?)?,
                negated: *negated,
            },
            Expr::IsNullScalar { expr, negated } => Expr::IsNullScalar {
                expr: self.normalize_upsert_scalar_expr(schema, table, expr)?,
                negated: *negated,
            },
            Expr::Is {
                left,
                right,
                negated,
            } => Expr::Is {
                left: self.normalize_upsert_scalar_expr(schema, table, left)?,
                right: self.normalize_upsert_scalar_expr(schema, table, right)?,
                negated: *negated,
            },
            Expr::IsBool {
                expr,
                value,
                negated,
                explicit,
            } => Expr::IsBool {
                expr: self.normalize_upsert_scalar_expr(schema, table, expr)?,
                value: *value,
                negated: *negated,
                explicit: *explicit,
            },
            Expr::InListScalar {
                expr,
                values,
                negated,
            } => Expr::InListScalar {
                expr: self.normalize_upsert_scalar_expr(schema, table, expr)?,
                values: values
                    .iter()
                    .map(|value| self.normalize_upsert_scalar_expr(schema, table, value))
                    .collect::<Result<Vec<_>>>()?,
                negated: *negated,
            },
            Expr::LikeScalar {
                expr,
                pattern,
                escape,
                negated,
            } => Expr::LikeScalar {
                expr: self.normalize_upsert_scalar_expr(schema, table, expr)?,
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            },
            Expr::GlobScalar {
                expr,
                pattern,
                negated,
            } => Expr::GlobScalar {
                expr: self.normalize_upsert_scalar_expr(schema, table, expr)?,
                pattern: pattern.clone(),
                negated: *negated,
            },
            Expr::BetweenScalar {
                expr,
                low,
                high,
                negated,
            } => Expr::BetweenScalar {
                expr: self.normalize_upsert_scalar_expr(schema, table, expr)?,
                low: self.normalize_upsert_scalar_expr(schema, table, low)?,
                high: self.normalize_upsert_scalar_expr(schema, table, high)?,
                negated: *negated,
            },
            Expr::Not(expr) => Expr::Not(Box::new(
                self.normalize_upsert_filter_expr(schema, table, expr)?,
            )),
            Expr::And(left, right) => Expr::And(
                Box::new(self.normalize_upsert_filter_expr(schema, table, left)?),
                Box::new(self.normalize_upsert_filter_expr(schema, table, right)?),
            ),
            Expr::Or(left, right) => Expr::Or(
                Box::new(self.normalize_upsert_filter_expr(schema, table, left)?),
                Box::new(self.normalize_upsert_filter_expr(schema, table, right)?),
            ),
            Expr::InList {
                column,
                values,
                negated,
            } => Expr::InListScalar {
                expr: self.normalize_upsert_scalar_expr(
                    schema,
                    table,
                    &ScalarExpr::Column(column.clone()),
                )?,
                values: values.iter().cloned().map(ScalarExpr::Literal).collect(),
                negated: *negated,
            },
            Expr::Like {
                column,
                pattern,
                escape,
                negated,
            } => Expr::LikeScalar {
                expr: self.normalize_upsert_scalar_expr(
                    schema,
                    table,
                    &ScalarExpr::Column(column.clone()),
                )?,
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            },
            Expr::Glob {
                column,
                pattern,
                negated,
            } => Expr::GlobScalar {
                expr: self.normalize_upsert_scalar_expr(
                    schema,
                    table,
                    &ScalarExpr::Column(column.clone()),
                )?,
                pattern: pattern.clone(),
                negated: *negated,
            },
            Expr::Between {
                column,
                low,
                high,
                negated,
            } => Expr::BetweenScalar {
                expr: self.normalize_upsert_scalar_expr(
                    schema,
                    table,
                    &ScalarExpr::Column(column.clone()),
                )?,
                low: ScalarExpr::Literal(low.clone()),
                high: ScalarExpr::Literal(high.clone()),
                negated: *negated,
            },
            Expr::InSubquery { .. }
            | Expr::InSubqueryScalar { .. }
            | Expr::CompareSubquery { .. }
            | Expr::CompareSubqueryScalar { .. }
            | Expr::ExistsSubquery { .. } => expr.clone(),
        })
    }

    fn plan_returning_items(
        &self,
        schema: &Schema,
        table: &str,
        returning: &[SelectItem],
    ) -> Result<Vec<SelectItem>> {
        let returning = returning
            .iter()
            .map(|item| self.normalize_select_item(schema, table, None, item))
            .collect::<Result<Vec<_>>>()?;
        for item in &returning {
            self.require_select_item_columns(schema, item)?;
        }
        Ok(returning)
    }

    fn require_insert_value_exprs(&self, values: &[ScalarExpr]) -> Result<()> {
        let scope = QueryScope { bindings: vec![] };
        for expr in values {
            self.require_scalar_expr_scope(&scope, expr)?;
        }
        Ok(())
    }

    fn validate_do_nothing_target(
        &self,
        schema: &Schema,
        table: &str,
        target: Option<&[String]>,
        context: &PlanningContext,
    ) -> Result<()> {
        let Some(target) = target else {
            return Ok(());
        };
        if target.is_empty() {
            return Err(DbError::plan(
                "ON CONFLICT target must include at least one column",
            ));
        }
        for column in target {
            self.require_column(schema, column)?;
        }

        let mut matches_primary_key = false;
        if let Some(primary_key) = &schema.primary_key_constraint {
            matches_primary_key = primary_key.columns == target;
        } else {
            let inline_primary_key = schema
                .columns
                .iter()
                .filter(|column| column.primary_key)
                .map(|column| column.name.clone())
                .collect::<Vec<_>>();
            if !inline_primary_key.is_empty() {
                matches_primary_key = inline_primary_key == target;
            }
        }
        if matches_primary_key {
            return Ok(());
        }

        if schema
            .unique_constraints
            .iter()
            .any(|constraint| constraint.columns == target)
        {
            return Ok(());
        }

        if context
            .indexes_for(table)
            .iter()
            .any(|index| index.unique && index.columns == target)
        {
            return Ok(());
        }

        Err(DbError::plan(format!(
            "ON CONFLICT target does not match any PRIMARY KEY or UNIQUE constraint on table {table}"
        )))
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
            SelectItem::Aggregate {
                func,
                arg,
                filter,
                alias,
            } => SelectItem::Aggregate {
                func: *func,
                arg: self.normalize_aggregate_arg(schema, table, table_alias, arg)?,
                filter: filter
                    .as_ref()
                    .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
                    .transpose()?,
                alias: alias.clone(),
            },
        })
    }

    fn normalize_aggregate_arg(
        &self,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        arg: &AggregateArg,
    ) -> Result<AggregateArg> {
        Ok(match arg {
            AggregateArg::Wildcard => AggregateArg::Wildcard,
            AggregateArg::Expr {
                expr,
                distinct,
                order_by,
            } => AggregateArg::Expr {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                distinct: *distinct,
                order_by: order_by
                    .iter()
                    .map(|item| self.normalize_order_by(schema, table, table_alias, &[], item))
                    .collect::<Result<Vec<_>>>()?,
            },
            AggregateArg::GroupConcat {
                expr,
                separator,
                distinct,
                order_by,
            } => AggregateArg::GroupConcat {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                separator: separator
                    .as_ref()
                    .map(|expr| self.normalize_scalar_expr(schema, table, table_alias, expr))
                    .transpose()?,
                distinct: *distinct,
                order_by: order_by
                    .iter()
                    .map(|item| self.normalize_order_by(schema, table, table_alias, &[], item))
                    .collect::<Result<Vec<_>>>()?,
            },
            AggregateArg::JsonGroupObject {
                key,
                value,
                order_by,
            } => AggregateArg::JsonGroupObject {
                key: self.normalize_scalar_expr(schema, table, table_alias, key)?,
                value: self.normalize_scalar_expr(schema, table, table_alias, value)?,
                order_by: order_by
                    .iter()
                    .map(|item| self.normalize_order_by(schema, table, table_alias, &[], item))
                    .collect::<Result<Vec<_>>>()?,
            },
            AggregateArg::Percentile {
                expr,
                fraction,
                order_by,
            } => AggregateArg::Percentile {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                fraction: self.normalize_scalar_expr(schema, table, table_alias, fraction)?,
                order_by: order_by
                    .iter()
                    .map(|item| self.normalize_order_by(schema, table, table_alias, &[], item))
                    .collect::<Result<Vec<_>>>()?,
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
                    collation: item.collation.clone(),
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
            collation: item.collation.clone(),
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
            collation: item.collation.clone(),
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
            Expr::Is {
                left,
                right,
                negated,
            } => Expr::Is {
                left: self.normalize_scalar_expr(schema, table, table_alias, left)?,
                right: self.normalize_scalar_expr(schema, table, table_alias, right)?,
                negated: *negated,
            },
            Expr::IsBool {
                expr,
                value,
                negated,
                explicit,
            } => Expr::IsBool {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                value: *value,
                negated: *negated,
                explicit: *explicit,
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
            Expr::InList {
                column,
                values,
                negated,
            } => Expr::InList {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
                values: values.clone(),
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
            Expr::InListScalar {
                expr,
                values,
                negated,
            } => Expr::InListScalar {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                values: values
                    .iter()
                    .map(|value| self.normalize_scalar_expr(schema, table, table_alias, value))
                    .collect::<Result<Vec<_>>>()?,
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
                escape,
                negated,
            } => Expr::Like {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            },
            Expr::LikeScalar {
                expr,
                pattern,
                escape,
                negated,
            } => Expr::LikeScalar {
                expr: self.normalize_scalar_expr(schema, table, table_alias, expr)?,
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            },
            Expr::Glob {
                column,
                pattern,
                negated,
            } => Expr::Glob {
                column: self.normalize_column_reference(schema, table, table_alias, column)?,
                pattern: pattern.clone(),
                negated: *negated,
            },
            Expr::GlobScalar {
                expr,
                pattern,
                negated,
            } => Expr::GlobScalar {
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
            Expr::Is {
                left,
                right,
                negated,
            } => Expr::Is {
                left: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    left,
                )?,
                right: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    right,
                )?,
                negated: *negated,
            },
            Expr::IsBool {
                expr,
                value,
                negated,
                explicit,
            } => Expr::IsBool {
                expr: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?,
                value: *value,
                negated: *negated,
                explicit: *explicit,
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
            Expr::InList {
                column,
                values,
                negated,
            } => Expr::InList {
                column: self.normalize_aggregate_column_reference(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    column,
                )?,
                values: values.clone(),
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
            Expr::InListScalar {
                expr,
                values,
                negated,
            } => Expr::InListScalar {
                expr: self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?,
                values: values
                    .iter()
                    .map(|value| {
                        self.normalize_aggregate_scalar_expr(
                            schema,
                            table,
                            table_alias,
                            columns,
                            group_by,
                            value,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
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
                escape,
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
                escape: escape.clone(),
                negated: *negated,
            },
            Expr::LikeScalar {
                expr,
                pattern,
                escape,
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
                escape: escape.clone(),
                negated: *negated,
            },
            Expr::Glob {
                column,
                pattern,
                negated,
            } => Expr::Glob {
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
            Expr::GlobScalar {
                expr,
                pattern,
                negated,
            } => Expr::GlobScalar {
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
            ScalarExpr::BitNot(expr) => {
                ScalarExpr::BitNot(Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?))
            }
            ScalarExpr::Not(expr) => {
                ScalarExpr::Not(Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?))
            }
            ScalarExpr::Collate { expr, collation } => ScalarExpr::Collate {
                expr: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?),
                collation: collation.clone(),
            },
            ScalarExpr::Cast { expr, ty } => ScalarExpr::Cast {
                expr: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?),
                ty: *ty,
            },
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => ScalarExpr::Is {
                left: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    left,
                )?),
                right: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    right,
                )?),
                negated: *negated,
            },
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => ScalarExpr::IsBool {
                expr: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?),
                value: *value,
                negated: *negated,
            },
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => ScalarExpr::InList {
                expr: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?),
                values: values
                    .iter()
                    .map(|value| {
                        self.normalize_aggregate_scalar_expr(
                            schema,
                            table,
                            table_alias,
                            columns,
                            group_by,
                            value,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
                negated: *negated,
            },
            ScalarExpr::InSubquery {
                expr,
                query,
                negated,
            } => ScalarExpr::InSubquery {
                expr: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?),
                query: query.clone(),
                negated: *negated,
            },
            ScalarExpr::Subquery { query } => ScalarExpr::Subquery {
                query: query.clone(),
            },
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => ScalarExpr::Like {
                expr: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?),
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            },
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => ScalarExpr::Glob {
                expr: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?),
                pattern: pattern.clone(),
                negated: *negated,
            },
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => ScalarExpr::Between {
                expr: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    expr,
                )?),
                low: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    low,
                )?),
                high: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    high,
                )?),
                negated: *negated,
            },
            ScalarExpr::Compare { left, op, right } => ScalarExpr::Compare {
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
            ScalarExpr::CompareSubquery { left, op, query } => ScalarExpr::CompareSubquery {
                left: Box::new(self.normalize_aggregate_scalar_expr(
                    schema,
                    table,
                    table_alias,
                    columns,
                    group_by,
                    left,
                )?),
                op: *op,
                query: query.clone(),
            },
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => ScalarExpr::Case {
                base: match base {
                    Some(expr) => Some(Box::new(self.normalize_aggregate_scalar_expr(
                        schema,
                        table,
                        table_alias,
                        columns,
                        group_by,
                        expr,
                    )?)),
                    None => None,
                },
                when_then_clauses: when_then_clauses
                    .iter()
                    .map(|(when_expr, then_expr)| {
                        Ok((
                            self.normalize_aggregate_scalar_expr(
                                schema,
                                table,
                                table_alias,
                                columns,
                                group_by,
                                when_expr,
                            )?,
                            self.normalize_aggregate_scalar_expr(
                                schema,
                                table,
                                table_alias,
                                columns,
                                group_by,
                                then_expr,
                            )?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
                else_expr: match else_expr {
                    Some(expr) => Some(Box::new(self.normalize_aggregate_scalar_expr(
                        schema,
                        table,
                        table_alias,
                        columns,
                        group_by,
                        expr,
                    )?)),
                    None => None,
                },
            },
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
            ScalarExpr::Aggregate { func, arg, filter } => ScalarExpr::Aggregate {
                func: *func,
                arg: Box::new(self.normalize_aggregate_arg(schema, table, table_alias, arg)?),
                filter: filter
                    .as_deref()
                    .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
                    .transpose()?
                    .map(Box::new),
            },
            ScalarExpr::Tuple(values) => ScalarExpr::Tuple(
                values
                    .iter()
                    .map(|value| {
                        self.normalize_aggregate_scalar_expr(
                            schema,
                            table,
                            table_alias,
                            columns,
                            group_by,
                            value,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
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
            self.require_schema_column_or_rowid(schema, suffix)?;
            return Ok(suffix.to_string());
        }
        if column.contains('.') {
            return Ok(column.to_string());
        }
        self.require_schema_column_or_rowid(schema, column)?;
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
            ScalarExpr::BitNot(expr) => ScalarExpr::BitNot(Box::new(self.normalize_scalar_expr(
                schema,
                table,
                table_alias,
                expr,
            )?)),
            ScalarExpr::Not(expr) => ScalarExpr::Not(Box::new(self.normalize_scalar_expr(
                schema,
                table,
                table_alias,
                expr,
            )?)),
            ScalarExpr::Collate { expr, collation } => ScalarExpr::Collate {
                expr: Box::new(self.normalize_scalar_expr(schema, table, table_alias, expr)?),
                collation: collation.clone(),
            },
            ScalarExpr::Cast { expr, ty } => ScalarExpr::Cast {
                expr: Box::new(self.normalize_scalar_expr(schema, table, table_alias, expr)?),
                ty: *ty,
            },
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => ScalarExpr::Is {
                left: Box::new(self.normalize_scalar_expr(schema, table, table_alias, left)?),
                right: Box::new(self.normalize_scalar_expr(schema, table, table_alias, right)?),
                negated: *negated,
            },
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => ScalarExpr::IsBool {
                expr: Box::new(self.normalize_scalar_expr(schema, table, table_alias, expr)?),
                value: *value,
                negated: *negated,
            },
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => ScalarExpr::InList {
                expr: Box::new(self.normalize_scalar_expr(schema, table, table_alias, expr)?),
                values: values
                    .iter()
                    .map(|value| self.normalize_scalar_expr(schema, table, table_alias, value))
                    .collect::<Result<Vec<_>>>()?,
                negated: *negated,
            },
            ScalarExpr::InSubquery {
                expr,
                query,
                negated,
            } => ScalarExpr::InSubquery {
                expr: Box::new(self.normalize_scalar_expr(schema, table, table_alias, expr)?),
                query: query.clone(),
                negated: *negated,
            },
            ScalarExpr::Subquery { query } => ScalarExpr::Subquery {
                query: query.clone(),
            },
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => ScalarExpr::Like {
                expr: Box::new(self.normalize_scalar_expr(schema, table, table_alias, expr)?),
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            },
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => ScalarExpr::Glob {
                expr: Box::new(self.normalize_scalar_expr(schema, table, table_alias, expr)?),
                pattern: pattern.clone(),
                negated: *negated,
            },
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => ScalarExpr::Between {
                expr: Box::new(self.normalize_scalar_expr(schema, table, table_alias, expr)?),
                low: Box::new(self.normalize_scalar_expr(schema, table, table_alias, low)?),
                high: Box::new(self.normalize_scalar_expr(schema, table, table_alias, high)?),
                negated: *negated,
            },
            ScalarExpr::Compare { left, op, right } => ScalarExpr::Compare {
                left: Box::new(self.normalize_scalar_expr(schema, table, table_alias, left)?),
                op: *op,
                right: Box::new(self.normalize_scalar_expr(schema, table, table_alias, right)?),
            },
            ScalarExpr::CompareSubquery { left, op, query } => ScalarExpr::CompareSubquery {
                left: Box::new(self.normalize_scalar_expr(schema, table, table_alias, left)?),
                op: *op,
                query: query.clone(),
            },
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => ScalarExpr::Case {
                base: match base {
                    Some(expr) => Some(Box::new(self.normalize_scalar_expr(
                        schema,
                        table,
                        table_alias,
                        expr,
                    )?)),
                    None => None,
                },
                when_then_clauses: when_then_clauses
                    .iter()
                    .map(|(when_expr, then_expr)| {
                        Ok((
                            self.normalize_scalar_expr(schema, table, table_alias, when_expr)?,
                            self.normalize_scalar_expr(schema, table, table_alias, then_expr)?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
                else_expr: match else_expr {
                    Some(expr) => Some(Box::new(self.normalize_scalar_expr(
                        schema,
                        table,
                        table_alias,
                        expr,
                    )?)),
                    None => None,
                },
            },
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
            ScalarExpr::Aggregate { func, arg, filter } => ScalarExpr::Aggregate {
                func: *func,
                arg: Box::new(self.normalize_aggregate_arg(schema, table, table_alias, arg)?),
                filter: filter
                    .as_deref()
                    .map(|expr| self.normalize_expr(schema, table, table_alias, expr))
                    .transpose()?
                    .map(Box::new),
            },
            ScalarExpr::Tuple(values) => ScalarExpr::Tuple(
                values
                    .iter()
                    .map(|value| self.normalize_scalar_expr(schema, table, table_alias, value))
                    .collect::<Result<Vec<_>>>()?,
            ),
        })
    }

    fn require_scalar_expr_columns(&self, schema: &Schema, expr: &ScalarExpr) -> Result<()> {
        match expr {
            ScalarExpr::Literal(_) => Ok(()),
            ScalarExpr::Column(name) => self.require_schema_column_or_rowid(schema, name),
            ScalarExpr::Tuple(values) => {
                for value in values {
                    self.require_scalar_expr_columns(schema, value)?;
                }
                Ok(())
            }
            ScalarExpr::UnaryMinus(expr) => self.require_scalar_expr_columns(schema, expr),
            ScalarExpr::BitNot(expr) => self.require_scalar_expr_columns(schema, expr),
            ScalarExpr::Not(expr) => self.require_scalar_expr_columns(schema, expr),
            ScalarExpr::Collate { expr, .. } => self.require_scalar_expr_columns(schema, expr),
            ScalarExpr::Cast { expr, .. } => self.require_scalar_expr_columns(schema, expr),
            ScalarExpr::Is { left, right, .. } => {
                self.require_scalar_expr_columns(schema, left)?;
                self.require_scalar_expr_columns(schema, right)
            }
            ScalarExpr::IsBool { expr, .. } => self.require_scalar_expr_columns(schema, expr),
            ScalarExpr::InList { expr, values, .. } => {
                self.require_scalar_expr_columns(schema, expr)?;
                for value in values {
                    self.require_scalar_expr_columns(schema, value)?;
                }
                Ok(())
            }
            ScalarExpr::InSubquery { expr, .. }
            | ScalarExpr::CompareSubquery { left: expr, .. } => {
                self.require_scalar_expr_columns(schema, expr)
            }
            ScalarExpr::Subquery { .. } => Ok(()),
            ScalarExpr::Like { expr, .. } | ScalarExpr::Glob { expr, .. } => {
                self.require_scalar_expr_columns(schema, expr)
            }
            ScalarExpr::Between {
                expr, low, high, ..
            } => {
                self.require_scalar_expr_columns(schema, expr)?;
                self.require_scalar_expr_columns(schema, low)?;
                self.require_scalar_expr_columns(schema, high)
            }
            ScalarExpr::Compare { left, right, .. } => {
                self.require_scalar_expr_columns(schema, left)?;
                self.require_scalar_expr_columns(schema, right)
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                if let Some(base) = base {
                    self.require_scalar_expr_columns(schema, base)?;
                }
                for (when_expr, then_expr) in when_then_clauses {
                    self.require_scalar_expr_columns(schema, when_expr)?;
                    self.require_scalar_expr_columns(schema, then_expr)?;
                }
                if let Some(else_expr) = else_expr {
                    self.require_scalar_expr_columns(schema, else_expr)?;
                }
                Ok(())
            }
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
            ScalarExpr::Aggregate { func, arg, filter } => {
                self.require_aggregate_arg_columns(schema, *func, arg)?;
                if let Some(filter) = filter {
                    self.require_filter_expr_columns(schema, filter)?;
                }
                Ok(())
            }
        }
    }

    fn require_scalar_expr_scope(&self, scope: &QueryScope, expr: &ScalarExpr) -> Result<()> {
        match expr {
            ScalarExpr::Literal(_) => Ok(()),
            ScalarExpr::Column(name) => self.resolve_column_in_scope(scope, name).map(|_| ()),
            ScalarExpr::Tuple(values) => {
                for value in values {
                    self.require_scalar_expr_scope(scope, value)?;
                }
                Ok(())
            }
            ScalarExpr::UnaryMinus(expr) => self.require_scalar_expr_scope(scope, expr),
            ScalarExpr::BitNot(expr) => self.require_scalar_expr_scope(scope, expr),
            ScalarExpr::Not(expr) => self.require_scalar_expr_scope(scope, expr),
            ScalarExpr::Collate { expr, .. } => self.require_scalar_expr_scope(scope, expr),
            ScalarExpr::Cast { expr, .. } => self.require_scalar_expr_scope(scope, expr),
            ScalarExpr::Is { left, right, .. } => {
                self.require_scalar_expr_scope(scope, left)?;
                self.require_scalar_expr_scope(scope, right)
            }
            ScalarExpr::IsBool { expr, .. } => self.require_scalar_expr_scope(scope, expr),
            ScalarExpr::InList { expr, values, .. } => {
                self.require_scalar_expr_scope(scope, expr)?;
                for value in values {
                    self.require_scalar_expr_scope(scope, value)?;
                }
                Ok(())
            }
            ScalarExpr::InSubquery { expr, .. }
            | ScalarExpr::CompareSubquery { left: expr, .. } => {
                self.require_scalar_expr_scope(scope, expr)
            }
            ScalarExpr::Subquery { .. } => Ok(()),
            ScalarExpr::Like { expr, .. } | ScalarExpr::Glob { expr, .. } => {
                self.require_scalar_expr_scope(scope, expr)
            }
            ScalarExpr::Between {
                expr, low, high, ..
            } => {
                self.require_scalar_expr_scope(scope, expr)?;
                self.require_scalar_expr_scope(scope, low)?;
                self.require_scalar_expr_scope(scope, high)
            }
            ScalarExpr::Compare { left, right, .. } => {
                self.require_scalar_expr_scope(scope, left)?;
                self.require_scalar_expr_scope(scope, right)
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                if let Some(base) = base {
                    self.require_scalar_expr_scope(scope, base)?;
                }
                for (when_expr, then_expr) in when_then_clauses {
                    self.require_scalar_expr_scope(scope, when_expr)?;
                    self.require_scalar_expr_scope(scope, then_expr)?;
                }
                if let Some(else_expr) = else_expr {
                    self.require_scalar_expr_scope(scope, else_expr)?;
                }
                Ok(())
            }
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
            ScalarExpr::Aggregate { func, arg, filter } => {
                self.require_aggregate_arg_scope(scope, *func, arg)?;
                if let Some(filter) = filter {
                    self.require_filter_expr_scope(scope, filter)?;
                }
                Ok(())
            }
        }
    }

    fn require_filter_expr_columns(&self, schema: &Schema, expr: &Expr) -> Result<()> {
        match expr {
            Expr::Compare { column, .. }
            | Expr::IsNull { column, .. }
            | Expr::Like { column, .. }
            | Expr::Glob { column, .. }
            | Expr::Between { column, .. }
            | Expr::InList { column, .. }
            | Expr::InSubquery { column, .. }
            | Expr::CompareSubquery { column, .. } => {
                self.require_schema_column_or_rowid(schema, column)
            }
            Expr::CompareColumns { left, right, .. } => {
                self.require_schema_column_or_rowid(schema, left)?;
                self.require_schema_column_or_rowid(schema, right)
            }
            Expr::CompareScalar { left, right, .. } | Expr::Is { left, right, .. } => {
                self.require_scalar_expr_columns(schema, left)?;
                self.require_scalar_expr_columns(schema, right)
            }
            Expr::IsNullScalar { expr, .. }
            | Expr::IsBool { expr, .. }
            | Expr::LikeScalar { expr, .. }
            | Expr::GlobScalar { expr, .. }
            | Expr::InSubqueryScalar { expr, .. }
            | Expr::CompareSubqueryScalar { left: expr, .. } => {
                self.require_scalar_expr_columns(schema, expr)
            }
            Expr::BetweenScalar {
                expr, low, high, ..
            } => {
                self.require_scalar_expr_columns(schema, expr)?;
                self.require_scalar_expr_columns(schema, low)?;
                self.require_scalar_expr_columns(schema, high)
            }
            Expr::InListScalar { expr, values, .. } => {
                self.require_scalar_expr_columns(schema, expr)?;
                for value in values {
                    self.require_scalar_expr_columns(schema, value)?;
                }
                Ok(())
            }
            Expr::ExistsSubquery { .. } => Ok(()),
            Expr::Not(expr) => self.require_filter_expr_columns(schema, expr),
            Expr::And(left, right) | Expr::Or(left, right) => {
                self.require_filter_expr_columns(schema, left)?;
                self.require_filter_expr_columns(schema, right)
            }
        }
    }

    fn require_filter_expr_scope(&self, scope: &QueryScope, expr: &Expr) -> Result<()> {
        self.require_scope_columns_with_outer(scope, None, expr)
    }

    fn require_aggregate_arg_columns(
        &self,
        schema: &Schema,
        func: AggregateFunc,
        arg: &AggregateArg,
    ) -> Result<()> {
        match arg {
            AggregateArg::Wildcard => {
                if func == AggregateFunc::Count {
                    Ok(())
                } else {
                    Err(DbError::plan(
                        "only COUNT supports wildcard aggregate argument",
                    ))
                }
            }
            AggregateArg::Expr { expr, order_by, .. } => {
                self.require_scalar_expr_columns(schema, expr)?;
                for item in order_by {
                    self.require_inner_order_by_columns(schema, item)?;
                }
                Ok(())
            }
            AggregateArg::GroupConcat {
                expr,
                separator,
                order_by,
                ..
            } => {
                self.require_scalar_expr_columns(schema, expr)?;
                if let Some(separator) = separator {
                    self.require_scalar_expr_columns(schema, separator)?;
                }
                for item in order_by {
                    self.require_inner_order_by_columns(schema, item)?;
                }
                Ok(())
            }
            AggregateArg::JsonGroupObject {
                key,
                value,
                order_by,
            } => {
                self.require_scalar_expr_columns(schema, key)?;
                self.require_scalar_expr_columns(schema, value)?;
                for item in order_by {
                    self.require_inner_order_by_columns(schema, item)?;
                }
                Ok(())
            }
            AggregateArg::Percentile {
                expr,
                fraction,
                order_by,
            } => {
                self.require_scalar_expr_columns(schema, expr)?;
                self.require_scalar_expr_columns(schema, fraction)?;
                for item in order_by {
                    self.require_inner_order_by_columns(schema, item)?;
                }
                Ok(())
            }
        }
    }

    fn require_aggregate_arg_scope(
        &self,
        scope: &QueryScope,
        func: AggregateFunc,
        arg: &AggregateArg,
    ) -> Result<()> {
        match arg {
            AggregateArg::Wildcard => {
                if func == AggregateFunc::Count {
                    Ok(())
                } else {
                    Err(DbError::plan(
                        "only COUNT supports wildcard aggregate argument",
                    ))
                }
            }
            AggregateArg::Expr { expr, order_by, .. } => {
                self.require_scalar_expr_scope(scope, expr)?;
                for item in order_by {
                    self.require_order_by_scope(scope, &[], item)?;
                }
                Ok(())
            }
            AggregateArg::GroupConcat {
                expr,
                separator,
                order_by,
                ..
            } => {
                self.require_scalar_expr_scope(scope, expr)?;
                if let Some(separator) = separator {
                    self.require_scalar_expr_scope(scope, separator)?;
                }
                for item in order_by {
                    self.require_order_by_scope(scope, &[], item)?;
                }
                Ok(())
            }
            AggregateArg::JsonGroupObject {
                key,
                value,
                order_by,
            } => {
                self.require_scalar_expr_scope(scope, key)?;
                self.require_scalar_expr_scope(scope, value)?;
                for item in order_by {
                    self.require_order_by_scope(scope, &[], item)?;
                }
                Ok(())
            }
            AggregateArg::Percentile {
                expr,
                fraction,
                order_by,
            } => {
                self.require_scalar_expr_scope(scope, expr)?;
                self.require_scalar_expr_scope(scope, fraction)?;
                for item in order_by {
                    self.require_order_by_scope(scope, &[], item)?;
                }
                Ok(())
            }
        }
    }

    fn select_has_aggregates(&self, columns: &[SelectItem]) -> bool {
        columns.iter().any(|column| match column {
            SelectItem::Aggregate { .. } => true,
            SelectItem::Expr { expr, .. } => Self::scalar_expr_has_aggregate(expr),
            SelectItem::Wildcard | SelectItem::Column(_) | SelectItem::AliasedColumn { .. } => {
                false
            }
        })
    }

    fn scalar_expr_has_aggregate(expr: &ScalarExpr) -> bool {
        match expr {
            ScalarExpr::Aggregate { .. } => true,
            ScalarExpr::UnaryMinus(expr)
            | ScalarExpr::BitNot(expr)
            | ScalarExpr::Not(expr)
            | ScalarExpr::Cast { expr, .. }
            | ScalarExpr::Collate { expr, .. }
            | ScalarExpr::IsBool { expr, .. }
            | ScalarExpr::Like { expr, .. }
            | ScalarExpr::Glob { expr, .. } => Self::scalar_expr_has_aggregate(expr),
            ScalarExpr::Is { left, right, .. }
            | ScalarExpr::Compare { left, right, .. }
            | ScalarExpr::Binary { left, right, .. } => {
                Self::scalar_expr_has_aggregate(left) || Self::scalar_expr_has_aggregate(right)
            }
            ScalarExpr::InList { expr, values, .. } => {
                Self::scalar_expr_has_aggregate(expr)
                    || values.iter().any(Self::scalar_expr_has_aggregate)
            }
            ScalarExpr::InSubquery { expr, .. }
            | ScalarExpr::CompareSubquery { left: expr, .. } => {
                Self::scalar_expr_has_aggregate(expr)
            }
            ScalarExpr::Subquery { .. } => false,
            ScalarExpr::Between {
                expr, low, high, ..
            } => {
                Self::scalar_expr_has_aggregate(expr)
                    || Self::scalar_expr_has_aggregate(low)
                    || Self::scalar_expr_has_aggregate(high)
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                base.as_deref().is_some_and(Self::scalar_expr_has_aggregate)
                    || when_then_clauses.iter().any(|(when_expr, then_expr)| {
                        Self::scalar_expr_has_aggregate(when_expr)
                            || Self::scalar_expr_has_aggregate(then_expr)
                    })
                    || else_expr
                        .as_deref()
                        .is_some_and(Self::scalar_expr_has_aggregate)
            }
            ScalarExpr::Function { args, .. } => args.iter().any(Self::scalar_expr_has_aggregate),
            ScalarExpr::Tuple(values) => values.iter().any(Self::scalar_expr_has_aggregate),
            ScalarExpr::Literal(_) | ScalarExpr::Column(_) => false,
        }
    }

    fn require_select_item_columns(&self, schema: &Schema, item: &SelectItem) -> Result<()> {
        match item {
            SelectItem::Wildcard => Ok(()),
            SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
                self.require_schema_column_or_rowid(schema, name)
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
                AggregateArg::Expr { expr, order_by, .. } => {
                    self.require_scalar_expr_columns(schema, expr)?;
                    for item in order_by {
                        self.require_inner_order_by_columns(schema, item)?;
                    }
                    Ok(())
                }
                AggregateArg::GroupConcat {
                    expr,
                    separator,
                    order_by,
                    ..
                } => {
                    self.require_scalar_expr_columns(schema, expr)?;
                    if let Some(separator) = separator {
                        self.require_scalar_expr_columns(schema, separator)?;
                    }
                    for item in order_by {
                        self.require_inner_order_by_columns(schema, item)?;
                    }
                    Ok(())
                }
                AggregateArg::JsonGroupObject {
                    key,
                    value,
                    order_by,
                } => {
                    self.require_scalar_expr_columns(schema, key)?;
                    self.require_scalar_expr_columns(schema, value)?;
                    for item in order_by {
                        self.require_inner_order_by_columns(schema, item)?;
                    }
                    Ok(())
                }
                AggregateArg::Percentile {
                    expr,
                    fraction,
                    order_by,
                } => {
                    self.require_scalar_expr_columns(schema, expr)?;
                    self.require_scalar_expr_columns(schema, fraction)?;
                    for item in order_by {
                        self.require_inner_order_by_columns(schema, item)?;
                    }
                    Ok(())
                }
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

    fn rewrite_group_by_positions(
        &self,
        group_by: &[ScalarExpr],
        columns: &[SelectItem],
    ) -> Vec<ScalarExpr> {
        group_by
            .iter()
            .map(|expr| match expr {
                ScalarExpr::Literal(Value::Integer(position)) if *position > 0 => {
                    let index = usize::try_from(*position - 1).ok();
                    index
                        .and_then(|index| columns.get(index))
                        .and_then(select_item_group_expr)
                        .unwrap_or_else(|| expr.clone())
                }
                _ => expr.clone(),
            })
            .collect()
    }

    fn validate_aggregate_projection(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
    ) -> Result<()> {
        let group_by = self.rewrite_group_by_positions(&select.group_by, &select.columns);
        if !select.joins.is_empty()
            || matches!(
                select.from,
                FromItem::Subquery { .. } | FromItem::Values { .. }
            )
        {
            let scope = self.build_scope(select, context)?;
            for expr in &group_by {
                self.require_scalar_expr_scope(&scope, expr)?;
            }
            for item in &select.columns {
                self.require_aggregate_select_item_in_scope(&scope, &group_by, item)?;
            }
            if let Some(having) = &select.having {
                self.require_aggregate_expr_references(having, &select.columns, &group_by)?;
            }
            for item in &select.order_by {
                self.require_aggregate_order_by_references(item, &select.columns, &group_by)?;
            }
            return Ok(());
        }

        let (table, table_alias) = select
            .base_table()
            .ok_or_else(|| DbError::plan("subquery sources are not supported yet"))?;
        let schema = self.require_schema(context, table)?;
        let normalized_columns =
            self.normalize_aggregate_select_items(table, table_alias, &select.columns, context)?;
        let normalized_group_by =
            self.normalize_group_by(table, table_alias, &group_by, context)?;
        let normalized_having = select
            .having
            .as_ref()
            .map(|expr| {
                self.normalize_aggregate_expr(
                    schema,
                    table,
                    table_alias,
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
                    table,
                    table_alias,
                    &normalized_columns,
                    &normalized_group_by,
                    item,
                )
            })
            .collect::<Result<Vec<_>>>()?;

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
                    if Self::scalar_expr_has_aggregate(expr) {
                        self.require_aggregate_scalar_reference(
                            expr,
                            &normalized_columns,
                            &normalized_group_by,
                        )?;
                    } else if !self.group_by_contains_expr(&normalized_group_by, expr) {
                        return Err(DbError::plan(format!(
                            "non-aggregate expression {} must appear in GROUP BY",
                            self.scalar_expr_display(expr)
                        )));
                    }
                }
                SelectItem::Aggregate { .. } => {}
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

    fn build_scope(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
    ) -> Result<QueryScope> {
        let mut bindings = Vec::with_capacity(select.joins.len() + 1);
        bindings.push(self.binding_from_from_item(&select.from, context)?);
        let mut left_columns = self.visible_from_item_output_columns(&select.from, context)?;
        for join in &select.joins {
            let right_columns = self.visible_from_item_output_columns(&join.source, context)?;
            let using_columns = join_output_using_columns(&left_columns, &right_columns, join);
            bindings.push(self.binding_from_from_item_with_hidden(
                &join.source,
                context,
                using_columns.clone(),
            )?);
            left_columns.extend(
                right_columns
                    .into_iter()
                    .filter(|column| !using_columns.iter().any(|using| using == column)),
            );
        }
        Ok(QueryScope { bindings })
    }

    fn build_derived_scope(&self, alias: &str, columns: &[String]) -> QueryScope {
        QueryScope {
            bindings: vec![TableBinding {
                table: alias.to_string(),
                alias: Some(alias.to_string()),
                schema: Schema::new(alias, self.derived_output_schema(columns)),
                exposes_rowid: false,
                hidden_columns: Vec::new(),
            }],
        }
    }

    fn build_join_scope(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
        join_index: usize,
    ) -> Result<QueryScope> {
        let mut bindings = Vec::with_capacity(join_index + 2);
        bindings.push(self.binding_from_from_item(&select.from, context)?);
        let mut left_columns = self.visible_from_item_output_columns(&select.from, context)?;
        for join in select.joins.iter().take(join_index + 1) {
            let right_columns = self.visible_from_item_output_columns(&join.source, context)?;
            let using_columns = join_output_using_columns(&left_columns, &right_columns, join);
            bindings.push(self.binding_from_from_item_with_hidden(
                &join.source,
                context,
                using_columns.clone(),
            )?);
            left_columns.extend(
                right_columns
                    .into_iter()
                    .filter(|column| !using_columns.iter().any(|using| using == column)),
            );
        }
        Ok(QueryScope { bindings })
    }

    fn binding_from_from_item(
        &self,
        from: &FromItem,
        context: &PlanningContext,
    ) -> Result<TableBinding> {
        self.binding_from_from_item_with_hidden(from, context, Vec::new())
    }

    fn binding_from_from_item_with_hidden(
        &self,
        from: &FromItem,
        context: &PlanningContext,
        hidden_columns: Vec<String>,
    ) -> Result<TableBinding> {
        match from {
            FromItem::Table { name, alias }
            | FromItem::TableIndexed { name, alias, .. }
            | FromItem::TableNotIndexed { name, alias } => {
                let schema = self.require_schema(context, name)?.clone();
                let exposes_rowid = self.schema_exposes_rowid(&schema);
                Ok(TableBinding {
                    table: name.clone(),
                    alias: alias.clone(),
                    schema,
                    exposes_rowid,
                    hidden_columns,
                })
            }
            FromItem::Subquery {
                query,
                alias,
                columns,
            } => {
                let output_columns = self.apply_output_columns_override(
                    self.derived_output_columns(query, context)?,
                    columns.as_ref(),
                )?;
                Ok(TableBinding {
                    table: alias.clone(),
                    alias: Some(alias.clone()),
                    schema: Schema::new(alias, self.derived_output_schema(&output_columns)),
                    exposes_rowid: false,
                    hidden_columns,
                })
            }
            FromItem::Values {
                rows,
                alias,
                columns,
            } => {
                let table = alias
                    .clone()
                    .unwrap_or_else(|| VALUES_SOURCE_TABLE.to_string());
                let output_columns = self.apply_output_columns_override(
                    self.values_output_columns(rows)?,
                    columns.as_ref(),
                )?;
                Ok(TableBinding {
                    table: table.clone(),
                    alias: alias.clone(),
                    schema: Schema::new(&table, self.derived_output_schema(&output_columns)),
                    exposes_rowid: false,
                    hidden_columns,
                })
            }
        }
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
                exposes_rowid: self.schema_exposes_rowid(self.require_schema(context, table)?),
                hidden_columns: Vec::new(),
            }],
        })
    }

    fn derived_output_schema(&self, columns: &[String]) -> Vec<ColumnDef> {
        columns
            .iter()
            .cloned()
            .map(|name| ColumnDef::new(name, ColumnType::Text))
            .collect()
    }

    fn values_output_columns(&self, rows: &[Vec<ScalarExpr>]) -> Result<Vec<String>> {
        let width = rows.first().map_or(0, Vec::len);
        if rows.iter().any(|row| row.len() != width) {
            return Err(DbError::plan(
                "VALUES rows must all have the same number of columns",
            ));
        }
        Ok((1..=width).map(|index| format!("column{index}")).collect())
    }

    fn apply_output_columns_override(
        &self,
        default_columns: Vec<String>,
        override_columns: Option<&Vec<String>>,
    ) -> Result<Vec<String>> {
        let Some(override_columns) = override_columns else {
            return Ok(default_columns);
        };
        if override_columns.len() != default_columns.len() {
            return Err(DbError::plan(format!(
                "CTE column name count {} does not match result column count {}",
                override_columns.len(),
                default_columns.len()
            )));
        }
        Ok(override_columns.clone())
    }

    fn derived_output_columns(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
    ) -> Result<Vec<String>> {
        let wildcard_columns = select
            .columns
            .iter()
            .any(|item| matches!(item, SelectItem::Wildcard))
            .then(|| self.visible_source_output_columns(select, context))
            .transpose()?;

        let mut output_columns = Vec::new();
        for item in &select.columns {
            match item {
                SelectItem::Wildcard => output_columns.extend(
                    wildcard_columns
                        .as_ref()
                        .expect("wildcard output columns must be precomputed")
                        .iter()
                        .cloned(),
                ),
                _ => output_columns.push(
                    self.select_item_output_name(item)
                        .expect("non-wildcard select items must expose an output name"),
                ),
            }
        }

        Ok(output_columns)
    }

    fn visible_source_output_columns(
        &self,
        select: &SelectStatement,
        context: &PlanningContext,
    ) -> Result<Vec<String>> {
        let mut columns = self.visible_from_item_output_columns(&select.from, context)?;
        for join in &select.joins {
            let right_columns = self.visible_from_item_output_columns(&join.source, context)?;
            let using_columns = join_output_using_columns(&columns, &right_columns, join);
            columns.extend(
                right_columns
                    .into_iter()
                    .filter(|column| !using_columns.iter().any(|using| using == column)),
            );
        }
        Ok(columns)
    }

    fn visible_from_item_output_columns(
        &self,
        from: &FromItem,
        context: &PlanningContext,
    ) -> Result<Vec<String>> {
        match from {
            FromItem::Table { name, .. }
            | FromItem::TableIndexed { name, .. }
            | FromItem::TableNotIndexed { name, .. }
                if name == SINGLE_ROW_SOURCE_TABLE =>
            {
                Ok(Vec::new())
            }
            FromItem::Table { name, .. }
            | FromItem::TableIndexed { name, .. }
            | FromItem::TableNotIndexed { name, .. } => Ok(self
                .require_schema(context, name)?
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect()),
            FromItem::Subquery { query, columns, .. } => self.apply_output_columns_override(
                self.derived_output_columns(query, context)?,
                columns.as_ref(),
            ),
            FromItem::Values { rows, columns, .. } => self
                .apply_output_columns_override(self.values_output_columns(rows)?, columns.as_ref()),
        }
    }

    fn plan_source_item(
        &self,
        from: &FromItem,
        context: &PlanningContext,
        outer_scope: Option<&QueryScope>,
    ) -> Result<Plan> {
        let wildcard = [SelectItem::Wildcard];
        let no_filter = None;
        match from {
            FromItem::Table { .. }
            | FromItem::TableIndexed { .. }
            | FromItem::TableNotIndexed { .. } => {
                let (name, alias, index_hint) =
                    table_source_parts(from).expect("table source pattern must expose table parts");
                self.plan_single_table_source(
                    SingleTablePlanInput {
                        table: name,
                        table_alias: alias,
                        index_hint,
                        columns: &wildcard,
                        filter: &no_filter,
                        order_by: &[],
                        limit: None,
                        offset: None,
                        distinct: false,
                    },
                    context,
                    outer_scope,
                )
            }
            FromItem::Subquery {
                query,
                alias,
                columns,
            } => {
                let source = self.plan_select_with_outer(query, context, outer_scope)?;
                let output_columns = self.apply_output_columns_override(
                    self.derived_output_columns(query, context)?,
                    columns.as_ref(),
                )?;
                self.plan_derived_source(DerivedSourcePlanInput {
                    alias,
                    source,
                    output_columns,
                    columns: &wildcard,
                    filter: &no_filter,
                    order_by: &[],
                    limit: None,
                    offset: None,
                    distinct: false,
                })
            }
            FromItem::Values { .. } => {
                let (source, alias, output_columns) = self
                    .plan_inline_source_item(from, context, outer_scope)?
                    .expect("VALUES source item should produce a plan");
                self.plan_derived_source(DerivedSourcePlanInput {
                    alias: &alias,
                    source,
                    output_columns,
                    columns: &wildcard,
                    filter: &no_filter,
                    order_by: &[],
                    limit: None,
                    offset: None,
                    distinct: false,
                })
            }
        }
    }

    fn plan_join_clauses(
        &self,
        from: &FromItem,
        joins: &[crate::sql::ast::JoinClause],
        context: &PlanningContext,
        outer_scope: Option<&QueryScope>,
    ) -> Result<Vec<JoinPlan>> {
        let mut left_columns = self.visible_from_item_output_columns(from, context)?;
        let mut left_qualifier = from_item_qualifier(from);
        let mut planned = Vec::new();
        for join in joins {
            let right_columns = self.visible_from_item_output_columns(&join.source, context)?;
            let right_qualifier = from_item_qualifier(&join.source);
            let using_columns = join_output_using_columns(&left_columns, &right_columns, join);
            let on = if join.natural {
                join_using_expr(
                    left_qualifier.as_deref(),
                    right_qualifier.as_deref(),
                    &using_columns,
                )
            } else {
                join.on.clone()
            };
            planned.push(JoinPlan {
                kind: join.kind,
                source: Box::new(self.plan_source_item(&join.source, context, outer_scope)?),
                on,
                using_columns: using_columns.clone(),
            });
            left_columns.extend(
                right_columns
                    .into_iter()
                    .filter(|column| !using_columns.iter().any(|using| using == column)),
            );
            left_qualifier = None;
        }
        Ok(planned)
    }

    fn select_item_output_name(&self, item: &SelectItem) -> Option<String> {
        match item {
            SelectItem::Wildcard => None,
            SelectItem::Column(name) => Some(
                name.rsplit('.')
                    .next()
                    .expect("column names should never be empty")
                    .to_string(),
            ),
            SelectItem::AliasedColumn { alias, .. } => Some(alias.clone()),
            SelectItem::Expr { expr, alias } => Some(
                alias
                    .clone()
                    .unwrap_or_else(|| self.scalar_expr_display(expr)),
            ),
            SelectItem::Aggregate {
                func, arg, alias, ..
            } => Some(
                alias
                    .clone()
                    .unwrap_or_else(|| self.aggregate_output_name(*func, arg)),
            ),
        }
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
                AggregateArg::Expr { expr, order_by, .. } => {
                    self.require_scalar_expr_scope(scope, expr)?;
                    for item in order_by {
                        self.require_order_by_scope(scope, &[], item)?;
                    }
                    Ok(())
                }
                AggregateArg::GroupConcat {
                    expr,
                    separator,
                    order_by,
                    ..
                } => {
                    self.require_scalar_expr_scope(scope, expr)?;
                    if let Some(separator) = separator {
                        self.require_scalar_expr_scope(scope, separator)?;
                    }
                    for item in order_by {
                        self.require_order_by_scope(scope, &[], item)?;
                    }
                    Ok(())
                }
                AggregateArg::JsonGroupObject {
                    key,
                    value,
                    order_by,
                } => {
                    self.require_scalar_expr_scope(scope, key)?;
                    self.require_scalar_expr_scope(scope, value)?;
                    for item in order_by {
                        self.require_order_by_scope(scope, &[], item)?;
                    }
                    Ok(())
                }
                AggregateArg::Percentile {
                    expr,
                    fraction,
                    order_by,
                } => {
                    self.require_scalar_expr_scope(scope, expr)?;
                    self.require_scalar_expr_scope(scope, fraction)?;
                    for item in order_by {
                        self.require_order_by_scope(scope, &[], item)?;
                    }
                    Ok(())
                }
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
                if Self::scalar_expr_has_aggregate(expr) {
                    self.require_scalar_expr_scope(scope, expr)?;
                    self.require_aggregate_scalar_reference(expr, &[item.clone()], group_by)?;
                } else {
                    self.require_scalar_expr_scope(scope, expr)?;
                }
                if !Self::scalar_expr_has_aggregate(expr)
                    && !self.group_by_contains_expr(group_by, expr)
                {
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
                AggregateArg::Expr { expr, order_by, .. } => {
                    self.require_scalar_expr_scope(scope, expr)?;
                    for item in order_by {
                        self.require_order_by_scope(scope, &[], item)?;
                    }
                    Ok(())
                }
                AggregateArg::GroupConcat {
                    expr,
                    separator,
                    order_by,
                    ..
                } => {
                    self.require_scalar_expr_scope(scope, expr)?;
                    if let Some(separator) = separator {
                        self.require_scalar_expr_scope(scope, separator)?;
                    }
                    for item in order_by {
                        self.require_order_by_scope(scope, &[], item)?;
                    }
                    Ok(())
                }
                AggregateArg::JsonGroupObject {
                    key,
                    value,
                    order_by,
                } => {
                    self.require_scalar_expr_scope(scope, key)?;
                    self.require_scalar_expr_scope(scope, value)?;
                    for item in order_by {
                        self.require_order_by_scope(scope, &[], item)?;
                    }
                    Ok(())
                }
                AggregateArg::Percentile {
                    expr,
                    fraction,
                    order_by,
                } => {
                    self.require_scalar_expr_scope(scope, expr)?;
                    self.require_scalar_expr_scope(scope, fraction)?;
                    for item in order_by {
                        self.require_order_by_scope(scope, &[], item)?;
                    }
                    Ok(())
                }
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
            ScalarExpr::Tuple(values) => format!(
                "({})",
                values
                    .iter()
                    .map(|value| self.scalar_expr_display(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ScalarExpr::UnaryMinus(expr) => format!("-{}", self.scalar_expr_display(expr)),
            ScalarExpr::BitNot(expr) => format!("~{}", self.scalar_expr_display(expr)),
            ScalarExpr::Not(expr) => format!("NOT {}", self.scalar_expr_display(expr)),
            ScalarExpr::Collate { expr, collation } => {
                format!("{} COLLATE {}", self.scalar_expr_display(expr), collation)
            }
            ScalarExpr::Cast { expr, ty } => {
                format!("CAST({} AS {})", self.scalar_expr_display(expr), ty.name())
            }
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => format!(
                "{} IS {}{}",
                self.scalar_expr_display(left),
                if *negated { "NOT " } else { "" },
                self.scalar_expr_display(right)
            ),
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => format!(
                "{} IS {}{}",
                self.scalar_expr_display(expr),
                if *negated { "NOT " } else { "" },
                if *value { "TRUE" } else { "FALSE" }
            ),
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => format!(
                "{} {}IN ({})",
                self.scalar_expr_display(expr),
                if *negated { "NOT " } else { "" },
                values
                    .iter()
                    .map(|value| self.scalar_expr_display(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ScalarExpr::InSubquery {
                expr,
                query: _,
                negated,
            } => format!(
                "{} {}IN (SELECT ...)",
                self.scalar_expr_display(expr),
                if *negated { "NOT " } else { "" }
            ),
            ScalarExpr::Subquery { .. } => "(SELECT ...)".to_string(),
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => format!(
                "{} {}LIKE '{}'{}",
                self.scalar_expr_display(expr),
                if *negated { "NOT " } else { "" },
                pattern,
                escape
                    .as_ref()
                    .map(|escape| format!(" ESCAPE '{}'", escape.replace('\'', "''")))
                    .unwrap_or_default()
            ),
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => format!(
                "{} {}GLOB '{}'",
                self.scalar_expr_display(expr),
                if *negated { "NOT " } else { "" },
                pattern
            ),
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => format!(
                "{} {}BETWEEN {} AND {}",
                self.scalar_expr_display(expr),
                if *negated { "NOT " } else { "" },
                self.scalar_expr_display(low),
                self.scalar_expr_display(high)
            ),
            ScalarExpr::Compare { left, op, right } => format!(
                "{} {} {}",
                self.scalar_expr_display(left),
                match op {
                    crate::sql::ast::CompareOp::Eq => "=",
                    crate::sql::ast::CompareOp::Ne => "!=",
                    crate::sql::ast::CompareOp::Gt => ">",
                    crate::sql::ast::CompareOp::Gte => ">=",
                    crate::sql::ast::CompareOp::Lt => "<",
                    crate::sql::ast::CompareOp::Lte => "<=",
                },
                self.scalar_expr_display(right)
            ),
            ScalarExpr::CompareSubquery { left, op, query: _ } => format!(
                "{} {} (SELECT ...)",
                self.scalar_expr_display(left),
                match op {
                    crate::sql::ast::CompareOp::Eq => "=",
                    crate::sql::ast::CompareOp::Ne => "!=",
                    crate::sql::ast::CompareOp::Gt => ">",
                    crate::sql::ast::CompareOp::Gte => ">=",
                    crate::sql::ast::CompareOp::Lt => "<",
                    crate::sql::ast::CompareOp::Lte => "<=",
                }
            ),
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                let mut parts = vec!["CASE".to_string()];
                if let Some(base) = base {
                    parts.push(self.scalar_expr_display(base));
                }
                for (when_expr, then_expr) in when_then_clauses {
                    parts.push(format!(
                        "WHEN {} THEN {}",
                        self.scalar_expr_display(when_expr),
                        self.scalar_expr_display(then_expr)
                    ));
                }
                if let Some(else_expr) = else_expr {
                    parts.push(format!("ELSE {}", self.scalar_expr_display(else_expr)));
                }
                parts.push("END".to_string());
                parts.join(" ")
            }
            ScalarExpr::Binary { left, op, right } => format!(
                "{} {} {}",
                self.scalar_expr_display(left),
                match op {
                    crate::sql::ast::ScalarBinaryOp::Add => "+",
                    crate::sql::ast::ScalarBinaryOp::Subtract => "-",
                    crate::sql::ast::ScalarBinaryOp::Multiply => "*",
                    crate::sql::ast::ScalarBinaryOp::Divide => "/",
                    crate::sql::ast::ScalarBinaryOp::Modulo => "%",
                    crate::sql::ast::ScalarBinaryOp::BitAnd => "&",
                    crate::sql::ast::ScalarBinaryOp::BitOr => "|",
                    crate::sql::ast::ScalarBinaryOp::ShiftLeft => "<<",
                    crate::sql::ast::ScalarBinaryOp::ShiftRight => ">>",
                    crate::sql::ast::ScalarBinaryOp::Concat => "||",
                    crate::sql::ast::ScalarBinaryOp::JsonExtract => "->",
                    crate::sql::ast::ScalarBinaryOp::JsonExtractText => "->>",
                },
                self.scalar_expr_display(right)
            ),
            ScalarExpr::Function { func, args } => format!(
                "{}({})",
                match func {
                    crate::sql::ast::ScalarFunc::Length => "LENGTH",
                    crate::sql::ast::ScalarFunc::OctetLength => "OCTET_LENGTH",
                    crate::sql::ast::ScalarFunc::MinScalar => "MIN",
                    crate::sql::ast::ScalarFunc::MaxScalar => "MAX",
                    crate::sql::ast::ScalarFunc::Date => "DATE",
                    crate::sql::ast::ScalarFunc::Time => "TIME",
                    crate::sql::ast::ScalarFunc::DateTime => "DATETIME",
                    crate::sql::ast::ScalarFunc::TimeDiff => "TIMEDIFF",
                    crate::sql::ast::ScalarFunc::Strftime => "STRFTIME",
                    crate::sql::ast::ScalarFunc::JulianDay => "JULIANDAY",
                    crate::sql::ast::ScalarFunc::UnixEpoch => "UNIXEPOCH",
                    crate::sql::ast::ScalarFunc::Changes => "CHANGES",
                    crate::sql::ast::ScalarFunc::TotalChanges => "TOTAL_CHANGES",
                    crate::sql::ast::ScalarFunc::Printf => "PRINTF",
                    crate::sql::ast::ScalarFunc::IIf => "IIF",
                    crate::sql::ast::ScalarFunc::If => "IF",
                    crate::sql::ast::ScalarFunc::Concat => "CONCAT",
                    crate::sql::ast::ScalarFunc::ConcatWs => "CONCAT_WS",
                    crate::sql::ast::ScalarFunc::SqliteSourceId => "SQLITE_SOURCE_ID",
                    crate::sql::ast::ScalarFunc::Sign => "SIGN",
                    crate::sql::ast::ScalarFunc::RandomBlob => "RANDOMBLOB",
                    crate::sql::ast::ScalarFunc::Random => "RANDOM",
                    crate::sql::ast::ScalarFunc::Unhex => "UNHEX",
                    crate::sql::ast::ScalarFunc::Unistr => "UNISTR",
                    crate::sql::ast::ScalarFunc::UnistrQuote => "UNISTR_QUOTE",
                    crate::sql::ast::ScalarFunc::SqliteVersion => "SQLITE_VERSION",
                    crate::sql::ast::ScalarFunc::SqliteCompileOptionUsed => {
                        "SQLITE_COMPILEOPTION_USED"
                    }
                    crate::sql::ast::ScalarFunc::SqliteCompileOptionGet => {
                        "SQLITE_COMPILEOPTION_GET"
                    }
                    crate::sql::ast::ScalarFunc::Likely => "LIKELY",
                    crate::sql::ast::ScalarFunc::Unlikely => "UNLIKELY",
                    crate::sql::ast::ScalarFunc::Likelihood => "LIKELIHOOD",
                    crate::sql::ast::ScalarFunc::Mod => "MOD",
                    crate::sql::ast::ScalarFunc::Ceil => "CEIL",
                    crate::sql::ast::ScalarFunc::Ceiling => "CEILING",
                    crate::sql::ast::ScalarFunc::Floor => "FLOOR",
                    crate::sql::ast::ScalarFunc::Trunc => "TRUNC",
                    crate::sql::ast::ScalarFunc::Pi => "PI",
                    crate::sql::ast::ScalarFunc::Sqrt => "SQRT",
                    crate::sql::ast::ScalarFunc::Power => "POWER",
                    crate::sql::ast::ScalarFunc::Exp => "EXP",
                    crate::sql::ast::ScalarFunc::Sin => "SIN",
                    crate::sql::ast::ScalarFunc::Cos => "COS",
                    crate::sql::ast::ScalarFunc::Tan => "TAN",
                    crate::sql::ast::ScalarFunc::Sinh => "SINH",
                    crate::sql::ast::ScalarFunc::Cosh => "COSH",
                    crate::sql::ast::ScalarFunc::Tanh => "TANH",
                    crate::sql::ast::ScalarFunc::Acos => "ACOS",
                    crate::sql::ast::ScalarFunc::Asin => "ASIN",
                    crate::sql::ast::ScalarFunc::Atan => "ATAN",
                    crate::sql::ast::ScalarFunc::Atan2 => "ATAN2",
                    crate::sql::ast::ScalarFunc::Acosh => "ACOSH",
                    crate::sql::ast::ScalarFunc::Asinh => "ASINH",
                    crate::sql::ast::ScalarFunc::Atanh => "ATANH",
                    crate::sql::ast::ScalarFunc::Ln => "LN",
                    crate::sql::ast::ScalarFunc::Log10 => "LOG10",
                    crate::sql::ast::ScalarFunc::Log2 => "LOG2",
                    crate::sql::ast::ScalarFunc::Log => "LOG",
                    crate::sql::ast::ScalarFunc::Degrees => "DEGREES",
                    crate::sql::ast::ScalarFunc::Radians => "RADIANS",
                    crate::sql::ast::ScalarFunc::Char => "CHAR",
                    crate::sql::ast::ScalarFunc::ZeroBlob => "ZEROBLOB",
                    crate::sql::ast::ScalarFunc::TypeOf => "TYPEOF",
                    crate::sql::ast::ScalarFunc::Subtype => "SUBTYPE",
                    crate::sql::ast::ScalarFunc::Hex => "HEX",
                    crate::sql::ast::ScalarFunc::Substr => "SUBSTR",
                    crate::sql::ast::ScalarFunc::Instr => "INSTR",
                    crate::sql::ast::ScalarFunc::Replace => "REPLACE",
                    crate::sql::ast::ScalarFunc::LikeFunc => "LIKE",
                    crate::sql::ast::ScalarFunc::GlobFunc => "GLOB",
                    crate::sql::ast::ScalarFunc::Quote => "QUOTE",
                    crate::sql::ast::ScalarFunc::Unicode => "UNICODE",
                    crate::sql::ast::ScalarFunc::Trim => "TRIM",
                    crate::sql::ast::ScalarFunc::LTrim => "LTRIM",
                    crate::sql::ast::ScalarFunc::RTrim => "RTRIM",
                    crate::sql::ast::ScalarFunc::Lower => "LOWER",
                    crate::sql::ast::ScalarFunc::Upper => "UPPER",
                    crate::sql::ast::ScalarFunc::Abs => "ABS",
                    crate::sql::ast::ScalarFunc::Round => "ROUND",
                    crate::sql::ast::ScalarFunc::LastInsertRowId => "LAST_INSERT_ROWID",
                    crate::sql::ast::ScalarFunc::Coalesce => "COALESCE",
                    crate::sql::ast::ScalarFunc::IfNull => "IFNULL",
                    crate::sql::ast::ScalarFunc::NullIf => "NULLIF",
                    crate::sql::ast::ScalarFunc::Json => "JSON",
                    crate::sql::ast::ScalarFunc::JsonValid => "JSON_VALID",
                    crate::sql::ast::ScalarFunc::JsonErrorPosition => "JSON_ERROR_POSITION",
                    crate::sql::ast::ScalarFunc::JsonPretty => "JSON_PRETTY",
                    crate::sql::ast::ScalarFunc::JsonQuote => "JSON_QUOTE",
                    crate::sql::ast::ScalarFunc::JsonExtract => "JSON_EXTRACT",
                    crate::sql::ast::ScalarFunc::JsonType => "JSON_TYPE",
                    crate::sql::ast::ScalarFunc::JsonArray => "JSON_ARRAY",
                    crate::sql::ast::ScalarFunc::JsonObject => "JSON_OBJECT",
                    crate::sql::ast::ScalarFunc::JsonArrayLength => "JSON_ARRAY_LENGTH",
                    crate::sql::ast::ScalarFunc::JsonRemove => "JSON_REMOVE",
                    crate::sql::ast::ScalarFunc::JsonSet => "JSON_SET",
                    crate::sql::ast::ScalarFunc::JsonInsert => "JSON_INSERT",
                    crate::sql::ast::ScalarFunc::JsonReplace => "JSON_REPLACE",
                    crate::sql::ast::ScalarFunc::JsonPatch => "JSON_PATCH",
                },
                args.iter()
                    .map(|arg| self.scalar_expr_display(arg))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ScalarExpr::Aggregate { func, arg, .. } => self.aggregate_output_name(*func, arg),
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

    fn require_inner_order_by_columns(&self, schema: &Schema, item: &OrderBy) -> Result<()> {
        match &item.expr {
            OrderByExpr::Column(column) => self.require_column(schema, column),
            OrderByExpr::Expr(expr) => self.require_scalar_expr_columns(schema, expr),
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
            | Expr::Glob { column, .. }
            | Expr::Between { column, .. }
            | Expr::InList { column, .. }
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
            | Expr::IsBool { expr, .. }
            | Expr::LikeScalar { expr, .. }
            | Expr::GlobScalar { expr, .. }
            | Expr::InSubqueryScalar { expr, .. } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            Expr::Is { left, right, .. } => {
                self.require_aggregate_scalar_reference(left, columns, group_by)?;
                self.require_aggregate_scalar_reference(right, columns, group_by)
            }
            Expr::BetweenScalar {
                expr, low, high, ..
            } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)?;
                self.require_aggregate_scalar_reference(low, columns, group_by)?;
                self.require_aggregate_scalar_reference(high, columns, group_by)
            }
            Expr::InListScalar { expr, values, .. } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)?;
                for value in values {
                    self.require_aggregate_scalar_reference(value, columns, group_by)?;
                }
                Ok(())
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
            ScalarExpr::Tuple(values) => {
                for value in values {
                    self.require_aggregate_scalar_reference(value, columns, group_by)?;
                }
                Ok(())
            }
            ScalarExpr::UnaryMinus(expr) => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            ScalarExpr::BitNot(expr) => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            ScalarExpr::Not(expr) => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            ScalarExpr::Collate { expr, .. } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            ScalarExpr::Cast { expr, .. } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            ScalarExpr::Is { left, right, .. } => {
                self.require_aggregate_scalar_reference(left, columns, group_by)?;
                self.require_aggregate_scalar_reference(right, columns, group_by)
            }
            ScalarExpr::IsBool { expr, .. } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            ScalarExpr::InList { expr, values, .. } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)?;
                for value in values {
                    self.require_aggregate_scalar_reference(value, columns, group_by)?;
                }
                Ok(())
            }
            ScalarExpr::InSubquery { expr, .. }
            | ScalarExpr::CompareSubquery { left: expr, .. } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            ScalarExpr::Subquery { .. } => Ok(()),
            ScalarExpr::Like { expr, .. } | ScalarExpr::Glob { expr, .. } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)
            }
            ScalarExpr::Between {
                expr, low, high, ..
            } => {
                self.require_aggregate_scalar_reference(expr, columns, group_by)?;
                self.require_aggregate_scalar_reference(low, columns, group_by)?;
                self.require_aggregate_scalar_reference(high, columns, group_by)
            }
            ScalarExpr::Compare { left, right, .. } => {
                self.require_aggregate_scalar_reference(left, columns, group_by)?;
                self.require_aggregate_scalar_reference(right, columns, group_by)
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                if let Some(base) = base {
                    self.require_aggregate_scalar_reference(base, columns, group_by)?;
                }
                for (when_expr, then_expr) in when_then_clauses {
                    self.require_aggregate_scalar_reference(when_expr, columns, group_by)?;
                    self.require_aggregate_scalar_reference(then_expr, columns, group_by)?;
                }
                if let Some(else_expr) = else_expr {
                    self.require_aggregate_scalar_reference(else_expr, columns, group_by)?;
                }
                Ok(())
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
            ScalarExpr::Aggregate { .. } => Ok(()),
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
                SelectItem::Aggregate {
                    func, arg, alias, ..
                } => {
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
            collation: item.collation.clone(),
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
            | Expr::InList { .. }
            | Expr::CompareSubquery { .. }
            | Expr::ExistsSubquery { .. }
            | Expr::Like { .. }
            | Expr::Glob { .. }
            | Expr::Between { .. } => expr.clone(),
            Expr::Is {
                left,
                right,
                negated,
            } => Expr::Is {
                left: self.rewrite_aggregate_scalar_group_references(left, group_by),
                right: self.rewrite_aggregate_scalar_group_references(right, group_by),
                negated: *negated,
            },
            Expr::CompareScalar { left, op, right } => Expr::CompareScalar {
                left: self.rewrite_aggregate_scalar_group_references(left, group_by),
                op: *op,
                right: self.rewrite_aggregate_scalar_group_references(right, group_by),
            },
            Expr::IsNullScalar { expr, negated } => Expr::IsNullScalar {
                expr: self.rewrite_aggregate_scalar_group_references(expr, group_by),
                negated: *negated,
            },
            Expr::IsBool {
                expr,
                value,
                negated,
                explicit,
            } => Expr::IsBool {
                expr: self.rewrite_aggregate_scalar_group_references(expr, group_by),
                value: *value,
                negated: *negated,
                explicit: *explicit,
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
            Expr::InListScalar {
                expr,
                values,
                negated,
            } => Expr::InListScalar {
                expr: self.rewrite_aggregate_scalar_group_references(expr, group_by),
                values: values
                    .iter()
                    .map(|value| self.rewrite_aggregate_scalar_group_references(value, group_by))
                    .collect(),
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
                escape,
                negated,
            } => Expr::LikeScalar {
                expr: self.rewrite_aggregate_scalar_group_references(expr, group_by),
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            },
            Expr::GlobScalar {
                expr,
                pattern,
                negated,
            } => Expr::GlobScalar {
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
            ScalarExpr::Tuple(values) => ScalarExpr::Tuple(
                values
                    .iter()
                    .map(|value| self.rewrite_aggregate_scalar_group_references(value, group_by))
                    .collect(),
            ),
            ScalarExpr::UnaryMinus(expr) => ScalarExpr::UnaryMinus(Box::new(
                self.rewrite_aggregate_scalar_group_references(expr, group_by),
            )),
            ScalarExpr::BitNot(expr) => ScalarExpr::BitNot(Box::new(
                self.rewrite_aggregate_scalar_group_references(expr, group_by),
            )),
            ScalarExpr::Not(expr) => ScalarExpr::Not(Box::new(
                self.rewrite_aggregate_scalar_group_references(expr, group_by),
            )),
            ScalarExpr::Collate { expr, collation } => ScalarExpr::Collate {
                expr: Box::new(self.rewrite_aggregate_scalar_group_references(expr, group_by)),
                collation: collation.clone(),
            },
            ScalarExpr::Cast { expr, ty } => ScalarExpr::Cast {
                expr: Box::new(self.rewrite_aggregate_scalar_group_references(expr, group_by)),
                ty: *ty,
            },
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => ScalarExpr::Is {
                left: Box::new(self.rewrite_aggregate_scalar_group_references(left, group_by)),
                right: Box::new(self.rewrite_aggregate_scalar_group_references(right, group_by)),
                negated: *negated,
            },
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => ScalarExpr::IsBool {
                expr: Box::new(self.rewrite_aggregate_scalar_group_references(expr, group_by)),
                value: *value,
                negated: *negated,
            },
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => ScalarExpr::InList {
                expr: Box::new(self.rewrite_aggregate_scalar_group_references(expr, group_by)),
                values: values
                    .iter()
                    .map(|value| self.rewrite_aggregate_scalar_group_references(value, group_by))
                    .collect(),
                negated: *negated,
            },
            ScalarExpr::InSubquery {
                expr,
                query,
                negated,
            } => ScalarExpr::InSubquery {
                expr: Box::new(self.rewrite_aggregate_scalar_group_references(expr, group_by)),
                query: query.clone(),
                negated: *negated,
            },
            ScalarExpr::Subquery { query } => ScalarExpr::Subquery {
                query: query.clone(),
            },
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => ScalarExpr::Like {
                expr: Box::new(self.rewrite_aggregate_scalar_group_references(expr, group_by)),
                pattern: pattern.clone(),
                escape: escape.clone(),
                negated: *negated,
            },
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => ScalarExpr::Glob {
                expr: Box::new(self.rewrite_aggregate_scalar_group_references(expr, group_by)),
                pattern: pattern.clone(),
                negated: *negated,
            },
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => ScalarExpr::Between {
                expr: Box::new(self.rewrite_aggregate_scalar_group_references(expr, group_by)),
                low: Box::new(self.rewrite_aggregate_scalar_group_references(low, group_by)),
                high: Box::new(self.rewrite_aggregate_scalar_group_references(high, group_by)),
                negated: *negated,
            },
            ScalarExpr::Compare { left, op, right } => ScalarExpr::Compare {
                left: Box::new(self.rewrite_aggregate_scalar_group_references(left, group_by)),
                op: *op,
                right: Box::new(self.rewrite_aggregate_scalar_group_references(right, group_by)),
            },
            ScalarExpr::CompareSubquery { left, op, query } => ScalarExpr::CompareSubquery {
                left: Box::new(self.rewrite_aggregate_scalar_group_references(left, group_by)),
                op: *op,
                query: query.clone(),
            },
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => ScalarExpr::Case {
                base: base.as_ref().map(|expr| {
                    Box::new(self.rewrite_aggregate_scalar_group_references(expr, group_by))
                }),
                when_then_clauses: when_then_clauses
                    .iter()
                    .map(|(when_expr, then_expr)| {
                        (
                            self.rewrite_aggregate_scalar_group_references(when_expr, group_by),
                            self.rewrite_aggregate_scalar_group_references(then_expr, group_by),
                        )
                    })
                    .collect(),
                else_expr: else_expr.as_ref().map(|expr| {
                    Box::new(self.rewrite_aggregate_scalar_group_references(expr, group_by))
                }),
            },
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
            ScalarExpr::Aggregate { func, arg, filter } => ScalarExpr::Aggregate {
                func: *func,
                arg: arg.clone(),
                filter: filter.clone(),
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
                AggregateFunc::Total => "TOTAL",
                AggregateFunc::Median => "MEDIAN",
                AggregateFunc::Percentile => "PERCENTILE",
                AggregateFunc::PercentileCont => "PERCENTILE_CONT",
                AggregateFunc::PercentileDisc => "PERCENTILE_DISC",
                AggregateFunc::GroupConcat => "GROUP_CONCAT",
                AggregateFunc::JsonGroupArray => "JSON_GROUP_ARRAY",
                AggregateFunc::JsonGroupObject => "JSON_GROUP_OBJECT",
                AggregateFunc::Min => "MIN",
                AggregateFunc::Max => "MAX",
            },
            match arg {
                AggregateArg::Wildcard => "*".to_string(),
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by,
                } => {
                    if *distinct {
                        let expr = format!("DISTINCT {}", self.scalar_expr_display(expr));
                        if order_by.is_empty() {
                            expr
                        } else {
                            let order_by = order_by
                                .iter()
                                .map(|item| self.aggregate_arg_order_by_display(item))
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{expr} ORDER BY {order_by}")
                        }
                    } else {
                        let expr = self.scalar_expr_display(expr);
                        if order_by.is_empty() {
                            expr
                        } else {
                            let order_by = order_by
                                .iter()
                                .map(|item| self.aggregate_arg_order_by_display(item))
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{expr} ORDER BY {order_by}")
                        }
                    }
                }
                AggregateArg::GroupConcat {
                    expr,
                    separator,
                    distinct,
                    order_by,
                } => {
                    let expr = if *distinct {
                        format!("DISTINCT {}", self.scalar_expr_display(expr))
                    } else {
                        self.scalar_expr_display(expr)
                    };
                    let args = if let Some(separator) = separator {
                        format!("{expr}, {}", self.scalar_expr_display(separator))
                    } else {
                        expr
                    };
                    if order_by.is_empty() {
                        args
                    } else {
                        let order_by = order_by
                            .iter()
                            .map(|item| self.aggregate_arg_order_by_display(item))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{args} ORDER BY {order_by}")
                    }
                }
                AggregateArg::JsonGroupObject {
                    key,
                    value,
                    order_by,
                } => {
                    let args = format!(
                        "{}, {}",
                        self.scalar_expr_display(key),
                        self.scalar_expr_display(value)
                    );
                    if order_by.is_empty() {
                        args
                    } else {
                        let order_by = order_by
                            .iter()
                            .map(|item| self.aggregate_arg_order_by_display(item))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{args} ORDER BY {order_by}")
                    }
                }
                AggregateArg::Percentile {
                    expr,
                    fraction,
                    order_by,
                } => {
                    let args = format!(
                        "{}, {}",
                        self.scalar_expr_display(expr),
                        self.scalar_expr_display(fraction)
                    );
                    if order_by.is_empty() {
                        args
                    } else {
                        let order_by = order_by
                            .iter()
                            .map(|item| self.aggregate_arg_order_by_display(item))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{args} ORDER BY {order_by}")
                    }
                }
            }
        )
    }

    fn aggregate_arg_order_by_display(&self, item: &OrderBy) -> String {
        let expr = match &item.expr {
            OrderByExpr::Column(column) => column.clone(),
            OrderByExpr::Position(position) => position.to_string(),
            OrderByExpr::Expr(expr) => self.scalar_expr_display(expr),
        };
        let collation = item
            .collation
            .as_ref()
            .map(|collation| format!(" COLLATE {collation}"))
            .unwrap_or_default();
        let direction = if item.descending { " DESC" } else { "" };
        let nulls = match item.nulls {
            Some(NullOrder::First) => " NULLS FIRST",
            Some(NullOrder::Last) => " NULLS LAST",
            None => "",
        };
        format!("{expr}{collation}{direction}{nulls}")
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
            | Expr::InList { column, .. }
            | Expr::InSubquery { column, .. }
            | Expr::CompareSubquery { column, .. }
            | Expr::Like { column, .. }
            | Expr::Glob { column, .. }
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
            Expr::Is { left, right, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, left)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, right)
            }
            Expr::IsBool { expr, .. }
            | Expr::LikeScalar { expr, .. }
            | Expr::GlobScalar { expr, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            Expr::BetweenScalar {
                expr, low, high, ..
            } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, low)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, high)
            }
            Expr::InListScalar { expr, values, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)?;
                for value in values {
                    self.require_scalar_expr_scope_chain(scope, outer_scope, value)?;
                }
                Ok(())
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
            Expr::InSubquery { query, .. } | Expr::CompareSubquery { query, .. } => {
                self.validate_select_subquery(query, context, outer_scope, 1)
            }
            Expr::InSubqueryScalar { expr, query, .. } => self.validate_select_subquery(
                query,
                context,
                outer_scope,
                Self::scalar_expr_row_width(expr),
            ),
            Expr::CompareSubqueryScalar { left, query, .. } => self.validate_select_subquery(
                query,
                context,
                outer_scope,
                Self::scalar_expr_row_width(left),
            ),
            Expr::ExistsSubquery { query, .. } => {
                let _ = self.plan_select_with_outer(query, context, Some(outer_scope))?;
                Ok(())
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
            | Expr::Is { .. }
            | Expr::IsBool { .. }
            | Expr::InList { .. }
            | Expr::InListScalar { .. }
            | Expr::LikeScalar { .. }
            | Expr::Like { .. }
            | Expr::GlobScalar { .. }
            | Expr::Glob { .. }
            | Expr::Between { .. } => Ok(()),
            Expr::BetweenScalar { .. } => Ok(()),
        }
    }

    fn validate_select_subquery(
        &self,
        query: &SelectStatement,
        context: &PlanningContext,
        outer_scope: &QueryScope,
        expected_width: usize,
    ) -> Result<()> {
        let _ = self.plan_select_with_outer(query, context, Some(outer_scope))?;
        let actual_width = self.derived_output_columns(query, context)?.len();
        if actual_width != expected_width {
            return Err(DbError::plan(format!(
                "subquery must return exactly {expected_width} column{}",
                if expected_width == 1 { "" } else { "s" }
            )));
        }
        Ok(())
    }

    fn scalar_expr_row_width(expr: &ScalarExpr) -> usize {
        match expr {
            ScalarExpr::Tuple(values) => values.len(),
            _ => 1,
        }
    }

    fn resolve_column_in_scope_chain(
        &self,
        scope: &QueryScope,
        outer_scope: Option<&QueryScope>,
        column: &str,
    ) -> Result<(String, String)> {
        match self.resolve_column_in_scope(scope, column) {
            Ok(resolved) => Ok(resolved),
            Err(DbError::Plan(message)) if message == format!("unknown column {column}") => {
                if let Some(outer_scope) = outer_scope {
                    self.resolve_column_in_scope(outer_scope, column)
                } else if self.allow_unresolved_outer_refs && column.contains('.') {
                    Ok((String::new(), column.to_string()))
                } else {
                    Err(DbError::plan(format!("unknown column {column}")))
                }
            }
            Err(error) => Err(error),
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
            ScalarExpr::Tuple(items) => {
                for item in items {
                    self.require_scalar_expr_scope_chain(scope, outer_scope, item)?;
                }
                Ok(())
            }
            ScalarExpr::Column(name) => self
                .resolve_column_in_scope_chain(scope, outer_scope, name)
                .map(|_| ()),
            ScalarExpr::UnaryMinus(expr) => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            ScalarExpr::BitNot(expr) => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            ScalarExpr::Not(expr) => self.require_scalar_expr_scope_chain(scope, outer_scope, expr),
            ScalarExpr::Collate { expr, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            ScalarExpr::Cast { expr, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            ScalarExpr::Is { left, right, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, left)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, right)
            }
            ScalarExpr::IsBool { expr, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            ScalarExpr::InList { expr, values, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)?;
                for value in values {
                    self.require_scalar_expr_scope_chain(scope, outer_scope, value)?;
                }
                Ok(())
            }
            ScalarExpr::InSubquery { expr, .. }
            | ScalarExpr::CompareSubquery { left: expr, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            ScalarExpr::Subquery { .. } => Ok(()),
            ScalarExpr::Like { expr, .. } | ScalarExpr::Glob { expr, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)
            }
            ScalarExpr::Between {
                expr, low, high, ..
            } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, expr)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, low)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, high)
            }
            ScalarExpr::Compare { left, right, .. } => {
                self.require_scalar_expr_scope_chain(scope, outer_scope, left)?;
                self.require_scalar_expr_scope_chain(scope, outer_scope, right)
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                if let Some(base) = base {
                    self.require_scalar_expr_scope_chain(scope, outer_scope, base)?;
                }
                for (when_expr, then_expr) in when_then_clauses {
                    self.require_scalar_expr_scope_chain(scope, outer_scope, when_expr)?;
                    self.require_scalar_expr_scope_chain(scope, outer_scope, then_expr)?;
                }
                if let Some(else_expr) = else_expr {
                    self.require_scalar_expr_scope_chain(scope, outer_scope, else_expr)?;
                }
                Ok(())
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
            ScalarExpr::Aggregate { func, arg, filter } => {
                self.require_aggregate_arg_scope(scope, *func, arg)?;
                if let Some(filter) = filter {
                    self.require_scope_columns_with_outer(scope, None, filter)?;
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
            let mut resolved_table = None;
            let mut match_count = 0;
            for binding in &scope.bindings {
                if binding.table != prefix && binding.alias.as_deref() != Some(prefix) {
                    continue;
                }
                let binding_match_count = self.binding_column_match_count(binding, suffix, true);
                if binding_match_count > 0 {
                    match_count += binding_match_count;
                    if resolved_table.is_none() {
                        resolved_table = Some(binding.table.clone());
                    }
                }
            }
            return match match_count {
                0 => Err(DbError::plan(format!("unknown column {column}"))),
                1 => Ok((
                    resolved_table.expect("qualified column match must include a binding table"),
                    suffix.to_string(),
                )),
                _ => Err(DbError::plan(format!(
                    "ambiguous column reference: {column}"
                ))),
            };
        }

        let mut resolved_table = None;
        let mut match_count = 0;
        for binding in &scope.bindings {
            let binding_match_count = self.binding_column_match_count(binding, column, false);
            if binding_match_count > 0 {
                match_count += binding_match_count;
                if resolved_table.is_none() {
                    resolved_table = Some(binding.table.clone());
                }
            }
        }

        match match_count {
            0 => Err(DbError::plan(format!("unknown column {column}"))),
            1 => Ok((
                resolved_table.expect("unqualified column match must include a binding table"),
                column.to_string(),
            )),
            _ => Err(DbError::plan(format!(
                "ambiguous column reference: {column}"
            ))),
        }
    }

    fn require_schema<'a>(&self, context: &'a PlanningContext, table: &str) -> Result<&'a Schema> {
        if let Some(schema) = context.schema(table) {
            return Ok(schema);
        }
        if let Some(schema) = PlanningContext::single_row_source_schema(table) {
            let leaked = Box::leak(Box::new(schema));
            return Ok(leaked);
        }
        if let Some(schema) = PlanningContext::sqlite_catalog_schema(table) {
            let leaked = Box::leak(Box::new(schema));
            return Ok(leaked);
        }
        Err(DbError::plan(format!("unknown table: {table}")))
    }

    fn require_column(&self, schema: &Schema, column: &str) -> Result<()> {
        if self.schema_has_column(schema, column) {
            Ok(())
        } else {
            Err(DbError::plan(format!(
                "unknown column {column} on table {}",
                schema.name
            )))
        }
    }

    fn require_index_term(&self, schema: &Schema, table: &str, term: &str) -> Result<()> {
        if self.schema_has_column(schema, term) {
            return Ok(());
        }

        let expr = parse_scalar_sql_expression(term)?;
        let normalized = self.normalize_scalar_expr(schema, table, None, &expr)?;
        self.require_scalar_expr_columns(schema, &normalized)
    }

    fn require_schema_column_or_rowid(&self, schema: &Schema, column: &str) -> Result<()> {
        if self.schema_has_column(schema, column) || self.schema_exposes_rowid_name(schema, column)
        {
            Ok(())
        } else {
            Err(DbError::plan(format!(
                "unknown column {column} on table {}",
                schema.name
            )))
        }
    }

    fn schema_has_column(&self, schema: &Schema, column: &str) -> bool {
        schema.columns.iter().any(|entry| entry.name == column)
    }

    fn schema_exposes_rowid(&self, schema: &Schema) -> bool {
        !schema.without_rowid
    }

    fn is_rowid_column_name(&self, column: &str) -> bool {
        matches!(
            column.to_ascii_lowercase().as_str(),
            "rowid" | "oid" | "_rowid_"
        )
    }

    fn schema_exposes_rowid_name(&self, schema: &Schema, column: &str) -> bool {
        self.schema_exposes_rowid(schema)
            && self.is_rowid_column_name(column)
            && !self.schema_has_column(schema, column)
    }

    fn binding_column_match_count(
        &self,
        binding: &TableBinding,
        column: &str,
        include_hidden: bool,
    ) -> usize {
        let schema_matches = binding
            .schema
            .columns
            .iter()
            .filter(|entry| {
                entry.name == column
                    && (include_hidden
                        || !binding.hidden_columns.iter().any(|hidden| hidden == column))
            })
            .count();
        let rowid_matches = usize::from(
            binding.exposes_rowid && self.schema_exposes_rowid_name(&binding.schema, column),
        );
        schema_matches + rowid_matches
    }
}

fn from_item_qualifier(from: &FromItem) -> Option<String> {
    match from {
        FromItem::Table { name, alias }
        | FromItem::TableIndexed { name, alias, .. }
        | FromItem::TableNotIndexed { name, alias } => {
            Some(alias.clone().unwrap_or_else(|| name.clone()))
        }
        FromItem::Subquery { alias, .. } => (!alias.is_empty()).then(|| alias.clone()),
        FromItem::Values { alias, .. } => alias.clone(),
    }
}

fn table_source_parts(from: &FromItem) -> Option<(&str, Option<&str>, TableIndexHintRef<'_>)> {
    match from {
        FromItem::Table { name, alias } => {
            Some((name.as_str(), alias.as_deref(), TableIndexHintRef::None))
        }
        FromItem::TableIndexed { name, alias, index } => Some((
            name.as_str(),
            alias.as_deref(),
            TableIndexHintRef::IndexedBy(index.as_str()),
        )),
        FromItem::TableNotIndexed { name, alias } => Some((
            name.as_str(),
            alias.as_deref(),
            TableIndexHintRef::NotIndexed,
        )),
        FromItem::Subquery { .. } | FromItem::Values { .. } => None,
    }
}

fn table_index_hint(from: &FromItem) -> TableIndexHintRef<'_> {
    table_source_parts(from)
        .map(|(_, _, hint)| hint)
        .unwrap_or(TableIndexHintRef::None)
}

fn join_output_using_columns(
    left_columns: &[String],
    right_columns: &[String],
    join: &crate::sql::ast::JoinClause,
) -> Vec<String> {
    if join.natural {
        left_columns
            .iter()
            .filter(|column| right_columns.iter().any(|right| right == *column))
            .cloned()
            .collect()
    } else {
        join.using_columns.clone()
    }
}

fn qualify_join_using_column(qualifier: Option<&str>, column: &str) -> String {
    qualifier.map_or_else(
        || column.to_string(),
        |qualifier| format!("{qualifier}.{column}"),
    )
}

fn scalar_column_name(expr: ScalarExpr) -> Result<String> {
    match expr {
        ScalarExpr::Column(name) => Ok(name),
        _ => Err(DbError::plan("expected column expression")),
    }
}

fn join_using_expr(
    left_qualifier: Option<&str>,
    right_qualifier: Option<&str>,
    columns: &[String],
) -> Expr {
    let mut terms = columns.iter().map(|column| Expr::CompareColumns {
        left: qualify_join_using_column(left_qualifier, column),
        op: CompareOp::Eq,
        right: qualify_join_using_column(right_qualifier, column),
    });
    let Some(mut expr) = terms.next() else {
        return Expr::CompareScalar {
            left: ScalarExpr::Literal(Value::Boolean(true)),
            op: CompareOp::Eq,
            right: ScalarExpr::Literal(Value::Boolean(true)),
        };
    };
    for term in terms {
        expr = Expr::And(Box::new(expr), Box::new(term));
    }
    expr
}

fn select_item_group_expr(item: &SelectItem) -> Option<ScalarExpr> {
    match item {
        SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
            Some(ScalarExpr::Column(name.clone()))
        }
        SelectItem::Expr { expr, .. } => Some(expr.clone()),
        SelectItem::Wildcard | SelectItem::Aggregate { .. } => None,
    }
}

#[derive(Debug, Clone)]
struct TableBinding {
    table: String,
    alias: Option<String>,
    schema: Schema,
    exposes_rowid: bool,
    hidden_columns: Vec<String>,
}

#[derive(Debug, Clone)]
struct QueryScope {
    bindings: Vec<TableBinding>,
}
