use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::common::error::{DbError, Result};
use crate::common::types::{
    ColumnDef, ColumnDefault, ColumnType, IndexMeta, Row, RowId, Schema, Value,
};
use crate::engine::{PlanningStorageEngine, TransactionId};
use crate::sql::ast::{
    AggregateArg, AggregateFunc, AlterTableAction, Assignment, CompareOp, CompoundOperator, Expr,
    JoinKind, NullOrder, OrderBy, OrderByExpr, SINGLE_ROW_SOURCE_TABLE, ScalarBinaryOp, ScalarExpr,
    ScalarFunc, SelectItem, Statement, TableConstraint,
};
use crate::sql::optimizer::Optimizer;
use crate::sql::parser::{parse_check_constraint_expression, parse_scalar_sql_expression};
use crate::sql::plan::{IndexScanMode, IndexScanSpec, JoinPlan, Plan};
use crate::sql::planner::Planner;

const SQLITE_COMPILE_OPTIONS: &[&str] = &[
    "DEFAULT_PAGE_SIZE=4096",
    "MAX_PAGE_SIZE=65536",
    "OMIT_LOAD_EXTENSION",
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum InsertConflictTarget {
    PrimaryKey(Vec<String>),
    UniqueIndex(Vec<String>),
}

#[derive(Debug, Clone)]
struct ColumnMeta {
    table: Option<String>,
    alias: Option<String>,
    name: String,
    output_name: String,
    collation: Option<String>,
    hidden: bool,
}

#[derive(Debug, Clone)]
struct RowSet {
    columns: Vec<ColumnMeta>,
    rows: Vec<Row>,
}

#[derive(Debug, Clone)]
enum AggregateState {
    Count(i64),
    CountDistinct(BTreeSet<Value>),
    Sum {
        int_sum: i128,
        real_sum: f64,
        seen: bool,
        saw_real: bool,
    },
    SumDistinct(BTreeSet<Value>),
    Avg {
        int_sum: i128,
        real_sum: f64,
        count: i64,
        saw_real: bool,
    },
    AvgDistinct(BTreeSet<Value>),
    Total(f64),
    TotalDistinct(BTreeSet<Value>),
    Median(Vec<f64>),
    Percentile {
        values: Vec<f64>,
        fraction: Option<f64>,
        discrete: bool,
    },
    GroupConcat {
        value: Option<String>,
        ordered: Vec<(Vec<Option<Value>>, String, String)>,
        order_by: Vec<OrderBy>,
    },
    GroupConcatDistinct {
        values: Vec<String>,
        seen: BTreeSet<Value>,
        ordered: Vec<(Vec<Option<Value>>, String)>,
        order_by: Vec<OrderBy>,
    },
    JsonGroupArray {
        values: Vec<String>,
        ordered: Vec<(Vec<Option<Value>>, String)>,
        order_by: Vec<OrderBy>,
    },
    JsonGroupObject {
        fields: Vec<String>,
        ordered: Vec<(Vec<Option<Value>>, String)>,
        order_by: Vec<OrderBy>,
    },
    Min(Option<Value>),
    Max(Option<Value>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AggregateCall {
    func: AggregateFunc,
    arg: AggregateArg,
    filter: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LikeToken {
    Any,
    One,
    Literal(char),
}

struct AggregateExecOptions<'a> {
    columns: &'a [SelectItem],
    group_by: &'a [ScalarExpr],
    having: Option<&'a Expr>,
    order_by: &'a [OrderBy],
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default)]
struct SqlitePrintfFlags {
    left_align: bool,
    sign_plus: bool,
    sign_space: bool,
    zero_pad: bool,
    grouping: bool,
    alternate: bool,
}

pub struct Executor<'a, S: PlanningStorageEngine> {
    storage: &'a S,
    current_txn: &'a Cell<Option<TransactionId>>,
    last_insert_rowid: &'a Cell<i64>,
    changes: &'a Cell<i64>,
    total_changes: &'a Cell<i64>,
    foreign_keys: &'a Cell<bool>,
    read_uncommitted: &'a Cell<bool>,
    query_only: &'a Cell<bool>,
    recursive_triggers: &'a Cell<bool>,
    trusted_schema: &'a Cell<bool>,
    threads: &'a Cell<u32>,
    cache_size: &'a Cell<i64>,
    busy_timeout: &'a Cell<i64>,
    reverse_unordered_selects: &'a Cell<bool>,
}

impl<'a, S: PlanningStorageEngine> Executor<'a, S> {
    #[must_use]
    pub fn new(
        storage: &'a S,
        current_txn: &'a Cell<Option<TransactionId>>,
        last_insert_rowid: &'a Cell<i64>,
        changes: &'a Cell<i64>,
        total_changes: &'a Cell<i64>,
        foreign_keys: &'a Cell<bool>,
        read_uncommitted: &'a Cell<bool>,
        query_only: &'a Cell<bool>,
        recursive_triggers: &'a Cell<bool>,
        trusted_schema: &'a Cell<bool>,
        threads: &'a Cell<u32>,
        cache_size: &'a Cell<i64>,
        busy_timeout: &'a Cell<i64>,
        reverse_unordered_selects: &'a Cell<bool>,
    ) -> Self {
        Self {
            storage,
            current_txn,
            last_insert_rowid,
            changes,
            total_changes,
            foreign_keys,
            read_uncommitted,
            query_only,
            recursive_triggers,
            trusted_schema,
            threads,
            cache_size,
            busy_timeout,
            reverse_unordered_selects,
        }
    }

    pub fn execute(&self, plan: Plan) -> Result<Vec<Row>> {
        match plan {
            Plan::BeginTxn { isolation_level } => {
                if self.current_txn.get().is_some() {
                    return Err(DbError::txn("transaction already active"));
                }
                let transaction_id = self.storage.begin_with_isolation(isolation_level)?;
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
        if self.query_only.get() && plan_writes_database(&plan) {
            return Err(DbError::storage("attempt to write a readonly database"));
        }
        match plan {
            Plan::CreateTable {
                name,
                columns,
                constraints,
                strict,
                without_rowid,
                if_not_exists,
            } => {
                let mut schema = Schema::new(name, columns);
                schema.strict = strict;
                schema.without_rowid = without_rowid;
                let mut unique_indexes = Vec::new();
                let mut autoindex_ordinal = usize::from(schema.without_rowid) + 1;
                for constraint in constraints {
                    match constraint {
                        TableConstraint::Check(check) => schema = schema.with_check(check),
                        TableConstraint::ForeignKey(foreign_key) => {
                            schema = schema.with_foreign_key(foreign_key);
                        }
                        TableConstraint::PrimaryKey(primary_key_constraint) => {
                            let columns = primary_key_constraint.columns.clone();
                            schema.mark_primary_key_columns(&primary_key_constraint)?;
                            if !schema.without_rowid {
                                unique_indexes.push(IndexMeta {
                                    name: autoindex_name(&schema.name, autoindex_ordinal),
                                    columns,
                                    decorated_columns: None,
                                    unique: true,
                                    predicate: None,
                                });
                                autoindex_ordinal += 1;
                            }
                        }
                        TableConstraint::Unique(unique_constraint) => {
                            let columns = unique_constraint.columns.clone();
                            schema = schema.with_unique_constraint(unique_constraint.clone());
                            unique_indexes.push(IndexMeta {
                                name: autoindex_name(&schema.name, autoindex_ordinal),
                                columns,
                                decorated_columns: None,
                                unique: true,
                                predicate: None,
                            });
                            autoindex_ordinal += 1;
                        }
                    }
                }
                for column in &schema.columns {
                    if column.primary_key
                        && column.column_type == crate::common::types::ColumnType::Integer
                        && matches!(
                            column.primary_key_sort_order,
                            Some(crate::common::types::SortOrder::Desc)
                        )
                    {
                        unique_indexes.push(IndexMeta {
                            name: autoindex_name(&schema.name, autoindex_ordinal),
                            columns: vec![column.name.clone()],
                            decorated_columns: None,
                            unique: true,
                            predicate: None,
                        });
                        autoindex_ordinal += 1;
                        continue;
                    }
                    if column.unique {
                        unique_indexes.push(IndexMeta {
                            name: autoindex_name(&schema.name, autoindex_ordinal),
                            columns: vec![column.name.clone()],
                            decorated_columns: None,
                            unique: true,
                            predicate: None,
                        });
                        autoindex_ordinal += 1;
                    }
                }
                self.validate_create_table_foreign_key_metadata(transaction_id, &schema)?;
                let table_name = schema.name.clone();
                if let Err(error) = self.storage.create_schema(transaction_id, schema) {
                    if if_not_exists && error.to_string().contains("already exists") {
                        return Ok(Vec::new());
                    }
                    return Err(error);
                }
                for index in unique_indexes {
                    self.storage
                        .create_index(transaction_id, &table_name, index)?;
                }
                self.storage.increment_schema_version()?;
                Ok(Vec::new())
            }
            Plan::CreateTableAs { name, source, .. } => {
                let source = self.execute_query_plan(transaction_id, *source)?;
                let schema = Self::schema_from_ctas_rowset(&name, &source)?;
                self.storage.create_schema(transaction_id, schema)?;

                let mut inserted_count = 0_i64;
                let mut last_insert_rowid = None;
                for row in source.rows {
                    let row_id = self.storage.insert_row(transaction_id, &name, row)?;
                    last_insert_rowid = Some(
                        i64::try_from(row_id.0)
                            .map_err(|_| DbError::storage("rowid exceeds i64 range"))?,
                    );
                    inserted_count += 1;
                }

                if let Some(rowid) = last_insert_rowid {
                    self.last_insert_rowid.set(rowid);
                }
                self.record_changes(inserted_count);
                self.storage.increment_schema_version()?;
                Ok(Vec::new())
            }
            Plan::PragmaTableInfo { table } => {
                self.execute_pragma_table_info(transaction_id, &table, false)
            }
            Plan::PragmaTableXInfo { table } => {
                self.execute_pragma_table_info(transaction_id, &table, true)
            }
            Plan::PragmaTableList { table, schema } => {
                self.execute_pragma_table_list(transaction_id, table.as_deref(), schema.as_deref())
            }
            Plan::PragmaIndexList { table } => {
                self.execute_pragma_index_list(transaction_id, &table)
            }
            Plan::PragmaIndexInfo { index } => {
                self.execute_pragma_index_info(transaction_id, &index)
            }
            Plan::PragmaIndexXInfo { index } => {
                self.execute_pragma_index_xinfo(transaction_id, &index)
            }
            Plan::PragmaForeignKeyList { table } => {
                self.execute_pragma_foreign_key_list(transaction_id, &table)
            }
            Plan::PragmaForeignKeyCheck { table } => {
                self.execute_pragma_foreign_key_check(transaction_id, table.as_deref())
            }
            Plan::PragmaForeignKeys => Ok(vec![vec![Value::Integer(if self.foreign_keys.get() {
                1
            } else {
                0
            })]]),
            Plan::SetPragmaForeignKeys { enabled } => {
                self.foreign_keys.set(enabled);
                Ok(Vec::new())
            }
            Plan::PragmaReadUncommitted => {
                Ok(vec![vec![Value::Integer(if self.read_uncommitted.get() {
                    1
                } else {
                    0
                })]])
            }
            Plan::SetPragmaReadUncommitted { enabled } => {
                self.read_uncommitted.set(enabled);
                Ok(Vec::new())
            }
            Plan::PragmaQueryOnly => Ok(vec![vec![Value::Integer(if self.query_only.get() {
                1
            } else {
                0
            })]]),
            Plan::SetPragmaQueryOnly { enabled } => {
                self.query_only.set(enabled);
                Ok(Vec::new())
            }
            Plan::PragmaRecursiveTriggers => Ok(vec![vec![Value::Integer(
                if self.recursive_triggers.get() { 1 } else { 0 },
            )]]),
            Plan::SetPragmaRecursiveTriggers { enabled } => {
                self.recursive_triggers.set(enabled);
                Ok(Vec::new())
            }
            Plan::PragmaTrustedSchema => {
                Ok(vec![vec![Value::Integer(if self.trusted_schema.get() {
                    1
                } else {
                    0
                })]])
            }
            Plan::SetPragmaTrustedSchema { enabled } => {
                self.trusted_schema.set(enabled);
                Ok(Vec::new())
            }
            Plan::PragmaIgnoreCheckConstraints => Ok(vec![vec![Value::Integer(
                if self.storage.ignore_check_constraints() {
                    1
                } else {
                    0
                },
            )]]),
            Plan::SetPragmaIgnoreCheckConstraints { enabled } => {
                self.storage.set_ignore_check_constraints(enabled)?;
                Ok(Vec::new())
            }
            Plan::PragmaEncoding => Ok(vec![vec![Value::from("UTF-8")]]),
            Plan::PragmaCollationList => Ok(vec![
                vec![Value::Integer(0), Value::from("BINARY")],
                vec![Value::Integer(1), Value::from("NOCASE")],
                vec![Value::Integer(2), Value::from("RTRIM")],
            ]),
            Plan::PragmaDataVersion => Ok(vec![vec![Value::Integer(2)]]),
            Plan::PragmaQuickCheck | Plan::PragmaIntegrityCheck => {
                Ok(vec![vec![Value::from("ok")]])
            }
            Plan::PragmaFunctionList => Ok(sqlite_function_list_rows()),
            Plan::PragmaCompileOptions => Ok(SQLITE_COMPILE_OPTIONS
                .iter()
                .map(|option| vec![Value::from(*option)])
                .collect()),
            Plan::PragmaJournalMode => Ok(vec![vec![Value::from(self.storage.journal_mode())]]),
            Plan::PragmaSynchronous => Ok(vec![vec![Value::Integer(2)]]),
            Plan::PragmaCacheSize => Ok(vec![vec![Value::Integer(self.cache_size.get())]]),
            Plan::SetPragmaCacheSize { value } => {
                self.cache_size.set(value);
                Ok(Vec::new())
            }
            Plan::PragmaTempStore => Ok(vec![vec![Value::Integer(0)]]),
            Plan::PragmaLockingMode => Ok(vec![vec![Value::from("normal")]]),
            Plan::PragmaBusyTimeout => Ok(vec![vec![Value::Integer(self.busy_timeout.get())]]),
            Plan::SetPragmaBusyTimeout { value } => {
                if value >= 0 {
                    self.busy_timeout.set(value);
                }
                Ok(Vec::new())
            }
            Plan::PragmaThreads => Ok(vec![vec![Value::Integer(i64::from(self.threads.get()))]]),
            Plan::SetPragmaThreads { value } => {
                self.threads.set(value);
                Ok(Vec::new())
            }
            Plan::PragmaCaseSensitiveLike => Ok(vec![vec![Value::Integer(
                if self.storage.case_sensitive_like() {
                    1
                } else {
                    0
                },
            )]]),
            Plan::SetPragmaCaseSensitiveLike { enabled } => {
                self.storage.set_case_sensitive_like(enabled)?;
                Ok(Vec::new())
            }
            Plan::PragmaReverseUnorderedSelects => Ok(vec![vec![Value::Integer(
                if self.reverse_unordered_selects.get() {
                    1
                } else {
                    0
                },
            )]]),
            Plan::SetPragmaReverseUnorderedSelects { enabled } => {
                self.reverse_unordered_selects.set(enabled);
                Ok(Vec::new())
            }
            Plan::PragmaDatabaseList => Ok(vec![vec![
                Value::Integer(0),
                Value::from("main"),
                self.storage
                    .database_path()
                    .map(|path| Value::from(path.to_string_lossy().as_ref()))
                    .unwrap_or_else(|| Value::from("")),
            ]]),
            Plan::PragmaPageSize => Ok(vec![vec![Value::Integer(i64::from(
                self.storage.database_page_size(),
            ))]]),
            Plan::PragmaPageCount => Ok(vec![vec![Value::Integer(i64::from(
                self.storage.database_page_count()?,
            ))]]),
            Plan::PragmaFreelistCount => Ok(vec![vec![Value::Integer(i64::from(
                self.storage.database_freelist_count()?,
            ))]]),
            Plan::PragmaUserVersion => Ok(vec![vec![Value::Integer(i64::from(
                self.storage.user_version()?,
            ))]]),
            Plan::SetPragmaUserVersion { value } => {
                self.storage.set_user_version(value)?;
                Ok(Vec::new())
            }
            Plan::PragmaApplicationId => Ok(vec![vec![Value::Integer(i64::from(
                self.storage.application_id()?,
            ))]]),
            Plan::SetPragmaApplicationId { value } => {
                self.storage.set_application_id(value)?;
                Ok(Vec::new())
            }
            Plan::PragmaSchemaVersion => Ok(vec![vec![Value::Integer(i64::from(
                self.storage.schema_version()?,
            ))]]),
            Plan::SetPragmaSchemaVersion { value } => {
                self.storage.set_schema_version(value)?;
                Ok(Vec::new())
            }
            Plan::CreateIndex {
                name,
                table,
                columns,
                decorated_columns,
                unique,
                predicate,
                if_not_exists,
            } => {
                if let Some(predicate_sql) = predicate.as_deref() {
                    let schema = self.require_schema(transaction_id, &table)?;
                    let predicate = parse_check_constraint_expression(predicate_sql)?;
                    schema.validate_check_expr_metadata(&predicate)?;
                }
                if let Err(error) = self.storage.create_index(
                    transaction_id,
                    &table,
                    IndexMeta {
                        name,
                        columns,
                        decorated_columns,
                        unique,
                        predicate,
                    },
                ) {
                    if if_not_exists && error.to_string().contains("index already exists") {
                        return Ok(Vec::new());
                    }
                    return Err(error);
                }
                self.storage.increment_schema_version()?;
                Ok(Vec::new())
            }
            Plan::DropTable { name, .. } => {
                self.storage.drop_schema(transaction_id, &name)?;
                self.storage.increment_schema_version()?;
                Ok(Vec::new())
            }
            Plan::DropIndex { table, name, .. } => {
                self.storage.drop_index(transaction_id, &table, &name)?;
                self.storage.increment_schema_version()?;
                Ok(Vec::new())
            }
            Plan::NoOp => Ok(Vec::new()),
            Plan::AlterTable { table, action } => {
                match action {
                    AlterTableAction::AddColumn(column) => {
                        self.validate_add_column_existing_rows(transaction_id, &table, &column)?;
                        self.storage.add_column(transaction_id, &table, column)?;
                    }
                    AlterTableAction::RenameTable { new_name } => {
                        self.storage
                            .rename_schema(transaction_id, &table, &new_name)?;
                    }
                    AlterTableAction::RenameColumn { old_name, new_name } => {
                        self.storage
                            .rename_column(transaction_id, &table, &old_name, &new_name)?;
                    }
                    AlterTableAction::DropColumn { old_name } => {
                        self.storage
                            .drop_column(transaction_id, &table, &old_name)?;
                    }
                }
                self.storage.increment_schema_version()?;
                Ok(Vec::new())
            }
            Plan::Insert {
                table,
                or_conflict,
                values,
            } => {
                let inserted = self.insert_prepared_rows(
                    transaction_id,
                    &table,
                    or_conflict.as_deref(),
                    vec![values],
                )?;
                self.record_changes(inserted);
                Ok(Vec::new())
            }
            Plan::InsertReturning {
                table,
                or_conflict,
                values,
                returning,
            } => self.execute_insert_many_returning(
                transaction_id,
                &table,
                or_conflict.as_deref(),
                vec![values],
                &returning,
            ),
            Plan::InsertUpsert {
                table,
                values,
                upsert,
            } => {
                self.execute_insert_many_upsert(transaction_id, &table, vec![values], &upsert, None)
            }
            Plan::InsertUpsertReturning {
                table,
                values,
                upsert,
                returning,
            } => self.execute_insert_many_upsert(
                transaction_id,
                &table,
                vec![values],
                &upsert,
                Some(&returning),
            ),
            Plan::InsertManyUpsert {
                table,
                rows,
                upsert,
            } => self.execute_insert_many_upsert(transaction_id, &table, rows, &upsert, None),
            Plan::InsertManyUpsertReturning {
                table,
                rows,
                upsert,
                returning,
            } => self.execute_insert_many_upsert(
                transaction_id,
                &table,
                rows,
                &upsert,
                Some(&returning),
            ),
            Plan::InsertManyReturning {
                table,
                or_conflict,
                rows,
                returning,
            } => self.execute_insert_many_returning(
                transaction_id,
                &table,
                or_conflict.as_deref(),
                rows,
                &returning,
            ),
            Plan::InsertMany {
                table,
                or_conflict,
                rows,
            } => {
                let inserted = self.insert_prepared_rows(
                    transaction_id,
                    &table,
                    or_conflict.as_deref(),
                    rows,
                )?;
                self.record_changes(inserted);
                Ok(Vec::new())
            }
            Plan::InsertDoNothing {
                table,
                target,
                values,
            } => {
                let inserted = self.insert_prepared_rows_do_nothing(
                    transaction_id,
                    &table,
                    target.as_deref(),
                    vec![values],
                )?;
                self.record_changes(inserted);
                Ok(Vec::new())
            }
            Plan::InsertDoNothingReturning {
                table,
                target,
                values,
                returning,
            } => self.execute_insert_many_do_nothing_returning(
                transaction_id,
                &table,
                target.as_deref(),
                vec![values],
                &returning,
            ),
            Plan::InsertManyDoNothing {
                table,
                target,
                rows,
            } => {
                let inserted = self.insert_prepared_rows_do_nothing(
                    transaction_id,
                    &table,
                    target.as_deref(),
                    rows,
                )?;
                self.record_changes(inserted);
                Ok(Vec::new())
            }
            Plan::InsertManyDoNothingReturning {
                table,
                target,
                rows,
                returning,
            } => self.execute_insert_many_do_nothing_returning(
                transaction_id,
                &table,
                target.as_deref(),
                rows,
                &returning,
            ),
            Plan::InsertExpr {
                table,
                or_conflict,
                values,
            } => {
                let evaluated = self.evaluate_insert_value_exprs(&values)?;
                let inserted = self.insert_prepared_rows(
                    transaction_id,
                    &table,
                    or_conflict.as_deref(),
                    vec![evaluated],
                )?;
                self.record_changes(inserted);
                Ok(Vec::new())
            }
            Plan::InsertExprReturning {
                table,
                or_conflict,
                values,
                returning,
            } => {
                let values = self.evaluate_insert_value_exprs(&values)?;
                self.execute_insert_many_returning(
                    transaction_id,
                    &table,
                    or_conflict.as_deref(),
                    vec![values],
                    &returning,
                )
            }
            Plan::InsertExprUpsert {
                table,
                values,
                upsert,
            } => {
                let values = self.evaluate_insert_value_exprs(&values)?;
                self.execute_insert_many_upsert(transaction_id, &table, vec![values], &upsert, None)
            }
            Plan::InsertExprUpsertReturning {
                table,
                values,
                upsert,
                returning,
            } => {
                let values = self.evaluate_insert_value_exprs(&values)?;
                self.execute_insert_many_upsert(
                    transaction_id,
                    &table,
                    vec![values],
                    &upsert,
                    Some(&returning),
                )
            }
            Plan::InsertManyExprUpsert {
                table,
                rows,
                upsert,
            } => {
                let evaluated_rows = rows
                    .iter()
                    .map(|values| self.evaluate_insert_value_exprs(values))
                    .collect::<Result<Vec<_>>>()?;
                self.execute_insert_many_upsert(
                    transaction_id,
                    &table,
                    evaluated_rows,
                    &upsert,
                    None,
                )
            }
            Plan::InsertManyExprUpsertReturning {
                table,
                rows,
                upsert,
                returning,
            } => {
                let evaluated_rows = rows
                    .iter()
                    .map(|values| self.evaluate_insert_value_exprs(values))
                    .collect::<Result<Vec<_>>>()?;
                self.execute_insert_many_upsert(
                    transaction_id,
                    &table,
                    evaluated_rows,
                    &upsert,
                    Some(&returning),
                )
            }
            Plan::InsertManyExpr {
                table,
                or_conflict,
                rows,
            } => {
                let evaluated_rows = rows
                    .iter()
                    .map(|values| self.evaluate_insert_value_exprs(values))
                    .collect::<Result<Vec<_>>>()?;
                let inserted = self.insert_prepared_rows(
                    transaction_id,
                    &table,
                    or_conflict.as_deref(),
                    evaluated_rows,
                )?;
                self.record_changes(inserted);
                Ok(Vec::new())
            }
            Plan::InsertManyExprReturning {
                table,
                or_conflict,
                rows,
                returning,
            } => {
                let evaluated_rows = rows
                    .iter()
                    .map(|values| self.evaluate_insert_value_exprs(values))
                    .collect::<Result<Vec<_>>>()?;
                self.execute_insert_many_returning(
                    transaction_id,
                    &table,
                    or_conflict.as_deref(),
                    evaluated_rows,
                    &returning,
                )
            }
            Plan::InsertExprDoNothing {
                table,
                target,
                values,
            } => {
                let evaluated = self.evaluate_insert_value_exprs(&values)?;
                let inserted = self.insert_prepared_rows_do_nothing(
                    transaction_id,
                    &table,
                    target.as_deref(),
                    vec![evaluated],
                )?;
                self.record_changes(inserted);
                Ok(Vec::new())
            }
            Plan::InsertExprDoNothingReturning {
                table,
                target,
                values,
                returning,
            } => {
                let evaluated = self.evaluate_insert_value_exprs(&values)?;
                self.execute_insert_many_do_nothing_returning(
                    transaction_id,
                    &table,
                    target.as_deref(),
                    vec![evaluated],
                    &returning,
                )
            }
            Plan::InsertManyExprDoNothing {
                table,
                target,
                rows,
            } => {
                let evaluated_rows = rows
                    .iter()
                    .map(|values| self.evaluate_insert_value_exprs(values))
                    .collect::<Result<Vec<_>>>()?;
                let inserted = self.insert_prepared_rows_do_nothing(
                    transaction_id,
                    &table,
                    target.as_deref(),
                    evaluated_rows,
                )?;
                self.record_changes(inserted);
                Ok(Vec::new())
            }
            Plan::InsertManyExprDoNothingReturning {
                table,
                target,
                rows,
                returning,
            } => {
                let evaluated_rows = rows
                    .iter()
                    .map(|values| self.evaluate_insert_value_exprs(values))
                    .collect::<Result<Vec<_>>>()?;
                self.execute_insert_many_do_nothing_returning(
                    transaction_id,
                    &table,
                    target.as_deref(),
                    evaluated_rows,
                    &returning,
                )
            }
            Plan::InsertSelect {
                table,
                columns,
                or_conflict,
                source,
            } => {
                let schema = self.require_schema(transaction_id, &table)?;
                let source = self.execute_query_plan(transaction_id, *source)?;
                let mut inserted_count = 0_i64;
                let mut last_insert_rowid = None;

                for source_row in source.rows {
                    let row = self.build_insert_select_row(
                        &schema,
                        &table,
                        columns.as_deref(),
                        source_row,
                    )?;
                    let row = self.normalize_insert_row_input(&schema, row)?;
                    let row = self.populate_generated_columns(&schema, row)?;
                    self.validate_foreign_key_references(transaction_id, &schema, &row)?;
                    let row_id = match self.storage.insert_row(transaction_id, &table, row.clone())
                    {
                        Ok(row_id) => row_id,
                        Err(error) if matches_ignore_conflict(or_conflict.as_deref(), &error) => {
                            continue;
                        }
                        Err(error) if matches_replace_conflict(or_conflict.as_deref(), &error) => {
                            self.replace_conflicting_rows_and_insert(
                                transaction_id,
                                &table,
                                &schema,
                                row,
                            )?
                        }
                        Err(error)
                            if matches_rollback_conflict(or_conflict.as_deref(), &error)
                                && self.current_txn.get() == Some(transaction_id) =>
                        {
                            self.storage.rollback(transaction_id)?;
                            self.current_txn.set(None);
                            return Err(error);
                        }
                        Err(error) => return Err(error),
                    };
                    last_insert_rowid = Some(
                        i64::try_from(row_id.0)
                            .map_err(|_| DbError::storage("rowid exceeds i64 range"))?,
                    );
                    inserted_count += 1;
                }

                if let Some(rowid) = last_insert_rowid {
                    self.last_insert_rowid.set(rowid);
                }
                self.record_changes(inserted_count);
                Ok(Vec::new())
            }
            Plan::InsertSelectReturning {
                table,
                columns,
                or_conflict,
                source,
                returning,
            } => {
                let schema = self.require_schema(transaction_id, &table)?;
                let source = self.execute_query_plan(transaction_id, *source)?;
                let mut rows = Vec::with_capacity(source.rows.len());

                for source_row in source.rows {
                    rows.push(self.build_insert_select_row(
                        &schema,
                        &table,
                        columns.as_deref(),
                        source_row,
                    )?);
                }

                self.execute_insert_many_returning(
                    transaction_id,
                    &table,
                    or_conflict.as_deref(),
                    rows,
                    &returning,
                )
            }
            Plan::InsertSelectUpsert {
                table,
                columns,
                source,
                upsert,
            } => {
                let schema = self.require_schema(transaction_id, &table)?;
                let source = self.execute_query_plan(transaction_id, *source)?;
                let mut rows = Vec::with_capacity(source.rows.len());

                for source_row in source.rows {
                    rows.push(self.build_insert_select_row(
                        &schema,
                        &table,
                        columns.as_deref(),
                        source_row,
                    )?);
                }

                self.execute_insert_many_upsert(transaction_id, &table, rows, &upsert, None)
            }
            Plan::InsertSelectUpsertReturning {
                table,
                columns,
                source,
                upsert,
                returning,
            } => {
                let schema = self.require_schema(transaction_id, &table)?;
                let source = self.execute_query_plan(transaction_id, *source)?;
                let mut rows = Vec::with_capacity(source.rows.len());

                for source_row in source.rows {
                    rows.push(self.build_insert_select_row(
                        &schema,
                        &table,
                        columns.as_deref(),
                        source_row,
                    )?);
                }

                self.execute_insert_many_upsert(
                    transaction_id,
                    &table,
                    rows,
                    &upsert,
                    Some(&returning),
                )
            }
            Plan::InsertSelectDoNothing {
                table,
                columns,
                target,
                source,
            } => {
                let schema = self.require_schema(transaction_id, &table)?;
                let source = self.execute_query_plan(transaction_id, *source)?;
                let mut rows = Vec::with_capacity(source.rows.len());

                for source_row in source.rows {
                    rows.push(self.build_insert_select_row(
                        &schema,
                        &table,
                        columns.as_deref(),
                        source_row,
                    )?);
                }

                let inserted = self.insert_prepared_rows_do_nothing(
                    transaction_id,
                    &table,
                    target.as_deref(),
                    rows,
                )?;
                self.record_changes(inserted);
                Ok(Vec::new())
            }
            Plan::InsertSelectDoNothingReturning {
                table,
                columns,
                target,
                source,
                returning,
            } => {
                let schema = self.require_schema(transaction_id, &table)?;
                let source = self.execute_query_plan(transaction_id, *source)?;
                let mut rows = Vec::with_capacity(source.rows.len());

                for source_row in source.rows {
                    rows.push(self.build_insert_select_row(
                        &schema,
                        &table,
                        columns.as_deref(),
                        source_row,
                    )?);
                }

                self.execute_insert_many_do_nothing_returning(
                    transaction_id,
                    &table,
                    target.as_deref(),
                    rows,
                    &returning,
                )
            }
            Plan::Delete { table, filter } => self.execute_delete(
                transaction_id,
                &table,
                filter.as_ref(),
                None,
                &[],
                None,
                None,
            ),
            Plan::DeleteLimited {
                table,
                filter,
                order_by,
                limit,
                offset,
            } => self.execute_delete(
                transaction_id,
                &table,
                filter.as_ref(),
                None,
                &order_by,
                limit,
                offset,
            ),
            Plan::DeleteReturning {
                table,
                filter,
                returning,
            } => self.execute_delete(
                transaction_id,
                &table,
                filter.as_ref(),
                Some(&returning),
                &[],
                None,
                None,
            ),
            Plan::DeleteReturningLimited {
                table,
                filter,
                returning,
                order_by,
                limit,
                offset,
            } => self.execute_delete(
                transaction_id,
                &table,
                filter.as_ref(),
                Some(&returning),
                &order_by,
                limit,
                offset,
            ),
            Plan::Update {
                table,
                assignments,
                filter,
            } => self.execute_update(
                transaction_id,
                &table,
                &assignments,
                filter.as_ref(),
                None,
                &[],
                None,
                None,
            ),
            Plan::UpdateLimited {
                table,
                assignments,
                filter,
                order_by,
                limit,
                offset,
            } => self.execute_update(
                transaction_id,
                &table,
                &assignments,
                filter.as_ref(),
                None,
                &order_by,
                limit,
                offset,
            ),
            Plan::UpdateReturning {
                table,
                assignments,
                filter,
                returning,
            } => self.execute_update(
                transaction_id,
                &table,
                &assignments,
                filter.as_ref(),
                Some(&returning),
                &[],
                None,
                None,
            ),
            Plan::UpdateReturningLimited {
                table,
                assignments,
                filter,
                returning,
                order_by,
                limit,
                offset,
            } => self.execute_update(
                transaction_id,
                &table,
                &assignments,
                filter.as_ref(),
                Some(&returning),
                &order_by,
                limit,
                offset,
            ),
            query_plan => Ok(self.execute_query_plan(transaction_id, query_plan)?.rows),
        }
    }

    fn execute_delete(
        &self,
        transaction_id: TransactionId,
        table: &str,
        filter: Option<&Expr>,
        returning: Option<&[SelectItem]>,
        order_by: &[OrderBy],
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Vec<Row>> {
        let schema = self.require_schema(transaction_id, table)?;
        let source = self.scan_table_rowset(transaction_id, table, None, None, None)?;
        let rows = self.storage.scan_rows(transaction_id, table)?;
        let mut pending_deletes = Vec::new();
        for ((row_id, stored_row), source_row) in rows.iter().zip(source.rows.iter()) {
            if self.matches_filter(transaction_id, &source, source_row, filter, None)? {
                pending_deletes.push((*row_id, stored_row.clone(), source_row.clone()));
            }
        }

        if !order_by.is_empty() || limit.is_some() || offset.is_some() {
            let mut ordered = pending_deletes
                .into_iter()
                .map(|(row_id, stored_row, source_row)| {
                    let sort_key = self.order_sort_key(
                        Some(transaction_id),
                        &source.columns,
                        &source_row,
                        &source.columns,
                        &source_row,
                        order_by,
                    )?;
                    Ok((sort_key, row_id, stored_row, source_row))
                })
                .collect::<Result<Vec<_>>>()?;
            if !order_by.is_empty() {
                ordered.sort_by(|(left_key, ..), (right_key, ..)| {
                    self.compare_order_keys(left_key, right_key, order_by)
                });
            }
            let mut selected = ordered
                .into_iter()
                .map(|(_, row_id, stored_row, source_row)| (row_id, stored_row, source_row))
                .collect::<Vec<_>>();
            Self::apply_limit_offset_for_delete(&mut selected, limit, offset);
            pending_deletes = selected;
        }

        for (_, stored_row, _) in &pending_deletes {
            self.validate_no_foreign_key_dependents(transaction_id, table, stored_row)?;
        }

        let deleted_count = i64::try_from(pending_deletes.len())
            .map_err(|_| DbError::storage("delete count exceeds i64 range"))?;
        let rowset = returning.map(|_| self.returning_rowset(&schema, table));
        let mut returned = Vec::new();
        for (row_id, stored_row, _) in pending_deletes {
            if let (Some(returning), Some(rowset)) = (returning, rowset.as_ref()) {
                let projected_row = self.append_hidden_rowid(
                    stored_row.clone(),
                    row_id,
                    &schema,
                    !schema.without_rowid,
                )?;
                returned.push(self.project_row(
                    Some(transaction_id),
                    rowset,
                    &projected_row,
                    returning,
                )?);
            }
            self.storage.delete_row(transaction_id, table, row_id)?;
        }
        self.record_changes(deleted_count);
        Ok(returned)
    }

    fn execute_update(
        &self,
        transaction_id: TransactionId,
        table: &str,
        assignments: &[Assignment],
        filter: Option<&Expr>,
        returning: Option<&[SelectItem]>,
        order_by: &[OrderBy],
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Vec<Row>> {
        let schema = self.require_schema(transaction_id, table)?;
        let source = self.scan_table_rowset(transaction_id, table, None, None, None)?;
        let rows = self.storage.scan_rows(transaction_id, table)?;
        let indexes = self.all_indexes(transaction_id, table)?;
        let mut candidate_updates = Vec::new();

        for ((row_id, stored_row), source_row) in rows.iter().zip(source.rows.iter()) {
            if self.matches_filter(transaction_id, &source, source_row, filter, None)? {
                let mut updated = stored_row.clone();
                for assignment in assignments {
                    let position = schema
                        .columns
                        .iter()
                        .position(|entry| entry.name == assignment.column)
                        .ok_or_else(|| {
                            DbError::plan(format!(
                                "unknown column {} on table {}",
                                assignment.column, schema.name
                            ))
                        })?;
                    if schema.columns[position].generated_expr.is_some() {
                        return Err(DbError::storage(format!(
                            "cannot UPDATE generated column {}",
                            assignment.column
                        )));
                    }
                    updated[position] =
                        self.evaluate_scalar_expr(&source, source_row, &assignment.value)?;
                }
                updated = self.populate_generated_columns(&schema, updated)?;
                candidate_updates.push((*row_id, stored_row.clone(), updated, source_row.clone()));
            }
        }

        if !order_by.is_empty() || limit.is_some() || offset.is_some() {
            let mut ordered = candidate_updates
                .into_iter()
                .map(|(row_id, stored_row, updated, source_row)| {
                    let sort_key = self.order_sort_key(
                        Some(transaction_id),
                        &source.columns,
                        &source_row,
                        &source.columns,
                        &source_row,
                        order_by,
                    )?;
                    Ok((sort_key, row_id, stored_row, updated, source_row))
                })
                .collect::<Result<Vec<_>>>()?;
            if !order_by.is_empty() {
                ordered.sort_by(|(left_key, ..), (right_key, ..)| {
                    self.compare_order_keys(left_key, right_key, order_by)
                });
            }
            let mut selected = ordered
                .into_iter()
                .map(|(_, row_id, stored_row, updated, source_row)| {
                    (row_id, stored_row, updated, source_row)
                })
                .collect::<Vec<_>>();
            Self::apply_limit_offset_for_update(&mut selected, limit, offset);
            candidate_updates = selected;
        }

        let selected_row_ids = candidate_updates
            .iter()
            .map(|(row_id, _, _, _)| *row_id)
            .collect::<std::collections::BTreeSet<_>>();
        let final_rows = rows
            .iter()
            .map(|(row_id, stored_row)| {
                if selected_row_ids.contains(row_id) {
                    candidate_updates
                        .iter()
                        .find(|(candidate_row_id, _, _, _)| candidate_row_id == row_id)
                        .map(|(_, _, updated, _)| updated.clone())
                        .unwrap_or_else(|| stored_row.clone())
                } else {
                    stored_row.clone()
                }
            })
            .collect::<Vec<_>>();
        let pending_updates = candidate_updates
            .into_iter()
            .map(|(row_id, stored_row, updated, _)| (row_id, stored_row, updated))
            .collect::<Vec<_>>();

        self.validate_update_result_constraints(transaction_id, &schema, &indexes, &final_rows)?;
        self.validate_update_parent_key_changes(transaction_id, table, &schema, &pending_updates)?;

        let updated_count = i64::try_from(pending_updates.len())
            .map_err(|_| DbError::storage("update count exceeds i64 range"))?;
        let rowset = returning.map(|_| self.returning_rowset(&schema, table));
        let mut returned = Vec::new();
        for (row_id, _, updated) in pending_updates {
            self.storage
                .update_row(transaction_id, table, row_id, updated.clone())?;
            if let (Some(returning), Some(rowset)) = (returning, rowset.as_ref()) {
                let updated =
                    self.append_hidden_rowid(updated, row_id, &schema, !schema.without_rowid)?;
                returned.push(self.project_row(
                    Some(transaction_id),
                    rowset,
                    &updated,
                    returning,
                )?);
            }
        }
        self.record_changes(updated_count);
        Ok(returned)
    }

    fn validate_update_result_constraints(
        &self,
        transaction_id: TransactionId,
        schema: &Schema,
        indexes: &[IndexMeta],
        final_rows: &[Row],
    ) -> Result<()> {
        for row in final_rows {
            schema.validate_row_values(row)?;
            schema.validate_check_constraints_with_like_mode(
                row,
                self.storage.case_sensitive_like(),
            )?;
            self.validate_foreign_key_references(transaction_id, schema, row)?;
        }

        self.validate_update_primary_key_uniqueness(schema, final_rows)?;
        self.validate_update_unique_index_constraints(schema, indexes, final_rows)
    }

    fn normalize_insert_row_input(&self, schema: &Schema, values: Row) -> Result<Row> {
        let generated_positions = schema
            .columns
            .iter()
            .enumerate()
            .filter_map(|(index, column)| column.generated_expr.as_ref().map(|_| index))
            .collect::<Vec<_>>();
        let writable_column_count = schema.columns.len() - generated_positions.len();

        if values.len() == schema.columns.len() {
            for index in generated_positions {
                if !matches!(values[index], Value::Null) {
                    return Err(DbError::storage(format!(
                        "cannot INSERT into generated column {}",
                        schema.columns[index].name
                    )));
                }
            }
            return Ok(values);
        }

        if values.len() != writable_column_count {
            return Err(DbError::storage(format!(
                "insert into {} expected {} values but got {}",
                schema.name,
                writable_column_count,
                values.len()
            )));
        }

        let mut input_values = values.into_iter();
        let mut row = Vec::with_capacity(schema.columns.len());
        for column in &schema.columns {
            if column.generated_expr.is_some() {
                row.push(Value::Null);
            } else {
                row.push(input_values.next().ok_or_else(|| {
                    DbError::storage(format!(
                        "insert into {} expected {} values but got {}",
                        schema.name,
                        writable_column_count,
                        writable_column_count.saturating_sub(1)
                    ))
                })?);
            }
        }
        Ok(row)
    }

    fn build_insert_select_row(
        &self,
        schema: &Schema,
        table: &str,
        columns: Option<&[String]>,
        values: Row,
    ) -> Result<Row> {
        match columns {
            None => Ok(values),
            Some(columns) => {
                if columns.is_empty() {
                    return Err(DbError::storage("insert column list cannot be empty"));
                }
                if columns.len() != values.len() {
                    return Err(DbError::storage(format!(
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
                for (column, value) in columns.iter().zip(values.into_iter()) {
                    if !seen.insert(column.clone()) {
                        return Err(DbError::storage(format!(
                            "duplicate insert column: {column}"
                        )));
                    }
                    let position = schema
                        .columns
                        .iter()
                        .position(|entry| entry.name == *column)
                        .ok_or_else(|| {
                            DbError::storage(format!(
                                "unknown column {column} on table {}",
                                schema.name
                            ))
                        })?;
                    if schema.columns[position].generated_expr.is_some() {
                        return Err(DbError::storage(format!(
                            "cannot INSERT into generated column {column}"
                        )));
                    }
                    row[position] = value;
                }
                Ok(row)
            }
        }
    }

    fn insert_prepared_rows(
        &self,
        transaction_id: TransactionId,
        table: &str,
        or_conflict: Option<&str>,
        rows: Vec<Row>,
    ) -> Result<i64> {
        let schema = self.require_schema(transaction_id, table)?;
        let mut inserted_count = 0_i64;
        let mut last_insert_rowid = None;

        for values in rows {
            let row = self.normalize_insert_row_input(&schema, values)?;
            let row = self.populate_generated_columns(&schema, row)?;
            self.validate_foreign_key_references(transaction_id, &schema, &row)?;
            let row_id = match self.storage.insert_row(transaction_id, table, row.clone()) {
                Ok(row_id) => row_id,
                Err(error) if matches_ignore_conflict(or_conflict, &error) => {
                    continue;
                }
                Err(error) if matches_replace_conflict(or_conflict, &error) => {
                    self.replace_conflicting_rows_and_insert(transaction_id, table, &schema, row)?
                }
                Err(error)
                    if matches_rollback_conflict(or_conflict, &error)
                        && self.current_txn.get() == Some(transaction_id) =>
                {
                    self.storage.rollback(transaction_id)?;
                    self.current_txn.set(None);
                    return Err(error);
                }
                Err(error) => return Err(error),
            };

            last_insert_rowid = Some(
                i64::try_from(row_id.0).map_err(|_| DbError::storage("rowid exceeds i64 range"))?,
            );
            inserted_count += 1;
        }

        if let Some(rowid) = last_insert_rowid {
            self.last_insert_rowid.set(rowid);
        }

        Ok(inserted_count)
    }

    fn execute_insert_many_returning(
        &self,
        transaction_id: TransactionId,
        table: &str,
        or_conflict: Option<&str>,
        rows: Vec<Row>,
        returning: &[SelectItem],
    ) -> Result<Vec<Row>> {
        let schema = self.require_schema(transaction_id, table)?;
        let rowset = self.returning_rowset(&schema, table);
        let mut returned = Vec::new();
        let mut inserted_count = 0_i64;
        let mut last_insert_rowid = None;

        for values in rows {
            let row = self.normalize_insert_row_input(&schema, values)?;
            let row = self.populate_generated_columns(&schema, row)?;
            self.validate_foreign_key_references(transaction_id, &schema, &row)?;
            let row_id = match self.storage.insert_row(transaction_id, table, row.clone()) {
                Ok(row_id) => row_id,
                Err(error) if matches_ignore_conflict(or_conflict, &error) => {
                    continue;
                }
                Err(error) if matches_replace_conflict(or_conflict, &error) => {
                    self.replace_conflicting_rows_and_insert(transaction_id, table, &schema, row)?
                }
                Err(error)
                    if matches_rollback_conflict(or_conflict, &error)
                        && self.current_txn.get() == Some(transaction_id) =>
                {
                    self.storage.rollback(transaction_id)?;
                    self.current_txn.set(None);
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            let row = self
                .storage
                .get_row(transaction_id, table, row_id)?
                .ok_or_else(|| DbError::storage("inserted row could not be read for RETURNING"))?;
            let row = self.append_hidden_rowid(row, row_id, &schema, !schema.without_rowid)?;
            returned.push(self.project_row(Some(transaction_id), &rowset, &row, returning)?);
            last_insert_rowid = Some(
                i64::try_from(row_id.0).map_err(|_| DbError::storage("rowid exceeds i64 range"))?,
            );
            inserted_count += 1;
        }

        if let Some(rowid) = last_insert_rowid {
            self.last_insert_rowid.set(rowid);
        }
        self.record_changes(inserted_count);

        Ok(returned)
    }

    fn execute_insert_many_upsert(
        &self,
        transaction_id: TransactionId,
        table: &str,
        rows: Vec<Row>,
        upsert: &crate::sql::ast::UpsertClause,
        returning: Option<&[SelectItem]>,
    ) -> Result<Vec<Row>> {
        let schema = self.require_schema(transaction_id, table)?;
        let rowset = returning.map(|_| self.returning_rowset(&schema, table));
        let mut returned = Vec::new();
        let mut changes = 0_i64;
        let mut last_insert_rowid = None;

        for values in rows {
            let (changed, inserted_rowid, returned_row) = self.execute_single_upsert_row(
                transaction_id,
                table,
                &schema,
                values,
                upsert,
                returning,
                rowset.as_ref(),
            )?;
            if changed {
                changes += 1;
            }
            if let Some(rowid) = inserted_rowid {
                last_insert_rowid = Some(rowid);
            }
            if let Some(row) = returned_row {
                returned.push(row);
            }
        }

        if let Some(rowid) = last_insert_rowid {
            self.last_insert_rowid.set(rowid);
        }
        self.record_changes(changes);

        Ok(returned)
    }

    fn execute_single_upsert_row(
        &self,
        transaction_id: TransactionId,
        table: &str,
        schema: &Schema,
        values: Row,
        upsert: &crate::sql::ast::UpsertClause,
        returning: Option<&[SelectItem]>,
        rowset: Option<&RowSet>,
    ) -> Result<(bool, Option<i64>, Option<Row>)> {
        let row = self.normalize_insert_row_input(&schema, values)?;
        let row = self.populate_generated_columns(&schema, row)?;
        self.validate_foreign_key_references(transaction_id, schema, &row)?;

        match self.storage.insert_row(transaction_id, table, row.clone()) {
            Ok(row_id) => {
                let last_insert_rowid = i64::try_from(row_id.0)
                    .map_err(|_| DbError::storage("rowid exceeds i64 range"))?;
                if let (Some(returning), Some(rowset)) = (returning, rowset) {
                    let stored = self
                        .storage
                        .get_row(transaction_id, table, row_id)?
                        .ok_or_else(|| {
                            DbError::storage("inserted row could not be read for RETURNING")
                        })?;
                    let stored =
                        self.append_hidden_rowid(stored, row_id, schema, !schema.without_rowid)?;
                    return Ok((
                        true,
                        Some(last_insert_rowid),
                        Some(self.project_row(Some(transaction_id), rowset, &stored, returning)?),
                    ));
                }
                Ok((true, Some(last_insert_rowid), None))
            }
            Err(insert_error) => {
                let indexes = self.storage.list_all_indexes(transaction_id, table)?;
                let conflicts =
                    self.classify_insert_conflicts(transaction_id, table, schema, &indexes, &row)?;
                if conflicts.is_empty()
                    || !self.do_nothing_target_matches(upsert.target.as_deref(), &conflicts)
                {
                    return Err(insert_error);
                }

                let mut conflicting_rows =
                    self.find_insert_conflicting_rows(transaction_id, table, &schema, &row)?;
                conflicting_rows.retain(|(_, existing)| {
                    self.row_conflicts_with_target(
                        schema,
                        &indexes,
                        existing,
                        &row,
                        upsert.target.as_deref(),
                    )
                    .unwrap_or(false)
                });
                if conflicting_rows.len() != 1 {
                    return Err(DbError::storage(
                        "ON CONFLICT DO UPDATE expected exactly one conflicting row",
                    ));
                }

                let (row_id, existing) = conflicting_rows.remove(0);
                if let Some(filter) = &upsert.filter
                    && !self.matches_upsert_filter(
                        transaction_id,
                        schema,
                        table,
                        &existing,
                        &row,
                        filter,
                    )?
                {
                    return Ok((false, None, None));
                }
                let mut updated =
                    self.evaluate_upsert_assignments(schema, table, &existing, &row, upsert)?;
                updated = self.populate_generated_columns(schema, updated)?;

                let rows = self.storage.scan_rows(transaction_id, table)?;
                let mut final_rows = Vec::with_capacity(rows.len());
                let mut pending_updates = Vec::new();
                for (existing_row_id, stored_row) in rows {
                    if existing_row_id == row_id {
                        final_rows.push(updated.clone());
                        pending_updates.push((existing_row_id, stored_row, updated.clone()));
                    } else {
                        final_rows.push(stored_row);
                    }
                }

                self.validate_update_result_constraints(
                    transaction_id,
                    schema,
                    &indexes,
                    &final_rows,
                )?;
                self.validate_update_parent_key_changes(
                    transaction_id,
                    table,
                    schema,
                    &pending_updates,
                )?;

                self.storage
                    .update_row(transaction_id, table, row_id, updated.clone())?;

                if let (Some(returning), Some(rowset)) = (returning, rowset) {
                    let updated =
                        self.append_hidden_rowid(updated, row_id, schema, !schema.without_rowid)?;
                    return Ok((
                        true,
                        None,
                        Some(self.project_row(
                            Some(transaction_id),
                            rowset,
                            &updated,
                            returning,
                        )?),
                    ));
                }
                Ok((true, None, None))
            }
        }
    }

    fn matches_upsert_filter(
        &self,
        transaction_id: TransactionId,
        schema: &Schema,
        table: &str,
        existing: &Row,
        excluded: &Row,
        filter: &Expr,
    ) -> Result<bool> {
        let (source, eval_row) = self.upsert_eval_source(schema, table, existing, excluded);
        self.matches_filter(transaction_id, &source, &eval_row, Some(filter), None)
    }

    fn evaluate_upsert_assignments(
        &self,
        schema: &Schema,
        table: &str,
        existing: &Row,
        excluded: &Row,
        upsert: &crate::sql::ast::UpsertClause,
    ) -> Result<Row> {
        let (source, eval_row) = self.upsert_eval_source(schema, table, existing, excluded);
        let mut updated = existing.to_vec();
        for assignment in &upsert.assignments {
            let position = schema
                .columns
                .iter()
                .position(|entry| entry.name == assignment.column)
                .ok_or_else(|| {
                    DbError::plan(format!(
                        "unknown column {} on table {}",
                        assignment.column, schema.name
                    ))
                })?;
            if schema.columns[position].generated_expr.is_some() {
                return Err(DbError::storage(format!(
                    "cannot UPDATE generated column {}",
                    assignment.column
                )));
            }
            updated[position] = self.evaluate_scalar_expr(&source, &eval_row, &assignment.value)?;
        }

        Ok(updated)
    }

    fn upsert_eval_source(
        &self,
        schema: &Schema,
        table: &str,
        existing: &Row,
        excluded: &Row,
    ) -> (RowSet, Row) {
        let mut columns = schema
            .columns
            .iter()
            .map(|column| ColumnMeta {
                table: Some(table.to_string()),
                alias: None,
                name: column.name.clone(),
                output_name: column.name.clone(),
                collation: column.collation.clone(),
                hidden: false,
            })
            .collect::<Vec<_>>();
        columns.extend(schema.columns.iter().map(|column| ColumnMeta {
            table: Some("excluded".to_string()),
            alias: None,
            name: column.name.clone(),
            output_name: format!("excluded.{}", column.name),
            collation: column.collation.clone(),
            hidden: true,
        }));
        let source = RowSet {
            columns,
            rows: Vec::new(),
        };
        let mut eval_row = existing.clone();
        eval_row.extend(excluded.iter().cloned());
        (source, eval_row)
    }

    fn row_conflicts_with_target(
        &self,
        schema: &Schema,
        indexes: &[IndexMeta],
        existing: &Row,
        candidate: &Row,
        target: Option<&[String]>,
    ) -> Result<bool> {
        let primary_key_columns = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.primary_key)
            .collect::<Vec<_>>();
        let primary_key_matches_target = target.is_none_or(|target| {
            primary_key_columns
                .iter()
                .map(|(_, column)| column.name.clone())
                .collect::<Vec<_>>()
                .as_slice()
                == target
        });
        if primary_key_matches_target
            && !primary_key_columns.is_empty()
            && !primary_key_columns
                .iter()
                .any(|(index, _)| matches!(candidate[*index], Value::Null))
            && primary_key_columns
                .iter()
                .all(|(index, _)| existing.get(*index) == Some(&candidate[*index]))
        {
            return Ok(true);
        }

        for index in indexes
            .iter()
            .filter(|index| index.unique && target.is_none_or(|target| index.columns == target))
        {
            if !self.row_matches_partial_index(schema, index, candidate)?
                || !self.row_matches_partial_index(schema, index, existing)?
            {
                continue;
            }
            let candidate_key = self.project_index_key(schema, index, candidate)?;
            if !index.enforces_unique_key(&candidate_key) {
                continue;
            }
            if self.project_index_key(schema, index, existing)? == candidate_key {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn returning_rowset(&self, schema: &Schema, table: &str) -> RowSet {
        let mut rowset = RowSet {
            columns: schema
                .columns
                .iter()
                .map(|column| ColumnMeta {
                    table: Some(schema.name.clone()),
                    alias: None,
                    name: column.name.clone(),
                    output_name: column.name.clone(),
                    collation: column.collation.clone(),
                    hidden: false,
                })
                .collect(),
            rows: Vec::new(),
        };
        self.append_hidden_rowid_columns(
            &mut rowset.columns,
            schema,
            table,
            None,
            !schema.without_rowid,
        );
        rowset
    }

    fn execute_insert_many_do_nothing_returning(
        &self,
        transaction_id: TransactionId,
        table: &str,
        target: Option<&[String]>,
        rows: Vec<Row>,
        returning: &[SelectItem],
    ) -> Result<Vec<Row>> {
        let schema = self.require_schema(transaction_id, table)?;
        let indexes = self.storage.list_all_indexes(transaction_id, table)?;
        let rowset = self.returning_rowset(&schema, table);
        let mut returned = Vec::new();
        let mut inserted_count = 0_i64;
        let mut last_insert_rowid = None;

        for values in rows {
            let row = self.normalize_insert_row_input(&schema, values)?;
            let row = self.populate_generated_columns(&schema, row)?;
            self.validate_foreign_key_references(transaction_id, &schema, &row)?;
            let row_id = match self.storage.insert_row(transaction_id, table, row.clone()) {
                Ok(row_id) => row_id,
                Err(error) => {
                    let conflicts = self.classify_insert_conflicts(
                        transaction_id,
                        table,
                        &schema,
                        &indexes,
                        &row,
                    )?;
                    if conflicts.is_empty() {
                        return Err(error);
                    }
                    if self.do_nothing_target_matches(target, &conflicts) {
                        continue;
                    }
                    return Err(error);
                }
            };
            let row = self
                .storage
                .get_row(transaction_id, table, row_id)?
                .ok_or_else(|| DbError::storage("inserted row could not be read for RETURNING"))?;
            let row = self.append_hidden_rowid(row, row_id, &schema, !schema.without_rowid)?;
            returned.push(self.project_row(Some(transaction_id), &rowset, &row, returning)?);
            last_insert_rowid = Some(
                i64::try_from(row_id.0).map_err(|_| DbError::storage("rowid exceeds i64 range"))?,
            );
            inserted_count += 1;
        }

        if let Some(rowid) = last_insert_rowid {
            self.last_insert_rowid.set(rowid);
        }
        self.record_changes(inserted_count);

        Ok(returned)
    }

    fn insert_prepared_rows_do_nothing(
        &self,
        transaction_id: TransactionId,
        table: &str,
        target: Option<&[String]>,
        rows: Vec<Row>,
    ) -> Result<i64> {
        let schema = self.require_schema(transaction_id, table)?;
        let indexes = self.storage.list_all_indexes(transaction_id, table)?;
        let mut inserted_count = 0_i64;
        let mut last_insert_rowid = None;

        for values in rows {
            let row = self.normalize_insert_row_input(&schema, values)?;
            let row = self.populate_generated_columns(&schema, row)?;
            self.validate_foreign_key_references(transaction_id, &schema, &row)?;
            let row_id = match self.storage.insert_row(transaction_id, table, row.clone()) {
                Ok(row_id) => row_id,
                Err(error) => {
                    let conflicts = self.classify_insert_conflicts(
                        transaction_id,
                        table,
                        &schema,
                        &indexes,
                        &row,
                    )?;
                    if conflicts.is_empty() {
                        return Err(error);
                    }
                    if self.do_nothing_target_matches(target, &conflicts) {
                        continue;
                    }
                    return Err(error);
                }
            };

            last_insert_rowid = Some(
                i64::try_from(row_id.0).map_err(|_| DbError::storage("rowid exceeds i64 range"))?,
            );
            inserted_count += 1;
        }

        if let Some(rowid) = last_insert_rowid {
            self.last_insert_rowid.set(rowid);
        }

        Ok(inserted_count)
    }

    fn evaluate_insert_value_exprs(&self, values: &[ScalarExpr]) -> Result<Row> {
        let source = self.single_row_source_rowset();
        let row = source.rows.first().cloned().unwrap_or_default();
        values
            .iter()
            .map(|expr| self.evaluate_scalar_expr(&source, &row, expr))
            .collect()
    }

    fn populate_generated_columns(&self, schema: &Schema, mut row: Row) -> Result<Row> {
        for (index, column) in schema.columns.iter().enumerate() {
            let Some(expr_sql) = &column.generated_expr else {
                continue;
            };
            let expr = parse_scalar_sql_expression(expr_sql)?;
            let source = RowSet {
                columns: schema
                    .columns
                    .iter()
                    .map(|entry| ColumnMeta {
                        table: Some(schema.name.clone()),
                        alias: None,
                        name: entry.name.clone(),
                        output_name: entry.name.clone(),
                        collation: entry.collation.clone(),
                        hidden: false,
                    })
                    .collect(),
                rows: Vec::new(),
            };
            let value = self.evaluate_scalar_expr(&source, &row, &expr)?;
            row[index] = value;
        }
        Ok(row)
    }

    fn validate_create_table_foreign_key_metadata(
        &self,
        transaction_id: TransactionId,
        schema: &Schema,
    ) -> Result<()> {
        schema.validate_constraints_metadata()?;

        for foreign_key in schema.all_foreign_keys() {
            let parent_schema = if foreign_key.ref_table == schema.name {
                schema.clone()
            } else {
                self.require_schema(transaction_id, &foreign_key.ref_table)?
            };
            let _ = self.resolve_foreign_key_parent_columns(&parent_schema, &foreign_key)?;
        }

        Ok(())
    }

    fn validate_add_column_existing_rows(
        &self,
        transaction_id: TransactionId,
        table: &str,
        column: &ColumnDef,
    ) -> Result<()> {
        let mut updated_schema = self.require_schema(transaction_id, table)?;
        if updated_schema
            .columns
            .iter()
            .any(|entry| entry.name == column.name)
        {
            return Err(DbError::storage(format!(
                "column already exists on table {table}: {}",
                column.name
            )));
        }
        let default_value = column
            .default_value
            .as_ref()
            .map_or(Ok(Value::Null), |default| default.evaluate())?;
        updated_schema.columns.push(column.clone());
        updated_schema.validate_constraints_metadata()?;

        for (_, row) in self.storage.scan_rows(transaction_id, table)? {
            let mut candidate = row;
            candidate.push(default_value.clone());
            updated_schema.validate_row_values(&candidate)?;
            updated_schema.validate_check_constraints_with_like_mode(
                &candidate,
                self.storage.case_sensitive_like(),
            )?;
            self.validate_foreign_key_references(transaction_id, &updated_schema, &candidate)?;
        }

        Ok(())
    }

    fn validate_foreign_key_references(
        &self,
        transaction_id: TransactionId,
        schema: &Schema,
        row: &Row,
    ) -> Result<()> {
        if !self.foreign_keys.get() {
            return Ok(());
        }
        for foreign_key in schema.all_foreign_keys() {
            let child_values = foreign_key
                .child_columns()
                .iter()
                .map(|column| schema.value_for_column(row, column).cloned())
                .collect::<Result<Vec<_>>>()?;
            if child_values
                .iter()
                .any(|value| matches!(value, Value::Null))
            {
                continue;
            }

            let parent_schema = self.require_schema(transaction_id, &foreign_key.ref_table)?;
            let parent_column_names =
                self.resolve_foreign_key_parent_columns(&parent_schema, &foreign_key)?;
            let parent_columns = parent_column_names
                .iter()
                .map(|column| parent_schema.column_index(column))
                .collect::<Result<Vec<_>>>()?;
            let parent_rows = self
                .storage
                .scan_rows(transaction_id, &foreign_key.ref_table)?;
            let found = parent_rows.iter().any(|(_, parent_row)| {
                parent_columns.iter().zip(child_values.iter()).all(
                    |(parent_column, child_value)| {
                        parent_row.get(*parent_column) == Some(child_value)
                    },
                )
            });

            if !found {
                let parent_column_display = parent_column_names.join(", ");
                return Err(DbError::storage(format!(
                    "foreign key constraint failed: {} references {}({})",
                    foreign_key.rendered_child_columns(),
                    foreign_key.ref_table,
                    parent_column_display
                )));
            }
        }

        Ok(())
    }

    fn validate_no_foreign_key_dependents(
        &self,
        transaction_id: TransactionId,
        parent_table: &str,
        parent_row: &Row,
    ) -> Result<()> {
        if !self.foreign_keys.get() {
            return Ok(());
        }
        let parent_schema = self.require_schema(transaction_id, parent_table)?;
        for child_schema in self.storage.list_schemas(transaction_id)? {
            for foreign_key in child_schema
                .all_foreign_keys()
                .into_iter()
                .filter(|foreign_key| foreign_key.ref_table == parent_table)
            {
                let parent_column_names =
                    self.resolve_foreign_key_parent_columns(&parent_schema, &foreign_key)?;
                let parent_values = parent_column_names
                    .iter()
                    .map(|column| parent_schema.value_for_column(parent_row, column).cloned())
                    .collect::<Result<Vec<_>>>()?;
                if parent_values
                    .iter()
                    .any(|value| matches!(value, Value::Null))
                {
                    continue;
                }

                self.validate_no_foreign_key_dependents_for_key(
                    transaction_id,
                    &child_schema,
                    foreign_key.child_columns(),
                    parent_table,
                    &parent_column_names,
                    &parent_values,
                )?;
            }
        }

        Ok(())
    }

    fn validate_no_foreign_key_dependents_for_key(
        &self,
        transaction_id: TransactionId,
        child_schema: &Schema,
        child_columns: &[String],
        parent_table: &str,
        parent_columns: &[String],
        parent_values: &[Value],
    ) -> Result<()> {
        let child_rows = self.storage.scan_rows(transaction_id, &child_schema.name)?;
        for (_, child_row) in child_rows {
            let child_values = child_columns
                .iter()
                .map(|column| child_schema.value_for_column(&child_row, column).cloned())
                .collect::<Result<Vec<_>>>()?;
            if child_values
                .iter()
                .any(|value| matches!(value, Value::Null))
            {
                continue;
            }
            if child_values == parent_values {
                return Err(DbError::storage(format!(
                    "foreign key constraint failed: {}.{} references {}({})",
                    child_schema.name,
                    child_columns.join(", "),
                    parent_table,
                    parent_columns.join(", ")
                )));
            }
        }

        Ok(())
    }

    fn validate_update_parent_key_changes(
        &self,
        transaction_id: TransactionId,
        parent_table: &str,
        parent_schema: &Schema,
        pending_updates: &[(RowId, Row, Row)],
    ) -> Result<()> {
        if !self.foreign_keys.get() {
            return Ok(());
        }
        if pending_updates.is_empty() {
            return Ok(());
        }

        for child_schema in self.storage.list_schemas(transaction_id)? {
            for foreign_key in child_schema
                .all_foreign_keys()
                .into_iter()
                .filter(|foreign_key| foreign_key.ref_table == parent_table)
            {
                let parent_column_names =
                    self.resolve_foreign_key_parent_columns(parent_schema, &foreign_key)?;
                for (_, old_row, updated_row) in pending_updates {
                    let old_parent_values = parent_column_names
                        .iter()
                        .map(|column| parent_schema.value_for_column(old_row, column).cloned())
                        .collect::<Result<Vec<_>>>()?;
                    let updated_parent_values = parent_column_names
                        .iter()
                        .map(|column| parent_schema.value_for_column(updated_row, column).cloned())
                        .collect::<Result<Vec<_>>>()?;

                    if old_parent_values == updated_parent_values
                        || old_parent_values
                            .iter()
                            .any(|value| matches!(value, Value::Null))
                    {
                        continue;
                    }

                    self.validate_no_foreign_key_dependents_for_key(
                        transaction_id,
                        &child_schema,
                        foreign_key.child_columns(),
                        parent_table,
                        &parent_column_names,
                        &old_parent_values,
                    )?;
                }
            }
        }

        Ok(())
    }

    fn resolve_foreign_key_parent_columns<'schema>(
        &self,
        parent_schema: &'schema Schema,
        foreign_key: &'schema crate::common::types::ForeignKey,
    ) -> Result<Vec<String>> {
        if let Some(ref_columns) = foreign_key.referenced_columns() {
            if ref_columns.len() != foreign_key.child_columns().len() {
                return Err(DbError::storage(format!(
                    "foreign key column count mismatch for parent table {}",
                    parent_schema.name
                )));
            }
            for ref_column in ref_columns {
                let _ = parent_schema.column_index(ref_column)?;
            }
            return Ok(ref_columns.to_vec());
        }

        let primary_key_columns = parent_schema
            .columns
            .iter()
            .filter(|column| column.primary_key)
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();

        if primary_key_columns.is_empty() {
            return Err(DbError::storage(format!(
                "foreign key parent table {} has no primary key for REFERENCES shorthand",
                parent_schema.name
            )));
        }

        if primary_key_columns.len() != foreign_key.child_columns().len() {
            return Err(DbError::storage(format!(
                "foreign key parent table {} primary key column count {} does not match child column count {}",
                parent_schema.name,
                primary_key_columns.len(),
                foreign_key.child_columns().len()
            )));
        }

        Ok(primary_key_columns)
    }

    fn validate_update_primary_key_uniqueness(
        &self,
        schema: &Schema,
        final_rows: &[Row],
    ) -> Result<()> {
        let primary_key_columns = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.primary_key)
            .collect::<Vec<_>>();
        if primary_key_columns.is_empty() {
            return Ok(());
        }

        let mut seen = BTreeSet::new();
        for row in final_rows {
            let key = primary_key_columns
                .iter()
                .map(|(index, _)| row[*index].clone())
                .collect::<Vec<_>>();
            if key.iter().any(|value| matches!(value, Value::Null)) {
                continue;
            }
            if !seen.insert(key.clone()) {
                if primary_key_columns.len() == 1 {
                    let (column_index, column) = primary_key_columns[0];
                    return Err(DbError::storage(format!(
                        "duplicate primary key value for column '{}': {}",
                        column.name, row[column_index]
                    )));
                }
                let column_names = primary_key_columns
                    .iter()
                    .map(|(_, column)| column.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(DbError::storage(format!(
                    "duplicate primary key value for columns ({column_names}): {:?}",
                    key
                )));
            }
        }

        Ok(())
    }

    fn validate_update_unique_index_constraints(
        &self,
        schema: &Schema,
        indexes: &[IndexMeta],
        final_rows: &[Row],
    ) -> Result<()> {
        for index in indexes.iter().filter(|index| index.unique) {
            let mut seen = BTreeSet::new();
            for row in final_rows {
                if !self.row_matches_partial_index(schema, index, row)? {
                    continue;
                }
                let key = self.project_index_key(schema, index, row)?;
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

        Ok(())
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
                    self.storage.case_sensitive_like(),
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
            self.storage.case_sensitive_like(),
        )
    }

    fn replace_conflicting_rows_and_insert(
        &self,
        transaction_id: TransactionId,
        table: &str,
        schema: &Schema,
        row: Row,
    ) -> Result<RowId> {
        let conflicts = self.find_insert_conflicting_rows(transaction_id, table, schema, &row)?;
        if conflicts.is_empty() {
            return Err(DbError::storage(
                "INSERT OR REPLACE found no conflicting rows to replace",
            ));
        }

        for (_, existing_row) in &conflicts {
            self.validate_no_foreign_key_dependents(transaction_id, table, existing_row)?;
        }

        for (row_id, _) in &conflicts {
            self.storage.delete_row(transaction_id, table, *row_id)?;
        }

        self.storage.insert_row(transaction_id, table, row)
    }

    fn classify_insert_conflicts(
        &self,
        transaction_id: TransactionId,
        table: &str,
        schema: &Schema,
        indexes: &[IndexMeta],
        candidate_row: &Row,
    ) -> Result<Vec<InsertConflictTarget>> {
        let existing_rows = self.storage.scan_rows(transaction_id, table)?;
        let mut conflicts = Vec::new();

        let primary_key_columns = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.primary_key)
            .collect::<Vec<_>>();
        if !primary_key_columns.is_empty()
            && !primary_key_columns
                .iter()
                .any(|(index, _)| matches!(candidate_row[*index], Value::Null))
        {
            let matches_primary_key = existing_rows.iter().any(|(_, existing_row)| {
                primary_key_columns
                    .iter()
                    .all(|(index, _)| existing_row.get(*index) == Some(&candidate_row[*index]))
            });
            if matches_primary_key {
                conflicts.push(InsertConflictTarget::PrimaryKey(
                    primary_key_columns
                        .iter()
                        .map(|(_, column)| column.name.clone())
                        .collect(),
                ));
            }
        }

        for index in indexes.iter().filter(|index| index.unique) {
            if !self.row_matches_partial_index(schema, index, candidate_row)? {
                continue;
            }
            let candidate_key = self.project_index_key(schema, index, candidate_row)?;
            if !index.enforces_unique_key(&candidate_key) {
                continue;
            }
            let matches_unique = existing_rows.iter().any(|(_, existing_row)| {
                if !self
                    .row_matches_partial_index(schema, index, existing_row)
                    .unwrap_or(false)
                {
                    return false;
                }
                self.project_index_key(schema, index, existing_row)
                    .map(|existing_key| existing_key == candidate_key)
                    .unwrap_or(false)
            });
            if matches_unique {
                conflicts.push(InsertConflictTarget::UniqueIndex(index.columns.clone()));
            }
        }

        Ok(conflicts)
    }

    fn do_nothing_target_matches(
        &self,
        target: Option<&[String]>,
        conflicts: &[InsertConflictTarget],
    ) -> bool {
        match target {
            None => !conflicts.is_empty(),
            Some(target) => conflicts.iter().any(|conflict| match conflict {
                InsertConflictTarget::PrimaryKey(columns)
                | InsertConflictTarget::UniqueIndex(columns) => columns == target,
            }),
        }
    }

    fn find_insert_conflicting_rows(
        &self,
        transaction_id: TransactionId,
        table: &str,
        schema: &Schema,
        candidate_row: &Row,
    ) -> Result<Vec<(RowId, Row)>> {
        let existing_rows = self.storage.scan_rows(transaction_id, table)?;
        let indexes = self.storage.list_all_indexes(transaction_id, table)?;
        let mut conflicts = BTreeMap::new();

        let primary_key_columns = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.primary_key)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if !primary_key_columns.is_empty()
            && !primary_key_columns
                .iter()
                .any(|index| matches!(candidate_row[*index], Value::Null))
        {
            for (row_id, existing_row) in &existing_rows {
                if primary_key_columns
                    .iter()
                    .all(|index| existing_row.get(*index) == Some(&candidate_row[*index]))
                {
                    conflicts.insert(*row_id, existing_row.clone());
                }
            }
        }

        for index in indexes.iter().filter(|index| index.unique) {
            if !self.row_matches_partial_index(schema, index, candidate_row)? {
                continue;
            }
            let candidate_key = self.project_index_key(schema, index, candidate_row)?;
            if !index.enforces_unique_key(&candidate_key) {
                continue;
            }
            for (row_id, existing_row) in &existing_rows {
                if !self.row_matches_partial_index(schema, index, existing_row)? {
                    continue;
                }
                let existing_key = self.project_index_key(schema, index, existing_row)?;
                if existing_key == candidate_key {
                    conflicts.insert(*row_id, existing_row.clone());
                }
            }
        }

        Ok(conflicts.into_iter().collect())
    }

    fn all_indexes(&self, transaction_id: TransactionId, table: &str) -> Result<Vec<IndexMeta>> {
        self.storage.list_all_indexes(transaction_id, table)
    }

    fn schema_from_ctas_rowset(name: &str, rowset: &RowSet) -> Result<Schema> {
        let mut seen = BTreeSet::new();
        let columns = rowset
            .columns
            .iter()
            .map(|column| {
                if column.output_name.is_empty() {
                    return Err(DbError::storage(
                        "CREATE TABLE AS SELECT produced an empty column name",
                    ));
                }
                let column_name = Self::deduplicate_ctas_column_name(&column.output_name, &mut seen);
                Ok(ColumnDef::new(column_name, ColumnType::Any))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Schema::new(name, columns))
    }

    fn deduplicate_ctas_column_name(name: &str, seen: &mut BTreeSet<String>) -> String {
        if seen.insert(name.to_string()) {
            return name.to_string();
        }

        let base_name = sqlite_ctas_column_dedup_base(name);
        let mut suffix = 1_usize;
        loop {
            let candidate = format!("{base_name}:{suffix}");
            if seen.insert(candidate.clone()) {
                return candidate;
            }
            suffix += 1;
        }
    }

    fn execute_query_plan(&self, transaction_id: TransactionId, plan: Plan) -> Result<RowSet> {
        self.execute_query_plan_with_outer(transaction_id, plan, None)
    }

    fn execute_query_plan_with_outer(
        &self,
        transaction_id: TransactionId,
        plan: Plan,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<RowSet> {
        match plan {
            Plan::Values { rows } => self.execute_values_plan(transaction_id, rows),
            Plan::SeqScan {
                table,
                table_alias,
                columns,
                filter,
                order_by,
                limit,
                offset,
                distinct,
            }
            | Plan::ForcedSeqScan {
                table,
                table_alias,
                columns,
                filter,
                order_by,
                limit,
                offset,
                distinct,
            } => {
                let source = self.scan_table_rowset(
                    transaction_id,
                    &table,
                    table_alias.as_deref(),
                    filter.as_ref(),
                    outer,
                )?;
                self.finish_projection(
                    transaction_id,
                    source,
                    &columns,
                    &order_by,
                    limit,
                    offset,
                    distinct,
                )
            }
            Plan::IndexScan {
                table,
                table_alias,
                columns,
                index,
                mode,
                key_prefix,
                range,
                filter,
                order_by,
                limit,
                offset,
                distinct,
            } => {
                let row_ids = self.scan_index_spec(
                    transaction_id,
                    &table,
                    &IndexScanSpec {
                        index,
                        mode,
                        key_prefix,
                        range,
                    },
                )?;
                let source = self.rowset_from_row_ids(
                    transaction_id,
                    &table,
                    table_alias.as_deref(),
                    &row_ids,
                    filter.as_ref(),
                    outer,
                )?;
                self.finish_projection(
                    transaction_id,
                    source,
                    &columns,
                    &order_by,
                    limit,
                    offset,
                    distinct,
                )
            }
            Plan::IndexUnion {
                table,
                table_alias,
                columns,
                scans,
                filter,
                order_by,
                limit,
                offset,
                distinct,
            } => {
                let mut row_ids = BTreeSet::new();
                for scan in &scans {
                    row_ids.extend(self.scan_index_spec(transaction_id, &table, scan)?);
                }
                let row_ids = row_ids.into_iter().collect::<Vec<_>>();
                let source = self.rowset_from_row_ids(
                    transaction_id,
                    &table,
                    table_alias.as_deref(),
                    &row_ids,
                    filter.as_ref(),
                    outer,
                )?;
                self.finish_projection(
                    transaction_id,
                    source,
                    &columns,
                    &order_by,
                    limit,
                    offset,
                    distinct,
                )
            }
            Plan::Union {
                left,
                right,
                operator,
                all,
                order_by,
                limit,
                offset,
            } => {
                let left = self.execute_query_plan_with_outer(transaction_id, *left, outer)?;
                let right = self.execute_query_plan_with_outer(transaction_id, *right, outer)?;
                if left.columns.len() != right.columns.len() {
                    return Err(DbError::plan("compound query output width mismatch"));
                }

                let rows = match operator {
                    CompoundOperator::Union | CompoundOperator::UnionAll => {
                        let mut rows = left.rows;
                        rows.extend(right.rows);
                        if all {
                            rows
                        } else {
                            Self::deduplicate_rows(rows)
                        }
                    }
                    CompoundOperator::Intersect => {
                        let right_rows = right.rows.into_iter().collect::<HashSet<_>>();
                        Self::deduplicate_rows(
                            left.rows
                                .into_iter()
                                .filter(|row| right_rows.contains(row))
                                .collect(),
                        )
                    }
                    CompoundOperator::Except => {
                        let right_rows = right.rows.into_iter().collect::<HashSet<_>>();
                        Self::deduplicate_rows(
                            left.rows
                                .into_iter()
                                .filter(|row| !right_rows.contains(row))
                                .collect(),
                        )
                    }
                };

                self.sort_and_limit_rows(
                    RowSet {
                        columns: left.columns,
                        rows,
                    },
                    &order_by,
                    limit,
                    offset,
                )
            }
            Plan::NestedLoopJoin {
                source,
                joins,
                columns,
                filter,
                order_by,
                limit,
                offset,
                distinct,
            } => {
                let source = self.execute_join_plan(
                    transaction_id,
                    *source,
                    &joins,
                    filter.as_ref(),
                    outer,
                )?;
                self.finish_projection(
                    transaction_id,
                    source,
                    &columns,
                    &order_by,
                    limit,
                    offset,
                    distinct,
                )
            }
            Plan::Aggregate {
                source,
                columns,
                group_by,
                having,
                order_by,
                limit,
                offset,
            } => {
                let source = self.execute_query_plan_with_outer(transaction_id, *source, outer)?;
                self.execute_aggregate(
                    transaction_id,
                    source,
                    AggregateExecOptions {
                        columns: &columns,
                        group_by: &group_by,
                        having: having.as_ref(),
                        order_by: &order_by,
                        limit,
                        offset,
                    },
                )
            }
            Plan::DerivedSource {
                source,
                alias,
                output_columns,
                columns,
                filter,
                order_by,
                limit,
                offset,
                distinct,
            } => {
                let mut source =
                    self.execute_query_plan_with_outer(transaction_id, *source, outer)?;
                if source.columns.len() != output_columns.len() {
                    return Err(DbError::plan("derived source output width mismatch"));
                }

                let source_qualifier = if alias.is_empty() {
                    None
                } else {
                    Some(alias.clone())
                };
                source.columns = output_columns
                    .iter()
                    .map(|name| ColumnMeta {
                        table: source_qualifier.clone(),
                        alias: source_qualifier.clone(),
                        name: name.clone(),
                        output_name: name.clone(),
                        collation: None,
                        hidden: false,
                    })
                    .collect();

                if let Some(filter) = filter.as_ref() {
                    let rows = source
                        .rows
                        .iter()
                        .filter_map(|row| {
                            match self.matches_filter(
                                transaction_id,
                                &source,
                                row,
                                Some(filter),
                                outer,
                            ) {
                                Ok(true) => Some(Ok(row.clone())),
                                Ok(false) => None,
                                Err(error) => Some(Err(error)),
                            }
                        })
                        .collect::<Result<Vec<_>>>()?;
                    source.rows = rows;
                }

                self.finish_projection(
                    transaction_id,
                    source,
                    &columns,
                    &order_by,
                    limit,
                    offset,
                    distinct,
                )
            }
            Plan::ExplainQueryPlan { plan } => Ok(self.explain_query_plan(&plan)),
            Plan::NoOp | Plan::BeginTxn { .. } | Plan::CommitTxn | Plan::RollbackTxn => Err(
                DbError::txn("transaction control plan reached data execution path"),
            ),
            other => Err(DbError::plan(format!("unexpected query plan: {other:?}"))),
        }
    }

    fn explain_query_plan(&self, plan: &Plan) -> RowSet {
        let mut rows = Vec::new();
        Self::collect_plan_rows(plan, 0, &mut rows);
        RowSet {
            columns: vec![
                ColumnMeta {
                    table: None,
                    alias: None,
                    name: "operation".to_string(),
                    output_name: "operation".to_string(),
                    collation: None,
                    hidden: false,
                },
                ColumnMeta {
                    table: None,
                    alias: None,
                    name: "detail".to_string(),
                    output_name: "detail".to_string(),
                    collation: None,
                    hidden: false,
                },
            ],
            rows,
        }
    }

    fn collect_plan_rows(plan: &Plan, depth: usize, rows: &mut Vec<Row>) {
        let indent = "  ".repeat(depth);
        match plan {
            Plan::SeqScan { table, .. } | Plan::ForcedSeqScan { table, .. } => rows.push(vec![
                Value::from(format!("{indent}SeqScan")),
                Value::from(format!("table={table}")),
            ]),
            Plan::IndexScan {
                table,
                index,
                mode,
                key_prefix,
                range,
                ..
            } => rows.push(vec![
                Value::from(format!("{indent}IndexScan")),
                Value::from(format!(
                    "table={table} index={index} mode={} key_prefix={}{}",
                    Self::format_index_scan_mode(*mode),
                    Self::format_values(key_prefix),
                    range
                        .as_ref()
                        .map(|range| format!(" range={}", Self::format_index_range(range)))
                        .unwrap_or_default()
                )),
            ]),
            Plan::IndexUnion { table, scans, .. } => {
                rows.push(vec![
                    Value::from(format!("{indent}IndexUnion")),
                    Value::from(format!("table={table} scans={}", scans.len())),
                ]);
                for scan in scans {
                    rows.push(vec![
                        Value::from(format!("{indent}  IndexScan")),
                        Value::from(format!(
                            "index={} mode={} key_prefix={}{}",
                            scan.index,
                            Self::format_index_scan_mode(scan.mode),
                            Self::format_values(&scan.key_prefix),
                            scan.range
                                .as_ref()
                                .map(|range| format!(" range={}", Self::format_index_range(range)))
                                .unwrap_or_default()
                        )),
                    ]);
                }
            }
            Plan::Union {
                left, right, all, ..
            } => {
                rows.push(vec![
                    Value::from(format!("{indent}Union")),
                    Value::from(format!("all={all}")),
                ]);
                Self::collect_plan_rows(left, depth + 1, rows);
                Self::collect_plan_rows(right, depth + 1, rows);
            }
            Plan::NestedLoopJoin { joins, .. } => rows.push(vec![
                Value::from(format!("{indent}NestedLoopJoin")),
                Value::from(format!("joins={}", joins.len())),
            ]),
            Plan::Aggregate { source, .. } => {
                rows.push(vec![
                    Value::from(format!("{indent}Aggregate")),
                    Value::from("grouped"),
                ]);
                Self::collect_plan_rows(source, depth + 1, rows);
            }
            Plan::DerivedSource {
                alias,
                source,
                output_columns,
                ..
            } => {
                rows.push(vec![
                    Value::from(format!("{indent}DerivedSource")),
                    Value::from(format!(
                        "alias={alias} output_columns={}",
                        output_columns.join(",")
                    )),
                ]);
                Self::collect_plan_rows(source, depth + 1, rows);
            }
            Plan::ExplainQueryPlan { plan } => Self::collect_plan_rows(plan, depth, rows),
            other => rows.push(vec![
                Value::from(format!("{indent}{}", Self::plan_name(other))),
                Value::from(""),
            ]),
        }
    }

    fn plan_name(plan: &Plan) -> &'static str {
        match plan {
            Plan::CreateTable { .. } => "CreateTable",
            Plan::CreateTableAs { .. } => "CreateTableAs",
            Plan::CreateIndex { .. } => "CreateIndex",
            Plan::DropTable { .. } => "DropTable",
            Plan::DropIndex { .. } => "DropIndex",
            Plan::AlterTable { .. } => "AlterTable",
            Plan::Insert { .. } => "Insert",
            Plan::InsertReturning { .. } => "InsertReturning",
            Plan::InsertUpsert { .. } => "InsertUpsert",
            Plan::InsertUpsertReturning { .. } => "InsertUpsertReturning",
            Plan::InsertMany { .. } => "InsertMany",
            Plan::InsertManyUpsert { .. } => "InsertManyUpsert",
            Plan::InsertManyUpsertReturning { .. } => "InsertManyUpsertReturning",
            Plan::InsertManyReturning { .. } => "InsertManyReturning",
            Plan::InsertDoNothing { .. } => "InsertDoNothing",
            Plan::InsertDoNothingReturning { .. } => "InsertDoNothingReturning",
            Plan::InsertManyDoNothing { .. } => "InsertManyDoNothing",
            Plan::InsertManyDoNothingReturning { .. } => "InsertManyDoNothingReturning",
            Plan::InsertExpr { .. } => "InsertExpr",
            Plan::InsertExprReturning { .. } => "InsertExprReturning",
            Plan::InsertExprUpsert { .. } => "InsertExprUpsert",
            Plan::InsertExprUpsertReturning { .. } => "InsertExprUpsertReturning",
            Plan::InsertManyExpr { .. } => "InsertManyExpr",
            Plan::InsertManyExprUpsert { .. } => "InsertManyExprUpsert",
            Plan::InsertManyExprUpsertReturning { .. } => "InsertManyExprUpsertReturning",
            Plan::InsertManyExprReturning { .. } => "InsertManyExprReturning",
            Plan::InsertExprDoNothing { .. } => "InsertExprDoNothing",
            Plan::InsertExprDoNothingReturning { .. } => "InsertExprDoNothingReturning",
            Plan::InsertManyExprDoNothing { .. } => "InsertManyExprDoNothing",
            Plan::InsertManyExprDoNothingReturning { .. } => "InsertManyExprDoNothingReturning",
            Plan::InsertSelect { .. } => "InsertSelect",
            Plan::InsertSelectReturning { .. } => "InsertSelectReturning",
            Plan::InsertSelectUpsert { .. } => "InsertSelectUpsert",
            Plan::InsertSelectUpsertReturning { .. } => "InsertSelectUpsertReturning",
            Plan::InsertSelectDoNothing { .. } => "InsertSelectDoNothing",
            Plan::InsertSelectDoNothingReturning { .. } => "InsertSelectDoNothingReturning",
            Plan::Delete { .. } => "Delete",
            Plan::DeleteLimited { .. } => "DeleteLimited",
            Plan::DeleteReturning { .. } => "DeleteReturning",
            Plan::DeleteReturningLimited { .. } => "DeleteReturningLimited",
            Plan::Update { .. } => "Update",
            Plan::UpdateLimited { .. } => "UpdateLimited",
            Plan::UpdateReturning { .. } => "UpdateReturning",
            Plan::UpdateReturningLimited { .. } => "UpdateReturningLimited",
            Plan::SeqScan { .. } => "SeqScan",
            Plan::ForcedSeqScan { .. } => "SeqScan",
            Plan::IndexScan { .. } => "IndexScan",
            Plan::IndexUnion { .. } => "IndexUnion",
            Plan::Union { .. } => "Union",
            Plan::NestedLoopJoin { .. } => "NestedLoopJoin",
            Plan::Aggregate { .. } => "Aggregate",
            Plan::Values { .. } => "Values",
            Plan::DerivedSource { .. } => "DerivedSource",
            Plan::ExplainQueryPlan { .. } => "ExplainQueryPlan",
            Plan::PragmaTableInfo { .. } => "PragmaTableInfo",
            Plan::PragmaTableXInfo { .. } => "PragmaTableXInfo",
            Plan::PragmaTableList { .. } => "PragmaTableList",
            Plan::PragmaIndexList { .. } => "PragmaIndexList",
            Plan::PragmaIndexInfo { .. } => "PragmaIndexInfo",
            Plan::PragmaIndexXInfo { .. } => "PragmaIndexXInfo",
            Plan::PragmaForeignKeyList { .. } => "PragmaForeignKeyList",
            Plan::PragmaForeignKeyCheck { .. } => "PragmaForeignKeyCheck",
            Plan::PragmaForeignKeys => "PragmaForeignKeys",
            Plan::SetPragmaForeignKeys { .. } => "SetPragmaForeignKeys",
            Plan::PragmaReadUncommitted => "PragmaReadUncommitted",
            Plan::SetPragmaReadUncommitted { .. } => "SetPragmaReadUncommitted",
            Plan::PragmaQueryOnly => "PragmaQueryOnly",
            Plan::SetPragmaQueryOnly { .. } => "SetPragmaQueryOnly",
            Plan::PragmaRecursiveTriggers => "PragmaRecursiveTriggers",
            Plan::SetPragmaRecursiveTriggers { .. } => "SetPragmaRecursiveTriggers",
            Plan::PragmaTrustedSchema => "PragmaTrustedSchema",
            Plan::SetPragmaTrustedSchema { .. } => "SetPragmaTrustedSchema",
            Plan::PragmaIgnoreCheckConstraints => "PragmaIgnoreCheckConstraints",
            Plan::SetPragmaIgnoreCheckConstraints { .. } => "SetPragmaIgnoreCheckConstraints",
            Plan::PragmaEncoding => "PragmaEncoding",
            Plan::PragmaCollationList => "PragmaCollationList",
            Plan::PragmaDataVersion => "PragmaDataVersion",
            Plan::PragmaQuickCheck => "PragmaQuickCheck",
            Plan::PragmaIntegrityCheck => "PragmaIntegrityCheck",
            Plan::PragmaFunctionList => "PragmaFunctionList",
            Plan::PragmaCompileOptions => "PragmaCompileOptions",
            Plan::PragmaJournalMode => "PragmaJournalMode",
            Plan::PragmaSynchronous => "PragmaSynchronous",
            Plan::PragmaCacheSize => "PragmaCacheSize",
            Plan::SetPragmaCacheSize { .. } => "SetPragmaCacheSize",
            Plan::PragmaTempStore => "PragmaTempStore",
            Plan::PragmaLockingMode => "PragmaLockingMode",
            Plan::PragmaBusyTimeout => "PragmaBusyTimeout",
            Plan::SetPragmaBusyTimeout { .. } => "SetPragmaBusyTimeout",
            Plan::PragmaThreads => "PragmaThreads",
            Plan::SetPragmaThreads { .. } => "SetPragmaThreads",
            Plan::PragmaCaseSensitiveLike => "PragmaCaseSensitiveLike",
            Plan::SetPragmaCaseSensitiveLike { .. } => "SetPragmaCaseSensitiveLike",
            Plan::PragmaReverseUnorderedSelects => "PragmaReverseUnorderedSelects",
            Plan::SetPragmaReverseUnorderedSelects { .. } => "SetPragmaReverseUnorderedSelects",
            Plan::PragmaDatabaseList => "PragmaDatabaseList",
            Plan::PragmaPageSize => "PragmaPageSize",
            Plan::PragmaPageCount => "PragmaPageCount",
            Plan::PragmaFreelistCount => "PragmaFreelistCount",
            Plan::PragmaUserVersion => "PragmaUserVersion",
            Plan::SetPragmaUserVersion { .. } => "SetPragmaUserVersion",
            Plan::PragmaApplicationId => "PragmaApplicationId",
            Plan::SetPragmaApplicationId { .. } => "SetPragmaApplicationId",
            Plan::PragmaSchemaVersion => "PragmaSchemaVersion",
            Plan::SetPragmaSchemaVersion { .. } => "SetPragmaSchemaVersion",
            Plan::NoOp => "NoOp",
            Plan::BeginTxn { .. } => "BeginTxn",
            Plan::CommitTxn => "CommitTxn",
            Plan::RollbackTxn => "RollbackTxn",
        }
    }

    fn format_values(values: &[Value]) -> String {
        format!(
            "[{}]",
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    fn format_index_range(range: &crate::sql::plan::IndexRange) -> String {
        format!(
            "{}:{}..{}",
            range.column,
            range
                .lower
                .as_ref()
                .map(|bound| format!("{:?} {}", bound.op, bound.value))
                .unwrap_or_else(|| "unbounded".to_string()),
            range
                .upper
                .as_ref()
                .map(|bound| format!("{:?} {}", bound.op, bound.value))
                .unwrap_or_else(|| "unbounded".to_string())
        )
    }

    fn format_index_scan_mode(mode: IndexScanMode) -> &'static str {
        match mode {
            IndexScanMode::Lookup => "lookup",
            IndexScanMode::Prefix => "prefix",
            IndexScanMode::Range => "range",
        }
    }

    fn scan_table_rowset(
        &self,
        transaction_id: TransactionId,
        table: &str,
        table_alias: Option<&str>,
        filter: Option<&Expr>,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<RowSet> {
        if table == SINGLE_ROW_SOURCE_TABLE {
            let source = self.single_row_source_rowset();
            let rows = source
                .rows
                .iter()
                .filter_map(|row| {
                    match self.matches_filter(transaction_id, &source, row, filter, outer) {
                        Ok(true) => Some(Ok(row.clone())),
                        Ok(false) => None,
                        Err(error) => Some(Err(error)),
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(RowSet {
                columns: source.columns,
                rows,
            });
        }
        if matches!(table, "sqlite_master" | "sqlite_schema") {
            let source = self.sqlite_catalog_rowset(transaction_id, table, table_alias)?;
            let rows = source
                .rows
                .iter()
                .filter_map(|row| {
                    match self.matches_filter(transaction_id, &source, row, filter, outer) {
                        Ok(true) => Some(Ok(row.clone())),
                        Ok(false) => None,
                        Err(error) => Some(Err(error)),
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(RowSet {
                columns: source.columns,
                rows,
            });
        }
        let schema = self.require_schema(transaction_id, table)?;
        let exposes_rowid = !schema.without_rowid;
        let mut rowset = RowSet {
            columns: schema
                .columns
                .iter()
                .map(|column| ColumnMeta {
                    table: Some(table.to_string()),
                    alias: table_alias.map(str::to_string),
                    name: column.name.clone(),
                    output_name: column.name.clone(),
                    collation: column.collation.clone(),
                    hidden: false,
                })
                .collect(),
            rows: Vec::new(),
        };
        self.append_hidden_rowid_columns(
            &mut rowset.columns,
            &schema,
            table,
            table_alias,
            exposes_rowid,
        );

        for (row_id, row) in self.storage.scan_rows(transaction_id, table)? {
            let row = self.append_hidden_rowid(row, row_id, &schema, exposes_rowid)?;
            if self.matches_filter(transaction_id, &rowset, &row, filter, outer)? {
                rowset.rows.push(row);
            }
        }

        Ok(rowset)
    }

    fn single_row_source_rowset(&self) -> RowSet {
        RowSet {
            columns: Vec::new(),
            rows: vec![Vec::new()],
        }
    }

    fn execute_values_plan(
        &self,
        transaction_id: TransactionId,
        rows: Vec<Vec<ScalarExpr>>,
    ) -> Result<RowSet> {
        let width = rows.first().map_or(0, Vec::len);
        if rows.iter().any(|row| row.len() != width) {
            return Err(DbError::plan(
                "VALUES rows must all have the same number of columns",
            ));
        }
        let source = self.single_row_source_rowset();
        let source_row = source.rows.first().cloned().unwrap_or_default();
        let rows = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|expr| {
                        self.evaluate_scalar_expr_in_context(
                            Some(transaction_id),
                            &source,
                            &source_row,
                            None,
                            expr,
                        )
                    })
                    .collect::<Result<Row>>()
            })
            .collect::<Result<Vec<_>>>()?;
        let columns = (1..=width)
            .map(|index| {
                let name = format!("column{index}");
                ColumnMeta {
                    table: None,
                    alias: None,
                    name: name.clone(),
                    output_name: name,
                    collation: None,
                    hidden: false,
                }
            })
            .collect();
        Ok(RowSet { columns, rows })
    }

    fn sqlite_catalog_rowset(
        &self,
        transaction_id: TransactionId,
        table: &str,
        table_alias: Option<&str>,
    ) -> Result<RowSet> {
        let mut rows = Vec::new();
        let schemas = self.storage.list_schemas(transaction_id)?;
        for (offset, schema) in schemas.iter().enumerate() {
            let rootpage = i64::try_from(offset + 2)
                .map_err(|_| DbError::storage("sqlite catalog rootpage overflow"))?;
            rows.push(vec![
                Value::from("table"),
                Value::from(schema.name.clone()),
                Value::from(schema.name.clone()),
                Value::Integer(rootpage),
                Value::from(self.render_catalog_create_table(schema)),
            ]);
        }

        let mut index_rootpage = i64::try_from(schemas.len() + 2)
            .map_err(|_| DbError::storage("sqlite catalog rootpage overflow"))?;
        for schema in &schemas {
            for index in self
                .storage
                .list_all_indexes(transaction_id, &schema.name)?
            {
                rows.push(vec![
                    Value::from("index"),
                    Value::from(index.name.clone()),
                    Value::from(schema.name.clone()),
                    Value::Integer(index_rootpage),
                    Value::from(self.render_catalog_create_index(&schema.name, &index)),
                ]);
                index_rootpage += 1;
            }
        }

        Ok(RowSet {
            columns: ["type", "name", "tbl_name", "rootpage", "sql"]
                .into_iter()
                .map(|name| ColumnMeta {
                    table: Some(table.to_string()),
                    alias: table_alias.map(str::to_string),
                    name: name.to_string(),
                    output_name: name.to_string(),
                    collation: None,
                    hidden: false,
                })
                .collect(),
            rows,
        })
    }

    fn execute_pragma_table_info(
        &self,
        transaction_id: TransactionId,
        table: &str,
        include_hidden: bool,
    ) -> Result<Vec<Row>> {
        let Some(schema) = self.storage.get_schema(transaction_id, table)? else {
            return Ok(Vec::new());
        };

        schema
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let cid = i64::try_from(index)
                    .map_err(|_| DbError::storage("PRAGMA table_info column index overflow"))?;
                let pk = if column.primary_key {
                    if let Some(primary_key) = &schema.primary_key_constraint {
                        primary_key
                            .columns
                            .iter()
                            .position(|name| name == &column.name)
                            .and_then(|position| i64::try_from(position + 1).ok())
                            .unwrap_or(1)
                    } else {
                        1
                    }
                } else {
                    0
                };
                let mut row = vec![
                    Value::Integer(cid),
                    Value::from(column.name.as_str()),
                    Value::from(column.column_type.name()),
                    Value::Integer(if column.nullable || column.primary_key {
                        0
                    } else {
                        1
                    }),
                    column
                        .default_value
                        .as_ref()
                        .map(render_pragma_default_value)
                        .map(Value::Text)
                        .unwrap_or(Value::Null),
                    Value::Integer(pk),
                ];
                if include_hidden {
                    let hidden = if column.generated_expr.is_none() {
                        0
                    } else if column.generated_stored {
                        3
                    } else {
                        2
                    };
                    row.push(Value::Integer(hidden));
                }
                Ok(row)
            })
            .collect()
    }

    fn execute_pragma_table_list(
        &self,
        transaction_id: TransactionId,
        table_filter: Option<&str>,
        schema_filter: Option<&str>,
    ) -> Result<Vec<Row>> {
        let mut schemas = self.storage.list_schemas(transaction_id)?;
        schemas.sort_by(|left, right| left.name.cmp(&right.name));

        let mut rows = Vec::new();
        if schema_filter.is_none_or(|schema| schema.eq_ignore_ascii_case("main")) {
            for schema in schemas {
                if table_filter.is_some_and(|filter| !schema.name.eq_ignore_ascii_case(filter)) {
                    continue;
                }
                rows.push(vec![
                    Value::from("main"),
                    Value::from(schema.name),
                    Value::from("table"),
                    Value::Integer(i64::try_from(schema.columns.len()).map_err(|_| {
                        DbError::storage("PRAGMA table_list column count overflow")
                    })?),
                    Value::Integer(if schema.without_rowid { 1 } else { 0 }),
                    Value::Integer(if schema.strict { 1 } else { 0 }),
                ]);
            }
        }

        for (schema_name, table_name) in [("main", "sqlite_schema"), ("temp", "sqlite_temp_schema")]
        {
            if schema_filter.is_some_and(|filter| !schema_name.eq_ignore_ascii_case(filter)) {
                continue;
            }
            if table_filter.is_some_and(|filter| !table_name.eq_ignore_ascii_case(filter)) {
                continue;
            }
            rows.push(vec![
                Value::from(schema_name),
                Value::from(table_name),
                Value::from("table"),
                Value::Integer(5),
                Value::Integer(0),
                Value::Integer(0),
            ]);
        }

        Ok(rows)
    }

    fn execute_pragma_index_list(
        &self,
        transaction_id: TransactionId,
        table: &str,
    ) -> Result<Vec<Row>> {
        if self.storage.get_schema(transaction_id, table)?.is_none() {
            return Ok(Vec::new());
        }

        self.storage
            .list_all_indexes(transaction_id, table)?
            .into_iter()
            .enumerate()
            .map(|(index, meta)| {
                let seq = i64::try_from(index)
                    .map_err(|_| DbError::storage("PRAGMA index_list sequence overflow"))?;
                let origin = if meta.name.starts_with("sqlite_autoindex_") {
                    "u"
                } else {
                    "c"
                };
                Ok(vec![
                    Value::Integer(seq),
                    Value::from(meta.name.as_str()),
                    Value::Integer(if meta.unique { 1 } else { 0 }),
                    Value::from(origin),
                    Value::Integer(if meta.predicate.is_some() { 1 } else { 0 }),
                ])
            })
            .collect()
    }

    fn execute_pragma_index_info(
        &self,
        transaction_id: TransactionId,
        index_name: &str,
    ) -> Result<Vec<Row>> {
        for schema in self.storage.list_schemas(transaction_id)? {
            for index in self
                .storage
                .list_all_indexes(transaction_id, &schema.name)?
            {
                if index.name != index_name {
                    continue;
                }

                return index
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(seqno, column)| {
                        let seqno = i64::try_from(seqno)
                            .map_err(|_| DbError::storage("PRAGMA index_info sequence overflow"))?;
                        let cid = schema
                            .columns
                            .iter()
                            .position(|entry| entry.name == *column)
                            .and_then(|position| i64::try_from(position).ok())
                            .unwrap_or(-2);
                        let name = if cid >= 0 {
                            Value::from(column.as_str())
                        } else {
                            Value::Null
                        };
                        Ok(vec![Value::Integer(seqno), Value::Integer(cid), name])
                    })
                    .collect();
            }
        }

        Ok(Vec::new())
    }

    fn execute_pragma_index_xinfo(
        &self,
        transaction_id: TransactionId,
        index_name: &str,
    ) -> Result<Vec<Row>> {
        for schema in self.storage.list_schemas(transaction_id)? {
            for index in self
                .storage
                .list_all_indexes(transaction_id, &schema.name)?
            {
                if index.name != index_name {
                    continue;
                }

                let mut rows = Vec::with_capacity(index.columns.len() + 1);
                for (seqno, column) in index.columns.iter().enumerate() {
                    let seqno = i64::try_from(seqno)
                        .map_err(|_| DbError::storage("PRAGMA index_xinfo sequence overflow"))?;
                    let decorated = index
                        .decorated_columns
                        .as_ref()
                        .and_then(|columns| columns.get(usize::try_from(seqno).ok()?))
                        .map(String::as_str)
                        .unwrap_or(column.as_str());
                    let column_name = index_term_column_name(column)
                        .or_else(|| index_term_column_name(decorated));
                    let cid = schema
                        .columns
                        .iter()
                        .position(|entry| column_name.as_deref() == Some(entry.name.as_str()))
                        .and_then(|position| i64::try_from(position).ok())
                        .unwrap_or(-2);
                    let name = if cid >= 0 {
                        Value::from(column_name.as_deref().unwrap_or(column.as_str()))
                    } else {
                        Value::Null
                    };
                    let desc = if decorated_index_term_is_desc(decorated) {
                        1
                    } else {
                        0
                    };
                    let collation = decorated_index_term_collation(decorated)
                        .or_else(|| {
                            (cid >= 0)
                                .then(|| {
                                    schema.columns[usize::try_from(cid).ok()?].collation.clone()
                                })
                                .flatten()
                        })
                        .unwrap_or_else(|| "BINARY".to_string());
                    rows.push(vec![
                        Value::Integer(seqno),
                        Value::Integer(cid),
                        name,
                        Value::Integer(desc),
                        Value::from(collation.as_str()),
                        Value::Integer(1),
                    ]);
                }

                let rowid_seqno = i64::try_from(rows.len())
                    .map_err(|_| DbError::storage("PRAGMA index_xinfo sequence overflow"))?;
                rows.push(vec![
                    Value::Integer(rowid_seqno),
                    Value::Integer(-1),
                    Value::Null,
                    Value::Integer(0),
                    Value::from("BINARY"),
                    Value::Integer(0),
                ]);
                return Ok(rows);
            }
        }

        Ok(Vec::new())
    }

    fn execute_pragma_foreign_key_list(
        &self,
        transaction_id: TransactionId,
        table: &str,
    ) -> Result<Vec<Row>> {
        let Some(schema) = self.storage.get_schema(transaction_id, table)? else {
            return Ok(Vec::new());
        };

        let mut rows = Vec::new();
        let foreign_keys = schema.all_foreign_keys();
        for (id, foreign_key) in foreign_keys.iter().enumerate() {
            let id = i64::try_from(id)
                .map_err(|_| DbError::storage("PRAGMA foreign_key_list id overflow"))?;
            let parent_columns = if let Some(columns) = foreign_key.referenced_columns() {
                columns.to_vec()
            } else {
                let parent_schema = self
                    .storage
                    .get_schema(transaction_id, &foreign_key.ref_table)?
                    .ok_or_else(|| {
                        DbError::storage(format!("unknown parent table: {}", foreign_key.ref_table))
                    })?;
                parent_schema
                    .columns
                    .iter()
                    .filter(|column| column.primary_key)
                    .map(|column| column.name.clone())
                    .collect::<Vec<_>>()
            };

            for (seq, child_column) in foreign_key.child_columns().iter().enumerate() {
                let seq = i64::try_from(seq)
                    .map_err(|_| DbError::storage("PRAGMA foreign_key_list sequence overflow"))?;
                let parent_column = parent_columns.get(usize::try_from(seq).unwrap_or(usize::MAX));
                rows.push(vec![
                    Value::Integer(id),
                    Value::Integer(seq),
                    Value::from(foreign_key.ref_table.as_str()),
                    Value::from(child_column.as_str()),
                    parent_column
                        .map(|column| Value::from(column.as_str()))
                        .unwrap_or(Value::Null),
                    Value::from(foreign_key.on_update.as_deref().unwrap_or("NO ACTION")),
                    Value::from(foreign_key.on_delete.as_deref().unwrap_or("NO ACTION")),
                    Value::from("NONE"),
                ]);
            }
        }

        Ok(rows)
    }

    fn execute_pragma_foreign_key_check(
        &self,
        transaction_id: TransactionId,
        table_filter: Option<&str>,
    ) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        let mut schemas = self.storage.list_schemas(transaction_id)?;
        schemas.sort_by(|left, right| left.name.cmp(&right.name));

        for schema in schemas {
            if table_filter.is_some_and(|table| !schema.name.eq_ignore_ascii_case(table)) {
                continue;
            }
            let foreign_keys = schema.all_foreign_keys();
            if foreign_keys.is_empty() {
                continue;
            }

            for (row_id, child_row) in self.storage.scan_rows(transaction_id, &schema.name)? {
                for (foreign_key_id, foreign_key) in foreign_keys.iter().enumerate() {
                    let child_values = foreign_key
                        .child_columns()
                        .iter()
                        .map(|column| schema.value_for_column(&child_row, column).cloned())
                        .collect::<Result<Vec<_>>>()?;
                    if child_values
                        .iter()
                        .any(|value| matches!(value, Value::Null))
                    {
                        continue;
                    }

                    let parent_schema =
                        self.require_schema(transaction_id, &foreign_key.ref_table)?;
                    let parent_column_names =
                        self.resolve_foreign_key_parent_columns(&parent_schema, foreign_key)?;
                    let parent_columns = parent_column_names
                        .iter()
                        .map(|column| parent_schema.column_index(column))
                        .collect::<Result<Vec<_>>>()?;
                    let parent_rows = self
                        .storage
                        .scan_rows(transaction_id, &foreign_key.ref_table)?;
                    let found = parent_rows.iter().any(|(_, parent_row)| {
                        parent_columns.iter().zip(child_values.iter()).all(
                            |(parent_column, child_value)| {
                                parent_row.get(*parent_column) == Some(child_value)
                            },
                        )
                    });

                    if !found {
                        let rowid = i64::try_from(row_id.0)
                            .map_err(|_| DbError::storage("foreign_key_check rowid overflow"))?;
                        let foreign_key_id = i64::try_from(foreign_key_id).map_err(|_| {
                            DbError::storage("foreign_key_check foreign key id overflow")
                        })?;
                        rows.push(vec![
                            Value::from(schema.name.as_str()),
                            Value::Integer(rowid),
                            Value::from(foreign_key.ref_table.as_str()),
                            Value::Integer(foreign_key_id),
                        ]);
                    }
                }
            }
        }

        Ok(rows)
    }

    fn render_catalog_create_table(&self, schema: &Schema) -> String {
        let mut definitions = schema
            .columns
            .iter()
            .map(|column| {
                let mut rendered =
                    if matches!(column.column_type, crate::common::types::ColumnType::Any) {
                        column.name.clone()
                    } else {
                        format!("{} {}", column.name, column.column_type.name())
                    };
                if column.primary_key {
                    rendered.push_str(" PRIMARY KEY");
                }
                rendered
            })
            .collect::<Vec<_>>();
        if let Some(primary_key) = &schema.primary_key_constraint {
            definitions.push(format!(
                "PRIMARY KEY({})",
                primary_key.rendered_columns().join(", ")
            ));
        }
        let strict = if schema.strict { " STRICT" } else { "" };
        let without_rowid = if schema.without_rowid {
            " WITHOUT ROWID"
        } else {
            ""
        };
        format!(
            "CREATE TABLE {} ({}){}{}",
            schema.name,
            definitions.join(", "),
            strict,
            without_rowid
        )
    }

    fn render_catalog_create_index(&self, table: &str, index: &IndexMeta) -> String {
        let unique = if index.unique { " UNIQUE" } else { "" };
        let predicate = index
            .predicate
            .as_ref()
            .map(|predicate| format!(" WHERE {predicate}"))
            .unwrap_or_default();
        format!(
            "CREATE{} INDEX {} ON {}({}){}",
            unique,
            index.name,
            table,
            index.rendered_columns().join(", "),
            predicate
        )
    }

    fn rowset_from_row_ids(
        &self,
        transaction_id: TransactionId,
        table: &str,
        table_alias: Option<&str>,
        row_ids: &[RowId],
        filter: Option<&Expr>,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<RowSet> {
        let schema = self.require_schema(transaction_id, table)?;
        let exposes_rowid = !schema.without_rowid;
        let mut rowset = RowSet {
            columns: schema
                .columns
                .iter()
                .map(|column| ColumnMeta {
                    table: Some(table.to_string()),
                    alias: table_alias.map(str::to_string),
                    name: column.name.clone(),
                    output_name: column.name.clone(),
                    collation: column.collation.clone(),
                    hidden: false,
                })
                .collect(),
            rows: Vec::new(),
        };
        self.append_hidden_rowid_columns(
            &mut rowset.columns,
            &schema,
            table,
            table_alias,
            exposes_rowid,
        );

        for row_id in row_ids {
            if let Some(row) = self
                .storage
                .get_row(transaction_id, table, *row_id)?
                .map(|row| self.append_hidden_rowid(row, *row_id, &schema, exposes_rowid))
                .transpose()?
                && self.matches_filter(transaction_id, &rowset, &row, filter, outer)?
            {
                rowset.rows.push(row);
            }
        }

        Ok(rowset)
    }

    fn execute_join_plan(
        &self,
        transaction_id: TransactionId,
        source: Plan,
        joins: &[JoinPlan],
        filter: Option<&Expr>,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<RowSet> {
        let mut current = self.execute_query_plan_with_outer(transaction_id, source, outer)?;

        for join in joins {
            let right =
                self.execute_query_plan_with_outer(transaction_id, (*join.source).clone(), outer)?;
            let right_columns = right
                .columns
                .iter()
                .cloned()
                .map(|mut column| {
                    if join
                        .using_columns
                        .iter()
                        .any(|using_column| using_column == &column.name)
                    {
                        column.hidden = true;
                    }
                    column
                })
                .collect::<Vec<_>>();
            let joined_columns = current
                .columns
                .iter()
                .cloned()
                .chain(right_columns.iter().cloned())
                .collect::<Vec<_>>();
            let left_width = current.columns.len();
            let right_width = right.columns.len();
            let mut joined_rows = Vec::new();
            let mut matched_right = vec![false; right.rows.len()];

            for left_row in &current.rows {
                let mut matched = false;
                for (right_index, right_row) in right.rows.iter().enumerate() {
                    let mut row = left_row.clone();
                    row.extend(right_row.clone());
                    let candidate = RowSet {
                        columns: joined_columns.clone(),
                        rows: Vec::new(),
                    };
                    if self.matches_filter(
                        transaction_id,
                        &candidate,
                        &row,
                        Some(&join.on),
                        outer,
                    )? {
                        joined_rows.push(row);
                        matched = true;
                        matched_right[right_index] = true;
                    }
                }
                if !matched && matches!(join.kind, JoinKind::Left | JoinKind::Full) {
                    let mut row = left_row.clone();
                    row.extend(std::iter::repeat_n(Value::Null, right_width));
                    joined_rows.push(row);
                }
            }
            if matches!(join.kind, JoinKind::Right | JoinKind::Full) {
                for (right_row, matched) in right.rows.iter().zip(matched_right) {
                    if !matched {
                        let mut row =
                            std::iter::repeat_n(Value::Null, left_width).collect::<Vec<_>>();
                        for using_column in &join.using_columns {
                            if let (Some(left_index), Some(right_index)) = (
                                current.columns.iter().position(|column| {
                                    !column.hidden && column.name == *using_column
                                }),
                                right
                                    .columns
                                    .iter()
                                    .position(|column| column.name == *using_column),
                            ) && let Some(value) = right_row.get(right_index)
                            {
                                row[left_index] = value.clone();
                            }
                        }
                        row.extend(right_row.clone());
                        joined_rows.push(row);
                    }
                }
            }

            current = RowSet {
                columns: joined_columns,
                rows: joined_rows,
            };
        }

        if let Some(filter) = filter {
            let rows = current
                .rows
                .iter()
                .filter_map(|row| {
                    match self.matches_filter(transaction_id, &current, row, Some(filter), outer) {
                        Ok(true) => Some(Ok(row.clone())),
                        Ok(false) => None,
                        Err(error) => Some(Err(error)),
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            current.rows = rows;
        }

        Ok(current)
    }

    fn finish_projection(
        &self,
        transaction_id: TransactionId,
        source: RowSet,
        columns: &[SelectItem],
        order_by: &[OrderBy],
        limit: Option<usize>,
        offset: Option<usize>,
        distinct: bool,
    ) -> Result<RowSet> {
        if columns.len() == 1 && matches!(columns.first(), Some(SelectItem::Wildcard)) {
            let visible_indexes = source
                .columns
                .iter()
                .enumerate()
                .filter_map(|(index, column)| (!column.hidden).then_some(index))
                .collect::<Vec<_>>();
            let source = RowSet {
                columns: source
                    .columns
                    .into_iter()
                    .filter(|column| !column.hidden)
                    .collect(),
                rows: source
                    .rows
                    .into_iter()
                    .map(|row| {
                        visible_indexes
                            .iter()
                            .filter_map(|index| row.get(*index).cloned())
                            .collect()
                    })
                    .collect(),
            };
            let mut rowset = self.sort_and_limit_rows(source, order_by, limit, offset)?;
            if distinct {
                rowset.rows = Self::deduplicate_rows(rowset.rows);
                Self::apply_limit_offset(&mut rowset.rows, limit, offset);
            }
            return Ok(rowset);
        }

        let projected_columns = self.projected_columns(&source.columns, columns)?;
        let entries = source
            .rows
            .iter()
            .map(|row| {
                Ok((
                    self.project_row(Some(transaction_id), &source, row, columns)?,
                    row.clone(),
                ))
            })
            .collect::<Result<Vec<(Row, Row)>>>()?;

        let mut rows = entries
            .into_iter()
            .map(|(projected, full)| {
                let sort_key = self.order_sort_key(
                    Some(transaction_id),
                    &projected_columns,
                    &projected,
                    &source.columns,
                    &full,
                    order_by,
                )?;
                Ok((sort_key, projected))
            })
            .collect::<Result<Vec<_>>>()?;
        if !order_by.is_empty() {
            rows.sort_by(|(left_key, _), (right_key, _)| {
                self.compare_order_keys(left_key, right_key, order_by)
            });
        }
        let mut rows = rows
            .into_iter()
            .map(|(_, projected)| projected)
            .collect::<Vec<_>>();
        if order_by.is_empty() && self.reverse_unordered_selects.get() {
            rows.reverse();
        }
        if distinct {
            rows = Self::deduplicate_rows(rows);
        }
        Self::apply_limit_offset(&mut rows, limit, offset);

        Ok(RowSet {
            columns: projected_columns,
            rows,
        })
    }

    fn deduplicate_rows(rows: Vec<Row>) -> Vec<Row> {
        let mut seen = HashSet::new();
        let mut deduped = Vec::with_capacity(rows.len());
        for row in rows {
            let key = row.iter().map(Self::value_dedup_key).collect::<Vec<_>>();
            if seen.insert(key) {
                deduped.push(row);
            }
        }
        deduped
    }

    fn value_dedup_key(value: &Value) -> (u8, String) {
        match value {
            Value::Null => (0, String::new()),
            Value::Boolean(b) => (1, b.to_string()),
            Value::Integer(i) => (2, i.to_string()),
            Value::Real(f) => (3, format!("{:016X}", f.to_bits())),
            Value::Blob(bytes) => (
                4,
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02X}"))
                    .collect::<String>(),
            ),
            Value::Text(t) => (5, t.clone()),
        }
    }

    fn execute_aggregate(
        &self,
        transaction_id: TransactionId,
        source: RowSet,
        options: AggregateExecOptions<'_>,
    ) -> Result<RowSet> {
        let AggregateExecOptions {
            columns,
            group_by,
            having,
            order_by,
            limit,
            offset,
        } = options;
        let output_columns = self.aggregate_output_columns(columns);
        let visible_width = output_columns.len();
        let aggregate_calls = self.collect_aggregate_calls(columns, having, order_by);
        let (aggregate_eval_columns, hidden_group_indexes, hidden_aggregate_indexes) =
            self.aggregate_eval_columns(&output_columns, group_by, &aggregate_calls);
        let has_aggregates = !aggregate_calls.is_empty();

        let mut groups = BTreeMap::<Vec<Value>, Vec<AggregateState>>::new();
        if source.rows.is_empty() && group_by.is_empty() {
            groups.insert(Vec::new(), self.initial_aggregate_states(&aggregate_calls));
        }

        for row in &source.rows {
            let key = group_by
                .iter()
                .map(|expr| self.evaluate_scalar_expr(&source, row, expr))
                .collect::<Result<Vec<_>>>()?;
            let states = groups
                .entry(key)
                .or_insert_with(|| self.initial_aggregate_states(&aggregate_calls));
            self.update_aggregate_states(transaction_id, &source, row, &aggregate_calls, states)?;
        }

        let mut rows = Vec::new();
        for (key, states) in groups {
            if !has_aggregates && !group_by.is_empty() && key.is_empty() {
                continue;
            }
            let aggregate_values = self.aggregate_state_values(&states)?;
            let aggregate_row = self.finalize_aggregate_row(
                &source,
                columns,
                group_by,
                &key,
                &aggregate_calls,
                &aggregate_values,
            )?;
            let aggregate_eval_row = self.aggregate_eval_row(
                aggregate_row.clone(),
                &key,
                &hidden_group_indexes,
                &aggregate_values,
                &hidden_aggregate_indexes,
            );
            if let Some(having) = having {
                let aggregate_rowset = RowSet {
                    columns: aggregate_eval_columns.clone(),
                    rows: Vec::new(),
                };
                if !self.matches_filter(
                    transaction_id,
                    &aggregate_rowset,
                    &aggregate_eval_row,
                    Some(having),
                    None,
                )? {
                    continue;
                }
            }
            rows.push(aggregate_eval_row);
        }

        let mut output = RowSet {
            columns: output_columns,
            rows,
        };

        if !order_by.is_empty() {
            let output_source = RowSet {
                columns: aggregate_eval_columns,
                rows: Vec::new(),
            };
            output.rows.sort_by(|left, right| {
                self.compare_aggregate_order(&output_source, order_by, left, right)
            });
        }
        Self::apply_limit_offset(&mut output.rows, limit, offset);
        output
            .rows
            .iter_mut()
            .for_each(|row| row.truncate(visible_width));
        Ok(output)
    }

    fn initial_aggregate_states(&self, calls: &[AggregateCall]) -> Vec<AggregateState> {
        calls
            .iter()
            .map(|call| self.initial_aggregate_state(call))
            .collect()
    }

    fn initial_aggregate_state(&self, call: &AggregateCall) -> AggregateState {
        let AggregateCall { func, arg, .. } = call;
        match (func, arg) {
            (AggregateFunc::Count, AggregateArg::Expr { distinct: true, .. }) => {
                AggregateState::CountDistinct(BTreeSet::new())
            }
            (AggregateFunc::Count, _) => AggregateState::Count(0),
            (AggregateFunc::Sum, AggregateArg::Expr { distinct: true, .. }) => {
                AggregateState::SumDistinct(BTreeSet::new())
            }
            (AggregateFunc::Sum, _) => AggregateState::Sum {
                int_sum: 0,
                real_sum: 0.0,
                seen: false,
                saw_real: false,
            },
            (AggregateFunc::Avg, AggregateArg::Expr { distinct: true, .. }) => {
                AggregateState::AvgDistinct(BTreeSet::new())
            }
            (AggregateFunc::Avg, _) => AggregateState::Avg {
                int_sum: 0,
                real_sum: 0.0,
                count: 0,
                saw_real: false,
            },
            (AggregateFunc::Total, AggregateArg::Expr { distinct: true, .. }) => {
                AggregateState::TotalDistinct(BTreeSet::new())
            }
            (AggregateFunc::Total, _) => AggregateState::Total(0.0),
            (AggregateFunc::Median, _) => AggregateState::Median(Vec::new()),
            (
                AggregateFunc::Percentile
                | AggregateFunc::PercentileCont
                | AggregateFunc::PercentileDisc,
                _,
            ) => AggregateState::Percentile {
                values: Vec::new(),
                fraction: None,
                discrete: matches!(func, AggregateFunc::PercentileDisc),
            },
            (
                AggregateFunc::GroupConcat,
                AggregateArg::GroupConcat {
                    distinct: false, ..
                },
            ) => {
                let AggregateArg::GroupConcat { order_by, .. } = arg else {
                    unreachable!("matched GROUP_CONCAT argument");
                };
                AggregateState::GroupConcat {
                    value: None,
                    ordered: Vec::new(),
                    order_by: order_by.clone(),
                }
            }
            (AggregateFunc::GroupConcat, AggregateArg::GroupConcat { distinct: true, .. }) => {
                let AggregateArg::GroupConcat { order_by, .. } = arg else {
                    unreachable!("matched GROUP_CONCAT argument");
                };
                AggregateState::GroupConcatDistinct {
                    values: Vec::new(),
                    seen: BTreeSet::new(),
                    ordered: Vec::new(),
                    order_by: order_by.clone(),
                }
            }
            (AggregateFunc::GroupConcat, _) => AggregateState::GroupConcat {
                value: None,
                ordered: Vec::new(),
                order_by: Vec::new(),
            },
            (AggregateFunc::JsonGroupArray, AggregateArg::Expr { order_by, .. }) => {
                AggregateState::JsonGroupArray {
                    values: Vec::new(),
                    ordered: Vec::new(),
                    order_by: order_by.clone(),
                }
            }
            (AggregateFunc::JsonGroupArray, _) => AggregateState::JsonGroupArray {
                values: Vec::new(),
                ordered: Vec::new(),
                order_by: Vec::new(),
            },
            (AggregateFunc::JsonGroupObject, AggregateArg::JsonGroupObject { order_by, .. }) => {
                AggregateState::JsonGroupObject {
                    fields: Vec::new(),
                    ordered: Vec::new(),
                    order_by: order_by.clone(),
                }
            }
            (AggregateFunc::JsonGroupObject, _) => AggregateState::JsonGroupObject {
                fields: Vec::new(),
                ordered: Vec::new(),
                order_by: Vec::new(),
            },
            (AggregateFunc::Min, _) => AggregateState::Min(None),
            (AggregateFunc::Max, _) => AggregateState::Max(None),
        }
    }

    fn update_aggregate_states(
        &self,
        transaction_id: TransactionId,
        source: &RowSet,
        row: &Row,
        calls: &[AggregateCall],
        states: &mut [AggregateState],
    ) -> Result<()> {
        for (state, call) in states.iter_mut().zip(calls) {
            let AggregateCall { func, arg, filter } = call;

            if let Some(filter) = filter.as_ref()
                && !self.matches_filter(transaction_id, source, row, Some(filter), None)?
            {
                continue;
            }

            match (state, func, arg) {
                (AggregateState::Count(count), AggregateFunc::Count, AggregateArg::Wildcard) => {
                    *count += 1;
                }
                (
                    AggregateState::Count(count),
                    AggregateFunc::Count,
                    AggregateArg::Expr { expr, .. },
                ) if self.evaluate_scalar_expr(source, row, expr)? != Value::Null => {
                    *count += 1;
                }
                (
                    AggregateState::CountDistinct(values),
                    AggregateFunc::Count,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    if value != Value::Null {
                        values.insert(value);
                    }
                }
                (AggregateState::Count(_), AggregateFunc::Count, AggregateArg::Expr { .. }) => {}
                (
                    AggregateState::Sum {
                        int_sum,
                        real_sum,
                        seen,
                        saw_real,
                    },
                    AggregateFunc::Sum,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    match Self::coerce_aggregate_numeric_value(&value) {
                        Some(Value::Integer(value)) => {
                            *int_sum += i128::from(value);
                            *real_sum += value as f64;
                            *seen = true;
                        }
                        Some(Value::Real(value)) => {
                            *real_sum += value;
                            *seen = true;
                            *saw_real = true;
                        }
                        Some(_) => {
                            unreachable!("aggregate numeric coercion only returns numeric values")
                        }
                        None => {}
                    }
                }
                (
                    AggregateState::SumDistinct(values),
                    AggregateFunc::Sum,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    if Self::coerce_aggregate_numeric_value(&value).is_some() {
                        values.insert(value);
                    }
                }
                (
                    AggregateState::Avg {
                        int_sum,
                        real_sum,
                        count,
                        saw_real,
                    },
                    AggregateFunc::Avg,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    match Self::coerce_aggregate_numeric_value(&value) {
                        Some(Value::Integer(value)) => {
                            *int_sum += i128::from(value);
                            *real_sum += value as f64;
                            *count += 1;
                        }
                        Some(Value::Real(value)) => {
                            *real_sum += value;
                            *count += 1;
                            *saw_real = true;
                        }
                        Some(_) => {
                            unreachable!("aggregate numeric coercion only returns numeric values")
                        }
                        None => {}
                    }
                }
                (
                    AggregateState::AvgDistinct(values),
                    AggregateFunc::Avg,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    if Self::coerce_aggregate_numeric_value(&value).is_some() {
                        values.insert(value);
                    }
                }
                (
                    AggregateState::Total(total),
                    AggregateFunc::Total,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    match Self::coerce_aggregate_numeric_value(&value) {
                        Some(Value::Integer(value)) => *total += value as f64,
                        Some(Value::Real(value)) => *total += value,
                        Some(_) => {
                            unreachable!("aggregate numeric coercion only returns numeric values")
                        }
                        None => {}
                    }
                }
                (
                    AggregateState::TotalDistinct(values),
                    AggregateFunc::Total,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    if Self::coerce_aggregate_numeric_value(&value).is_some() {
                        values.insert(value);
                    }
                }
                (
                    AggregateState::Median(values),
                    AggregateFunc::Median,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    if !matches!(value, Value::Null) {
                        values.push(Self::median_numeric_value(&value)?);
                    }
                }
                (
                    AggregateState::Percentile {
                        values, fraction, ..
                    },
                    func @ (AggregateFunc::Percentile
                    | AggregateFunc::PercentileCont
                    | AggregateFunc::PercentileDisc),
                    AggregateArg::Percentile {
                        expr,
                        fraction: fraction_expr,
                        ..
                    },
                ) => {
                    let fraction_value = self.evaluate_scalar_expr(source, row, fraction_expr)?;
                    let next_fraction = Self::percentile_fraction_value(func, &fraction_value)?;
                    match fraction {
                        Some(existing) if (*existing - next_fraction).abs() > f64::EPSILON => {
                            return Err(DbError::plan(format!(
                                "the fraction argument to {}() is not the same for all input rows",
                                Self::aggregate_function_name(*func)
                            )));
                        }
                        Some(_) => {}
                        None => *fraction = Some(next_fraction),
                    }

                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    if !matches!(value, Value::Null) {
                        values.push(Self::percentile_numeric_value(
                            Self::aggregate_function_name(*func),
                            &value,
                        )?);
                    }
                }
                (
                    AggregateState::GroupConcat {
                        value: current,
                        ordered,
                        ..
                    },
                    AggregateFunc::GroupConcat,
                    AggregateArg::GroupConcat {
                        expr,
                        separator: separator_expr,
                        order_by,
                        ..
                    },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    if value != Value::Null {
                        let text = Self::coerce_text_like_value(&value);
                        let separator = if let Some(separator_expr) = separator_expr {
                            match self.evaluate_scalar_expr(source, row, separator_expr)? {
                                Value::Null => String::new(),
                                value => Self::coerce_text_like_value(&value),
                            }
                        } else {
                            ",".to_string()
                        };
                        if order_by.is_empty() {
                            if let Some(current) = current {
                                current.push_str(&separator);
                                current.push_str(&text);
                            } else {
                                *current = Some(text);
                            }
                        } else {
                            let sort_key = self.order_sort_key(
                                Some(transaction_id),
                                &source.columns,
                                row,
                                &source.columns,
                                row,
                                order_by,
                            )?;
                            ordered.push((sort_key, text, separator));
                        }
                    }
                }
                (
                    AggregateState::GroupConcatDistinct {
                        values,
                        seen,
                        ordered,
                        ..
                    },
                    AggregateFunc::GroupConcat,
                    AggregateArg::GroupConcat { expr, order_by, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    if value != Value::Null {
                        let text = Self::coerce_text_like_value(&value);
                        if seen.insert(value) {
                            if order_by.is_empty() {
                                values.push(text);
                            } else {
                                let sort_key = self.order_sort_key(
                                    Some(transaction_id),
                                    &source.columns,
                                    row,
                                    &source.columns,
                                    row,
                                    order_by,
                                )?;
                                ordered.push((sort_key, text));
                            }
                        }
                    }
                }
                (
                    AggregateState::JsonGroupArray {
                        values,
                        ordered,
                        order_by,
                    },
                    AggregateFunc::JsonGroupArray,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    let json_value = Self::sql_value_to_json(&value)?;
                    let json = serde_json::to_string(&json_value).map_err(|error| {
                        DbError::plan(format!("failed to render JSON aggregate value: {error}"))
                    })?;
                    if order_by.is_empty() {
                        values.push(json);
                    } else {
                        let sort_key = self.order_sort_key(
                            Some(transaction_id),
                            &source.columns,
                            row,
                            &source.columns,
                            row,
                            order_by,
                        )?;
                        ordered.push((sort_key, json));
                    }
                }
                (
                    AggregateState::JsonGroupObject {
                        fields,
                        ordered,
                        order_by,
                    },
                    AggregateFunc::JsonGroupObject,
                    AggregateArg::JsonGroupObject { key, value, .. },
                ) => {
                    let key = self.evaluate_scalar_expr(source, row, key)?;
                    if key != Value::Null {
                        let key = Self::coerce_text_like_value(&key);
                        let key = serde_json::to_string(&key).map_err(|error| {
                            DbError::plan(format!("failed to render JSON object key: {error}"))
                        })?;
                        let value = self.evaluate_scalar_expr(source, row, value)?;
                        let value = Self::sql_value_to_json(&value)?;
                        let value = serde_json::to_string(&value).map_err(|error| {
                            DbError::plan(format!("failed to render JSON aggregate value: {error}"))
                        })?;
                        let field = format!("{key}:{value}");
                        if order_by.is_empty() {
                            fields.push(field);
                        } else {
                            let sort_key = self.order_sort_key(
                                Some(transaction_id),
                                &source.columns,
                                row,
                                &source.columns,
                                row,
                                order_by,
                            )?;
                            ordered.push((sort_key, field));
                        }
                    }
                }
                (
                    AggregateState::Min(current),
                    AggregateFunc::Min,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    if value != Value::Null {
                        match current.as_ref() {
                            None => *current = Some(value),
                            Some(existing)
                                if self.compare(existing, &value)? == Some(Ordering::Greater) =>
                            {
                                *current = Some(value)
                            }
                            _ => {}
                        }
                    }
                }
                (
                    AggregateState::Max(current),
                    AggregateFunc::Max,
                    AggregateArg::Expr { expr, .. },
                ) => {
                    let value = self.evaluate_scalar_expr(source, row, expr)?;
                    if value != Value::Null {
                        match current.as_ref() {
                            None => *current = Some(value),
                            Some(existing)
                                if self.compare(existing, &value)? == Some(Ordering::Less) =>
                            {
                                *current = Some(value)
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn finalize_aggregate_row(
        &self,
        source: &RowSet,
        columns: &[SelectItem],
        group_by: &[ScalarExpr],
        key: &[Value],
        calls: &[AggregateCall],
        aggregate_values: &[Value],
    ) -> Result<Row> {
        let mut row = Vec::with_capacity(columns.len());
        for item in columns {
            match item {
                SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
                    let key_index = self.group_key_index(source, group_by, name)?;
                    row.push(key[key_index].clone());
                }
                SelectItem::Expr { expr, .. } => {
                    if Self::scalar_expr_has_aggregate(expr) {
                        let (eval_source, eval_row) =
                            self.aggregate_expr_eval_source(group_by, key, calls, aggregate_values);
                        row.push(self.evaluate_scalar_expr(&eval_source, &eval_row, expr)?);
                    } else {
                        let key_index = self.group_expr_index(source, group_by, expr)?;
                        row.push(key[key_index].clone());
                    }
                }
                SelectItem::Aggregate {
                    func, arg, filter, ..
                } => {
                    let call = AggregateCall {
                        func: *func,
                        arg: arg.clone(),
                        filter: filter.clone(),
                    };
                    let index = calls
                        .iter()
                        .position(|candidate| candidate == &call)
                        .ok_or_else(|| {
                            DbError::plan(format!(
                                "missing aggregate state for {}",
                                self.aggregate_call_name(&call)
                            ))
                        })?;
                    row.push(aggregate_values[index].clone());
                }
                SelectItem::Wildcard => {
                    return Err(DbError::plan(
                        "wildcard cannot be used with GROUP BY or aggregate projections",
                    ));
                }
            }
        }
        Ok(row)
    }

    fn aggregate_state_value(&self, state: &AggregateState) -> Result<Value> {
        Ok(match state {
            AggregateState::Count(count) => Value::Integer(*count),
            AggregateState::CountDistinct(values) => Value::Integer(
                i64::try_from(values.len()).map_err(|_| DbError::plan("COUNT overflowed i64"))?,
            ),
            AggregateState::Sum {
                int_sum,
                real_sum,
                seen,
                saw_real,
            } => {
                if *seen {
                    if *saw_real {
                        Value::Real(*real_sum)
                    } else {
                        Value::Integer(
                            i64::try_from(*int_sum)
                                .map_err(|_| DbError::plan("SUM overflowed i64"))?,
                        )
                    }
                } else {
                    Value::Null
                }
            }
            AggregateState::SumDistinct(values) => {
                if values.is_empty() {
                    Value::Null
                } else {
                    let mut int_sum = 0_i128;
                    let mut real_sum = 0.0;
                    let mut saw_real = false;
                    for value in values {
                        match Self::coerce_aggregate_numeric_value(value) {
                            Some(Value::Integer(value)) => {
                                int_sum += i128::from(value);
                                real_sum += value as f64;
                            }
                            Some(Value::Real(value)) => {
                                real_sum += value;
                                saw_real = true;
                            }
                            Some(_) => {
                                unreachable!(
                                    "aggregate numeric coercion only returns numeric values"
                                )
                            }
                            None => return Err(DbError::plan("SUM only supports numeric columns")),
                        }
                    }
                    if saw_real {
                        Value::Real(real_sum)
                    } else {
                        Value::Integer(
                            i64::try_from(int_sum)
                                .map_err(|_| DbError::plan("SUM overflowed i64"))?,
                        )
                    }
                }
            }
            AggregateState::Avg {
                real_sum, count, ..
            } => {
                if *count == 0 {
                    Value::Null
                } else {
                    Value::Real(*real_sum / (*count as f64))
                }
            }
            AggregateState::AvgDistinct(values) => {
                if values.is_empty() {
                    Value::Null
                } else {
                    let mut real_sum = 0.0;
                    for value in values {
                        match Self::coerce_aggregate_numeric_value(value) {
                            Some(Value::Integer(value)) => real_sum += value as f64,
                            Some(Value::Real(value)) => real_sum += value,
                            Some(_) => {
                                unreachable!(
                                    "aggregate numeric coercion only returns numeric values"
                                )
                            }
                            None => return Err(DbError::plan("AVG only supports numeric columns")),
                        }
                    }
                    Value::Real(real_sum / (values.len() as f64))
                }
            }
            AggregateState::Total(total) => Value::Real(*total),
            AggregateState::TotalDistinct(values) => {
                let mut total = 0.0;
                for value in values {
                    match Self::coerce_aggregate_numeric_value(value) {
                        Some(Value::Integer(value)) => total += value as f64,
                        Some(Value::Real(value)) => total += value,
                        Some(_) => {
                            unreachable!("aggregate numeric coercion only returns numeric values")
                        }
                        None => return Err(DbError::plan("TOTAL only supports numeric columns")),
                    }
                }
                Value::Real(total)
            }
            AggregateState::Median(values) => {
                if values.is_empty() {
                    Value::Null
                } else {
                    let mut values = values.clone();
                    values.sort_by(|left, right| left.total_cmp(right));
                    let mid = values.len() / 2;
                    if values.len() % 2 == 1 {
                        Value::Real(values[mid])
                    } else {
                        Value::Real((values[mid - 1] + values[mid]) / 2.0)
                    }
                }
            }
            AggregateState::Percentile {
                values,
                fraction,
                discrete,
            } => {
                if values.is_empty() {
                    Value::Null
                } else {
                    let fraction = fraction.unwrap_or(0.0);
                    let mut values = values.clone();
                    values.sort_by(|left, right| left.total_cmp(right));
                    if *discrete {
                        Value::Real(sqlite_percentile_disc(&values, fraction))
                    } else {
                        Value::Real(sqlite_percentile_cont(&values, fraction))
                    }
                }
            }
            AggregateState::GroupConcat {
                value,
                ordered,
                order_by,
            } => {
                if !ordered.is_empty() {
                    let mut entries = ordered.clone();
                    entries.sort_by(|(left_key, _, _), (right_key, _, _)| {
                        self.compare_order_keys(left_key, right_key, order_by)
                    });
                    let mut iter = entries.into_iter();
                    let Some((_key, first, _separator)) = iter.next() else {
                        return Ok(Value::Null);
                    };
                    let mut text = first;
                    for (_key, value, separator) in iter {
                        text.push_str(&separator);
                        text.push_str(&value);
                    }
                    Value::Text(text)
                } else {
                    value.clone().map_or(Value::Null, Value::Text)
                }
            }
            AggregateState::GroupConcatDistinct {
                values,
                ordered,
                order_by,
                ..
            } => {
                if !ordered.is_empty() {
                    let mut entries = ordered.clone();
                    entries.sort_by(|(left_key, _), (right_key, _)| {
                        self.compare_order_keys(left_key, right_key, order_by)
                    });
                    Value::Text(
                        entries
                            .into_iter()
                            .map(|(_key, value)| value)
                            .collect::<Vec<_>>()
                            .join(","),
                    )
                } else if values.is_empty() {
                    Value::Null
                } else {
                    Value::Text(values.join(","))
                }
            }
            AggregateState::JsonGroupArray {
                values,
                ordered,
                order_by,
            } => {
                if !ordered.is_empty() {
                    let mut entries = ordered.clone();
                    entries.sort_by(|(left_key, _), (right_key, _)| {
                        self.compare_order_keys(left_key, right_key, order_by)
                    });
                    Value::Text(format!(
                        "[{}]",
                        entries
                            .into_iter()
                            .map(|(_key, value)| value)
                            .collect::<Vec<_>>()
                            .join(",")
                    ))
                } else {
                    Value::Text(format!("[{}]", values.join(",")))
                }
            }
            AggregateState::JsonGroupObject {
                fields,
                ordered,
                order_by,
            } => {
                if !ordered.is_empty() {
                    let mut entries = ordered.clone();
                    entries.sort_by(|(left_key, _), (right_key, _)| {
                        self.compare_order_keys(left_key, right_key, order_by)
                    });
                    Value::Text(format!(
                        "{{{}}}",
                        entries
                            .into_iter()
                            .map(|(_key, field)| field)
                            .collect::<Vec<_>>()
                            .join(",")
                    ))
                } else {
                    Value::Text(format!("{{{}}}", fields.join(",")))
                }
            }
            AggregateState::Min(value) | AggregateState::Max(value) => {
                value.clone().unwrap_or(Value::Null)
            }
        })
    }

    fn aggregate_state_values(&self, states: &[AggregateState]) -> Result<Vec<Value>> {
        states
            .iter()
            .map(|state| self.aggregate_state_value(state))
            .collect()
    }

    fn collect_aggregate_calls(
        &self,
        columns: &[SelectItem],
        having: Option<&Expr>,
        order_by: &[OrderBy],
    ) -> Vec<AggregateCall> {
        let mut calls = Vec::new();
        for item in columns {
            match item {
                SelectItem::Expr { expr, .. } => {
                    Self::collect_scalar_expr_aggregate_calls(expr, &mut calls);
                }
                SelectItem::Aggregate {
                    func, arg, filter, ..
                } => {
                    Self::push_unique_aggregate_call(
                        &mut calls,
                        AggregateCall {
                            func: *func,
                            arg: arg.clone(),
                            filter: filter.clone(),
                        },
                    );
                }
                SelectItem::Wildcard | SelectItem::Column(_) | SelectItem::AliasedColumn { .. } => {
                }
            }
        }
        if let Some(having) = having {
            Self::collect_expr_aggregate_calls(having, &mut calls);
        }
        for item in order_by {
            if let OrderByExpr::Expr(expr) = &item.expr {
                Self::collect_scalar_expr_aggregate_calls(expr, &mut calls);
            }
        }
        calls
    }

    fn collect_expr_aggregate_calls(expr: &Expr, calls: &mut Vec<AggregateCall>) {
        match expr {
            Expr::CompareScalar { left, right, .. } | Expr::Is { left, right, .. } => {
                Self::collect_scalar_expr_aggregate_calls(left, calls);
                Self::collect_scalar_expr_aggregate_calls(right, calls);
            }
            Expr::IsNullScalar { expr, .. }
            | Expr::IsBool { expr, .. }
            | Expr::LikeScalar { expr, .. }
            | Expr::GlobScalar { expr, .. }
            | Expr::InSubqueryScalar { expr, .. } => {
                Self::collect_scalar_expr_aggregate_calls(expr, calls);
            }
            Expr::BetweenScalar {
                expr, low, high, ..
            } => {
                Self::collect_scalar_expr_aggregate_calls(expr, calls);
                Self::collect_scalar_expr_aggregate_calls(low, calls);
                Self::collect_scalar_expr_aggregate_calls(high, calls);
            }
            Expr::InListScalar { expr, values, .. } => {
                Self::collect_scalar_expr_aggregate_calls(expr, calls);
                for value in values {
                    Self::collect_scalar_expr_aggregate_calls(value, calls);
                }
            }
            Expr::CompareSubqueryScalar { left, .. } => {
                Self::collect_scalar_expr_aggregate_calls(left, calls);
            }
            Expr::Not(expr) => Self::collect_expr_aggregate_calls(expr, calls),
            Expr::And(left, right) | Expr::Or(left, right) => {
                Self::collect_expr_aggregate_calls(left, calls);
                Self::collect_expr_aggregate_calls(right, calls);
            }
            Expr::Compare { .. }
            | Expr::CompareColumns { .. }
            | Expr::IsNull { .. }
            | Expr::InSubquery { .. }
            | Expr::InList { .. }
            | Expr::CompareSubquery { .. }
            | Expr::ExistsSubquery { .. }
            | Expr::Like { .. }
            | Expr::Glob { .. }
            | Expr::Between { .. } => {}
        }
    }

    fn collect_scalar_expr_aggregate_calls(expr: &ScalarExpr, calls: &mut Vec<AggregateCall>) {
        match expr {
            ScalarExpr::Aggregate { func, arg, filter } => {
                Self::push_unique_aggregate_call(
                    calls,
                    AggregateCall {
                        func: *func,
                        arg: arg.as_ref().clone(),
                        filter: filter.as_deref().cloned(),
                    },
                );
            }
            ScalarExpr::Tuple(values) => {
                for value in values {
                    Self::collect_scalar_expr_aggregate_calls(value, calls);
                }
            }
            ScalarExpr::UnaryMinus(expr)
            | ScalarExpr::BitNot(expr)
            | ScalarExpr::Not(expr)
            | ScalarExpr::Cast { expr, .. }
            | ScalarExpr::Collate { expr, .. }
            | ScalarExpr::IsBool { expr, .. } => {
                Self::collect_scalar_expr_aggregate_calls(expr, calls);
            }
            ScalarExpr::Is { left, right, .. }
            | ScalarExpr::Compare { left, right, .. }
            | ScalarExpr::Binary { left, right, .. } => {
                Self::collect_scalar_expr_aggregate_calls(left, calls);
                Self::collect_scalar_expr_aggregate_calls(right, calls);
            }
            ScalarExpr::InList { expr, values, .. } => {
                Self::collect_scalar_expr_aggregate_calls(expr, calls);
                for value in values {
                    Self::collect_scalar_expr_aggregate_calls(value, calls);
                }
            }
            ScalarExpr::InSubquery { expr, .. }
            | ScalarExpr::CompareSubquery { left: expr, .. } => {
                Self::collect_scalar_expr_aggregate_calls(expr, calls);
            }
            ScalarExpr::Subquery { .. } => {}
            ScalarExpr::Like { expr, .. } | ScalarExpr::Glob { expr, .. } => {
                Self::collect_scalar_expr_aggregate_calls(expr, calls);
            }
            ScalarExpr::Between {
                expr, low, high, ..
            } => {
                Self::collect_scalar_expr_aggregate_calls(expr, calls);
                Self::collect_scalar_expr_aggregate_calls(low, calls);
                Self::collect_scalar_expr_aggregate_calls(high, calls);
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                if let Some(base) = base {
                    Self::collect_scalar_expr_aggregate_calls(base, calls);
                }
                for (when_expr, then_expr) in when_then_clauses {
                    Self::collect_scalar_expr_aggregate_calls(when_expr, calls);
                    Self::collect_scalar_expr_aggregate_calls(then_expr, calls);
                }
                if let Some(else_expr) = else_expr {
                    Self::collect_scalar_expr_aggregate_calls(else_expr, calls);
                }
            }
            ScalarExpr::Function { args, .. } => {
                for arg in args {
                    Self::collect_scalar_expr_aggregate_calls(arg, calls);
                }
            }
            ScalarExpr::Literal(_) | ScalarExpr::Column(_) => {}
        }
    }

    fn push_unique_aggregate_call(calls: &mut Vec<AggregateCall>, call: AggregateCall) {
        if !calls.iter().any(|candidate| candidate == &call) {
            calls.push(call);
        }
    }

    fn scalar_expr_has_aggregate(expr: &ScalarExpr) -> bool {
        let mut calls = Vec::new();
        Self::collect_scalar_expr_aggregate_calls(expr, &mut calls);
        !calls.is_empty()
    }

    fn aggregate_expr_eval_source(
        &self,
        group_by: &[ScalarExpr],
        group_key: &[Value],
        calls: &[AggregateCall],
        aggregate_values: &[Value],
    ) -> (RowSet, Row) {
        let mut columns = Vec::with_capacity(group_by.len() + calls.len());
        let mut row = Vec::with_capacity(group_by.len() + calls.len());

        for (expr, value) in group_by.iter().zip(group_key) {
            let label = self.scalar_expr_name(expr);
            columns.push(ColumnMeta {
                table: None,
                alias: None,
                name: label.clone(),
                output_name: label,
                collation: None,
                hidden: false,
            });
            row.push(value.clone());
        }

        for (call, value) in calls.iter().zip(aggregate_values) {
            let label = self.aggregate_call_name(call);
            columns.push(ColumnMeta {
                table: None,
                alias: None,
                name: label.clone(),
                output_name: label,
                collation: None,
                hidden: false,
            });
            row.push(value.clone());
        }

        (
            RowSet {
                columns,
                rows: Vec::new(),
            },
            row,
        )
    }

    fn aggregate_output_columns(&self, columns: &[SelectItem]) -> Vec<ColumnMeta> {
        columns
            .iter()
            .map(|item| ColumnMeta {
                table: None,
                alias: None,
                name: self.output_name(item),
                output_name: self.output_name(item),
                collation: None,
                hidden: false,
            })
            .collect()
    }

    fn aggregate_eval_columns(
        &self,
        output_columns: &[ColumnMeta],
        group_by: &[ScalarExpr],
        calls: &[AggregateCall],
    ) -> (Vec<ColumnMeta>, Vec<usize>, Vec<usize>) {
        let mut columns = output_columns.to_vec();
        let mut hidden_group_indexes = Vec::new();
        let mut hidden_aggregate_indexes = Vec::new();

        for (index, expr) in group_by.iter().enumerate() {
            let label = self.scalar_expr_name(expr);
            if columns
                .iter()
                .any(|column| column.name == label || column.output_name == label)
            {
                continue;
            }
            columns.push(ColumnMeta {
                table: None,
                alias: None,
                name: label.clone(),
                output_name: label,
                collation: None,
                hidden: true,
            });
            hidden_group_indexes.push(index);
        }

        for (index, call) in calls.iter().enumerate() {
            let label = self.aggregate_call_name(call);
            if columns
                .iter()
                .any(|column| column.name == label || column.output_name == label)
            {
                continue;
            }
            columns.push(ColumnMeta {
                table: None,
                alias: None,
                name: label.clone(),
                output_name: label,
                collation: None,
                hidden: true,
            });
            hidden_aggregate_indexes.push(index);
        }

        (columns, hidden_group_indexes, hidden_aggregate_indexes)
    }

    fn append_hidden_rowid(
        &self,
        mut row: Row,
        row_id: RowId,
        schema: &Schema,
        exposes_rowid: bool,
    ) -> Result<Row> {
        if exposes_rowid {
            let rowid = i64::try_from(row_id.0)
                .map_err(|_| DbError::storage("sqlite rowid does not fit in i64"))?;
            for alias_name in ["rowid", "oid", "_rowid_"] {
                if schema
                    .columns
                    .iter()
                    .any(|column| column.name.eq_ignore_ascii_case(alias_name))
                {
                    continue;
                }
                row.push(Value::Integer(rowid));
            }
        }
        Ok(row)
    }

    fn append_hidden_rowid_columns(
        &self,
        columns: &mut Vec<ColumnMeta>,
        schema: &Schema,
        table: &str,
        table_alias: Option<&str>,
        exposes_rowid: bool,
    ) {
        if !exposes_rowid {
            return;
        }
        for alias_name in ["rowid", "oid", "_rowid_"] {
            if schema
                .columns
                .iter()
                .any(|column| column.name.eq_ignore_ascii_case(alias_name))
            {
                continue;
            }
            columns.push(ColumnMeta {
                table: Some(table.to_string()),
                alias: table_alias.map(str::to_string),
                name: alias_name.to_string(),
                output_name: alias_name.to_string(),
                collation: None,
                hidden: true,
            });
        }
    }

    fn aggregate_eval_row(
        &self,
        mut visible_row: Row,
        group_key: &[Value],
        hidden_group_indexes: &[usize],
        aggregate_values: &[Value],
        hidden_aggregate_indexes: &[usize],
    ) -> Row {
        for index in hidden_group_indexes {
            visible_row.push(group_key[*index].clone());
        }
        for index in hidden_aggregate_indexes {
            visible_row.push(aggregate_values[*index].clone());
        }
        visible_row
    }

    fn group_key_index(
        &self,
        source: &RowSet,
        group_by: &[ScalarExpr],
        name: &str,
    ) -> Result<usize> {
        let target = self.resolve_column_index(&source.columns, name)?;
        for (index, expr) in group_by.iter().enumerate() {
            let ScalarExpr::Column(column) = expr else {
                continue;
            };
            if self.resolve_column_index(&source.columns, column)? == target {
                return Ok(index);
            }
        }
        Err(DbError::plan(format!(
            "non-aggregate column {name} must appear in GROUP BY"
        )))
    }

    fn group_expr_index(
        &self,
        source: &RowSet,
        group_by: &[ScalarExpr],
        expr: &ScalarExpr,
    ) -> Result<usize> {
        if let Some(index) = group_by.iter().position(|group_expr| group_expr == expr) {
            return Ok(index);
        }

        if let ScalarExpr::Column(name) = expr {
            return self.group_key_index(source, group_by, name);
        }

        Err(DbError::plan(format!(
            "non-aggregate expression {} must appear in GROUP BY",
            self.scalar_expr_name(expr)
        )))
    }

    fn sort_and_limit_rows(
        &self,
        mut rowset: RowSet,
        order_by: &[OrderBy],
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<RowSet> {
        if !order_by.is_empty() {
            let mut rows = rowset
                .rows
                .into_iter()
                .map(|row| {
                    let sort_key = self.order_sort_key(
                        None,
                        &rowset.columns,
                        &row,
                        &rowset.columns,
                        &row,
                        order_by,
                    )?;
                    Ok((sort_key, row))
                })
                .collect::<Result<Vec<_>>>()?;
            rows.sort_by(|(left_key, _), (right_key, _)| {
                self.compare_order_keys(left_key, right_key, order_by)
            });
            rowset.rows = rows.into_iter().map(|(_, row)| row).collect();
        } else if self.reverse_unordered_selects.get() {
            rowset.rows.reverse();
        }
        Self::apply_limit_offset(&mut rowset.rows, limit, offset);
        Ok(rowset)
    }

    fn apply_limit_offset(rows: &mut Vec<Row>, limit: Option<usize>, offset: Option<usize>) {
        let offset = offset.unwrap_or(0);
        if offset >= rows.len() {
            rows.clear();
            return;
        }
        if offset > 0 {
            rows.drain(..offset);
        }
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
    }

    fn apply_limit_offset_for_delete(
        rows: &mut Vec<(RowId, Row, Row)>,
        limit: Option<usize>,
        offset: Option<usize>,
    ) {
        let offset = offset.unwrap_or(0);
        if offset >= rows.len() {
            rows.clear();
            return;
        }
        if offset > 0 {
            rows.drain(..offset);
        }
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
    }

    fn apply_limit_offset_for_update(
        rows: &mut Vec<(RowId, Row, Row, Row)>,
        limit: Option<usize>,
        offset: Option<usize>,
    ) {
        let offset = offset.unwrap_or(0);
        if offset >= rows.len() {
            rows.clear();
            return;
        }
        if offset > 0 {
            rows.drain(..offset);
        }
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
    }

    fn order_sort_key(
        &self,
        transaction_id: Option<TransactionId>,
        projected_columns: &[ColumnMeta],
        projected_row: &Row,
        full_columns: &[ColumnMeta],
        full_row: &Row,
        order_by: &[OrderBy],
    ) -> Result<Vec<Option<Value>>> {
        let full_source = RowSet {
            columns: full_columns.to_vec(),
            rows: vec![],
        };
        order_by
            .iter()
            .map(|item| match &item.expr {
                OrderByExpr::Expr(expr) => self
                    .evaluate_scalar_expr_in_context(
                        transaction_id,
                        &full_source,
                        full_row,
                        None,
                        expr,
                    )
                    .map(Some),
                _ => Ok(self
                    .resolve_order_value(
                        projected_columns,
                        projected_row,
                        full_columns,
                        full_row,
                        &item.expr,
                    )
                    .cloned()),
            })
            .collect()
    }

    fn compare_order_keys(
        &self,
        left_key: &[Option<Value>],
        right_key: &[Option<Value>],
        order_by: &[OrderBy],
    ) -> Ordering {
        for ((left_value, right_value), item) in left_key.iter().zip(right_key).zip(order_by) {
            let ordering = match (left_value, right_value) {
                (Some(left), Some(right)) => self.compare_order_values(left, right, item),
                _ => Ordering::Equal,
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    fn compare_aggregate_order(
        &self,
        source: &RowSet,
        order_by: &[OrderBy],
        left: &Row,
        right: &Row,
    ) -> Ordering {
        for item in order_by {
            if let OrderByExpr::Position(position) = item.expr {
                let index = position.saturating_sub(1);
                let ordering = match (left.get(index), right.get(index)) {
                    (Some(left), Some(right)) => self.compare_order_values(left, right, item),
                    _ => Ordering::Equal,
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
                continue;
            }
            let left_value = match &item.expr {
                OrderByExpr::Column(column) => self
                    .try_lookup_value(&source.columns, left, column)
                    .cloned(),
                OrderByExpr::Expr(expr) => self.evaluate_scalar_expr(source, left, expr).ok(),
                OrderByExpr::Position(_) => unreachable!(),
            };
            let right_value = match &item.expr {
                OrderByExpr::Column(column) => self
                    .try_lookup_value(&source.columns, right, column)
                    .cloned(),
                OrderByExpr::Expr(expr) => self.evaluate_scalar_expr(source, right, expr).ok(),
                OrderByExpr::Position(_) => unreachable!(),
            };
            let ordering = match (left_value.as_ref(), right_value.as_ref()) {
                (Some(left), Some(right)) => self.compare_order_values(left, right, item),
                _ => Ordering::Equal,
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    fn compare_order_values(&self, left: &Value, right: &Value, item: &OrderBy) -> Ordering {
        let nulls = item.nulls;
        let ordering = match (nulls, left, right) {
            (Some(_), Value::Null, Value::Null) => Ordering::Equal,
            (Some(NullOrder::First), Value::Null, _) => Ordering::Less,
            (Some(NullOrder::First), _, Value::Null) => Ordering::Greater,
            (Some(NullOrder::Last), Value::Null, _) => Ordering::Greater,
            (Some(NullOrder::Last), _, Value::Null) => Ordering::Less,
            (None, Value::Null, Value::Null) => Ordering::Equal,
            (None, Value::Null, _) => Ordering::Less,
            (None, _, Value::Null) => Ordering::Greater,
            _ => self.compare_order_non_null_values(left, right, item),
        };

        let should_reverse = item.descending
            && (nulls.is_none() || !matches!((left, right), (Value::Null, _) | (_, Value::Null)));

        if should_reverse {
            ordering.reverse()
        } else {
            ordering
        }
    }

    fn compare_order_non_null_values(
        &self,
        left: &Value,
        right: &Value,
        item: &OrderBy,
    ) -> Ordering {
        if matches!(item.collation.as_deref(), Some(collation) if collation.eq_ignore_ascii_case("NOCASE"))
            && let (Value::Text(left), Value::Text(right)) = (left, right)
        {
            return sqlite_nocase_cmp(left, right);
        }
        self.compare(left, right)
            .unwrap_or(None)
            .unwrap_or(Ordering::Equal)
    }

    fn scan_index_spec(
        &self,
        transaction_id: TransactionId,
        table: &str,
        scan: &IndexScanSpec,
    ) -> Result<Vec<RowId>> {
        match scan.mode {
            IndexScanMode::Lookup => {
                self.storage
                    .lookup_index(transaction_id, table, &scan.index, &scan.key_prefix)
            }
            IndexScanMode::Range => {
                let Some(range) = &scan.range else {
                    return Err(DbError::plan("range index scan is missing range bounds"));
                };
                self.storage.scan_index_range(
                    transaction_id,
                    table,
                    &scan.index,
                    &scan.key_prefix,
                    range.lower.as_ref().map(|bound| (bound.op, &bound.value)),
                    range.upper.as_ref().map(|bound| (bound.op, &bound.value)),
                )
            }
            IndexScanMode::Prefix => {
                self.storage
                    .scan_index_prefix(transaction_id, table, &scan.index, &scan.key_prefix)
            }
        }
    }

    fn require_schema(&self, transaction_id: TransactionId, table: &str) -> Result<Schema> {
        self.storage
            .get_schema(transaction_id, table)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {table}")))
    }

    fn project_row(
        &self,
        transaction_id: Option<TransactionId>,
        source: &RowSet,
        row: &Row,
        columns: &[SelectItem],
    ) -> Result<Row> {
        let mut projected = Vec::new();
        for column in columns {
            match column {
                SelectItem::Wildcard => {
                    for (index, source_column) in source.columns.iter().enumerate() {
                        if !source_column.hidden
                            && let Some(value) = row.get(index)
                        {
                            projected.push(value.clone());
                        }
                    }
                }
                SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
                    projected.push(self.lookup_value(&source.columns, row, name)?.clone());
                }
                SelectItem::Expr { expr, .. } => {
                    projected.push(self.evaluate_scalar_expr_in_context(
                        transaction_id,
                        source,
                        row,
                        None,
                        expr,
                    )?);
                }
                SelectItem::Aggregate { .. } => {
                    return Err(DbError::plan(
                        "aggregate projection requires aggregate execution path",
                    ));
                }
            }
        }
        Ok(projected)
    }

    fn projected_columns(
        &self,
        source_columns: &[ColumnMeta],
        columns: &[SelectItem],
    ) -> Result<Vec<ColumnMeta>> {
        let mut projected = Vec::new();
        for column in columns {
            match column {
                SelectItem::Wildcard => projected.extend(
                    source_columns
                        .iter()
                        .filter(|source_column| !source_column.hidden)
                        .cloned(),
                ),
                SelectItem::Column(name) => {
                    let source = self.resolve_column_index(source_columns, name)?;
                    let mut meta = source_columns[source].clone();
                    meta.output_name = name.clone();
                    projected.push(meta);
                }
                SelectItem::AliasedColumn { name, alias } => {
                    let source = self.resolve_column_index(source_columns, name)?;
                    let mut meta = source_columns[source].clone();
                    meta.output_name = alias.clone();
                    meta.name = alias.clone();
                    projected.push(meta);
                }
                SelectItem::Expr { expr, alias } => {
                    let output_name = alias.clone().unwrap_or_else(|| self.scalar_expr_name(expr));
                    projected.push(ColumnMeta {
                        table: None,
                        alias: None,
                        name: output_name.clone(),
                        output_name,
                        collation: None,
                        hidden: false,
                    });
                }
                SelectItem::Aggregate { .. } => {
                    return Err(DbError::plan(
                        "aggregate projection requires aggregate execution path",
                    ));
                }
            }
        }
        Ok(projected)
    }

    fn resolve_order_value<'b>(
        &self,
        projected_columns: &[ColumnMeta],
        projected: &'b Row,
        full_columns: &[ColumnMeta],
        full: &'b Row,
        expr: &OrderByExpr,
    ) -> Option<&'b Value> {
        match expr {
            OrderByExpr::Column(column) => self
                .try_lookup_value(projected_columns, projected, column)
                .or_else(|| self.try_lookup_value(full_columns, full, column)),
            OrderByExpr::Position(position) => projected.get(position.saturating_sub(1)),
            OrderByExpr::Expr(_) => None,
        }
    }

    fn matches_filter(
        &self,
        transaction_id: TransactionId,
        rowset: &RowSet,
        row: &Row,
        filter: Option<&Expr>,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<bool> {
        let Some(filter) = filter else {
            return Ok(true);
        };

        match filter {
            Expr::Compare { column, op, value } => {
                let (left, meta) =
                    self.lookup_filter_value_with_meta(rowset, row, outer, column)?;
                self.compare_with_operator_with_collation(
                    &left,
                    op,
                    value,
                    meta.and_then(|meta| meta.collation.as_deref()),
                )
            }
            Expr::CompareColumns { left, op, right } => {
                let (left, left_meta) =
                    self.lookup_filter_value_with_meta(rowset, row, outer, left)?;
                let (right, right_meta) =
                    self.lookup_filter_value_with_meta(rowset, row, outer, right)?;
                self.compare_with_operator_with_collation(
                    &left,
                    op,
                    &right,
                    left_meta
                        .and_then(|meta| meta.collation.as_deref())
                        .or_else(|| right_meta.and_then(|meta| meta.collation.as_deref())),
                )
            }
            Expr::CompareScalar { left, op, right } => {
                if let (ScalarExpr::Tuple(left_exprs), ScalarExpr::Tuple(right_exprs)) =
                    (left, right)
                {
                    let left =
                        self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, left_exprs)?;
                    let right =
                        self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, right_exprs)?;
                    return Ok(match self.tuple_compare_result_value(&left, op, &right)? {
                        Value::Boolean(value) => value,
                        Value::Null => false,
                        _ => unreachable!("tuple comparison only returns boolean or NULL"),
                    });
                }
                let left_value = self.evaluate_filter_scalar_expr(rowset, row, outer, left)?;
                let right_value = self.evaluate_filter_scalar_expr(rowset, row, outer, right)?;
                self.compare_with_operator_with_collation(
                    &left_value,
                    op,
                    &right_value,
                    scalar_expr_collation(left).or_else(|| scalar_expr_collation(right)),
                )
            }
            Expr::IsNull { column, negated } => {
                let left = self.lookup_filter_value(rowset, row, outer, column)?;
                Ok((left == Value::Null) ^ *negated)
            }
            Expr::IsNullScalar { expr, negated } => {
                let value = self.evaluate_filter_scalar_expr(rowset, row, outer, expr)?;
                Ok((value == Value::Null) ^ *negated)
            }
            Expr::Is {
                left,
                right,
                negated,
            } => {
                if let (ScalarExpr::Tuple(left_exprs), ScalarExpr::Tuple(right_exprs)) =
                    (left, right)
                {
                    let left =
                        self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, left_exprs)?;
                    let right =
                        self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, right_exprs)?;
                    return Ok(self.tuple_is_with_negation(&left, &right, *negated));
                }
                let left = self.evaluate_filter_scalar_expr(rowset, row, outer, left)?;
                let right = self.evaluate_filter_scalar_expr(rowset, row, outer, right)?;
                Ok(self.is_with_negation(&left, &right, *negated))
            }
            Expr::IsBool {
                expr,
                value,
                negated,
                explicit: _,
            } => {
                let matches = match self.evaluate_filter_scalar_expr(rowset, row, outer, expr)? {
                    Value::Null => false,
                    evaluated => Self::sqlite_is_true_value(&evaluated) == *value,
                };
                Ok(matches ^ *negated)
            }
            Expr::InSubquery {
                column,
                query,
                negated,
            } => {
                let left = self.lookup_filter_value(rowset, row, outer, column)?;
                let rows = self.execute_subquery(transaction_id, query, Some((rowset, row)))?;
                Ok(self.evaluate_in_rows(&left, &rows.rows, *negated))
            }
            Expr::InList {
                column,
                values,
                negated,
            } => {
                let left = self.lookup_filter_value(rowset, row, outer, column)?;
                Ok(self.evaluate_in_values(&left, values, *negated))
            }
            Expr::InSubqueryScalar {
                expr,
                query,
                negated,
            } => {
                if let ScalarExpr::Tuple(left_exprs) = expr {
                    let left =
                        self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, left_exprs)?;
                    let rows =
                        self.execute_subquery_rows(transaction_id, query, Some((rowset, row)))?;
                    return Ok(
                        match self.tuple_in_result_value(&left, &rows.rows, *negated) {
                            Value::Boolean(value) => value,
                            Value::Null => false,
                            _ => unreachable!("tuple IN only returns boolean or NULL"),
                        },
                    );
                }
                let left = self.evaluate_filter_scalar_expr(rowset, row, outer, expr)?;
                let rows = self.execute_subquery(transaction_id, query, Some((rowset, row)))?;
                Ok(self.evaluate_in_rows(&left, &rows.rows, *negated))
            }
            Expr::InListScalar {
                expr,
                values,
                negated,
            } => {
                if let ScalarExpr::Tuple(left_exprs) = expr {
                    let left =
                        self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, left_exprs)?;
                    let candidates = values
                        .iter()
                        .map(|value| match value {
                            ScalarExpr::Tuple(values) => {
                                self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, values)
                            }
                            value => self
                                .evaluate_filter_scalar_expr(rowset, row, outer, value)
                                .map(|value| vec![value]),
                        })
                        .collect::<Result<Vec<_>>>()?;
                    return Ok(
                        match self.tuple_in_result_value(&left, &candidates, *negated) {
                            Value::Boolean(value) => value,
                            Value::Null => false,
                            _ => unreachable!("tuple IN only returns boolean or NULL"),
                        },
                    );
                }
                let left = self.evaluate_filter_scalar_expr(rowset, row, outer, expr)?;
                let candidates = values
                    .iter()
                    .map(|value| self.evaluate_filter_scalar_expr(rowset, row, outer, value))
                    .collect::<Result<Vec<_>>>()?;
                Ok(self.evaluate_in_values(&left, &candidates, *negated))
            }
            Expr::CompareSubquery { column, op, query } => {
                let left = self.lookup_filter_value(rowset, row, outer, column)?;
                let right =
                    self.scalar_subquery_value(transaction_id, query, Some((rowset, row)))?;
                self.compare_with_operator(&left, op, &right)
            }
            Expr::CompareSubqueryScalar { left, op, query } => {
                if let ScalarExpr::Tuple(left_exprs) = left {
                    let left =
                        self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, left_exprs)?;
                    let right =
                        self.tuple_subquery_value(transaction_id, query, Some((rowset, row)))?;
                    return Ok(match self.tuple_compare_result_value(&left, op, &right)? {
                        Value::Boolean(value) => value,
                        Value::Null => false,
                        _ => unreachable!("tuple comparison only returns boolean or NULL"),
                    });
                }
                let left = self.evaluate_filter_scalar_expr(rowset, row, outer, left)?;
                let right =
                    self.scalar_subquery_value(transaction_id, query, Some((rowset, row)))?;
                self.compare_with_operator(&left, op, &right)
            }
            Expr::ExistsSubquery { query, negated } => {
                let rows = self.execute_subquery(transaction_id, query, Some((rowset, row)))?;
                Ok((!rows.rows.is_empty()) ^ *negated)
            }
            Expr::Like {
                column,
                pattern,
                escape,
                negated,
            } => {
                let Value::Text(value) = self.lookup_filter_value(rowset, row, outer, column)?
                else {
                    return Ok(false);
                };
                Ok(Self::matches_like_pattern(
                    &value,
                    pattern,
                    escape,
                    self.storage.case_sensitive_like(),
                )? ^ *negated)
            }
            Expr::LikeScalar {
                expr,
                pattern,
                escape,
                negated,
            } => {
                let Value::Text(value) =
                    self.evaluate_filter_scalar_expr(rowset, row, outer, expr)?
                else {
                    return Ok(false);
                };
                Ok(Self::matches_like_pattern(
                    &value,
                    pattern,
                    escape,
                    self.storage.case_sensitive_like(),
                )? ^ *negated)
            }
            Expr::Glob {
                column,
                pattern,
                negated,
            } => {
                let Value::Text(value) = self.lookup_filter_value(rowset, row, outer, column)?
                else {
                    return Ok(false);
                };
                Ok(Self::matches_glob_pattern(&value, pattern) ^ *negated)
            }
            Expr::GlobScalar {
                expr,
                pattern,
                negated,
            } => {
                let Value::Text(value) =
                    self.evaluate_filter_scalar_expr(rowset, row, outer, expr)?
                else {
                    return Ok(false);
                };
                Ok(Self::matches_glob_pattern(&value, pattern) ^ *negated)
            }
            Expr::Between {
                column,
                low,
                high,
                negated,
            } => {
                let value = self.lookup_filter_value(rowset, row, outer, column)?;
                let Some(low_cmp) = self.compare(&value, low)? else {
                    return Ok(false);
                };
                let Some(high_cmp) = self.compare(&value, high)? else {
                    return Ok(false);
                };
                let matches = matches!(low_cmp, Ordering::Greater | Ordering::Equal)
                    && matches!(high_cmp, Ordering::Less | Ordering::Equal);
                Ok(matches ^ *negated)
            }
            Expr::BetweenScalar {
                expr,
                low,
                high,
                negated,
            } => {
                if let (
                    ScalarExpr::Tuple(value_exprs),
                    ScalarExpr::Tuple(low_exprs),
                    ScalarExpr::Tuple(high_exprs),
                ) = (expr, low, high)
                {
                    let value =
                        self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, value_exprs)?;
                    let low =
                        self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, low_exprs)?;
                    let high =
                        self.evaluate_filter_scalar_tuple_expr(rowset, row, outer, high_exprs)?;
                    return Ok(
                        match self.tuple_between_result_value(&value, &low, &high, *negated)? {
                            Value::Boolean(value) => value,
                            Value::Null => false,
                            _ => unreachable!("tuple BETWEEN only returns boolean or NULL"),
                        },
                    );
                }
                let value = self.evaluate_filter_scalar_expr(rowset, row, outer, expr)?;
                let low = self.evaluate_filter_scalar_expr(rowset, row, outer, low)?;
                let high = self.evaluate_filter_scalar_expr(rowset, row, outer, high)?;
                let Some(low_cmp) = self.compare(&value, &low)? else {
                    return Ok(false);
                };
                let Some(high_cmp) = self.compare(&value, &high)? else {
                    return Ok(false);
                };
                let matches = matches!(low_cmp, Ordering::Greater | Ordering::Equal)
                    && matches!(high_cmp, Ordering::Less | Ordering::Equal);
                Ok(matches ^ *negated)
            }
            Expr::Not(expr) => Ok(!self.matches_filter(
                transaction_id,
                rowset,
                row,
                Some(expr.as_ref()),
                outer,
            )?),
            Expr::And(left, right) => {
                Ok(
                    self.matches_filter(transaction_id, rowset, row, Some(left.as_ref()), outer)?
                        && self.matches_filter(
                            transaction_id,
                            rowset,
                            row,
                            Some(right.as_ref()),
                            outer,
                        )?,
                )
            }
            Expr::Or(left, right) => {
                Ok(
                    self.matches_filter(transaction_id, rowset, row, Some(left.as_ref()), outer)?
                        || self.matches_filter(
                            transaction_id,
                            rowset,
                            row,
                            Some(right.as_ref()),
                            outer,
                        )?,
                )
            }
        }
    }

    fn execute_subquery(
        &self,
        transaction_id: TransactionId,
        query: &crate::sql::ast::SelectStatement,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<RowSet> {
        let rows = self.execute_subquery_rows(transaction_id, query, outer)?;
        if rows.columns.len() != 1 {
            return Err(DbError::plan("subquery must return exactly one column"));
        }
        Ok(rows)
    }

    fn execute_subquery_rows(
        &self,
        transaction_id: TransactionId,
        query: &crate::sql::ast::SelectStatement,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<RowSet> {
        let planner = Planner::with_unresolved_outer_refs();
        let context = self
            .storage
            .planning_context_snapshot(Some(transaction_id))?;
        let plan = planner.plan_statement(&Statement::Select(query.clone()), &context)?;
        let plan = Optimizer::new().optimize_with_context(plan, &context)?;
        self.execute_query_plan_with_outer(transaction_id, plan, outer)
    }

    fn scalar_subquery_value(
        &self,
        transaction_id: TransactionId,
        query: &crate::sql::ast::SelectStatement,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<Value> {
        let rows = self.execute_subquery(transaction_id, query, outer)?;
        Ok(rows
            .rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(Value::Null))
    }

    fn tuple_subquery_value(
        &self,
        transaction_id: TransactionId,
        query: &crate::sql::ast::SelectStatement,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<Vec<Value>> {
        let rows = self.execute_subquery_rows(transaction_id, query, outer)?;
        match rows.rows.as_slice() {
            [] => Ok(vec![Value::Null; rows.columns.len()]),
            [row] => Ok(row.clone()),
            _ => Err(DbError::plan("scalar subquery returned more than one row")),
        }
    }

    fn matches_glob_pattern(value: &str, pattern: &str) -> bool {
        fn matches_char_class(pattern: &[char], start: usize, ch: char) -> Option<(bool, usize)> {
            let mut index = start + 1;
            let negated = matches!(pattern.get(index), Some('^'));
            if negated {
                index += 1;
            }
            let mut matched = false;

            while index < pattern.len() {
                if pattern[index] == ']' {
                    return Some((matched ^ negated, index + 1));
                }

                if index + 2 < pattern.len()
                    && pattern[index + 1] == '-'
                    && pattern[index + 2] != ']'
                {
                    let range_start = pattern[index];
                    let range_end = pattern[index + 2];
                    if range_start <= ch && ch <= range_end {
                        matched = true;
                    }
                    index += 3;
                } else {
                    if pattern[index] == ch {
                        matched = true;
                    }
                    index += 1;
                }
            }

            None
        }

        fn matches(value: &[char], pattern: &[char]) -> bool {
            match pattern.first() {
                None => value.is_empty(),
                Some('*') => {
                    matches(value, &pattern[1..])
                        || (!value.is_empty() && matches(&value[1..], pattern))
                }
                Some('?') => !value.is_empty() && matches(&value[1..], &pattern[1..]),
                Some('[') => {
                    if value.is_empty() {
                        return false;
                    }
                    let Some((matched, next_index)) = matches_char_class(pattern, 0, value[0])
                    else {
                        return false;
                    };
                    matched && matches(&value[1..], &pattern[next_index..])
                }
                Some(expected) => {
                    !value.is_empty()
                        && value[0] == *expected
                        && matches(&value[1..], &pattern[1..])
                }
            }
        }

        let value = value.chars().collect::<Vec<_>>();
        let pattern = pattern.chars().collect::<Vec<_>>();
        matches(&value, &pattern)
    }

    fn lookup_value<'b>(
        &self,
        columns: &[ColumnMeta],
        row: &'b Row,
        column: &str,
    ) -> Result<&'b Value> {
        let position = self.resolve_column_index(columns, column)?;
        row.get(position)
            .ok_or_else(|| DbError::storage(format!("row is missing column {column}")))
    }

    fn try_lookup_value<'b>(
        &self,
        columns: &[ColumnMeta],
        row: &'b Row,
        column: &str,
    ) -> Option<&'b Value> {
        self.resolve_column_index(columns, column)
            .ok()
            .and_then(|index| row.get(index))
    }

    fn lookup_filter_value(
        &self,
        rowset: &RowSet,
        row: &Row,
        outer: Option<(&RowSet, &Row)>,
        column: &str,
    ) -> Result<Value> {
        self.lookup_filter_value_with_meta(rowset, row, outer, column)
            .map(|(value, _)| value)
    }

    fn lookup_filter_value_with_meta<'b>(
        &self,
        rowset: &'b RowSet,
        row: &'b Row,
        outer: Option<(&'b RowSet, &'b Row)>,
        column: &str,
    ) -> Result<(Value, Option<&'b ColumnMeta>)> {
        if let Some(value) = self.try_lookup_value(&rowset.columns, row, column) {
            let index = self.resolve_column_index(&rowset.columns, column)?;
            return Ok((value.clone(), rowset.columns.get(index)));
        }
        if let Some((outer_rowset, outer_row)) = outer
            && let Some(value) = self.try_lookup_value(&outer_rowset.columns, outer_row, column)
        {
            let index = self.resolve_column_index(&outer_rowset.columns, column)?;
            return Ok((value.clone(), outer_rowset.columns.get(index)));
        }
        let index = self.resolve_column_index(&rowset.columns, column)?;
        let value = row
            .get(index)
            .ok_or_else(|| DbError::storage(format!("row is missing column {column}")))?;
        Ok((value.clone(), rowset.columns.get(index)))
    }

    fn resolve_column_index(&self, columns: &[ColumnMeta], column: &str) -> Result<usize> {
        if let Some((prefix, suffix)) = column.split_once('.') {
            let matches = columns
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    entry.output_name == column
                        || (entry.name == suffix
                            && (entry.table.as_deref() == Some(prefix)
                                || entry.alias.as_deref() == Some(prefix)))
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            return match matches.as_slice() {
                [] => Err(DbError::plan(format!("unknown column {column}"))),
                [index] => Ok(*index),
                _ => Err(DbError::plan(format!(
                    "ambiguous column reference: {column}"
                ))),
            };
        }

        let matches = columns
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                !entry.hidden && (entry.output_name == column || entry.name == column)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => {}
            [index] => return Ok(*index),
            _ => {
                return Err(DbError::plan(format!(
                    "ambiguous column reference: {column}"
                )));
            }
        }
        let hidden_matches = columns
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.hidden && (entry.output_name == column || entry.name == column)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match hidden_matches.as_slice() {
            [] => Err(DbError::plan(format!("unknown column {column}"))),
            [index] => Ok(*index),
            _ => Err(DbError::plan(format!(
                "ambiguous column reference: {column}"
            ))),
        }
    }

    fn compare_with_operator(&self, left: &Value, op: &CompareOp, right: &Value) -> Result<bool> {
        self.compare_with_operator_with_collation(left, op, right, None)
    }

    fn compare_with_operator_with_collation(
        &self,
        left: &Value,
        op: &CompareOp,
        right: &Value,
        collation: Option<&str>,
    ) -> Result<bool> {
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(false);
        }
        let ordering = self.compare_with_collation(left, right, collation)?;
        match op {
            CompareOp::Eq => Ok(ordering == Some(Ordering::Equal)),
            CompareOp::Ne => Ok(ordering != Some(Ordering::Equal)),
            CompareOp::Gt => Ok(ordering == Some(Ordering::Greater)),
            CompareOp::Gte => Ok(matches!(
                ordering,
                Some(Ordering::Greater | Ordering::Equal)
            )),
            CompareOp::Lt => Ok(ordering == Some(Ordering::Less)),
            CompareOp::Lte => Ok(matches!(ordering, Some(Ordering::Less | Ordering::Equal))),
        }
    }

    fn compare_result_value_with_collation(
        &self,
        left: &Value,
        op: &CompareOp,
        right: &Value,
        collation: Option<&str>,
    ) -> Result<Value> {
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
        Ok(Value::Boolean(self.compare_with_operator_with_collation(
            left, op, right, collation,
        )?))
    }

    fn compare_with_collation(
        &self,
        left: &Value,
        right: &Value,
        collation: Option<&str>,
    ) -> Result<Option<Ordering>> {
        if matches!(collation, Some(collation) if collation.eq_ignore_ascii_case("NOCASE"))
            && let (Value::Text(left), Value::Text(right)) = (left, right)
        {
            return Ok(Some(sqlite_nocase_cmp(left, right)));
        }
        if matches!(collation, Some(collation) if collation.eq_ignore_ascii_case("RTRIM"))
            && let (Value::Text(left), Value::Text(right)) = (left, right)
        {
            return Ok(Some(sqlite_rtrim_cmp(left, right)));
        }
        self.compare(left, right)
    }

    fn evaluate_scalar_expr(&self, source: &RowSet, row: &Row, expr: &ScalarExpr) -> Result<Value> {
        Ok(match expr {
            ScalarExpr::Literal(value) => value.clone(),
            ScalarExpr::Column(name) => self.lookup_value(&source.columns, row, name)?.clone(),
            ScalarExpr::BitNot(expr) => match self.evaluate_scalar_expr(source, row, expr)? {
                Value::Null => Value::Null,
                value => Value::Integer(!Self::sqlite_bitwise_integer_arg(&value)?),
            },
            ScalarExpr::Not(expr) => match self.evaluate_scalar_expr(source, row, expr)? {
                value => Self::sqlite_not_value(&value),
            },
            ScalarExpr::UnaryMinus(expr) => match self.evaluate_scalar_expr(source, row, expr)? {
                Value::Null => Value::Null,
                value => match Self::coerce_sqlite_numeric_value(&value) {
                    Value::Integer(value) => Value::Integer(-value),
                    Value::Real(value) => Value::Real(-value),
                    Value::Null => Value::Null,
                    _ => unreachable!("sqlite numeric coercion only returns numeric values"),
                },
            },
            ScalarExpr::Cast { expr, ty } => {
                let value = self.evaluate_scalar_expr(source, row, expr)?;
                Self::cast_value(value, *ty)?
            }
            ScalarExpr::Collate { expr, .. } => self.evaluate_scalar_expr(source, row, expr)?,
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => {
                if let (ScalarExpr::Tuple(left_exprs), ScalarExpr::Tuple(right_exprs)) =
                    (left.as_ref(), right.as_ref())
                {
                    let left = self.evaluate_scalar_tuple_expr(source, row, left_exprs)?;
                    let right = self.evaluate_scalar_tuple_expr(source, row, right_exprs)?;
                    return Ok(Value::Boolean(
                        self.tuple_is_with_negation(&left, &right, *negated),
                    ));
                }
                let left = self.evaluate_scalar_expr(source, row, left)?;
                let right = self.evaluate_scalar_expr(source, row, right)?;
                Value::Boolean(self.is_with_negation(&left, &right, *negated))
            }
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => {
                let matches = match self.evaluate_scalar_expr(source, row, expr)? {
                    Value::Null => false,
                    evaluated => Self::sqlite_is_true_value(&evaluated) == *value,
                };
                Value::Boolean(matches ^ *negated)
            }
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => {
                if let ScalarExpr::Tuple(left_exprs) = expr.as_ref() {
                    let left = self.evaluate_scalar_tuple_expr(source, row, left_exprs)?;
                    let candidates = values
                        .iter()
                        .map(|value| match value {
                            ScalarExpr::Tuple(values) => {
                                self.evaluate_scalar_tuple_expr(source, row, values)
                            }
                            value => self
                                .evaluate_scalar_expr(source, row, value)
                                .map(|value| vec![value]),
                        })
                        .collect::<Result<Vec<_>>>()?;
                    return Ok(self.tuple_in_result_value(&left, &candidates, *negated));
                }
                let left = self.evaluate_scalar_expr(source, row, expr)?;
                let values = values
                    .iter()
                    .map(|value| self.evaluate_scalar_expr(source, row, value))
                    .collect::<Result<Vec<_>>>()?;
                self.in_result_value(&left, &values, *negated)
            }
            ScalarExpr::InSubquery { .. }
            | ScalarExpr::Subquery { .. }
            | ScalarExpr::CompareSubquery { .. } => {
                return Err(DbError::plan(
                    "subquery scalar expressions require query context",
                ));
            }
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => match self.evaluate_scalar_expr(source, row, expr)? {
                Value::Text(value) => Value::Boolean(
                    Self::matches_like_pattern(
                        &value,
                        pattern,
                        escape,
                        self.storage.case_sensitive_like(),
                    )? ^ *negated,
                ),
                Value::Null => Value::Null,
                _ => Value::Null,
            },
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => match self.evaluate_scalar_expr(source, row, expr)? {
                Value::Text(value) => {
                    Value::Boolean(Self::matches_glob_pattern(&value, pattern) ^ *negated)
                }
                Value::Null => Value::Null,
                _ => Value::Null,
            },
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                if let (
                    ScalarExpr::Tuple(value_exprs),
                    ScalarExpr::Tuple(low_exprs),
                    ScalarExpr::Tuple(high_exprs),
                ) = (expr.as_ref(), low.as_ref(), high.as_ref())
                {
                    let value = self.evaluate_scalar_tuple_expr(source, row, value_exprs)?;
                    let low = self.evaluate_scalar_tuple_expr(source, row, low_exprs)?;
                    let high = self.evaluate_scalar_tuple_expr(source, row, high_exprs)?;
                    return self.tuple_between_result_value(&value, &low, &high, *negated);
                }
                let value = self.evaluate_scalar_expr(source, row, expr)?;
                let low = self.evaluate_scalar_expr(source, row, low)?;
                let high = self.evaluate_scalar_expr(source, row, high)?;
                let Some(low_cmp) = self.compare(&value, &low)? else {
                    return Ok(Value::Null);
                };
                let Some(high_cmp) = self.compare(&value, &high)? else {
                    return Ok(Value::Null);
                };
                let matches = matches!(low_cmp, Ordering::Greater | Ordering::Equal)
                    && matches!(high_cmp, Ordering::Less | Ordering::Equal);
                Value::Boolean(matches ^ *negated)
            }
            ScalarExpr::Compare { left, op, right } => {
                if let (ScalarExpr::Tuple(left_exprs), ScalarExpr::Tuple(right_exprs)) =
                    (left.as_ref(), right.as_ref())
                {
                    let left = self.evaluate_scalar_tuple_expr(source, row, left_exprs)?;
                    let right = self.evaluate_scalar_tuple_expr(source, row, right_exprs)?;
                    return self.tuple_compare_result_value(&left, op, &right);
                }
                let left_value = self.evaluate_scalar_expr(source, row, left)?;
                let right_value = self.evaluate_scalar_expr(source, row, right)?;
                self.compare_result_value_with_collation(
                    &left_value,
                    op,
                    &right_value,
                    scalar_expr_collation(left).or_else(|| scalar_expr_collation(right)),
                )?
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => self.evaluate_case_scalar_expr(
                source,
                row,
                base.as_deref(),
                when_then_clauses,
                else_expr.as_deref(),
            )?,
            ScalarExpr::Binary { left, op, right } => {
                let left = self.evaluate_scalar_expr(source, row, left)?;
                let right = self.evaluate_scalar_expr(source, row, right)?;
                self.evaluate_binary_scalar(*op, left, right)?
            }
            ScalarExpr::Function { func, args } => {
                if matches!(
                    func,
                    ScalarFunc::Coalesce | ScalarFunc::IfNull | ScalarFunc::IIf | ScalarFunc::If
                ) {
                    return self.evaluate_short_circuit_scalar_function(source, row, *func, args);
                }
                let args = args
                    .iter()
                    .map(|arg| self.evaluate_scalar_expr(source, row, arg))
                    .collect::<Result<Vec<_>>>()?;
                self.evaluate_scalar_function(*func, args)?
            }
            ScalarExpr::Tuple(_) => {
                return Err(DbError::plan("row value misused"));
            }
            ScalarExpr::Aggregate { func, arg, filter } => {
                let call = AggregateCall {
                    func: *func,
                    arg: arg.as_ref().clone(),
                    filter: filter.as_deref().cloned(),
                };
                self.lookup_value(&source.columns, row, &self.aggregate_call_name(&call))?
                    .clone()
            }
        })
    }

    fn evaluate_scalar_expr_in_context(
        &self,
        transaction_id: Option<TransactionId>,
        source: &RowSet,
        row: &Row,
        outer: Option<(&RowSet, &Row)>,
        expr: &ScalarExpr,
    ) -> Result<Value> {
        match expr {
            ScalarExpr::InSubquery {
                expr,
                query,
                negated,
            } => {
                let transaction_id = transaction_id.ok_or_else(|| {
                    DbError::plan("subquery scalar expressions require query context")
                })?;
                let subquery_outer = outer.or(Some((source, row)));
                if let ScalarExpr::Tuple(left_exprs) = expr.as_ref() {
                    let left = self.evaluate_scalar_tuple_expr(source, row, left_exprs)?;
                    let rows = self.execute_subquery_rows(transaction_id, query, subquery_outer)?;
                    return Ok(self.tuple_in_result_value(&left, &rows.rows, *negated));
                }
                let left = self.evaluate_scalar_expr(source, row, expr)?;
                let rows = self.execute_subquery(transaction_id, query, subquery_outer)?;
                Ok(Value::Boolean(
                    self.evaluate_in_rows(&left, &rows.rows, *negated),
                ))
            }
            ScalarExpr::CompareSubquery { left, op, query } => {
                let transaction_id = transaction_id.ok_or_else(|| {
                    DbError::plan("subquery scalar expressions require query context")
                })?;
                let subquery_outer = outer.or(Some((source, row)));
                if let ScalarExpr::Tuple(left_exprs) = left.as_ref() {
                    let left = self.evaluate_scalar_tuple_expr(source, row, left_exprs)?;
                    let right = self.tuple_subquery_value(transaction_id, query, subquery_outer)?;
                    return self.tuple_compare_result_value(&left, op, &right);
                }
                let collation = scalar_expr_collation(left);
                let left = self.evaluate_scalar_expr(source, row, left)?;
                let right = self.scalar_subquery_value(transaction_id, query, subquery_outer)?;
                self.compare_result_value_with_collation(&left, op, &right, collation)
            }
            ScalarExpr::Subquery { query } => {
                let transaction_id = transaction_id.ok_or_else(|| {
                    DbError::plan("subquery scalar expressions require query context")
                })?;
                self.scalar_subquery_value(transaction_id, query, outer.or(Some((source, row))))
            }
            _ if Self::scalar_expr_contains_subquery(expr) => {
                let transaction_id = transaction_id.ok_or_else(|| {
                    DbError::plan("subquery scalar expressions require query context")
                })?;
                let previous_txn = self.current_txn.get();
                self.current_txn.set(Some(transaction_id));
                let result = self.evaluate_filter_scalar_expr(source, row, outer, expr);
                self.current_txn.set(previous_txn);
                result
            }
            _ => self.evaluate_scalar_expr(source, row, expr),
        }
    }

    fn scalar_expr_contains_subquery(expr: &ScalarExpr) -> bool {
        match expr {
            ScalarExpr::InSubquery { .. }
            | ScalarExpr::Subquery { .. }
            | ScalarExpr::CompareSubquery { .. } => true,
            ScalarExpr::Tuple(values) | ScalarExpr::Function { args: values, .. } => {
                values.iter().any(Self::scalar_expr_contains_subquery)
            }
            ScalarExpr::UnaryMinus(expr)
            | ScalarExpr::BitNot(expr)
            | ScalarExpr::Not(expr)
            | ScalarExpr::Cast { expr, .. }
            | ScalarExpr::Collate { expr, .. }
            | ScalarExpr::IsBool { expr, .. }
            | ScalarExpr::Like { expr, .. }
            | ScalarExpr::Glob { expr, .. } => Self::scalar_expr_contains_subquery(expr),
            ScalarExpr::Is { left, right, .. }
            | ScalarExpr::Compare { left, right, .. }
            | ScalarExpr::Binary { left, right, .. } => {
                Self::scalar_expr_contains_subquery(left)
                    || Self::scalar_expr_contains_subquery(right)
            }
            ScalarExpr::InList { expr, values, .. } => {
                Self::scalar_expr_contains_subquery(expr)
                    || values.iter().any(Self::scalar_expr_contains_subquery)
            }
            ScalarExpr::Between {
                expr, low, high, ..
            } => {
                Self::scalar_expr_contains_subquery(expr)
                    || Self::scalar_expr_contains_subquery(low)
                    || Self::scalar_expr_contains_subquery(high)
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                base.as_deref()
                    .is_some_and(Self::scalar_expr_contains_subquery)
                    || when_then_clauses.iter().any(|(when_expr, then_expr)| {
                        Self::scalar_expr_contains_subquery(when_expr)
                            || Self::scalar_expr_contains_subquery(then_expr)
                    })
                    || else_expr
                        .as_deref()
                        .is_some_and(Self::scalar_expr_contains_subquery)
            }
            ScalarExpr::Aggregate { arg, filter, .. } => {
                Self::aggregate_arg_contains_subquery(arg)
                    || filter
                        .as_deref()
                        .is_some_and(Self::filter_expr_contains_subquery)
            }
            ScalarExpr::Literal(_) | ScalarExpr::Column(_) => false,
        }
    }

    fn aggregate_arg_contains_subquery(arg: &AggregateArg) -> bool {
        match arg {
            AggregateArg::Wildcard => false,
            AggregateArg::Expr { expr, order_by, .. } => {
                Self::scalar_expr_contains_subquery(expr)
                    || order_by.iter().any(Self::order_by_contains_subquery)
            }
            AggregateArg::GroupConcat {
                expr,
                separator,
                order_by,
                ..
            } => {
                Self::scalar_expr_contains_subquery(expr)
                    || separator
                        .as_ref()
                        .is_some_and(Self::scalar_expr_contains_subquery)
                    || order_by.iter().any(Self::order_by_contains_subquery)
            }
            AggregateArg::JsonGroupObject {
                key,
                value,
                order_by,
            } => {
                Self::scalar_expr_contains_subquery(key)
                    || Self::scalar_expr_contains_subquery(value)
                    || order_by.iter().any(Self::order_by_contains_subquery)
            }
            AggregateArg::Percentile {
                expr,
                fraction,
                order_by,
            } => {
                Self::scalar_expr_contains_subquery(expr)
                    || Self::scalar_expr_contains_subquery(fraction)
                    || order_by.iter().any(Self::order_by_contains_subquery)
            }
        }
    }

    fn order_by_contains_subquery(order_by: &OrderBy) -> bool {
        match &order_by.expr {
            OrderByExpr::Expr(expr) => Self::scalar_expr_contains_subquery(expr),
            OrderByExpr::Column(_) | OrderByExpr::Position(_) => false,
        }
    }

    fn filter_expr_contains_subquery(expr: &Expr) -> bool {
        match expr {
            Expr::InSubquery { .. }
            | Expr::CompareSubquery { .. }
            | Expr::ExistsSubquery { .. } => true,
            Expr::InSubqueryScalar { expr, .. } => Self::scalar_expr_contains_subquery(expr),
            Expr::CompareSubqueryScalar { left, .. } => Self::scalar_expr_contains_subquery(left),
            Expr::CompareScalar { left, right, .. } | Expr::Is { left, right, .. } => {
                Self::scalar_expr_contains_subquery(left)
                    || Self::scalar_expr_contains_subquery(right)
            }
            Expr::IsNullScalar { expr, .. }
            | Expr::IsBool { expr, .. }
            | Expr::LikeScalar { expr, .. }
            | Expr::GlobScalar { expr, .. } => Self::scalar_expr_contains_subquery(expr),
            Expr::InListScalar { expr, values, .. } => {
                Self::scalar_expr_contains_subquery(expr)
                    || values.iter().any(Self::scalar_expr_contains_subquery)
            }
            Expr::BetweenScalar {
                expr, low, high, ..
            } => {
                Self::scalar_expr_contains_subquery(expr)
                    || Self::scalar_expr_contains_subquery(low)
                    || Self::scalar_expr_contains_subquery(high)
            }
            Expr::Not(expr) => Self::filter_expr_contains_subquery(expr),
            Expr::And(left, right) | Expr::Or(left, right) => {
                Self::filter_expr_contains_subquery(left)
                    || Self::filter_expr_contains_subquery(right)
            }
            Expr::Compare { .. }
            | Expr::CompareColumns { .. }
            | Expr::IsNull { .. }
            | Expr::InList { .. }
            | Expr::Like { .. }
            | Expr::Glob { .. }
            | Expr::Between { .. } => false,
        }
    }

    fn evaluate_scalar_tuple_expr(
        &self,
        source: &RowSet,
        row: &Row,
        exprs: &[ScalarExpr],
    ) -> Result<Vec<Value>> {
        exprs
            .iter()
            .map(|expr| self.evaluate_scalar_expr(source, row, expr))
            .collect()
    }

    fn evaluate_filter_scalar_expr(
        &self,
        source: &RowSet,
        row: &Row,
        outer: Option<(&RowSet, &Row)>,
        expr: &ScalarExpr,
    ) -> Result<Value> {
        Ok(match expr {
            ScalarExpr::Literal(value) => value.clone(),
            ScalarExpr::Column(name) => self.lookup_filter_value(source, row, outer, name)?,
            ScalarExpr::BitNot(expr) => {
                match self.evaluate_filter_scalar_expr(source, row, outer, expr)? {
                    Value::Null => Value::Null,
                    value => Value::Integer(!Self::sqlite_bitwise_integer_arg(&value)?),
                }
            }
            ScalarExpr::Not(expr) => {
                match self.evaluate_filter_scalar_expr(source, row, outer, expr)? {
                    value => Self::sqlite_not_value(&value),
                }
            }
            ScalarExpr::UnaryMinus(expr) => {
                match self.evaluate_filter_scalar_expr(source, row, outer, expr)? {
                    Value::Null => Value::Null,
                    value => match Self::coerce_sqlite_numeric_value(&value) {
                        Value::Integer(value) => Value::Integer(-value),
                        Value::Real(value) => Value::Real(-value),
                        Value::Null => Value::Null,
                        _ => unreachable!("sqlite numeric coercion only returns numeric values"),
                    },
                }
            }
            ScalarExpr::Cast { expr, ty } => {
                let value = self.evaluate_filter_scalar_expr(source, row, outer, expr)?;
                Self::cast_value(value, *ty)?
            }
            ScalarExpr::Collate { expr, .. } => {
                self.evaluate_filter_scalar_expr(source, row, outer, expr)?
            }
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => {
                if let (ScalarExpr::Tuple(left_exprs), ScalarExpr::Tuple(right_exprs)) =
                    (left.as_ref(), right.as_ref())
                {
                    let left =
                        self.evaluate_filter_scalar_tuple_expr(source, row, outer, left_exprs)?;
                    let right =
                        self.evaluate_filter_scalar_tuple_expr(source, row, outer, right_exprs)?;
                    return Ok(Value::Boolean(
                        self.tuple_is_with_negation(&left, &right, *negated),
                    ));
                }
                let left = self.evaluate_filter_scalar_expr(source, row, outer, left)?;
                let right = self.evaluate_filter_scalar_expr(source, row, outer, right)?;
                Value::Boolean(self.is_with_negation(&left, &right, *negated))
            }
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => {
                let matches = match self.evaluate_filter_scalar_expr(source, row, outer, expr)? {
                    Value::Null => false,
                    evaluated => Self::sqlite_is_true_value(&evaluated) == *value,
                };
                Value::Boolean(matches ^ *negated)
            }
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => {
                if let ScalarExpr::Tuple(left_exprs) = expr.as_ref() {
                    let left =
                        self.evaluate_filter_scalar_tuple_expr(source, row, outer, left_exprs)?;
                    let candidates = values
                        .iter()
                        .map(|value| match value {
                            ScalarExpr::Tuple(values) => {
                                self.evaluate_filter_scalar_tuple_expr(source, row, outer, values)
                            }
                            value => self
                                .evaluate_filter_scalar_expr(source, row, outer, value)
                                .map(|value| vec![value]),
                        })
                        .collect::<Result<Vec<_>>>()?;
                    return Ok(self.tuple_in_result_value(&left, &candidates, *negated));
                }
                let left = self.evaluate_filter_scalar_expr(source, row, outer, expr)?;
                let values = values
                    .iter()
                    .map(|value| self.evaluate_filter_scalar_expr(source, row, outer, value))
                    .collect::<Result<Vec<_>>>()?;
                self.in_result_value(&left, &values, *negated)
            }
            ScalarExpr::InSubquery {
                expr,
                query,
                negated,
            } => {
                let left = self.evaluate_filter_scalar_expr(source, row, outer, expr)?;
                let transaction_id = self.current_txn.get().ok_or_else(|| {
                    DbError::plan("subquery scalar expressions require query context")
                })?;
                let rows = self.execute_subquery(transaction_id, query, Some((source, row)))?;
                Value::Boolean(self.evaluate_in_rows(&left, &rows.rows, *negated))
            }
            ScalarExpr::CompareSubquery { left, op, query } => {
                let left_value = self.evaluate_filter_scalar_expr(source, row, outer, left)?;
                let transaction_id = self.current_txn.get().ok_or_else(|| {
                    DbError::plan("subquery scalar expressions require query context")
                })?;
                let right =
                    self.scalar_subquery_value(transaction_id, query, Some((source, row)))?;
                self.compare_result_value_with_collation(
                    &left_value,
                    op,
                    &right,
                    scalar_expr_collation(left),
                )?
            }
            ScalarExpr::Subquery { query } => {
                let transaction_id = self.current_txn.get().ok_or_else(|| {
                    DbError::plan("subquery scalar expressions require query context")
                })?;
                self.scalar_subquery_value(transaction_id, query, Some((source, row)))?
            }
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => match self.evaluate_filter_scalar_expr(source, row, outer, expr)? {
                Value::Text(value) => Value::Boolean(
                    Self::matches_like_pattern(
                        &value,
                        pattern,
                        escape,
                        self.storage.case_sensitive_like(),
                    )? ^ *negated,
                ),
                Value::Null => Value::Null,
                _ => Value::Null,
            },
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => match self.evaluate_filter_scalar_expr(source, row, outer, expr)? {
                Value::Text(value) => {
                    Value::Boolean(Self::matches_glob_pattern(&value, pattern) ^ *negated)
                }
                Value::Null => Value::Null,
                _ => Value::Null,
            },
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                if let (
                    ScalarExpr::Tuple(value_exprs),
                    ScalarExpr::Tuple(low_exprs),
                    ScalarExpr::Tuple(high_exprs),
                ) = (expr.as_ref(), low.as_ref(), high.as_ref())
                {
                    let value =
                        self.evaluate_filter_scalar_tuple_expr(source, row, outer, value_exprs)?;
                    let low =
                        self.evaluate_filter_scalar_tuple_expr(source, row, outer, low_exprs)?;
                    let high =
                        self.evaluate_filter_scalar_tuple_expr(source, row, outer, high_exprs)?;
                    return self.tuple_between_result_value(&value, &low, &high, *negated);
                }
                let value = self.evaluate_filter_scalar_expr(source, row, outer, expr)?;
                let low = self.evaluate_filter_scalar_expr(source, row, outer, low)?;
                let high = self.evaluate_filter_scalar_expr(source, row, outer, high)?;
                let Some(low_cmp) = self.compare(&value, &low)? else {
                    return Ok(Value::Null);
                };
                let Some(high_cmp) = self.compare(&value, &high)? else {
                    return Ok(Value::Null);
                };
                let matches = matches!(low_cmp, Ordering::Greater | Ordering::Equal)
                    && matches!(high_cmp, Ordering::Less | Ordering::Equal);
                Value::Boolean(matches ^ *negated)
            }
            ScalarExpr::Compare { left, op, right } => {
                if let (ScalarExpr::Tuple(left_exprs), ScalarExpr::Tuple(right_exprs)) =
                    (left.as_ref(), right.as_ref())
                {
                    let left =
                        self.evaluate_filter_scalar_tuple_expr(source, row, outer, left_exprs)?;
                    let right =
                        self.evaluate_filter_scalar_tuple_expr(source, row, outer, right_exprs)?;
                    return self.tuple_compare_result_value(&left, op, &right);
                }
                let left_value = self.evaluate_filter_scalar_expr(source, row, outer, left)?;
                let right_value = self.evaluate_filter_scalar_expr(source, row, outer, right)?;
                self.compare_result_value_with_collation(
                    &left_value,
                    op,
                    &right_value,
                    scalar_expr_collation(left).or_else(|| scalar_expr_collation(right)),
                )?
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => self.evaluate_filter_case_scalar_expr(
                source,
                row,
                outer,
                base.as_deref(),
                when_then_clauses,
                else_expr.as_deref(),
            )?,
            ScalarExpr::Binary { left, op, right } => {
                let left = self.evaluate_filter_scalar_expr(source, row, outer, left)?;
                let right = self.evaluate_filter_scalar_expr(source, row, outer, right)?;
                self.evaluate_binary_scalar(*op, left, right)?
            }
            ScalarExpr::Function { func, args } => {
                if matches!(
                    func,
                    ScalarFunc::Coalesce | ScalarFunc::IfNull | ScalarFunc::IIf | ScalarFunc::If
                ) {
                    return self.evaluate_filter_short_circuit_scalar_function(
                        source, row, outer, *func, args,
                    );
                }
                let args = args
                    .iter()
                    .map(|arg| self.evaluate_filter_scalar_expr(source, row, outer, arg))
                    .collect::<Result<Vec<_>>>()?;
                self.evaluate_scalar_function(*func, args)?
            }
            ScalarExpr::Tuple(_) => {
                return Err(DbError::plan("row value misused"));
            }
            ScalarExpr::Aggregate { func, arg, filter } => {
                let call = AggregateCall {
                    func: *func,
                    arg: arg.as_ref().clone(),
                    filter: filter.as_deref().cloned(),
                };
                self.lookup_filter_value(source, row, outer, &self.aggregate_call_name(&call))?
            }
        })
    }

    fn evaluate_filter_scalar_tuple_expr(
        &self,
        source: &RowSet,
        row: &Row,
        outer: Option<(&RowSet, &Row)>,
        exprs: &[ScalarExpr],
    ) -> Result<Vec<Value>> {
        exprs
            .iter()
            .map(|expr| self.evaluate_filter_scalar_expr(source, row, outer, expr))
            .collect()
    }

    fn evaluate_filter_short_circuit_scalar_function(
        &self,
        source: &RowSet,
        row: &Row,
        outer: Option<(&RowSet, &Row)>,
        func: ScalarFunc,
        args: &[ScalarExpr],
    ) -> Result<Value> {
        match func {
            ScalarFunc::Coalesce => {
                if args.len() < 2 {
                    return Err(DbError::plan("COALESCE expects at least 2 arguments"));
                }
                for arg in args {
                    let value = self.evaluate_filter_scalar_expr(source, row, outer, arg)?;
                    if !matches!(value, Value::Null) {
                        return Ok(value);
                    }
                }
                Ok(Value::Null)
            }
            ScalarFunc::IfNull => {
                if args.len() != 2 {
                    return Err(DbError::plan(format!(
                        "IFNULL expects 2 arguments but got {}",
                        args.len()
                    )));
                }
                let first = self.evaluate_filter_scalar_expr(source, row, outer, &args[0])?;
                if matches!(first, Value::Null) {
                    self.evaluate_filter_scalar_expr(source, row, outer, &args[1])
                } else {
                    Ok(first)
                }
            }
            ScalarFunc::IIf | ScalarFunc::If => {
                let function_name = Self::scalar_function_name(func);
                if args.len() < 2 {
                    return Err(DbError::plan(format!(
                        "{function_name} expects at least 2 arguments but got {}",
                        args.len()
                    )));
                }
                let pair_count = args.len() / 2;
                for pair_index in 0..pair_count {
                    let condition_index = pair_index * 2;
                    let condition = Self::cast_value(
                        self.evaluate_filter_scalar_expr(
                            source,
                            row,
                            outer,
                            &args[condition_index],
                        )?,
                        ColumnType::Boolean,
                    )?;
                    if matches!(condition, Value::Boolean(true)) {
                        return self.evaluate_filter_scalar_expr(
                            source,
                            row,
                            outer,
                            &args[condition_index + 1],
                        );
                    }
                }
                if args.len() % 2 == 1 {
                    self.evaluate_filter_scalar_expr(
                        source,
                        row,
                        outer,
                        args.last().expect("odd IIF arity has default argument"),
                    )
                } else {
                    Ok(Value::Null)
                }
            }
            _ => unreachable!("only short-circuit scalar functions are dispatched here"),
        }
    }

    fn evaluate_short_circuit_scalar_function(
        &self,
        source: &RowSet,
        row: &Row,
        func: ScalarFunc,
        args: &[ScalarExpr],
    ) -> Result<Value> {
        match func {
            ScalarFunc::Coalesce => {
                if args.len() < 2 {
                    return Err(DbError::plan("COALESCE expects at least 2 arguments"));
                }
                for arg in args {
                    let value = self.evaluate_scalar_expr(source, row, arg)?;
                    if !matches!(value, Value::Null) {
                        return Ok(value);
                    }
                }
                Ok(Value::Null)
            }
            ScalarFunc::IfNull => {
                if args.len() != 2 {
                    return Err(DbError::plan(format!(
                        "IFNULL expects 2 arguments but got {}",
                        args.len()
                    )));
                }
                let first = self.evaluate_scalar_expr(source, row, &args[0])?;
                if matches!(first, Value::Null) {
                    self.evaluate_scalar_expr(source, row, &args[1])
                } else {
                    Ok(first)
                }
            }
            ScalarFunc::IIf | ScalarFunc::If => {
                let function_name = Self::scalar_function_name(func);
                if args.len() < 2 {
                    return Err(DbError::plan(format!(
                        "{function_name} expects at least 2 arguments but got {}",
                        args.len()
                    )));
                }
                let pair_count = args.len() / 2;
                for pair_index in 0..pair_count {
                    let condition_index = pair_index * 2;
                    let condition = Self::cast_value(
                        self.evaluate_scalar_expr(source, row, &args[condition_index])?,
                        ColumnType::Boolean,
                    )?;
                    if matches!(condition, Value::Boolean(true)) {
                        return self.evaluate_scalar_expr(source, row, &args[condition_index + 1]);
                    }
                }
                if args.len() % 2 == 1 {
                    self.evaluate_scalar_expr(
                        source,
                        row,
                        args.last().expect("odd IIF arity has default argument"),
                    )
                } else {
                    Ok(Value::Null)
                }
            }
            _ => unreachable!("non-short-circuit scalar function"),
        }
    }

    fn evaluate_case_scalar_expr(
        &self,
        source: &RowSet,
        row: &Row,
        base: Option<&ScalarExpr>,
        when_then_clauses: &[(ScalarExpr, ScalarExpr)],
        else_expr: Option<&ScalarExpr>,
    ) -> Result<Value> {
        if let Some(base) = base {
            let base_value = self.evaluate_scalar_expr(source, row, base)?;
            for (when_expr, then_expr) in when_then_clauses {
                let when_value = self.evaluate_scalar_expr(source, row, when_expr)?;
                if self.compare(&base_value, &when_value)? == Some(Ordering::Equal) {
                    return self.evaluate_scalar_expr(source, row, then_expr);
                }
            }
        } else {
            for (when_expr, then_expr) in when_then_clauses {
                let condition = Self::cast_value(
                    self.evaluate_scalar_expr(source, row, when_expr)?,
                    ColumnType::Boolean,
                )?;
                if matches!(condition, Value::Boolean(true)) {
                    return self.evaluate_scalar_expr(source, row, then_expr);
                }
            }
        }

        if let Some(else_expr) = else_expr {
            self.evaluate_scalar_expr(source, row, else_expr)
        } else {
            Ok(Value::Null)
        }
    }

    fn evaluate_filter_case_scalar_expr(
        &self,
        source: &RowSet,
        row: &Row,
        outer: Option<(&RowSet, &Row)>,
        base: Option<&ScalarExpr>,
        when_then_clauses: &[(ScalarExpr, ScalarExpr)],
        else_expr: Option<&ScalarExpr>,
    ) -> Result<Value> {
        if let Some(base) = base {
            let base_value = self.evaluate_filter_scalar_expr(source, row, outer, base)?;
            for (when_expr, then_expr) in when_then_clauses {
                let when_value = self.evaluate_filter_scalar_expr(source, row, outer, when_expr)?;
                if self.compare(&base_value, &when_value)? == Some(Ordering::Equal) {
                    return self.evaluate_filter_scalar_expr(source, row, outer, then_expr);
                }
            }
        } else {
            for (when_expr, then_expr) in when_then_clauses {
                let condition = Self::cast_value(
                    self.evaluate_filter_scalar_expr(source, row, outer, when_expr)?,
                    ColumnType::Boolean,
                )?;
                if matches!(condition, Value::Boolean(true)) {
                    return self.evaluate_filter_scalar_expr(source, row, outer, then_expr);
                }
            }
        }

        if let Some(else_expr) = else_expr {
            self.evaluate_filter_scalar_expr(source, row, outer, else_expr)
        } else {
            Ok(Value::Null)
        }
    }

    fn evaluate_scalar_function(&self, func: ScalarFunc, args: Vec<Value>) -> Result<Value> {
        match func {
            ScalarFunc::Length => {
                Self::expect_arity("LENGTH", &args, 1)?;
                match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::Text(value) => Ok(Value::Integer(value.chars().count() as i64)),
                    Value::Blob(value) => Ok(Value::Integer(value.len() as i64)),
                    value => Ok(Value::Integer(
                        Self::coerce_text_like_value(value).chars().count() as i64,
                    )),
                }
            }
            ScalarFunc::OctetLength => {
                Self::expect_arity("OCTET_LENGTH", &args, 1)?;
                match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::Blob(value) => Ok(Value::Integer(value.len() as i64)),
                    Value::Text(value) => Ok(Value::Integer(value.len() as i64)),
                    Value::Integer(value) => Ok(Value::Integer(value.to_string().len() as i64)),
                    Value::Real(value) => Ok(Value::Integer(
                        Self::sqlite_real_to_text(*value).len() as i64,
                    )),
                    Value::Boolean(_) => Ok(Value::Integer(1)),
                }
            }
            ScalarFunc::Date => {
                Self::evaluate_date_time_family_function("DATE", &args, DateTimeResultKind::Date)
            }
            ScalarFunc::Time => {
                Self::evaluate_date_time_family_function("TIME", &args, DateTimeResultKind::Time)
            }
            ScalarFunc::DateTime => Self::evaluate_date_time_family_function(
                "DATETIME",
                &args,
                DateTimeResultKind::DateTime,
            ),
            ScalarFunc::TimeDiff => Self::evaluate_timediff_function(&args),
            ScalarFunc::Strftime => {
                if args.is_empty() {
                    return Ok(Value::Null);
                }
                if args.len() > 1 {
                    return Self::evaluate_strftime_function(&args);
                }
                let args = [args[0].clone(), Value::from("now")];
                Self::evaluate_strftime_function(&args)
            }
            ScalarFunc::JulianDay => Self::evaluate_julianday_function(&args),
            ScalarFunc::UnixEpoch => Self::evaluate_unixepoch_function(&args),
            ScalarFunc::MinScalar => {
                if args.is_empty() {
                    return Err(DbError::plan(format!("MIN expects at least 1 argument")));
                }
                self.evaluate_min_max_scalar_function("MIN", &args, true)
            }
            ScalarFunc::MaxScalar => {
                if args.is_empty() {
                    return Err(DbError::plan("MAX expects at least 1 argument"));
                }
                self.evaluate_min_max_scalar_function("MAX", &args, false)
            }
            ScalarFunc::Changes => {
                Self::expect_arity("CHANGES", &args, 0)?;
                Ok(Value::Integer(self.changes.get()))
            }
            ScalarFunc::TotalChanges => {
                Self::expect_arity("TOTAL_CHANGES", &args, 0)?;
                Ok(Value::Integer(self.total_changes.get()))
            }
            ScalarFunc::Printf => {
                if args.is_empty() {
                    return Err(DbError::plan("PRINTF expects at least 1 argument"));
                }

                let format = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };

                Ok(Value::Text(Self::sqlite_printf(&format, &args[1..])?))
            }
            ScalarFunc::Concat => {
                let mut result = String::new();
                for arg in args {
                    match arg {
                        Value::Null => {}
                        value => result.push_str(&Self::coerce_text_like_value(&value)),
                    }
                }
                Ok(Value::Text(result))
            }
            ScalarFunc::ConcatWs => {
                if args.is_empty() {
                    return Err(DbError::plan("CONCAT_WS expects at least 1 argument"));
                }

                let separator = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };

                let mut parts = Vec::new();
                for arg in args.into_iter().skip(1) {
                    match arg {
                        Value::Null => {}
                        value => parts.push(Self::coerce_text_like_value(&value)),
                    }
                }

                Ok(Value::Text(parts.join(&separator)))
            }
            ScalarFunc::Sign => {
                Self::expect_arity("SIGN", &args, 1)?;
                match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::Boolean(value) => Ok(Value::Integer(if *value { 1 } else { 0 })),
                    Value::Integer(value) => Ok(Value::Integer(value.signum())),
                    Value::Real(value) => Ok(Value::Integer(if *value > 0.0 {
                        1
                    } else if *value < 0.0 {
                        -1
                    } else {
                        0
                    })),
                    Value::Text(value) => {
                        if let Ok(value) = value.parse::<i64>() {
                            Ok(Value::Integer(value.signum()))
                        } else if let Ok(value) = value.parse::<f64>() {
                            Ok(Value::Integer(if value > 0.0 {
                                1
                            } else if value < 0.0 {
                                -1
                            } else {
                                0
                            }))
                        } else {
                            Ok(Value::Null)
                        }
                    }
                    Value::Blob(_) => Ok(Value::Null),
                }
            }
            ScalarFunc::Random => {
                Self::expect_arity("RANDOM", &args, 0)?;
                Ok(Value::Integer(random_i64()?))
            }
            ScalarFunc::Unhex => {
                if !matches!(args.len(), 1 | 2) {
                    return Err(DbError::plan(format!(
                        "UNHEX expects 1 or 2 arguments but got {}",
                        args.len()
                    )));
                }

                let value = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };

                let ignore = if args.len() == 2 {
                    match &args[1] {
                        Value::Null => return Ok(Value::Null),
                        value => Some(Self::coerce_text_like_value(value)),
                    }
                } else {
                    None
                };

                let mut filtered = String::with_capacity(value.len());
                for ch in value.chars() {
                    if ignore.as_ref().is_some_and(|ignore| ignore.contains(ch)) {
                        continue;
                    }
                    filtered.push(ch);
                }

                if filtered.len() % 2 != 0 {
                    return Ok(Value::Null);
                }

                let mut bytes = Vec::with_capacity(filtered.len() / 2);
                let chars = filtered.as_bytes();
                for pair in chars.chunks_exact(2) {
                    let high = hex_nibble(pair[0]);
                    let low = hex_nibble(pair[1]);
                    let Some((high, low)) = high.zip(low) else {
                        return Ok(Value::Null);
                    };
                    bytes.push((high << 4) | low);
                }
                Ok(Value::Blob(bytes))
            }
            ScalarFunc::Unistr => {
                Self::expect_arity("UNISTR", &args, 1)?;
                let value = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };
                Ok(Value::Text(sqlite_unistr(&value)?))
            }
            ScalarFunc::UnistrQuote => {
                Self::expect_arity("UNISTR_QUOTE", &args, 1)?;
                Ok(Value::Text(sqlite_unistr_quote(&args[0])))
            }
            ScalarFunc::RandomBlob => {
                Self::expect_arity("RANDOMBLOB", &args, 1)?;
                let length = match Self::cast_value(
                    args[0].clone(),
                    crate::common::types::ColumnType::Integer,
                )? {
                    Value::Null => return Ok(Value::Null),
                    Value::Integer(value) => value,
                    _ => unreachable!("integer cast must yield INTEGER or NULL"),
                };
                let length = if length <= 0 { 1 } else { length };
                let length = usize::try_from(length)
                    .map_err(|_| DbError::plan("RANDOMBLOB length is too large"))?;
                Ok(Value::Blob(random_bytes(length)?))
            }
            ScalarFunc::SqliteSourceId => {
                Self::expect_arity("SQLITE_SOURCE_ID", &args, 0)?;
                Ok(Value::Text(
                    "2025-06-12 13:14:41 f0ca7bba1c5e232e5d279fad6338121ab55af0c8c68c84cdfb18ba5114dcaapl"
                        .to_string(),
                ))
            }
            ScalarFunc::SqliteVersion => {
                Self::expect_arity("SQLITE_VERSION", &args, 0)?;
                Ok(Value::Text("3.46.0".to_string()))
            }
            ScalarFunc::SqliteCompileOptionUsed => {
                Self::expect_arity("SQLITE_COMPILEOPTION_USED", &args, 1)?;
                if matches!(args[0], Value::Null) {
                    return Ok(Value::Null);
                }
                let requested = Self::coerce_text_like_value(&args[0]);
                Ok(Value::Integer(if sqlite_compile_option_used(&requested) {
                    1
                } else {
                    0
                }))
            }
            ScalarFunc::SqliteCompileOptionGet => {
                Self::expect_arity("SQLITE_COMPILEOPTION_GET", &args, 1)?;
                let index = match Self::cast_value(args[0].clone(), ColumnType::Integer)? {
                    Value::Null => 0,
                    Value::Integer(value) => value,
                    _ => unreachable!("integer cast must yield INTEGER or NULL"),
                };
                let option = usize::try_from(index)
                    .ok()
                    .and_then(|index| SQLITE_COMPILE_OPTIONS.get(index));
                Ok(option.map_or(Value::Null, |option| Value::from(*option)))
            }
            ScalarFunc::Likely => {
                Self::expect_arity("LIKELY", &args, 1)?;
                Ok(args.into_iter().next().unwrap_or(Value::Null))
            }
            ScalarFunc::Unlikely => {
                Self::expect_arity("UNLIKELY", &args, 1)?;
                Ok(args.into_iter().next().unwrap_or(Value::Null))
            }
            ScalarFunc::Likelihood => {
                Self::expect_arity("LIKELIHOOD", &args, 2)?;
                Ok(args.into_iter().next().unwrap_or(Value::Null))
            }
            ScalarFunc::Mod => {
                Self::expect_arity("MOD", &args, 2)?;
                let mut args = args.into_iter();
                let left = args.next().unwrap_or(Value::Null);
                let right = args.next().unwrap_or(Value::Null);
                Self::sqlite_mod_function(left, right)
            }
            ScalarFunc::Ceil => {
                Self::expect_arity("CEIL", &args, 1)?;
                Self::sqlite_rounding_function(&args[0], "CEIL", f64::ceil)
            }
            ScalarFunc::Ceiling => {
                Self::expect_arity("CEILING", &args, 1)?;
                Self::sqlite_rounding_function(&args[0], "CEILING", f64::ceil)
            }
            ScalarFunc::Floor => {
                Self::expect_arity("FLOOR", &args, 1)?;
                Self::sqlite_rounding_function(&args[0], "FLOOR", f64::floor)
            }
            ScalarFunc::Trunc => {
                Self::expect_arity("TRUNC", &args, 1)?;
                Self::sqlite_rounding_function(&args[0], "TRUNC", f64::trunc)
            }
            ScalarFunc::Pi => {
                Self::expect_arity("PI", &args, 0)?;
                Ok(Value::Real(std::f64::consts::PI))
            }
            ScalarFunc::Sqrt => {
                Self::expect_arity("SQRT", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "SQRT", |value| {
                    if value < 0.0 {
                        None
                    } else {
                        Some(value.sqrt())
                    }
                })
            }
            ScalarFunc::Power => {
                Self::expect_arity("POWER", &args, 2)?;
                Self::sqlite_binary_math_function(&args[0], &args[1], "POWER", |left, right| {
                    Some(left.powf(right))
                })
            }
            ScalarFunc::Exp => {
                Self::expect_arity("EXP", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "EXP", |value| Some(value.exp()))
            }
            ScalarFunc::Sin => {
                Self::expect_arity("SIN", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "SIN", |value| Some(value.sin()))
            }
            ScalarFunc::Cos => {
                Self::expect_arity("COS", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "COS", |value| Some(value.cos()))
            }
            ScalarFunc::Tan => {
                Self::expect_arity("TAN", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "TAN", |value| Some(value.tan()))
            }
            ScalarFunc::Sinh => {
                Self::expect_arity("SINH", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "SINH", |value| Some(value.sinh()))
            }
            ScalarFunc::Cosh => {
                Self::expect_arity("COSH", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "COSH", |value| Some(value.cosh()))
            }
            ScalarFunc::Tanh => {
                Self::expect_arity("TANH", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "TANH", |value| Some(value.tanh()))
            }
            ScalarFunc::Acos => {
                Self::expect_arity("ACOS", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "ACOS", |value| {
                    if !(-1.0..=1.0).contains(&value) {
                        None
                    } else {
                        Some(value.acos())
                    }
                })
            }
            ScalarFunc::Asin => {
                Self::expect_arity("ASIN", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "ASIN", |value| {
                    if !(-1.0..=1.0).contains(&value) {
                        None
                    } else {
                        Some(value.asin())
                    }
                })
            }
            ScalarFunc::Atan => {
                Self::expect_arity("ATAN", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "ATAN", |value| Some(value.atan()))
            }
            ScalarFunc::Atan2 => {
                Self::expect_arity("ATAN2", &args, 2)?;
                Self::sqlite_binary_math_function(&args[0], &args[1], "ATAN2", |left, right| {
                    Some(left.atan2(right))
                })
            }
            ScalarFunc::Acosh => {
                Self::expect_arity("ACOSH", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "ACOSH", |value| {
                    if value < 1.0 {
                        None
                    } else {
                        Some(value.acosh())
                    }
                })
            }
            ScalarFunc::Asinh => {
                Self::expect_arity("ASINH", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "ASINH", |value| Some(value.asinh()))
            }
            ScalarFunc::Atanh => {
                Self::expect_arity("ATANH", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "ATANH", |value| {
                    if value <= -1.0 || value >= 1.0 {
                        None
                    } else {
                        Some(value.atanh())
                    }
                })
            }
            ScalarFunc::Ln => {
                Self::expect_arity("LN", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "LN", |value| {
                    if value <= 0.0 { None } else { Some(value.ln()) }
                })
            }
            ScalarFunc::Log10 => {
                Self::expect_arity("LOG10", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "LOG10", |value| {
                    if value <= 0.0 {
                        None
                    } else {
                        Some(value.log10())
                    }
                })
            }
            ScalarFunc::Log2 => {
                Self::expect_arity("LOG2", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "LOG2", |value| {
                    if value <= 0.0 {
                        None
                    } else {
                        Some(value.log2())
                    }
                })
            }
            ScalarFunc::Log => {
                if !matches!(args.len(), 1 | 2) {
                    return Err(DbError::plan(format!(
                        "LOG expects 1 or 2 arguments but got {}",
                        args.len()
                    )));
                }
                if args.len() == 1 {
                    Self::sqlite_unary_math_function(&args[0], "LOG", |value| {
                        if value <= 0.0 {
                            None
                        } else {
                            Some(value.log10())
                        }
                    })
                } else {
                    Self::sqlite_binary_math_function(&args[0], &args[1], "LOG", |base, value| {
                        if base <= 0.0 || value <= 0.0 || base == 1.0 {
                            None
                        } else {
                            Some(value.log(base))
                        }
                    })
                }
            }
            ScalarFunc::Degrees => {
                Self::expect_arity("DEGREES", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "DEGREES", |value| {
                    Some(value.to_degrees())
                })
            }
            ScalarFunc::Radians => {
                Self::expect_arity("RADIANS", &args, 1)?;
                Self::sqlite_unary_math_function(&args[0], "RADIANS", |value| {
                    Some(value.to_radians())
                })
            }
            ScalarFunc::Char => {
                let mut result = String::new();
                for arg in args {
                    let code_point =
                        match Self::cast_value(arg, crate::common::types::ColumnType::Integer)? {
                            Value::Null => 0,
                            Value::Integer(value) => value,
                            _ => unreachable!("integer cast must yield INTEGER or NULL"),
                        };

                    let normalized = if code_point < 0 {
                        0
                    } else {
                        u32::try_from(code_point)
                            .map_err(|_| DbError::plan("CHAR code point out of range"))?
                    };
                    let ch = char::from_u32(normalized)
                        .ok_or_else(|| DbError::plan("CHAR received invalid code point"))?;
                    result.push(ch);
                }
                Ok(Value::Text(result))
            }
            ScalarFunc::ZeroBlob => {
                Self::expect_arity("ZEROBLOB", &args, 1)?;
                let length = match Self::cast_value(
                    args[0].clone(),
                    crate::common::types::ColumnType::Integer,
                )? {
                    Value::Null => return Ok(Value::Null),
                    Value::Integer(value) => value,
                    _ => unreachable!("integer cast must yield INTEGER or NULL"),
                };
                if length < 0 {
                    return Err(DbError::plan("ZEROBLOB length must be non-negative"));
                }
                let length = usize::try_from(length)
                    .map_err(|_| DbError::plan("ZEROBLOB length is too large"))?;
                Ok(Value::Blob(vec![0; length]))
            }
            ScalarFunc::TypeOf => {
                Self::expect_arity("TYPEOF", &args, 1)?;
                let sqlite_type_name = match &args[0] {
                    Value::Null => "null",
                    Value::Boolean(_) | Value::Integer(_) => "integer",
                    Value::Real(_) => "real",
                    Value::Blob(_) => "blob",
                    Value::Text(_) => "text",
                };
                Ok(Value::Text(sqlite_type_name.to_string()))
            }
            ScalarFunc::Subtype => {
                Self::expect_arity("SUBTYPE", &args, 1)?;
                Ok(Value::Integer(0))
            }
            ScalarFunc::Hex => {
                Self::expect_arity("HEX", &args, 1)?;
                match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::Blob(value) => Ok(Value::Text(
                        value
                            .iter()
                            .map(|byte| format!("{byte:02X}"))
                            .collect::<String>(),
                    )),
                    Value::Text(value) => Ok(Value::Text(
                        value
                            .as_bytes()
                            .iter()
                            .map(|byte| format!("{byte:02X}"))
                            .collect::<String>(),
                    )),
                    Value::Integer(value) => Ok(Value::Text(
                        value
                            .to_string()
                            .as_bytes()
                            .iter()
                            .map(|byte| format!("{byte:02X}"))
                            .collect::<String>(),
                    )),
                    Value::Real(value) => Ok(Value::Text(
                        Self::sqlite_real_to_text(*value)
                            .as_bytes()
                            .iter()
                            .map(|byte| format!("{byte:02X}"))
                            .collect::<String>(),
                    )),
                    Value::Boolean(value) => Ok(Value::Text(
                        if *value { "1" } else { "0" }
                            .as_bytes()
                            .iter()
                            .map(|byte| format!("{byte:02X}"))
                            .collect::<String>(),
                    )),
                }
            }
            ScalarFunc::Substr => {
                if !matches!(args.len(), 2 | 3) {
                    return Err(DbError::plan(format!(
                        "SUBSTR expects 2 or 3 arguments but got {}",
                        args.len()
                    )));
                }

                let value = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => value,
                };

                let start = match Self::cast_value(
                    args[1].clone(),
                    crate::common::types::ColumnType::Integer,
                )? {
                    Value::Null => return Ok(Value::Null),
                    Value::Integer(value) => value,
                    _ => unreachable!("integer cast must yield INTEGER or NULL"),
                };

                let length = if args.len() == 3 {
                    match Self::cast_value(
                        args[2].clone(),
                        crate::common::types::ColumnType::Integer,
                    )? {
                        Value::Null => return Ok(Value::Null),
                        Value::Integer(value) => Some(value),
                        _ => unreachable!("integer cast must yield INTEGER or NULL"),
                    }
                } else {
                    None
                };

                match value {
                    Value::Blob(value) => Ok(Value::Blob(sqlite_substr_blob(value, start, length))),
                    value => Ok(Value::Text(sqlite_substr_text(
                        &Self::coerce_text_like_value(value),
                        start,
                        length,
                    ))),
                }
            }
            ScalarFunc::Instr => {
                Self::expect_arity("INSTR", &args, 2)?;

                let haystack = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };

                let needle = match &args[1] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };

                if needle.is_empty() {
                    return Ok(Value::Integer(1));
                }

                let position = haystack
                    .find(&needle)
                    .map(|byte_index| haystack[..byte_index].chars().count() as i64 + 1)
                    .unwrap_or(0);
                Ok(Value::Integer(position))
            }
            ScalarFunc::Replace => {
                Self::expect_arity("REPLACE", &args, 3)?;

                let value = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };

                let pattern = match &args[1] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };

                let replacement = match &args[2] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };

                if pattern.is_empty() {
                    return Ok(Value::Text(value));
                }

                Ok(Value::Text(value.replace(&pattern, &replacement)))
            }
            ScalarFunc::LikeFunc => {
                if !(2..=3).contains(&args.len()) {
                    return Err(DbError::plan(format!(
                        "LIKE expects 2 or 3 arguments but got {}",
                        args.len()
                    )));
                }
                let pattern = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };
                let value = match &args[1] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };
                let escape = if let Some(escape) = args.get(2) {
                    match escape {
                        Value::Null => return Ok(Value::Null),
                        value => Some(Self::coerce_text_like_value(value)),
                    }
                } else {
                    None
                };
                Ok(Value::Boolean(Self::matches_like_pattern(
                    &value,
                    &pattern,
                    &escape,
                    self.storage.case_sensitive_like(),
                )?))
            }
            ScalarFunc::GlobFunc => {
                Self::expect_arity("GLOB", &args, 2)?;
                let pattern = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };
                let value = match &args[1] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };
                Ok(Value::Boolean(Self::matches_glob_pattern(&value, &pattern)))
            }
            ScalarFunc::Quote => {
                Self::expect_arity("QUOTE", &args, 1)?;
                let quoted = match &args[0] {
                    Value::Null => "NULL".to_string(),
                    Value::Boolean(value) => {
                        if *value {
                            "1".to_string()
                        } else {
                            "0".to_string()
                        }
                    }
                    Value::Integer(value) => value.to_string(),
                    Value::Real(value) => Self::sqlite_real_to_text(*value),
                    Value::Blob(value) => format!(
                        "X'{}'",
                        value
                            .iter()
                            .map(|byte| format!("{byte:02X}"))
                            .collect::<String>()
                    ),
                    Value::Text(value) => format!("'{}'", value.replace('\'', "''")),
                };
                Ok(Value::Text(quoted))
            }
            ScalarFunc::Unicode => {
                Self::expect_arity("UNICODE", &args, 1)?;
                match &args[0] {
                    Value::Null => Ok(Value::Null),
                    value => Ok(Self::coerce_text_like_value(value)
                        .chars()
                        .next()
                        .map(|ch| Value::Integer(i64::from(u32::from(ch))))
                        .unwrap_or(Value::Null)),
                }
            }
            ScalarFunc::Trim => {
                Self::evaluate_trim_family_function("TRIM", &args, |value, characters| {
                    value.trim_matches(|ch| characters.contains(ch)).to_string()
                })
            }
            ScalarFunc::LTrim => {
                Self::evaluate_trim_family_function("LTRIM", &args, |value, characters| {
                    value
                        .trim_start_matches(|ch| characters.contains(ch))
                        .to_string()
                })
            }
            ScalarFunc::RTrim => {
                Self::evaluate_trim_family_function("RTRIM", &args, |value, characters| {
                    value
                        .trim_end_matches(|ch| characters.contains(ch))
                        .to_string()
                })
            }
            ScalarFunc::Lower => {
                Self::expect_arity("LOWER", &args, 1)?;
                match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::Text(value) => Ok(Value::Text(sqlite_ascii_lower(value))),
                    value => Ok(Value::Text(sqlite_ascii_lower(
                        &Self::coerce_text_like_value(value),
                    ))),
                }
            }
            ScalarFunc::Upper => {
                Self::expect_arity("UPPER", &args, 1)?;
                match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::Text(value) => Ok(Value::Text(sqlite_ascii_upper(value))),
                    value => Ok(Value::Text(sqlite_ascii_upper(
                        &Self::coerce_text_like_value(value),
                    ))),
                }
            }
            ScalarFunc::Abs => {
                Self::expect_arity("ABS", &args, 1)?;
                match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::Integer(value) => value
                        .checked_abs()
                        .map(Value::Integer)
                        .ok_or_else(|| DbError::plan("ABS overflowed i64")),
                    Value::Real(value) => Ok(Value::Real(value.abs())),
                    value => Ok(Value::Real(Self::coerce_sqlite_numeric_real(&value)?.abs())),
                }
            }
            ScalarFunc::Round => {
                if !matches!(args.len(), 1 | 2) {
                    return Err(DbError::plan(format!(
                        "ROUND expects 1 or 2 arguments but got {}",
                        args.len()
                    )));
                }
                let value = match args[0] {
                    Value::Null => return Ok(Value::Null),
                    Value::Integer(value) => value as f64,
                    Value::Real(value) => value,
                    ref value => Self::coerce_sqlite_numeric_real(value)?,
                };
                let precision = if args.len() == 2 {
                    match &args[1] {
                        Value::Null => return Ok(Value::Null),
                        value => match Self::cast_value(
                            value.clone(),
                            crate::common::types::ColumnType::Integer,
                        )? {
                            Value::Integer(value) => i32::try_from(value).map_err(|_| {
                                DbError::plan("ROUND precision does not fit in i32")
                            })?,
                            Value::Null => return Ok(Value::Null),
                            _ => unreachable!("integer cast must yield INTEGER or NULL"),
                        },
                    }
                } else {
                    0
                };
                let factor = 10_f64.powi(precision);
                Ok(Value::Real((value * factor).round() / factor))
            }
            ScalarFunc::LastInsertRowId => {
                Self::expect_arity("LAST_INSERT_ROWID", &args, 0)?;
                Ok(Value::Integer(self.last_insert_rowid.get()))
            }
            ScalarFunc::Json => {
                Self::expect_arity("JSON", &args, 1)?;
                Self::json_normalize_value(&args[0])
            }
            ScalarFunc::JsonValid => {
                if !matches!(args.len(), 1 | 2) {
                    return Err(DbError::plan(format!(
                        "JSON_VALID expects 1 or 2 arguments but got {}",
                        args.len()
                    )));
                }
                let json = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };
                let valid = if let Some(flags) = args.get(1) {
                    let flags = Self::json_valid_flags(flags)?;
                    if flags & 0x02 != 0 {
                        parse_sqlite_json_value(&json).is_ok()
                    } else {
                        serde_json::from_str::<serde_json::Value>(&json).is_ok()
                    }
                } else {
                    serde_json::from_str::<serde_json::Value>(&json).is_ok()
                };
                Ok(Value::Integer(i64::from(valid)))
            }
            ScalarFunc::JsonErrorPosition => {
                Self::expect_arity("JSON_ERROR_POSITION", &args, 1)?;
                Self::json_error_position_value(&args[0])
            }
            ScalarFunc::JsonPretty => Self::json_pretty_value(&args),
            ScalarFunc::JsonQuote => {
                Self::expect_arity("JSON_QUOTE", &args, 1)?;
                Ok(Value::Text(Self::json_quote_value(&args[0])?))
            }
            ScalarFunc::JsonExtract => {
                if args.len() < 2 {
                    return Err(DbError::plan(format!(
                        "JSON_EXTRACT expects at least 2 arguments but got {}",
                        args.len()
                    )));
                }
                let json = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };
                let path = match &args[1] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };
                Self::json_extract_value(&json, &path)
            }
            ScalarFunc::JsonType => {
                if !matches!(args.len(), 1 | 2) {
                    return Err(DbError::plan(format!(
                        "JSON_TYPE expects 1 or 2 arguments but got {}",
                        args.len()
                    )));
                }
                let json = match &args[0] {
                    Value::Null => return Ok(Value::Null),
                    value => Self::coerce_text_like_value(value),
                };
                let parsed = parse_sqlite_json_value(&json)
                    .map_err(|error| DbError::plan(format!("malformed JSON: {error}")))?;
                let value = if let Some(path) = args.get(1) {
                    let path = match path {
                        Value::Null => return Ok(Value::Null),
                        value => Self::coerce_text_like_value(value),
                    };
                    let Some(value) = json_path_lookup(&parsed, &path)? else {
                        return Ok(Value::Null);
                    };
                    value
                } else {
                    &parsed
                };
                Ok(Value::Text(json_type_name(value).to_string()))
            }
            ScalarFunc::JsonArray => {
                let values = args
                    .iter()
                    .map(Self::sql_value_to_json)
                    .collect::<Result<Vec<_>>>()?;
                serde_json::to_string(&values)
                    .map(Value::Text)
                    .map_err(|error| DbError::plan(format!("failed to render JSON array: {error}")))
            }
            ScalarFunc::JsonObject => Self::json_object_value(&args),
            ScalarFunc::JsonArrayLength => Self::json_array_length_value(&args),
            ScalarFunc::JsonRemove => Self::json_remove_value(&args),
            ScalarFunc::JsonSet => Self::json_set_value(&args),
            ScalarFunc::JsonInsert => {
                Self::json_write_value("json_insert", &args, JsonWriteMode::Insert)
            }
            ScalarFunc::JsonReplace => {
                Self::json_write_value("json_replace", &args, JsonWriteMode::Replace)
            }
            ScalarFunc::JsonPatch => Self::json_patch_value(&args),
            ScalarFunc::Coalesce => {
                if args.len() < 2 {
                    return Err(DbError::plan("COALESCE expects at least 2 arguments"));
                }
                Ok(args
                    .into_iter()
                    .find(|value| !matches!(value, Value::Null))
                    .unwrap_or(Value::Null))
            }
            ScalarFunc::IfNull => {
                Self::expect_arity("IFNULL", &args, 2)?;
                if matches!(args[0], Value::Null) {
                    Ok(args[1].clone())
                } else {
                    Ok(args[0].clone())
                }
            }
            ScalarFunc::IIf | ScalarFunc::If => {
                unreachable!("short-circuit scalar functions are evaluated before eager dispatch")
            }
            ScalarFunc::NullIf => {
                Self::expect_arity("NULLIF", &args, 2)?;
                if self.compare(&args[0], &args[1])? == Some(Ordering::Equal) {
                    Ok(Value::Null)
                } else {
                    Ok(args[0].clone())
                }
            }
        }
    }

    fn json_quote_value(value: &Value) -> Result<String> {
        match value {
            Value::Null => Ok("null".to_string()),
            Value::Boolean(value) => Ok(if *value { "1" } else { "0" }.to_string()),
            Value::Integer(value) => Ok(value.to_string()),
            Value::Real(value) => Ok(Self::sqlite_real_to_text(*value)),
            Value::Text(value) => serde_json::to_string(value)
                .map_err(|error| DbError::plan(format!("failed to quote JSON string: {error}"))),
            Value::Blob(value) => serde_json::to_string(&String::from_utf8_lossy(value))
                .map_err(|error| DbError::plan(format!("failed to quote JSON string: {error}"))),
        }
    }

    fn sql_value_to_json(value: &Value) -> Result<serde_json::Value> {
        Ok(match value {
            Value::Null => serde_json::Value::Null,
            Value::Boolean(value) => serde_json::Value::Number(if *value { 1 } else { 0 }.into()),
            Value::Integer(value) => serde_json::Value::Number((*value).into()),
            Value::Real(value) => serde_json::Number::from_f64(*value)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::Text(value) => serde_json::Value::String(value.clone()),
            Value::Blob(value) => {
                serde_json::Value::String(String::from_utf8_lossy(value).into_owned())
            }
        })
    }

    fn json_normalize_value(value: &Value) -> Result<Value> {
        let json = match value {
            Value::Null => return Ok(Value::Null),
            value => Self::coerce_text_like_value(value),
        };
        let parsed = parse_sqlite_json_value(&json)
            .map_err(|error| DbError::plan(format!("malformed JSON: {error}")))?;
        serde_json::to_string(&parsed)
            .map(Value::Text)
            .map_err(|error| DbError::plan(format!("failed to render JSON value: {error}")))
    }

    fn json_valid_flags(value: &Value) -> Result<i64> {
        let Value::Integer(flags) =
            Self::cast_value(value.clone(), crate::common::types::ColumnType::Integer)?
        else {
            return Err(DbError::plan(
                "FLAGS parameter to json_valid() must be between 1 and 15",
            ));
        };
        if !(1..=15).contains(&flags) {
            return Err(DbError::plan(
                "FLAGS parameter to json_valid() must be between 1 and 15",
            ));
        }
        Ok(flags)
    }

    fn json_error_position_value(value: &Value) -> Result<Value> {
        let json = match value {
            Value::Null => return Ok(Value::Null),
            value => Self::coerce_text_like_value(value),
        };
        match parse_sqlite_json_value(&json) {
            Ok(_) => Ok(Value::Integer(0)),
            Err(error) => Ok(Value::Integer(json_error_position(&json, &error))),
        }
    }

    fn json_pretty_value(args: &[Value]) -> Result<Value> {
        if !matches!(args.len(), 1 | 2) {
            return Err(DbError::plan(format!(
                "JSON_PRETTY expects 1 or 2 arguments but got {}",
                args.len()
            )));
        }
        let json = match &args[0] {
            Value::Null => return Ok(Value::Null),
            value => Self::coerce_text_like_value(value),
        };
        let indent = if let Some(indent) = args.get(1) {
            match indent {
                Value::Null => return Ok(Value::Null),
                value => Self::coerce_text_like_value(value),
            }
        } else {
            "    ".to_string()
        };
        let parsed = parse_sqlite_json_value(&json)
            .map_err(|error| DbError::plan(format!("malformed JSON: {error}")))?;
        Ok(Value::Text(json_pretty_render(&parsed, &indent)))
    }

    fn json_object_value(args: &[Value]) -> Result<Value> {
        if args.len() % 2 != 0 {
            return Err(DbError::plan(
                "json_object() requires an even number of arguments",
            ));
        }

        let mut fields = Vec::with_capacity(args.len() / 2);
        for pair in args.chunks_exact(2) {
            let Value::Text(label) = &pair[0] else {
                return Err(DbError::plan("json_object() labels must be TEXT"));
            };
            let label = serde_json::to_string(label)
                .map_err(|error| DbError::plan(format!("failed to quote JSON label: {error}")))?;
            let value = Self::sql_value_to_json(&pair[1])?;
            let value = serde_json::to_string(&value).map_err(|error| {
                DbError::plan(format!("failed to render JSON object value: {error}"))
            })?;
            fields.push(format!("{label}:{value}"));
        }

        Ok(Value::Text(format!("{{{}}}", fields.join(","))))
    }

    fn json_array_length_value(args: &[Value]) -> Result<Value> {
        if !matches!(args.len(), 1 | 2) {
            return Err(DbError::plan(format!(
                "JSON_ARRAY_LENGTH expects 1 or 2 arguments but got {}",
                args.len()
            )));
        }

        let json = match &args[0] {
            Value::Null => return Ok(Value::Null),
            value => Self::coerce_text_like_value(value),
        };
        let parsed = parse_sqlite_json_value(&json)
            .map_err(|error| DbError::plan(format!("malformed JSON: {error}")))?;
        let value = if let Some(path) = args.get(1) {
            let path = match path {
                Value::Null => return Ok(Value::Null),
                value => Self::coerce_text_like_value(value),
            };
            let Some(value) = json_path_lookup(&parsed, &path)? else {
                return Ok(Value::Null);
            };
            value
        } else {
            &parsed
        };

        Ok(Value::Integer(match value {
            serde_json::Value::Array(values) => values.len() as i64,
            _ => 0,
        }))
    }

    fn json_remove_value(args: &[Value]) -> Result<Value> {
        if args.is_empty() {
            return Err(DbError::plan("JSON_REMOVE expects at least 1 argument"));
        }
        let json = match &args[0] {
            Value::Null => return Ok(Value::Null),
            value => Self::coerce_text_like_value(value),
        };
        let mut parsed = parse_sqlite_json_value(&json)
            .map_err(|error| DbError::plan(format!("malformed JSON: {error}")))?;
        for path in &args[1..] {
            let path = match path {
                Value::Null => return Ok(Value::Null),
                value => Self::coerce_text_like_value(value),
            };
            if path == "$" {
                return Ok(Value::Null);
            }
            json_remove_path(&mut parsed, &path)
                .map_err(|_| DbError::plan(format!("bad JSON path: '{path}'")))?;
        }
        serde_json::to_string(&parsed)
            .map(Value::Text)
            .map_err(|error| DbError::plan(format!("failed to render JSON value: {error}")))
    }

    fn json_set_value(args: &[Value]) -> Result<Value> {
        Self::json_write_value("json_set", args, JsonWriteMode::Set)
    }

    fn json_write_value(function_name: &str, args: &[Value], mode: JsonWriteMode) -> Result<Value> {
        if args.len() % 2 == 0 {
            return Err(DbError::plan(format!(
                "{function_name}() needs an odd number of arguments"
            )));
        }
        let json = match &args[0] {
            Value::Null => return Ok(Value::Null),
            value => Self::coerce_text_like_value(value),
        };
        let mut parsed = parse_sqlite_json_value(&json)
            .map_err(|error| DbError::plan(format!("malformed JSON: {error}")))?;
        for pair in args[1..].chunks_exact(2) {
            let path = match &pair[0] {
                Value::Null => continue,
                value => Self::coerce_text_like_value(value),
            };
            let replacement = Self::sql_value_to_json(&pair[1])?;
            if path == "$" {
                match mode {
                    JsonWriteMode::Set | JsonWriteMode::Replace => parsed = replacement,
                    JsonWriteMode::Insert => {}
                }
                continue;
            }
            json_write_path(&mut parsed, &path, replacement, mode)
                .map_err(|_| DbError::plan(format!("bad JSON path: '{path}'")))?;
        }
        serde_json::to_string(&parsed)
            .map(Value::Text)
            .map_err(|error| DbError::plan(format!("failed to render JSON value: {error}")))
    }

    fn json_patch_value(args: &[Value]) -> Result<Value> {
        Self::expect_arity("JSON_PATCH", args, 2)?;
        let target = match &args[0] {
            Value::Null => return Ok(Value::Null),
            value => Self::coerce_text_like_value(value),
        };
        let patch = match &args[1] {
            Value::Null => return Ok(Value::Null),
            value => Self::coerce_text_like_value(value),
        };
        let mut target = parse_sqlite_json_value(&target)
            .map_err(|error| DbError::plan(format!("malformed JSON: {error}")))?;
        let patch = parse_sqlite_json_value(&patch)
            .map_err(|error| DbError::plan(format!("malformed JSON: {error}")))?;
        json_merge_patch(&mut target, patch);
        serde_json::to_string(&target)
            .map(Value::Text)
            .map_err(|error| DbError::plan(format!("failed to render JSON value: {error}")))
    }

    fn json_extract_value(json: &str, path: &str) -> Result<Value> {
        let parsed = parse_sqlite_json_value(json)
            .map_err(|error| DbError::plan(format!("malformed JSON: {error}")))?;
        let Some(value) = json_path_lookup(&parsed, path)? else {
            return Ok(Value::Null);
        };
        json_value_to_sql(value)
    }

    fn evaluate_trim_family_function<F>(
        function_name: &str,
        args: &[Value],
        trim: F,
    ) -> Result<Value>
    where
        F: FnOnce(&str, &str) -> String,
    {
        if !matches!(args.len(), 1 | 2) {
            return Err(DbError::plan(format!(
                "{function_name} expects 1 or 2 arguments but got {}",
                args.len()
            )));
        }

        let value = match &args[0] {
            Value::Null => return Ok(Value::Null),
            value => Self::coerce_text_like_value(value),
        };

        let characters = if args.len() == 2 {
            match &args[1] {
                Value::Null => return Ok(Value::Null),
                value => Self::coerce_text_like_value(value),
            }
        } else {
            " ".to_string()
        };

        Ok(Value::Text(trim(&value, &characters)))
    }

    fn evaluate_date_time_family_function(
        function_name: &str,
        args: &[Value],
        kind: DateTimeResultKind,
    ) -> Result<Value> {
        let subsecond = Self::date_time_args_have_subsecond(args);
        Ok(Self::parse_date_time_args(function_name, args)?
            .map(|parts| {
                Value::Text(match kind {
                    DateTimeResultKind::Date => {
                        format!("{:04}-{:02}-{:02}", parts.year, parts.month, parts.day)
                    }
                    DateTimeResultKind::Time if subsecond => format!(
                        "{:02}:{:02}:{:02}.{:03}",
                        parts.hour, parts.minute, parts.second, parts.millisecond
                    ),
                    DateTimeResultKind::Time => {
                        format!("{:02}:{:02}:{:02}", parts.hour, parts.minute, parts.second)
                    }
                    DateTimeResultKind::DateTime if subsecond => format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
                        parts.year,
                        parts.month,
                        parts.day,
                        parts.hour,
                        parts.minute,
                        parts.second,
                        parts.millisecond
                    ),
                    DateTimeResultKind::DateTime => format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        parts.year, parts.month, parts.day, parts.hour, parts.minute, parts.second
                    ),
                })
            })
            .unwrap_or(Value::Null))
    }

    fn evaluate_strftime_function(args: &[Value]) -> Result<Value> {
        let format = match &args[0] {
            Value::Null => return Ok(Value::Null),
            value => Self::coerce_text_like_value(value),
        };
        let subsecond = Self::date_time_args_have_subsecond(&args[1..]);

        Ok(Self::parse_date_time_args("STRFTIME", &args[1..])?
            .and_then(|parts| sqlite_strftime_minimal(&format, parts, subsecond))
            .map(Value::Text)
            .unwrap_or(Value::Null))
    }

    fn evaluate_julianday_function(args: &[Value]) -> Result<Value> {
        Ok(Self::parse_date_time_args("JULIANDAY", args)?
            .map(sqlite_julianday)
            .map(Value::Real)
            .unwrap_or(Value::Null))
    }

    fn evaluate_unixepoch_function(args: &[Value]) -> Result<Value> {
        let subsecond = Self::date_time_args_have_subsecond(args);
        Ok(Self::parse_date_time_args("UNIXEPOCH", args)?
            .map(|parts| {
                if subsecond {
                    Value::Real(sqlite_unixepoch_subsecond(parts))
                } else {
                    Value::Integer(sqlite_unixepoch(parts))
                }
            })
            .unwrap_or(Value::Null))
    }

    fn evaluate_timediff_function(args: &[Value]) -> Result<Value> {
        Self::expect_arity("TIMEDIFF", args, 2)?;
        let left = Self::parse_date_time_args("TIMEDIFF", &args[..1])?;
        let right = Self::parse_date_time_args("TIMEDIFF", &args[1..2])?;
        let (Some(left), Some(right)) = (left, right) else {
            return Ok(Value::Null);
        };
        Ok(Value::Text(sqlite_timediff_between(right, left)))
    }

    fn date_time_args_have_subsecond(args: &[Value]) -> bool {
        let modifier_args = if args.is_empty() { &[][..] } else { &args[1..] };
        modifier_args.iter().any(|value| match value {
            Value::Null => false,
            value => {
                let modifier = Self::coerce_text_like_value(value);
                modifier.eq_ignore_ascii_case("subsec")
                    || modifier.eq_ignore_ascii_case("subsecond")
            }
        })
    }

    fn parse_date_time_args(
        function_name: &str,
        args: &[Value],
    ) -> Result<Option<ParsedDateTimeParts>> {
        let default_now = Value::from("now");
        let value = args.first().unwrap_or(&default_now);
        let modifier_args = if args.is_empty() { &[][..] } else { &args[1..] };
        let Some(modifiers) = collect_date_time_modifiers(function_name, modifier_args)? else {
            return Ok(None);
        };
        let uses_unixepoch = matches!(modifiers.first(), Some(modifier) if modifier.eq_ignore_ascii_case("unixepoch"));
        let uses_auto =
            matches!(modifiers.first(), Some(modifier) if modifier.eq_ignore_ascii_case("auto"));
        if modifiers.iter().skip(1).any(|modifier| {
            modifier.eq_ignore_ascii_case("unixepoch") || modifier.eq_ignore_ascii_case("auto")
        }) {
            return Ok(None);
        }

        let mut parts = match value {
            Value::Null => return Ok(None),
            Value::Text(value) => {
                if uses_unixepoch {
                    value
                        .parse::<f64>()
                        .ok()
                        .and_then(parse_sqlite_unixepoch_real_value)
                } else if uses_auto {
                    value.parse::<f64>().ok().and_then(parse_sqlite_auto_value)
                } else {
                    parse_sqlite_date_time_text(value)
                }
            }
            Value::Blob(value) => {
                let value = String::from_utf8_lossy(value);
                if uses_unixepoch {
                    value
                        .parse::<f64>()
                        .ok()
                        .and_then(parse_sqlite_unixepoch_real_value)
                } else if uses_auto {
                    value.parse::<f64>().ok().and_then(parse_sqlite_auto_value)
                } else {
                    parse_sqlite_date_time_text(&value)
                }
            }
            Value::Integer(value) if uses_unixepoch => parse_sqlite_unixepoch_value(*value),
            Value::Real(value) if uses_unixepoch => parse_sqlite_unixepoch_real_value(*value),
            Value::Integer(value) if uses_auto => parse_sqlite_auto_value(*value as f64),
            Value::Real(value) if uses_auto => parse_sqlite_auto_value(*value),
            Value::Integer(_) | Value::Real(_) => None,
            _ => None,
        };

        let mut index = 0;
        while index < modifiers.len() {
            let modifier = &modifiers[index];
            if modifier.eq_ignore_ascii_case("unixepoch")
                || modifier.eq_ignore_ascii_case("auto")
                || modifier.eq_ignore_ascii_case("subsec")
                || modifier.eq_ignore_ascii_case("subsecond")
                || modifier.eq_ignore_ascii_case("floor")
                || modifier.eq_ignore_ascii_case("ceiling")
            {
                index += 1;
                continue;
            }
            let rounding = modifiers.get(index + 1).and_then(|modifier| {
                if modifier.eq_ignore_ascii_case("floor") {
                    Some(DateShiftRounding::Floor)
                } else if modifier.eq_ignore_ascii_case("ceiling") {
                    Some(DateShiftRounding::Ceiling)
                } else {
                    None
                }
            });
            parts = parts.and_then(|parts| {
                apply_sqlite_date_time_modifier(parts, modifier, rounding.unwrap_or_default())
            });
            index += 1 + usize::from(rounding.is_some());
        }

        Ok(parts)
    }

    fn evaluate_min_max_scalar_function(
        &self,
        function_name: &str,
        args: &[Value],
        want_min: bool,
    ) -> Result<Value> {
        if args.iter().any(|value| matches!(value, Value::Null)) {
            return Ok(Value::Null);
        }

        let mut best = args
            .first()
            .cloned()
            .ok_or_else(|| DbError::plan(format!("{function_name} expects at least 1 argument")))?;
        for candidate in args.iter().skip(1) {
            let ordering = self.compare(candidate, &best)?.ok_or_else(|| {
                DbError::plan(format!(
                    "{function_name} cannot compare {} and {}",
                    candidate.type_name(),
                    best.type_name()
                ))
            })?;
            let replace = if want_min {
                matches!(ordering, Ordering::Less | Ordering::Equal)
            } else {
                ordering == Ordering::Greater
            };
            if replace {
                best = candidate.clone();
            }
        }

        Ok(Self::canonicalize_scalar_min_max_result(best))
    }

    fn expect_arity(function_name: &str, args: &[Value], expected: usize) -> Result<()> {
        if args.len() == expected {
            Ok(())
        } else {
            Err(DbError::plan(format!(
                "{function_name} expects {expected} arguments but got {}",
                args.len()
            )))
        }
    }

    fn canonicalize_scalar_min_max_result(value: Value) -> Value {
        match value {
            Value::Boolean(value) => Value::Integer(if value { 1 } else { 0 }),
            value => value,
        }
    }

    fn evaluate_binary_scalar(
        &self,
        op: ScalarBinaryOp,
        left: Value,
        right: Value,
    ) -> Result<Value> {
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
        match op {
            ScalarBinaryOp::Add => Self::sqlite_numeric_binary_op(left, right, |l, r| l + r),
            ScalarBinaryOp::Subtract => Self::sqlite_numeric_binary_op(left, right, |l, r| l - r),
            ScalarBinaryOp::Multiply => Self::sqlite_numeric_binary_op(left, right, |l, r| l * r),
            ScalarBinaryOp::Divide => Self::sqlite_divide_op(left, right),
            ScalarBinaryOp::Modulo => Self::sqlite_modulo_op(left, right),
            ScalarBinaryOp::BitAnd => Self::sqlite_bitwise_binary_op(left, right, |l, r| l & r),
            ScalarBinaryOp::BitOr => Self::sqlite_bitwise_binary_op(left, right, |l, r| l | r),
            ScalarBinaryOp::ShiftLeft => Self::sqlite_bitwise_binary_op(left, right, |l, r| l << r),
            ScalarBinaryOp::ShiftRight => {
                Self::sqlite_bitwise_binary_op(left, right, |l, r| l >> r)
            }
            ScalarBinaryOp::Concat => match (left, right) {
                (left, right) => Ok(Value::Text(format!(
                    "{}{}",
                    Self::coerce_text_like_value(&left),
                    Self::coerce_text_like_value(&right)
                ))),
            },
            ScalarBinaryOp::JsonExtract => Self::evaluate_json_arrow_operator(&left, &right, false),
            ScalarBinaryOp::JsonExtractText => {
                Self::evaluate_json_arrow_operator(&left, &right, true)
            }
        }
    }

    fn evaluate_json_arrow_operator(
        json: &Value,
        path: &Value,
        text_result: bool,
    ) -> Result<Value> {
        let json = Self::coerce_text_like_value(json);
        let path = Self::sqlite_json_arrow_path(path);
        let parsed = parse_sqlite_json_value(&json)
            .map_err(|error| DbError::plan(format!("malformed JSON: {error}")))?;
        let Some(value) = json_path_lookup(&parsed, &path)? else {
            return Ok(Value::Null);
        };
        if text_result {
            json_value_to_sql(value)
        } else {
            serde_json::to_string(value)
                .map(Value::Text)
                .map_err(|error| DbError::plan(format!("failed to render JSON value: {error}")))
        }
    }

    fn sqlite_json_arrow_path(path: &Value) -> String {
        match path {
            Value::Integer(index) if *index >= 0 => format!("$[{index}]"),
            Value::Text(path) if path.starts_with('$') => path.clone(),
            value => format!("$.{}", Self::coerce_text_like_value(value)),
        }
    }

    fn coerce_text_like_value(value: &Value) -> String {
        match value {
            Value::Null => String::new(),
            Value::Boolean(value) => {
                if *value {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }
            Value::Integer(value) => value.to_string(),
            Value::Real(value) => Self::sqlite_real_to_text(*value),
            Value::Blob(value) => String::from_utf8_lossy(value).into_owned(),
            Value::Text(value) => value.clone(),
        }
    }

    fn sqlite_real_to_text(value: f64) -> String {
        let rendered = value.to_string();
        if rendered.contains(['.', 'e', 'E']) {
            rendered
        } else {
            format!("{rendered}.0")
        }
    }

    fn sqlite_printf(format: &str, args: &[Value]) -> Result<String> {
        let mut rendered = String::new();
        let mut chars = format.chars().peekable();
        let mut arg_index = 0usize;

        while let Some(ch) = chars.next() {
            if ch != '%' {
                rendered.push(ch);
                continue;
            }

            if chars.peek() == Some(&'%') {
                chars.next();
                rendered.push('%');
                continue;
            }

            let mut flags = SqlitePrintfFlags::default();
            let mut width = String::new();
            while let Some(flag) = chars.peek().copied() {
                match flag {
                    '-' => flags.left_align = true,
                    '+' => flags.sign_plus = true,
                    ' ' => flags.sign_space = true,
                    ',' => flags.grouping = true,
                    '0' => flags.zero_pad = true,
                    '#' => flags.alternate = true,
                    _ => break,
                }
                chars.next();
            }
            while let Some(next) = chars.peek() {
                if next.is_ascii_digit() {
                    width.push(*next);
                    chars.next();
                } else {
                    break;
                }
            }
            let mut dynamic_width = None;
            if width.is_empty() && chars.peek() == Some(&'*') {
                chars.next();
                let width_arg = args.get(arg_index).cloned().unwrap_or(Value::Null);
                arg_index += 1;
                dynamic_width = Some(Self::sqlite_printf_integer_arg(&width_arg));
            }
            let precision = if chars.peek() == Some(&'.') {
                chars.next();
                if chars.peek() == Some(&'*') {
                    chars.next();
                    let precision_arg = args.get(arg_index).cloned().unwrap_or(Value::Null);
                    arg_index += 1;
                    let precision = Self::sqlite_printf_integer_arg(&precision_arg);
                    Some(if precision < 0 {
                        precision.unsigned_abs() as usize
                    } else {
                        usize::try_from(precision).unwrap_or(0)
                    })
                } else {
                    let mut precision = String::new();
                    while let Some(next) = chars.peek() {
                        if next.is_ascii_digit() {
                            precision.push(*next);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    Some(precision.parse::<usize>().unwrap_or(0))
                }
            } else {
                None
            };
            let width = match dynamic_width {
                Some(width) if width < 0 => {
                    flags.left_align = true;
                    width.unsigned_abs() as usize
                }
                Some(width) => usize::try_from(width).unwrap_or(0),
                None => width.parse::<usize>().unwrap_or(0),
            };

            while chars.peek() == Some(&'l') {
                chars.next();
            }

            let spec = chars
                .next()
                .ok_or_else(|| DbError::plan("PRINTF format string ended after %"))?;
            let arg = args.get(arg_index).cloned().unwrap_or(Value::Null);
            arg_index += 1;

            match spec {
                'd' | 'i' => {
                    let value = Self::sqlite_printf_integer_arg(&arg);
                    let rendered_value =
                        Self::format_sqlite_signed_integer(value, flags, precision);
                    Self::push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
                }
                'u' => {
                    let value = Self::sqlite_printf_integer_arg(&arg) as u64;
                    let rendered_value =
                        Self::format_sqlite_unsigned_integer(value, flags, precision);
                    Self::push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
                }
                'f' => {
                    let value = Self::sqlite_printf_real_arg(&arg);
                    let mut rendered_value = if let Some(precision) = precision {
                        format!("{value:.precision$}")
                    } else {
                        format!("{value:.6}")
                    };
                    if flags.alternate && !rendered_value.contains('.') {
                        rendered_value.push('.');
                    }
                    rendered_value = Self::apply_sqlite_numeric_flags(rendered_value, flags);
                    Self::push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
                }
                'e' | 'E' => {
                    let value = Self::sqlite_printf_real_arg(&arg);
                    let precision = precision.unwrap_or(6);
                    let mut rendered_value = format!("{value:.precision$e}");
                    rendered_value = Self::normalize_sqlite_exponent(rendered_value, spec);
                    if flags.alternate {
                        rendered_value = Self::ensure_sqlite_exponent_decimal_point(rendered_value);
                    }
                    rendered_value = Self::apply_sqlite_numeric_flags(rendered_value, flags);
                    Self::push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
                }
                'g' | 'G' => {
                    let value = Self::sqlite_printf_real_arg(&arg);
                    let precision = precision.unwrap_or(6);
                    let mut rendered_value = Self::sqlite_printf_general_float(value, precision);
                    if spec == 'G' {
                        rendered_value = rendered_value.replace('e', "E");
                    }
                    if flags.alternate && !rendered_value.contains('.') {
                        if let Some(index) = rendered_value.find(['e', 'E']) {
                            rendered_value.insert(index, '.');
                        } else {
                            rendered_value.push('.');
                        }
                    }
                    rendered_value = Self::apply_sqlite_numeric_flags(rendered_value, flags);
                    Self::push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
                }
                'x' | 'X' | 'o' | 'p' => {
                    let value = Self::sqlite_printf_integer_arg(&arg) as u64;
                    let raw = match spec {
                        'x' => format!("{value:x}"),
                        'X' | 'p' => format!("{value:X}"),
                        'o' => format!("{value:o}"),
                        _ => unreachable!("format specifier already matched"),
                    };
                    let raw = if let Some(precision) = precision {
                        format!("{raw:0>precision$}")
                    } else {
                        raw
                    };
                    let raw = if flags.alternate && value != 0 {
                        match spec {
                            'x' => format!("0x{raw}"),
                            'X' | 'p' => format!("0X{raw}"),
                            'o' => format!("0{raw}"),
                            _ => raw,
                        }
                    } else {
                        raw
                    };
                    if flags.zero_pad && width > 0 {
                        Self::push_sqlite_printf_prefixed_numeric(
                            &mut rendered,
                            &raw,
                            width,
                            flags,
                        );
                    } else if width > 0 {
                        Self::push_sqlite_printf_text(&mut rendered, &raw, width, flags.left_align);
                    } else {
                        rendered.push_str(&raw);
                    }
                }
                'n' => {}
                'c' => {
                    let rendered_value = match arg {
                        Value::Null => String::new(),
                        value => Self::coerce_text_like_value(&value)
                            .chars()
                            .next()
                            .map(|ch| ch.to_string())
                            .unwrap_or_default(),
                    };
                    Self::push_sqlite_printf_text(
                        &mut rendered,
                        &rendered_value,
                        width,
                        flags.left_align && !flags.zero_pad,
                    );
                }
                's' => {
                    let mut value = match arg {
                        Value::Null => String::new(),
                        value => Self::coerce_text_like_value(&value),
                    };
                    if let Some(precision) = precision {
                        value = Self::truncate_sqlite_printf_text(&value, precision);
                    }
                    Self::push_sqlite_printf_text(
                        &mut rendered,
                        &value,
                        width,
                        flags.left_align && !flags.zero_pad,
                    );
                }
                'z' => {
                    let mut value = match arg {
                        Value::Null => String::new(),
                        value => Self::coerce_text_like_value(&value),
                    };
                    if let Some(precision) = precision {
                        value = Self::truncate_sqlite_printf_text(&value, precision);
                    }
                    Self::push_sqlite_printf_text(
                        &mut rendered,
                        &value,
                        width,
                        flags.left_align && !flags.zero_pad,
                    );
                }
                'q' => {
                    let value = match arg {
                        Value::Null => "(NULL)".to_string(),
                        value => Self::coerce_text_like_value(&value).replace('\'', "''"),
                    };
                    Self::push_sqlite_printf_text(
                        &mut rendered,
                        &value,
                        width,
                        flags.left_align && !flags.zero_pad,
                    );
                }
                'Q' => {
                    let value = match arg {
                        Value::Null => "NULL".to_string(),
                        value => format!(
                            "'{}'",
                            Self::coerce_text_like_value(&value).replace('\'', "''")
                        ),
                    };
                    Self::push_sqlite_printf_text(
                        &mut rendered,
                        &value,
                        width,
                        flags.left_align && !flags.zero_pad,
                    );
                }
                'w' => {
                    let value = match arg {
                        Value::Null => "(NULL)".to_string(),
                        value => Self::coerce_text_like_value(&value).replace('"', "\"\""),
                    };
                    Self::push_sqlite_printf_text(
                        &mut rendered,
                        &value,
                        width,
                        flags.left_align && !flags.zero_pad,
                    );
                }
                other => {
                    return Err(DbError::plan(format!(
                        "PRINTF format specifier %{other} is not supported yet"
                    )));
                }
            }
        }

        Ok(rendered)
    }

    fn push_sqlite_printf_text(rendered: &mut String, value: &str, width: usize, left_align: bool) {
        if width > 0 {
            if left_align {
                rendered.push_str(&format!("{value:<width$}", width = width));
            } else {
                rendered.push_str(&format!("{value:>width$}", width = width));
            }
        } else {
            rendered.push_str(value);
        }
    }

    fn push_sqlite_printf_numeric(
        rendered: &mut String,
        value: &str,
        width: usize,
        flags: SqlitePrintfFlags,
    ) {
        if flags.zero_pad && width > value.len() {
            if let Some(stripped) = value.strip_prefix('-') {
                rendered.push('-');
                rendered.push_str(&format!(
                    "{stripped:0>width$}",
                    width = width.saturating_sub(1)
                ));
            } else if let Some(stripped) = value.strip_prefix(['+', ' ']) {
                let sign = value
                    .chars()
                    .next()
                    .expect("value with stripped prefix has sign");
                rendered.push(sign);
                rendered.push_str(&format!(
                    "{stripped:0>width$}",
                    width = width.saturating_sub(1)
                ));
            } else {
                rendered.push_str(&format!("{value:0>width$}", width = width));
            }
        } else if width > 0 {
            if flags.left_align {
                rendered.push_str(&format!("{value:<width$}", width = width));
            } else {
                rendered.push_str(&format!("{value:>width$}", width = width));
            }
        } else {
            rendered.push_str(value);
        }
    }

    fn push_sqlite_printf_prefixed_numeric(
        rendered: &mut String,
        value: &str,
        width: usize,
        flags: SqlitePrintfFlags,
    ) {
        let prefix_len = if value.starts_with("0x") || value.starts_with("0X") {
            2
        } else if value.starts_with('0') && value.len() > 1 {
            1
        } else {
            0
        };
        if flags.zero_pad && width > value.len() && prefix_len > 0 {
            let (prefix, digits) = value.split_at(prefix_len);
            rendered.push_str(prefix);
            let digit_width = if prefix_len == 2 {
                width
            } else {
                width.saturating_sub(prefix_len)
            };
            rendered.push_str(&format!("{digits:0>width$}", width = digit_width));
        } else {
            Self::push_sqlite_printf_numeric(rendered, value, width, flags);
        }
    }

    fn format_sqlite_signed_integer(
        value: i64,
        flags: SqlitePrintfFlags,
        precision: Option<usize>,
    ) -> String {
        let mut digits = value.unsigned_abs().to_string();
        if let Some(precision) = precision {
            digits = format!("{digits:0>precision$}");
        }
        let magnitude = if flags.grouping {
            Self::sqlite_group_digits(digits)
        } else {
            digits
        };

        if value < 0 {
            format!("-{magnitude}")
        } else if flags.sign_plus {
            format!("+{magnitude}")
        } else if flags.sign_space {
            format!(" {magnitude}")
        } else {
            magnitude
        }
    }

    fn format_sqlite_unsigned_integer(
        value: u64,
        flags: SqlitePrintfFlags,
        precision: Option<usize>,
    ) -> String {
        let mut digits = value.to_string();
        if let Some(precision) = precision {
            digits = format!("{digits:0>precision$}");
        }
        if flags.grouping {
            Self::sqlite_group_digits(digits)
        } else {
            digits
        }
    }

    fn truncate_sqlite_printf_text(value: &str, precision: usize) -> String {
        value.chars().take(precision).collect()
    }

    fn apply_sqlite_numeric_flags(rendered: String, flags: SqlitePrintfFlags) -> String {
        let with_grouping = if flags.grouping {
            Self::sqlite_group_decimal(rendered)
        } else {
            rendered
        };
        if with_grouping.starts_with('-') {
            with_grouping
        } else if flags.sign_plus {
            format!("+{with_grouping}")
        } else if flags.sign_space {
            format!(" {with_grouping}")
        } else {
            with_grouping
        }
    }

    fn sqlite_group_decimal(rendered: String) -> String {
        let Some(split_index) = rendered
            .find('.')
            .or_else(|| rendered.find('e'))
            .or_else(|| rendered.find('E'))
        else {
            return Self::sqlite_group_signed_digits(rendered);
        };
        let integer = Self::sqlite_group_signed_digits(rendered[..split_index].to_string());
        format!("{integer}{}", &rendered[split_index..])
    }

    fn sqlite_group_signed_digits(rendered: String) -> String {
        if let Some(digits) = rendered.strip_prefix('-') {
            format!("-{}", Self::sqlite_group_digits(digits.to_string()))
        } else if let Some(digits) = rendered.strip_prefix('+') {
            format!("+{}", Self::sqlite_group_digits(digits.to_string()))
        } else if let Some(digits) = rendered.strip_prefix(' ') {
            format!(" {}", Self::sqlite_group_digits(digits.to_string()))
        } else {
            Self::sqlite_group_digits(rendered)
        }
    }

    fn sqlite_group_digits(digits: String) -> String {
        let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
        let first_group_len = match digits.len() % 3 {
            0 => 3,
            len => len,
        };
        for (index, ch) in digits.chars().enumerate() {
            if index > 0
                && (index == first_group_len
                    || (index > first_group_len && (index - first_group_len) % 3 == 0))
            {
                grouped.push(',');
            }
            grouped.push(ch);
        }
        grouped
    }

    fn normalize_sqlite_exponent(rendered: String, spec: char) -> String {
        let Some(index) = rendered.find(['e', 'E']) else {
            return rendered;
        };
        let mantissa = &rendered[..index];
        let exponent = &rendered[index + 1..];
        let (sign, digits) = match exponent.as_bytes().first().copied() {
            Some(b'+') | Some(b'-') => (&exponent[..1], &exponent[1..]),
            _ => ("+", exponent),
        };
        let normalized_digits = if digits.len() >= 2 {
            digits.to_string()
        } else {
            format!("{digits:0>2}")
        };
        let marker = if spec == 'E' || spec == 'G' { 'E' } else { 'e' };
        format!("{mantissa}{marker}{sign}{normalized_digits}")
    }

    fn ensure_sqlite_exponent_decimal_point(mut rendered: String) -> String {
        if let Some(index) = rendered.find(['e', 'E'])
            && !rendered[..index].contains('.')
        {
            rendered.insert(index, '.');
        }
        rendered
    }

    fn sqlite_printf_general_float(value: f64, precision: usize) -> String {
        if value == 0.0 {
            return "0".to_string();
        }

        let abs = value.abs();
        let exponent = abs.log10().floor() as i32;
        let significant = precision.max(1);
        let use_exponent = exponent < -4 || exponent >= significant as i32;

        let rendered = if use_exponent {
            let decimals = significant.saturating_sub(1);
            let raw = format!("{value:.decimals$e}");
            Self::normalize_sqlite_exponent(raw, 'e')
        } else {
            let decimals = (significant as i32 - exponent - 1).max(0) as usize;
            format!("{value:.decimals$}")
        };

        Self::trim_printf_general_float(rendered)
    }

    fn trim_printf_general_float(rendered: String) -> String {
        if let Some(index) = rendered.find(['e', 'E']) {
            let mut mantissa = rendered[..index].to_string();
            while mantissa.contains('.') && mantissa.ends_with('0') {
                mantissa.pop();
            }
            if mantissa.ends_with('.') {
                mantissa.pop();
            }
            format!("{}{}", mantissa, &rendered[index..])
        } else {
            let mut value = rendered;
            while value.contains('.') && value.ends_with('0') {
                value.pop();
            }
            if value.ends_with('.') {
                value.pop();
            }
            value
        }
    }

    fn sqlite_printf_integer_arg(value: &Value) -> i64 {
        match value {
            Value::Null => 0,
            Value::Integer(value) => *value,
            Value::Real(value) => *value as i64,
            Value::Boolean(value) => {
                if *value {
                    1
                } else {
                    0
                }
            }
            Value::Text(value) => value.trim().parse::<i64>().unwrap_or(0),
            Value::Blob(value) => String::from_utf8_lossy(value)
                .trim()
                .parse::<i64>()
                .unwrap_or(0),
        }
    }

    fn sqlite_printf_real_arg(value: &Value) -> f64 {
        match value {
            Value::Null => 0.0,
            Value::Integer(value) => *value as f64,
            Value::Real(value) => *value,
            Value::Boolean(value) => {
                if *value {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Text(value) => value.trim().parse::<f64>().unwrap_or(0.0),
            Value::Blob(value) => String::from_utf8_lossy(value)
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0),
        }
    }

    fn cast_value(value: Value, ty: crate::common::types::ColumnType) -> Result<Value> {
        use crate::common::types::ColumnType;

        match ty {
            ColumnType::Any => Ok(value),
            ColumnType::Text => match value {
                Value::Null => Ok(Value::Null),
                value => Ok(Value::Text(Self::coerce_text_like_value(&value))),
            },
            ColumnType::Blob => match value {
                Value::Null => Ok(Value::Null),
                Value::Blob(value) => Ok(Value::Blob(value)),
                value => Ok(Value::Blob(
                    Self::coerce_text_like_value(&value).into_bytes(),
                )),
            },
            ColumnType::Integer => match value {
                Value::Null => Ok(Value::Null),
                Value::Integer(value) => Ok(Value::Integer(value)),
                Value::Boolean(value) => Ok(Value::Integer(if value { 1 } else { 0 })),
                Value::Real(value) => Ok(Value::Integer(value as i64)),
                Value::Text(value) => Ok(Value::Integer(sqlite_text_integer_prefix(&value))),
                Value::Blob(value) => Ok(Value::Integer(sqlite_text_integer_prefix(
                    &String::from_utf8_lossy(&value),
                ))),
            },
            ColumnType::Numeric => match value {
                Value::Null => Ok(Value::Null),
                Value::Integer(value) => Ok(Value::Integer(value)),
                Value::Boolean(value) => Ok(Value::Integer(if value { 1 } else { 0 })),
                Value::Real(value) => Ok(Value::Real(value)),
                Value::Text(value) => Ok(sqlite_text_numeric_prefix(&value)),
                Value::Blob(value) => {
                    Ok(sqlite_text_numeric_prefix(&String::from_utf8_lossy(&value)))
                }
            },
            ColumnType::Real => match value {
                Value::Null => Ok(Value::Null),
                Value::Integer(value) => Ok(Value::Real(value as f64)),
                Value::Boolean(value) => Ok(Value::Real(if value { 1.0 } else { 0.0 })),
                Value::Real(value) => Ok(Value::Real(value)),
                Value::Text(value) => Ok(Value::Real(sqlite_text_real_prefix(&value))),
                Value::Blob(value) => Ok(Value::Real(sqlite_text_real_prefix(
                    &String::from_utf8_lossy(&value),
                ))),
            },
            ColumnType::Boolean => match value {
                Value::Null => Ok(Value::Null),
                Value::Boolean(value) => Ok(Value::Boolean(value)),
                Value::Integer(value) => Ok(Value::Boolean(value != 0)),
                Value::Real(value) => Ok(Value::Boolean(value != 0.0)),
                Value::Text(value) => Ok(Value::Boolean(!value.is_empty() && value != "0")),
                Value::Blob(value) => Ok(Value::Boolean(!value.is_empty() && value != b"0")),
            },
        }
    }

    fn coerce_sqlite_numeric_real(value: &Value) -> Result<f64> {
        match Self::cast_value(value.clone(), crate::common::types::ColumnType::Real)? {
            Value::Real(value) => Ok(value),
            Value::Null => Ok(0.0),
            _ => unreachable!("real cast must yield REAL or NULL"),
        }
    }

    fn coerce_aggregate_numeric_value(value: &Value) -> Option<Value> {
        match value {
            Value::Null => None,
            Value::Integer(value) => Some(Value::Integer(*value)),
            Value::Real(value) => Some(Value::Real(*value)),
            Value::Boolean(value) => Some(Value::Integer(if *value { 1 } else { 0 })),
            Value::Text(value) => Some(Self::parse_sqlite_aggregate_numeric_text(value)),
            Value::Blob(value) => Some(Self::parse_sqlite_aggregate_numeric_text(
                &String::from_utf8_lossy(value),
            )),
        }
    }

    fn parse_sqlite_aggregate_numeric_text(value: &str) -> Value {
        let trimmed = value.trim();
        if let Ok(integer) = trimmed.parse::<i64>() {
            Value::Integer(integer)
        } else if let Ok(real) = trimmed.parse::<f64>() {
            Value::Real(real)
        } else {
            Value::Real(0.0)
        }
    }

    fn median_numeric_value(value: &Value) -> Result<f64> {
        match value {
            Value::Integer(value) => Ok(*value as f64),
            Value::Real(value) => Ok(*value),
            Value::Boolean(value) => Ok(if *value { 1.0 } else { 0.0 }),
            Value::Text(value) => value
                .trim()
                .parse::<f64>()
                .map_err(|_| DbError::plan("input to median() is not numeric")),
            Value::Blob(value) => String::from_utf8_lossy(value)
                .trim()
                .parse::<f64>()
                .map_err(|_| DbError::plan("input to median() is not numeric")),
            Value::Null => unreachable!("NULL values are skipped before median numeric coercion"),
        }
    }

    fn percentile_fraction_value(func: &AggregateFunc, value: &Value) -> Result<f64> {
        let function_name = Self::aggregate_function_name(*func);
        let fraction = Self::median_numeric_value(value).map_err(|_| {
            DbError::plan(format!(
                "the fraction argument to {function_name}() is not numeric"
            ))
        })?;
        let max = if matches!(func, AggregateFunc::Percentile) {
            100.0
        } else {
            1.0
        };
        if !(0.0..=max).contains(&fraction) {
            return Err(DbError::plan(format!(
                "the fraction argument to {function_name}() is not between 0.0 and {max}"
            )));
        }
        Ok(if matches!(func, AggregateFunc::Percentile) {
            fraction / 100.0
        } else {
            fraction
        })
    }

    fn percentile_numeric_value(function_name: &'static str, value: &Value) -> Result<f64> {
        Self::median_numeric_value(value)
            .map_err(|_| DbError::plan(format!("input to {function_name}() is not numeric")))
    }

    fn aggregate_function_name(func: AggregateFunc) -> &'static str {
        match func {
            AggregateFunc::Count => "count",
            AggregateFunc::Sum => "sum",
            AggregateFunc::Avg => "avg",
            AggregateFunc::Total => "total",
            AggregateFunc::Median => "median",
            AggregateFunc::Percentile => "percentile",
            AggregateFunc::PercentileCont => "percentile_cont",
            AggregateFunc::PercentileDisc => "percentile_disc",
            AggregateFunc::GroupConcat => "group_concat",
            AggregateFunc::JsonGroupArray => "json_group_array",
            AggregateFunc::JsonGroupObject => "json_group_object",
            AggregateFunc::Min => "min",
            AggregateFunc::Max => "max",
        }
    }

    fn coerce_sqlite_numeric_value(value: &Value) -> Value {
        match value {
            Value::Null => Value::Null,
            Value::Integer(value) => Value::Integer(*value),
            Value::Real(value) => Value::Real(*value),
            Value::Boolean(value) => Value::Integer(if *value { 1 } else { 0 }),
            Value::Text(value) => Self::coerce_sqlite_arithmetic_text(value),
            Value::Blob(value) => {
                Self::coerce_sqlite_arithmetic_text(&String::from_utf8_lossy(value))
            }
        }
    }

    fn sqlite_not_value(value: &Value) -> Value {
        match value {
            Value::Null => Value::Null,
            value => Value::Boolean(!Self::sqlite_is_true_value(value)),
        }
    }

    fn sqlite_is_true_value(value: &Value) -> bool {
        match value {
            Value::Null => false,
            Value::Integer(value) => *value != 0,
            Value::Real(value) => *value != 0.0,
            Value::Boolean(value) => *value,
            Value::Text(value) => match sqlite_text_numeric_prefix(value) {
                Value::Integer(value) => value != 0,
                Value::Real(value) => value != 0.0,
                _ => unreachable!("sqlite numeric prefix only returns numeric values"),
            },
            Value::Blob(value) => match sqlite_text_numeric_prefix(&String::from_utf8_lossy(value))
            {
                Value::Integer(value) => value != 0,
                Value::Real(value) => value != 0.0,
                _ => unreachable!("sqlite numeric prefix only returns numeric values"),
            },
        }
    }

    fn coerce_sqlite_arithmetic_text(value: &str) -> Value {
        let trimmed = value.trim();
        if let Ok(integer) = trimmed.parse::<i64>() {
            Value::Integer(integer)
        } else if let Ok(real) = trimmed.parse::<f64>() {
            Value::Real(real)
        } else {
            Value::Integer(0)
        }
    }

    fn sqlite_numeric_binary_op(
        left: Value,
        right: Value,
        f: impl FnOnce(f64, f64) -> f64,
    ) -> Result<Value> {
        let left = Self::coerce_sqlite_numeric_value(&left);
        let right = Self::coerce_sqlite_numeric_value(&right);
        match (left, right) {
            (Value::Integer(left), Value::Integer(right)) => {
                let result = f(left as f64, right as f64);
                if result.fract() == 0.0 {
                    Ok(Value::Integer(result as i64))
                } else {
                    Ok(Value::Real(result))
                }
            }
            (Value::Integer(left), Value::Real(right)) => Ok(Value::Real(f(left as f64, right))),
            (Value::Real(left), Value::Integer(right)) => Ok(Value::Real(f(left, right as f64))),
            (Value::Real(left), Value::Real(right)) => Ok(Value::Real(f(left, right))),
            _ => unreachable!("sqlite numeric coercion only returns numeric values"),
        }
    }

    fn sqlite_divide_op(left: Value, right: Value) -> Result<Value> {
        let left = Self::coerce_sqlite_numeric_value(&left);
        let right = Self::coerce_sqlite_numeric_value(&right);
        match (left, right) {
            (Value::Integer(_), Value::Integer(0))
            | (Value::Integer(_), Value::Real(0.0))
            | (Value::Real(_), Value::Integer(0))
            | (Value::Real(_), Value::Real(0.0)) => Ok(Value::Null),
            (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(left / right)),
            (Value::Integer(left), Value::Real(right)) => Ok(Value::Real(left as f64 / right)),
            (Value::Real(left), Value::Integer(right)) => Ok(Value::Real(left / right as f64)),
            (Value::Real(left), Value::Real(right)) => Ok(Value::Real(left / right)),
            _ => unreachable!("sqlite numeric coercion only returns numeric values"),
        }
    }

    fn sqlite_modulo_op(left: Value, right: Value) -> Result<Value> {
        let left = Self::coerce_sqlite_numeric_value(&left);
        let right = Self::coerce_sqlite_numeric_value(&right);
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(Value::Null);
        }
        let left = match left {
            Value::Integer(value) => value as f64,
            Value::Real(value) => value,
            _ => unreachable!("sqlite numeric coercion only returns numeric values"),
        };
        let right = match right {
            Value::Integer(value) => value as f64,
            Value::Real(value) => value,
            _ => unreachable!("sqlite numeric coercion only returns numeric values"),
        };
        if right == 0.0 {
            return Ok(Value::Null);
        }
        let result = left % right;
        if result.fract() == 0.0 {
            Ok(Value::Integer(result as i64))
        } else {
            Ok(Value::Real(result))
        }
    }

    fn sqlite_mod_function(left: Value, right: Value) -> Result<Value> {
        let left = match Self::coerce_sqlite_numeric_value(&left) {
            Value::Null => return Ok(Value::Null),
            Value::Integer(value) => value as f64,
            Value::Real(value) => value,
            _ => unreachable!("sqlite numeric coercion only returns numeric values"),
        };
        let right = match Self::coerce_sqlite_numeric_value(&right) {
            Value::Null => return Ok(Value::Null),
            Value::Integer(value) => value as f64,
            Value::Real(value) => value,
            _ => unreachable!("sqlite numeric coercion only returns numeric values"),
        };
        if right == 0.0 {
            return Ok(Value::Null);
        }
        Ok(Value::Real(left % right))
    }

    fn sqlite_bitwise_binary_op(
        left: Value,
        right: Value,
        op: impl FnOnce(i64, i64) -> i64,
    ) -> Result<Value> {
        let left = Self::sqlite_bitwise_integer_arg(&left)?;
        let right = Self::sqlite_bitwise_integer_arg(&right)?;
        Ok(Value::Integer(op(left, right)))
    }

    fn sqlite_bitwise_integer_arg(value: &Value) -> Result<i64> {
        match Self::cast_value(value.clone(), ColumnType::Integer)? {
            Value::Integer(value) => Ok(value),
            Value::Null => Ok(0),
            _ => unreachable!("integer cast must yield INTEGER or NULL"),
        }
    }

    fn sqlite_rounding_function(
        value: &Value,
        function_name: &str,
        op: impl FnOnce(f64) -> f64,
    ) -> Result<Value> {
        match value {
            Value::Null => Ok(Value::Null),
            Value::Integer(value) => Ok(Value::Integer(*value)),
            Value::Real(value) => Ok(Value::Real(op(*value))),
            Value::Boolean(value) => Ok(Value::Integer(if *value { 1 } else { 0 })),
            Value::Text(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Ok(Value::Null);
                }
                let parsed = trimmed.parse::<f64>().map_err(|_| {
                    DbError::plan(format!("{function_name} cannot coerce text to numeric"))
                });
                match parsed {
                    Ok(number) => Ok(Value::Real(op(number))),
                    Err(_) => Ok(Value::Null),
                }
            }
            Value::Blob(_) => Ok(Value::Null),
        }
    }

    fn sqlite_unary_math_function(
        value: &Value,
        function_name: &str,
        op: impl FnOnce(f64) -> Option<f64>,
    ) -> Result<Value> {
        match value {
            Value::Null => Ok(Value::Null),
            Value::Integer(value) => Ok(op(*value as f64).map(Value::Real).unwrap_or(Value::Null)),
            Value::Real(value) => Ok(op(*value).map(Value::Real).unwrap_or(Value::Null)),
            Value::Boolean(value) => Ok(op(if *value { 1.0 } else { 0.0 })
                .map(Value::Real)
                .unwrap_or(Value::Null)),
            Value::Text(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Ok(Value::Null);
                }
                match trimmed.parse::<f64>() {
                    Ok(number) => Ok(op(number).map(Value::Real).unwrap_or(Value::Null)),
                    Err(_) => Ok(Value::Null),
                }
            }
            Value::Blob(_) => {
                let _ = function_name;
                Ok(Value::Null)
            }
        }
    }

    fn sqlite_binary_math_function(
        left: &Value,
        right: &Value,
        function_name: &str,
        op: impl FnOnce(f64, f64) -> Option<f64>,
    ) -> Result<Value> {
        let left = match Self::sqlite_math_arg(left, function_name)? {
            Some(value) => value,
            None => return Ok(Value::Null),
        };
        let right = match Self::sqlite_math_arg(right, function_name)? {
            Some(value) => value,
            None => return Ok(Value::Null),
        };
        Ok(op(left, right).map(Value::Real).unwrap_or(Value::Null))
    }

    fn sqlite_math_arg(value: &Value, function_name: &str) -> Result<Option<f64>> {
        let _ = function_name;
        match value {
            Value::Null => Ok(None),
            Value::Integer(value) => Ok(Some(*value as f64)),
            Value::Real(value) => Ok(Some(*value)),
            Value::Boolean(value) => Ok(Some(if *value { 1.0 } else { 0.0 })),
            Value::Text(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                Ok(trimmed.parse::<f64>().ok())
            }
            Value::Blob(_) => Ok(None),
        }
    }

    fn compare(&self, left: &Value, right: &Value) -> Result<Option<Ordering>> {
        let ordering = match (left, right) {
            (Value::Null, Value::Null) => Some(Ordering::Equal),
            (Value::Boolean(left), Value::Boolean(right)) => Some(left.cmp(right)),
            (Value::Boolean(left), Value::Integer(right)) => {
                Some((if *left { 1_i64 } else { 0_i64 }).cmp(right))
            }
            (Value::Integer(left), Value::Boolean(right)) => {
                Some(left.cmp(&(if *right { 1_i64 } else { 0_i64 })))
            }
            (Value::Boolean(left), Value::Real(right)) => {
                Some((if *left { 1.0_f64 } else { 0.0_f64 }).total_cmp(right))
            }
            (Value::Real(left), Value::Boolean(right)) => {
                Some(left.total_cmp(&(if *right { 1.0_f64 } else { 0.0_f64 })))
            }
            (Value::Integer(left), Value::Integer(right)) => Some(left.cmp(right)),
            (Value::Integer(left), Value::Real(right)) => Some((*left as f64).total_cmp(right)),
            (Value::Real(left), Value::Integer(right)) => Some(left.total_cmp(&(*right as f64))),
            (Value::Real(left), Value::Real(right)) => Some(left.total_cmp(right)),
            (Value::Blob(left), Value::Blob(right)) => Some(left.cmp(right)),
            (Value::Text(left), Value::Text(right)) => Some(left.cmp(right)),
            _ => None,
        };

        Ok(ordering)
    }

    fn is_with_negation(&self, left: &Value, right: &Value, negated: bool) -> bool {
        let matches = matches!((left, right), (Value::Null, Value::Null))
            || (!matches!(left, Value::Null)
                && !matches!(right, Value::Null)
                && self.compare(left, right).ok().flatten() == Some(Ordering::Equal));
        matches ^ negated
    }

    fn tuple_is_with_negation(&self, left: &[Value], right: &[Value], negated: bool) -> bool {
        let matches = left.len() == right.len()
            && left.iter().zip(right).all(|(left_value, right_value)| {
                self.is_with_negation(left_value, right_value, false)
            });
        matches ^ negated
    }

    fn evaluate_in_rows(&self, left: &Value, rows: &[Row], negated: bool) -> bool {
        let candidates = rows
            .iter()
            .map(|row| row.first().cloned().unwrap_or(Value::Null))
            .collect::<Vec<_>>();
        self.evaluate_in_values(left, &candidates, negated)
    }

    fn evaluate_in_values(&self, left: &Value, values: &[Value], negated: bool) -> bool {
        match self.in_membership(left, values) {
            Some(result) => result ^ negated,
            None => false,
        }
    }

    fn in_result_value(&self, left: &Value, values: &[Value], negated: bool) -> Value {
        match self.in_membership(left, values) {
            Some(result) => Value::Boolean(result ^ negated),
            None => Value::Null,
        }
    }

    fn tuple_in_result_value(
        &self,
        left: &[Value],
        candidates: &[Vec<Value>],
        negated: bool,
    ) -> Value {
        match self.tuple_in_membership(left, candidates) {
            Some(result) => Value::Boolean(result ^ negated),
            None => Value::Null,
        }
    }

    fn tuple_compare_result_value(
        &self,
        left: &[Value],
        op: &CompareOp,
        right: &[Value],
    ) -> Result<Value> {
        match op {
            CompareOp::Eq | CompareOp::Ne => self.tuple_equality_result_value(left, op, right),
            CompareOp::Gt | CompareOp::Gte | CompareOp::Lt | CompareOp::Lte => {
                self.tuple_order_result_value(left, op, right)
            }
        }
    }

    fn tuple_between_result_value(
        &self,
        value: &[Value],
        low: &[Value],
        high: &[Value],
        negated: bool,
    ) -> Result<Value> {
        let lower = self.tuple_compare_result_value(low, &CompareOp::Lte, value)?;
        let upper = self.tuple_compare_result_value(value, &CompareOp::Lte, high)?;
        let result = match (&lower, &upper) {
            (Value::Boolean(false), _) | (_, Value::Boolean(false)) => Value::Boolean(false),
            (Value::Boolean(true), Value::Boolean(true)) => Value::Boolean(true),
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            _ => unreachable!("tuple comparison only returns boolean or NULL"),
        };
        Ok(match result {
            Value::Boolean(value) => Value::Boolean(value ^ negated),
            Value::Null => Value::Null,
            _ => unreachable!("tuple BETWEEN only returns boolean or NULL"),
        })
    }

    fn tuple_equality_result_value(
        &self,
        left: &[Value],
        op: &CompareOp,
        right: &[Value],
    ) -> Result<Value> {
        if left.len() != right.len() {
            return Err(DbError::plan(
                "row-value comparisons require the same arity",
            ));
        }

        let mut saw_null = false;
        for (left_value, right_value) in left.iter().zip(right) {
            if matches!(
                (left_value, right_value),
                (Value::Null, _) | (_, Value::Null)
            ) {
                saw_null = true;
                continue;
            }

            match self.compare(left_value, right_value)? {
                Some(Ordering::Equal) => {}
                Some(_) | None => return Ok(Value::Boolean(matches!(op, CompareOp::Ne))),
            }
        }

        if saw_null {
            Ok(Value::Null)
        } else {
            Ok(Value::Boolean(matches!(op, CompareOp::Eq)))
        }
    }

    fn tuple_order_result_value(
        &self,
        left: &[Value],
        op: &CompareOp,
        right: &[Value],
    ) -> Result<Value> {
        if left.len() != right.len() {
            return Err(DbError::plan(
                "row-value comparisons require the same arity",
            ));
        }

        for (left_value, right_value) in left.iter().zip(right) {
            if matches!(
                (left_value, right_value),
                (Value::Null, _) | (_, Value::Null)
            ) {
                return Ok(Value::Null);
            }

            match self.compare(left_value, right_value)? {
                Some(Ordering::Equal) => {}
                Some(ordering) => {
                    let matches = match op {
                        CompareOp::Gt => ordering == Ordering::Greater,
                        CompareOp::Gte => matches!(ordering, Ordering::Greater | Ordering::Equal),
                        CompareOp::Lt => ordering == Ordering::Less,
                        CompareOp::Lte => matches!(ordering, Ordering::Less | Ordering::Equal),
                        CompareOp::Eq | CompareOp::Ne => {
                            unreachable!("equality handled separately")
                        }
                    };
                    return Ok(Value::Boolean(matches));
                }
                None => return Ok(Value::Boolean(false)),
            }
        }

        Ok(Value::Boolean(matches!(
            op,
            CompareOp::Gte | CompareOp::Lte
        )))
    }

    fn tuple_in_membership(&self, left: &[Value], candidates: &[Vec<Value>]) -> Option<bool> {
        if candidates.is_empty() {
            return Some(false);
        }

        let mut saw_null = false;
        for candidate in candidates {
            if candidate.len() != left.len() {
                continue;
            }

            let mut row_can_match = true;
            let mut row_has_null = false;
            for (left_value, right_value) in left.iter().zip(candidate) {
                if matches!(
                    (left_value, right_value),
                    (Value::Null, _) | (_, Value::Null)
                ) {
                    row_has_null = true;
                    continue;
                }
                match self.compare(left_value, right_value).ok().flatten() {
                    Some(Ordering::Equal) => {}
                    Some(_) | None => {
                        row_can_match = false;
                        break;
                    }
                }
            }
            if row_can_match && !row_has_null {
                return Some(true);
            }
            saw_null |= row_can_match && row_has_null;
        }

        if saw_null { None } else { Some(false) }
    }

    fn in_membership(&self, left: &Value, values: &[Value]) -> Option<bool> {
        if values.is_empty() {
            return Some(false);
        }
        if matches!(left, Value::Null) {
            return None;
        }

        let mut saw_null = false;
        for value in values {
            if matches!(value, Value::Null) {
                saw_null = true;
                continue;
            }
            if self.compare(left, value).ok().flatten() == Some(Ordering::Equal) {
                return Some(true);
            }
        }

        if saw_null { None } else { Some(false) }
    }

    fn matches_like_pattern(
        value: &str,
        pattern: &str,
        escape: &Option<String>,
        case_sensitive: bool,
    ) -> Result<bool> {
        let escape = match escape {
            Some(escape) => {
                let mut chars = escape.chars();
                let Some(ch) = chars.next() else {
                    return Err(DbError::plan(
                        "ESCAPE expression must be a single character",
                    ));
                };
                if chars.next().is_some() {
                    return Err(DbError::plan(
                        "ESCAPE expression must be a single character",
                    ));
                }
                Some(ch)
            }
            None => None,
        };
        let value = value.chars().collect::<Vec<_>>();
        let pattern = Self::like_tokens(pattern, escape);
        let mut dp = vec![vec![false; pattern.len() + 1]; value.len() + 1];
        dp[0][0] = true;
        for pattern_index in 1..=pattern.len() {
            if pattern[pattern_index - 1] == LikeToken::Any {
                dp[0][pattern_index] = dp[0][pattern_index - 1];
            }
        }
        for value_index in 1..=value.len() {
            for pattern_index in 1..=pattern.len() {
                dp[value_index][pattern_index] = match pattern[pattern_index - 1] {
                    LikeToken::Any => {
                        dp[value_index][pattern_index - 1] || dp[value_index - 1][pattern_index]
                    }
                    LikeToken::One => dp[value_index - 1][pattern_index - 1],
                    LikeToken::Literal(ch) => {
                        dp[value_index - 1][pattern_index - 1]
                            && Self::sqlite_like_chars_equal(
                                value[value_index - 1],
                                ch,
                                case_sensitive,
                            )
                    }
                };
            }
        }
        Ok(dp[value.len()][pattern.len()])
    }

    fn like_tokens(pattern: &str, escape: Option<char>) -> Vec<LikeToken> {
        let mut tokens = Vec::new();
        let mut chars = pattern.chars().peekable();
        while let Some(ch) = chars.next() {
            if Some(ch) == escape {
                if let Some(next) = chars.next() {
                    tokens.push(LikeToken::Literal(next));
                } else {
                    tokens.push(LikeToken::Literal(ch));
                }
                continue;
            }
            match ch {
                '%' => tokens.push(LikeToken::Any),
                '_' => tokens.push(LikeToken::One),
                ch => tokens.push(LikeToken::Literal(ch)),
            }
        }
        tokens
    }

    fn sqlite_like_chars_equal(left: char, right: char, case_sensitive: bool) -> bool {
        if !case_sensitive && left.is_ascii() && right.is_ascii() {
            left.eq_ignore_ascii_case(&right)
        } else {
            left == right
        }
    }

    fn output_name(&self, item: &SelectItem) -> String {
        match item {
            SelectItem::Wildcard => "*".to_string(),
            SelectItem::Column(name) => name.clone(),
            SelectItem::AliasedColumn { alias, .. } => alias.clone(),
            SelectItem::Expr { expr, alias } => {
                alias.clone().unwrap_or_else(|| self.scalar_expr_name(expr))
            }
            SelectItem::Aggregate {
                func, arg, alias, ..
            } => alias.clone().unwrap_or_else(|| {
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
                        AggregateArg::Expr { expr, distinct, .. } => {
                            if *distinct {
                                format!("DISTINCT {}", self.scalar_expr_name(expr))
                            } else {
                                self.scalar_expr_name(expr)
                            }
                        }
                        AggregateArg::GroupConcat {
                            expr,
                            separator,
                            distinct,
                            ..
                        } => {
                            let expr = if *distinct {
                                format!("DISTINCT {}", self.scalar_expr_name(expr))
                            } else {
                                self.scalar_expr_name(expr)
                            };
                            if let Some(separator) = separator {
                                format!("{expr}, {}", self.scalar_expr_name(separator))
                            } else {
                                expr
                            }
                        }
                        AggregateArg::JsonGroupObject { key, value, .. } => {
                            format!(
                                "{}, {}",
                                self.scalar_expr_name(key),
                                self.scalar_expr_name(value)
                            )
                        }
                        AggregateArg::Percentile { expr, fraction, .. } => {
                            format!(
                                "{}, {}",
                                self.scalar_expr_name(expr),
                                self.scalar_expr_name(fraction)
                            )
                        }
                    }
                )
            }),
        }
    }

    fn scalar_expr_name(&self, expr: &ScalarExpr) -> String {
        match expr {
            ScalarExpr::Literal(value) => value.to_string(),
            ScalarExpr::Column(name) => name.clone(),
            ScalarExpr::UnaryMinus(expr) => format!("-{}", self.scalar_expr_name(expr)),
            ScalarExpr::BitNot(expr) => format!("~{}", self.scalar_expr_name(expr)),
            ScalarExpr::Not(expr) => format!("NOT {}", self.scalar_expr_name(expr)),
            ScalarExpr::Cast { expr, ty } => {
                format!("CAST({} AS {})", self.scalar_expr_name(expr), ty.name())
            }
            ScalarExpr::Collate { expr, collation } => {
                format!("{} COLLATE {}", self.scalar_expr_name(expr), collation)
            }
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => format!(
                "{} IS {}{}",
                self.scalar_expr_name(left),
                if *negated { "NOT " } else { "" },
                self.scalar_expr_name(right)
            ),
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => format!(
                "{} IS {}{}",
                self.scalar_expr_name(expr),
                if *negated { "NOT " } else { "" },
                if *value { "TRUE" } else { "FALSE" }
            ),
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => format!(
                "{} {}IN ({})",
                self.scalar_expr_name(expr),
                if *negated { "NOT " } else { "" },
                values
                    .iter()
                    .map(|value| self.scalar_expr_name(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ScalarExpr::InSubquery {
                expr,
                query: _,
                negated,
            } => format!(
                "{} {}IN (SELECT ...)",
                self.scalar_expr_name(expr),
                if *negated { "NOT " } else { "" }
            ),
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => format!(
                "{} {}LIKE '{}'{}",
                self.scalar_expr_name(expr),
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
                self.scalar_expr_name(expr),
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
                self.scalar_expr_name(expr),
                if *negated { "NOT " } else { "" },
                self.scalar_expr_name(low),
                self.scalar_expr_name(high)
            ),
            ScalarExpr::Compare { left, op, right } => format!(
                "{} {} {}",
                self.scalar_expr_name(left),
                match op {
                    CompareOp::Eq => "=",
                    CompareOp::Ne => "!=",
                    CompareOp::Gt => ">",
                    CompareOp::Gte => ">=",
                    CompareOp::Lt => "<",
                    CompareOp::Lte => "<=",
                },
                self.scalar_expr_name(right)
            ),
            ScalarExpr::CompareSubquery { left, op, query: _ } => format!(
                "{} {} (SELECT ...)",
                self.scalar_expr_name(left),
                match op {
                    CompareOp::Eq => "=",
                    CompareOp::Ne => "!=",
                    CompareOp::Gt => ">",
                    CompareOp::Gte => ">=",
                    CompareOp::Lt => "<",
                    CompareOp::Lte => "<=",
                }
            ),
            ScalarExpr::Subquery { .. } => "(SELECT ...)".to_string(),
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                let mut parts = vec!["CASE".to_string()];
                if let Some(base) = base {
                    parts.push(self.scalar_expr_name(base));
                }
                for (when_expr, then_expr) in when_then_clauses {
                    parts.push(format!(
                        "WHEN {} THEN {}",
                        self.scalar_expr_name(when_expr),
                        self.scalar_expr_name(then_expr)
                    ));
                }
                if let Some(else_expr) = else_expr {
                    parts.push(format!("ELSE {}", self.scalar_expr_name(else_expr)));
                }
                parts.push("END".to_string());
                parts.join(" ")
            }
            ScalarExpr::Binary { left, op, right } => format!(
                "{} {} {}",
                self.scalar_expr_name(left),
                match op {
                    ScalarBinaryOp::Add => "+",
                    ScalarBinaryOp::Subtract => "-",
                    ScalarBinaryOp::Multiply => "*",
                    ScalarBinaryOp::Divide => "/",
                    ScalarBinaryOp::Modulo => "%",
                    ScalarBinaryOp::BitAnd => "&",
                    ScalarBinaryOp::BitOr => "|",
                    ScalarBinaryOp::ShiftLeft => "<<",
                    ScalarBinaryOp::ShiftRight => ">>",
                    ScalarBinaryOp::Concat => "||",
                    ScalarBinaryOp::JsonExtract => "->",
                    ScalarBinaryOp::JsonExtractText => "->>",
                },
                self.scalar_expr_name(right)
            ),
            ScalarExpr::Function { func, args } => format!(
                "{}({})",
                Self::scalar_function_name(*func),
                args.iter()
                    .map(|arg| self.scalar_expr_name(arg))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ScalarExpr::Aggregate { func, arg, .. } => {
                self.aggregate_output_name(*func, arg.as_ref())
            }
            ScalarExpr::Tuple(values) => format!(
                "({})",
                values
                    .iter()
                    .map(|value| self.scalar_expr_name(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn aggregate_call_name(&self, call: &AggregateCall) -> String {
        self.aggregate_output_name(call.func, &call.arg)
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
                AggregateArg::Expr { expr, distinct, .. } => {
                    if *distinct {
                        format!("DISTINCT {}", self.scalar_expr_name(expr))
                    } else {
                        self.scalar_expr_name(expr)
                    }
                }
                AggregateArg::GroupConcat {
                    expr,
                    separator,
                    distinct,
                    ..
                } => {
                    let expr = if *distinct {
                        format!("DISTINCT {}", self.scalar_expr_name(expr))
                    } else {
                        self.scalar_expr_name(expr)
                    };
                    if let Some(separator) = separator {
                        format!("{expr}, {}", self.scalar_expr_name(separator))
                    } else {
                        expr
                    }
                }
                AggregateArg::JsonGroupObject { key, value, .. } => {
                    format!(
                        "{}, {}",
                        self.scalar_expr_name(key),
                        self.scalar_expr_name(value)
                    )
                }
                AggregateArg::Percentile { expr, fraction, .. } => {
                    format!(
                        "{}, {}",
                        self.scalar_expr_name(expr),
                        self.scalar_expr_name(fraction)
                    )
                }
            }
        )
    }

    fn scalar_function_name(func: ScalarFunc) -> &'static str {
        match func {
            ScalarFunc::Length => "LENGTH",
            ScalarFunc::OctetLength => "OCTET_LENGTH",
            ScalarFunc::MinScalar => "MIN",
            ScalarFunc::MaxScalar => "MAX",
            ScalarFunc::Date => "DATE",
            ScalarFunc::Time => "TIME",
            ScalarFunc::DateTime => "DATETIME",
            ScalarFunc::TimeDiff => "TIMEDIFF",
            ScalarFunc::Strftime => "STRFTIME",
            ScalarFunc::JulianDay => "JULIANDAY",
            ScalarFunc::UnixEpoch => "UNIXEPOCH",
            ScalarFunc::Changes => "CHANGES",
            ScalarFunc::TotalChanges => "TOTAL_CHANGES",
            ScalarFunc::Printf => "PRINTF",
            ScalarFunc::IIf => "IIF",
            ScalarFunc::If => "IF",
            ScalarFunc::Concat => "CONCAT",
            ScalarFunc::ConcatWs => "CONCAT_WS",
            ScalarFunc::SqliteSourceId => "SQLITE_SOURCE_ID",
            ScalarFunc::Sign => "SIGN",
            ScalarFunc::RandomBlob => "RANDOMBLOB",
            ScalarFunc::Random => "RANDOM",
            ScalarFunc::Unhex => "UNHEX",
            ScalarFunc::Unistr => "UNISTR",
            ScalarFunc::UnistrQuote => "UNISTR_QUOTE",
            ScalarFunc::SqliteVersion => "SQLITE_VERSION",
            ScalarFunc::SqliteCompileOptionUsed => "SQLITE_COMPILEOPTION_USED",
            ScalarFunc::SqliteCompileOptionGet => "SQLITE_COMPILEOPTION_GET",
            ScalarFunc::Likely => "LIKELY",
            ScalarFunc::Unlikely => "UNLIKELY",
            ScalarFunc::Likelihood => "LIKELIHOOD",
            ScalarFunc::Mod => "MOD",
            ScalarFunc::Ceil => "CEIL",
            ScalarFunc::Ceiling => "CEILING",
            ScalarFunc::Floor => "FLOOR",
            ScalarFunc::Trunc => "TRUNC",
            ScalarFunc::Pi => "PI",
            ScalarFunc::Sqrt => "SQRT",
            ScalarFunc::Power => "POWER",
            ScalarFunc::Exp => "EXP",
            ScalarFunc::Sin => "SIN",
            ScalarFunc::Cos => "COS",
            ScalarFunc::Tan => "TAN",
            ScalarFunc::Sinh => "SINH",
            ScalarFunc::Cosh => "COSH",
            ScalarFunc::Tanh => "TANH",
            ScalarFunc::Acos => "ACOS",
            ScalarFunc::Asin => "ASIN",
            ScalarFunc::Atan => "ATAN",
            ScalarFunc::Atan2 => "ATAN2",
            ScalarFunc::Acosh => "ACOSH",
            ScalarFunc::Asinh => "ASINH",
            ScalarFunc::Atanh => "ATANH",
            ScalarFunc::Ln => "LN",
            ScalarFunc::Log10 => "LOG10",
            ScalarFunc::Log2 => "LOG2",
            ScalarFunc::Log => "LOG",
            ScalarFunc::Degrees => "DEGREES",
            ScalarFunc::Radians => "RADIANS",
            ScalarFunc::Char => "CHAR",
            ScalarFunc::ZeroBlob => "ZEROBLOB",
            ScalarFunc::TypeOf => "TYPEOF",
            ScalarFunc::Subtype => "SUBTYPE",
            ScalarFunc::Hex => "HEX",
            ScalarFunc::Substr => "SUBSTR",
            ScalarFunc::Instr => "INSTR",
            ScalarFunc::Replace => "REPLACE",
            ScalarFunc::LikeFunc => "LIKE",
            ScalarFunc::GlobFunc => "GLOB",
            ScalarFunc::Quote => "QUOTE",
            ScalarFunc::Unicode => "UNICODE",
            ScalarFunc::Trim => "TRIM",
            ScalarFunc::LTrim => "LTRIM",
            ScalarFunc::RTrim => "RTRIM",
            ScalarFunc::Lower => "LOWER",
            ScalarFunc::Upper => "UPPER",
            ScalarFunc::Abs => "ABS",
            ScalarFunc::Round => "ROUND",
            ScalarFunc::LastInsertRowId => "LAST_INSERT_ROWID",
            ScalarFunc::Coalesce => "COALESCE",
            ScalarFunc::IfNull => "IFNULL",
            ScalarFunc::NullIf => "NULLIF",
            ScalarFunc::Json => "JSON",
            ScalarFunc::JsonValid => "JSON_VALID",
            ScalarFunc::JsonErrorPosition => "JSON_ERROR_POSITION",
            ScalarFunc::JsonPretty => "JSON_PRETTY",
            ScalarFunc::JsonQuote => "JSON_QUOTE",
            ScalarFunc::JsonExtract => "JSON_EXTRACT",
            ScalarFunc::JsonType => "JSON_TYPE",
            ScalarFunc::JsonArray => "JSON_ARRAY",
            ScalarFunc::JsonObject => "JSON_OBJECT",
            ScalarFunc::JsonArrayLength => "JSON_ARRAY_LENGTH",
            ScalarFunc::JsonRemove => "JSON_REMOVE",
            ScalarFunc::JsonSet => "JSON_SET",
            ScalarFunc::JsonInsert => "JSON_INSERT",
            ScalarFunc::JsonReplace => "JSON_REPLACE",
            ScalarFunc::JsonPatch => "JSON_PATCH",
        }
    }

    fn record_changes(&self, changes: i64) {
        self.changes.set(changes);
        self.total_changes
            .set(self.total_changes.get().saturating_add(changes));
    }
}

fn random_bytes(length: usize) -> Result<Vec<u8>> {
    let mut state = random_seed(length as u64)?;
    let mut bytes = Vec::with_capacity(length);
    for _ in 0..length {
        let word = next_random_u64(&mut state);
        bytes.push((word & 0xFF) as u8);
    }
    Ok(bytes)
}

fn random_i64() -> Result<i64> {
    let mut state = random_seed(0xA5A5_A5A5_A5A5_A5A5)?;
    Ok(next_random_u64(&mut state) as i64)
}

fn random_seed(mix: u64) -> Result<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DbError::storage(format!("system clock is before unix epoch: {error}")))?;
    Ok(duration.as_nanos() as u64 ^ mix.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn next_random_u64(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn render_pragma_default_value(default_value: &ColumnDefault) -> String {
    match default_value {
        ColumnDefault::Literal(value) => render_pragma_literal(value),
        ColumnDefault::CurrentTimestamp => "CURRENT_TIMESTAMP".to_string(),
        ColumnDefault::CurrentDate => "CURRENT_DATE".to_string(),
        ColumnDefault::CurrentTime => "CURRENT_TIME".to_string(),
    }
}

fn render_pragma_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(true) => "true".to_string(),
        Value::Boolean(false) => "false".to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Real(value) => value.to_string(),
        Value::Text(value) => format!("'{}'", value.replace('\'', "''")),
        Value::Blob(value) => {
            let hex = value
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>();
            format!("X'{hex}'")
        }
    }
}

fn decorated_index_term_is_desc(term: &str) -> bool {
    term.split_whitespace()
        .last()
        .is_some_and(|word| word.eq_ignore_ascii_case("DESC"))
}

fn decorated_index_term_collation(term: &str) -> Option<String> {
    let mut words = term.split_whitespace();
    while let Some(word) = words.next() {
        if word.eq_ignore_ascii_case("COLLATE") {
            return words.next().map(|collation| {
                collation
                    .trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | '[' | ']'))
                    .to_string()
            });
        }
    }
    None
}

fn index_term_column_name(term: &str) -> Option<String> {
    let trimmed = term.trim();
    if trimmed.is_empty() || trimmed.contains('(') {
        return None;
    }
    let first = trimmed.split_whitespace().next()?;
    let column = first.trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | '[' | ']'));
    (!column.is_empty()).then(|| column.to_string())
}

#[derive(Clone, Copy)]
enum DateTimeResultKind {
    Date,
    Time,
    DateTime,
}

#[derive(Clone, Copy, Default)]
enum DateShiftRounding {
    #[default]
    Ceiling,
    Floor,
}

#[derive(Clone, Copy)]
struct ParsedDateTimeParts {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millisecond: i64,
}

fn parse_sqlite_date_time_text(value: &str) -> Option<ParsedDateTimeParts> {
    if value.eq_ignore_ascii_case("now") {
        return current_date_time_parts().ok();
    }

    if let Some((year, month, day)) = parse_iso_date(value) {
        return Some(ParsedDateTimeParts {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            second: 0,
            millisecond: 0,
        });
    }

    if let Some((hour, minute, second, millisecond, timezone_offset_minutes)) =
        parse_iso_time_with_timezone(value)
    {
        let parts = ParsedDateTimeParts {
            year: 2000,
            month: 1,
            day: 1,
            hour,
            minute,
            second,
            millisecond,
        };
        return apply_timezone_offset(parts, timezone_offset_minutes);
    }

    let (date, time) = split_sqlite_datetime_text(value)?;
    let (year, month, day) = parse_iso_date(date)?;
    let (hour, minute, second, millisecond, timezone_offset_minutes) =
        parse_iso_time_with_timezone(time)?;
    let parts = ParsedDateTimeParts {
        year,
        month,
        day,
        hour,
        minute,
        second,
        millisecond,
    };
    apply_timezone_offset(parts, timezone_offset_minutes)
}

fn split_sqlite_datetime_text(value: &str) -> Option<(&str, &str)> {
    let trimmed = value.trim();
    let split_at = trimmed
        .char_indices()
        .find_map(|(index, ch)| (ch == ' ' || ch == 'T').then_some(index))?;
    let date = trimmed[..split_at].trim();
    let time = trimmed[split_at..].trim_start_matches([' ', 'T']).trim();
    if date.is_empty() || time.is_empty() {
        return None;
    }
    Some((date, time))
}

fn current_date_time_parts() -> Result<ParsedDateTimeParts> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DbError::storage(format!("system clock is before unix epoch: {error}")))?;
    let seconds = i64::try_from(duration.as_secs())
        .map_err(|_| DbError::storage("system clock seconds do not fit in i64"))?;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Ok(ParsedDateTimeParts {
        year,
        month,
        day,
        hour: seconds_of_day / 3_600,
        minute: (seconds_of_day % 3_600) / 60,
        second: seconds_of_day % 60,
        millisecond: 0,
    })
}

fn parse_iso_date(value: &str) -> Option<(i64, i64, i64)> {
    let mut parts = value.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<i64>().ok()?;
    let day = parts.next()?.parse::<i64>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

fn parse_iso_time(value: &str) -> Option<(i64, i64, i64, i64)> {
    let value = value.strip_suffix('Z').unwrap_or(value);
    let mut parts = value.split(':');
    let hour = parts.next()?.parse::<i64>().ok()?;
    let minute = parts.next()?.parse::<i64>().ok()?;
    let second_part = parts.next()?;
    let (second_text, fractional_text) = second_part
        .split_once('.')
        .map_or((second_part, None), |(second, fractional)| {
            (second, Some(fractional))
        });
    let second = second_text.parse::<i64>().ok()?;
    let millisecond = match fractional_text {
        Some(fractional) if !fractional.is_empty() => {
            if !fractional.chars().all(|ch| ch.is_ascii_digit()) {
                return None;
            }
            let mut digits = fractional.chars().take(3).collect::<String>();
            while digits.len() < 3 {
                digits.push('0');
            }
            digits.parse::<i64>().ok()?
        }
        Some(_) => return None,
        None => 0,
    };
    if parts.next().is_some()
        || !(0..=24).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }
    Some((hour, minute, second, millisecond))
}

fn parse_iso_time_with_timezone(value: &str) -> Option<(i64, i64, i64, i64, i64)> {
    let (time, timezone_offset_minutes) = split_timezone_offset(value.trim())?;
    let (hour, minute, second, millisecond) = parse_iso_time(time)?;
    Some((hour, minute, second, millisecond, timezone_offset_minutes))
}

fn split_timezone_offset(value: &str) -> Option<(&str, i64)> {
    let value = value.trim();
    if let Some(time) = value.strip_suffix('Z') {
        return Some((time.trim_end(), 0));
    }

    for (index, ch) in value.char_indices().rev() {
        if ch != '+' && ch != '-' {
            continue;
        }
        let time = value[..index].trim_end();
        let offset = value[index..].trim();
        let (hours, minutes) = offset[1..].split_once(':')?;
        let hours = hours.parse::<i64>().ok()?;
        let minutes = minutes.parse::<i64>().ok()?;
        if !(0..=14).contains(&hours) || !(0..=59).contains(&minutes) {
            return None;
        }
        let sign = if ch == '+' { 1 } else { -1 };
        return Some((time, sign * ((hours * 60) + minutes)));
    }

    Some((value, 0))
}

fn apply_timezone_offset(
    parts: ParsedDateTimeParts,
    timezone_offset_minutes: i64,
) -> Option<ParsedDateTimeParts> {
    if timezone_offset_minutes == 0 {
        return Some(parts);
    }
    shift_parsed_date_time_parts_by_millis(parts, timezone_offset_minutes.checked_mul(-60_000)?)
}

fn sqlite_julianday(parts: ParsedDateTimeParts) -> f64 {
    let a = (14 - parts.month) / 12;
    let y = parts.year + 4800 - a;
    let m = parts.month + 12 * a - 3;
    let julian_day_number =
        parts.day + ((153 * m + 2) / 5) + 365 * y + (y / 4) - (y / 100) + (y / 400) - 32045;
    let seconds = (parts.hour as f64 * 3600.0)
        + (parts.minute as f64 * 60.0)
        + parts.second as f64
        + (parts.millisecond as f64 / 1000.0);
    julian_day_number as f64 - 0.5 + (seconds / 86_400.0)
}

fn sqlite_unixepoch(parts: ParsedDateTimeParts) -> i64 {
    let days_since_unix_epoch = days_from_civil(parts.year, parts.month, parts.day);
    days_since_unix_epoch * 86_400 + (parts.hour * 3_600) + (parts.minute * 60) + parts.second
}

fn sqlite_unixepoch_subsecond(parts: ParsedDateTimeParts) -> f64 {
    sqlite_unixepoch(parts) as f64 + (parts.millisecond as f64 / 1000.0)
}

fn sqlite_timediff_between(start: ParsedDateTimeParts, end: ParsedDateTimeParts) -> String {
    if compare_date_time_parts(start, end) == Ordering::Greater {
        return sqlite_timediff_calendar_fields(end, start, '-');
    }
    sqlite_timediff_calendar_fields(start, end, '+')
}

fn sqlite_timediff_calendar_fields(
    start: ParsedDateTimeParts,
    end: ParsedDateTimeParts,
    sign: char,
) -> String {
    let mut years = end.year - start.year;
    let mut months = end.month - start.month;
    let mut days = end.day - start.day;
    let mut hours = end.hour - start.hour;
    let mut minutes = end.minute - start.minute;
    let mut seconds = end.second - start.second;
    let mut millis = end.millisecond - start.millisecond;

    if millis < 0 {
        millis += 1000;
        seconds -= 1;
    }
    if seconds < 0 {
        seconds += 60;
        minutes -= 1;
    }
    if minutes < 0 {
        minutes += 60;
        hours -= 1;
    }
    if hours < 0 {
        hours += 24;
        days -= 1;
    }

    if days < 0 {
        months -= 1;
        let (previous_year, previous_month) = previous_month(end.year, end.month);
        days += days_in_month(previous_year, previous_month);
    }
    if months < 0 {
        months += 12;
        years -= 1;
    }

    format!(
        "{sign}{years:04}-{months:02}-{days:02} {hours:02}:{minutes:02}:{seconds:02}.{millis:03}"
    )
}

fn previous_month(year: i64, month: i64) -> (i64, i64) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

fn compare_date_time_parts(left: ParsedDateTimeParts, right: ParsedDateTimeParts) -> Ordering {
    (
        left.year,
        left.month,
        left.day,
        left.hour,
        left.minute,
        left.second,
        left.millisecond,
    )
        .cmp(&(
            right.year,
            right.month,
            right.day,
            right.hour,
            right.minute,
            right.second,
            right.millisecond,
        ))
}

fn apply_sqlite_date_time_modifier(
    parts: ParsedDateTimeParts,
    modifier: &str,
    rounding: DateShiftRounding,
) -> Option<ParsedDateTimeParts> {
    let modifier = modifier.trim();
    if modifier.eq_ignore_ascii_case("start of day") {
        return Some(ParsedDateTimeParts {
            year: parts.year,
            month: parts.month,
            day: parts.day,
            hour: 0,
            minute: 0,
            second: 0,
            millisecond: 0,
        });
    }

    if modifier.eq_ignore_ascii_case("start of month") {
        return Some(ParsedDateTimeParts {
            year: parts.year,
            month: parts.month,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            millisecond: 0,
        });
    }

    if modifier.eq_ignore_ascii_case("start of year") {
        return Some(ParsedDateTimeParts {
            year: parts.year,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            millisecond: 0,
        });
    }

    if let Some(offset) = parse_sqlite_modifier_offset_millis(modifier, " day", 86_400_000.0) {
        return shift_parsed_date_time_parts_by_millis(parts, offset);
    }

    if let Some(offset) = parse_sqlite_modifier_offset_millis(modifier, " hour", 3_600_000.0) {
        return shift_parsed_date_time_parts_by_millis(parts, offset);
    }

    if let Some(offset) = parse_sqlite_modifier_offset_millis(modifier, " minute", 60_000.0) {
        return shift_parsed_date_time_parts_by_millis(parts, offset);
    }

    if let Some(offset) = parse_sqlite_modifier_offset_millis(modifier, " second", 1_000.0) {
        return shift_parsed_date_time_parts_by_millis(parts, offset);
    }

    if let Some(offset) = parse_sqlite_modifier_offset(modifier, " month") {
        return shift_parsed_date_time_parts_by_months(parts, offset, rounding);
    }

    if let Some(offset) = parse_sqlite_modifier_offset(modifier, " year") {
        return shift_parsed_date_time_parts_by_months(parts, offset.checked_mul(12)?, rounding);
    }

    if let Some(target_weekday) = parse_sqlite_weekday_modifier(modifier) {
        return shift_parsed_date_time_parts_to_weekday(parts, target_weekday);
    }

    None
}

fn parse_sqlite_modifier_offset(modifier: &str, suffix: &str) -> Option<i64> {
    if !modifier.ends_with(suffix) {
        return None;
    }

    modifier[..modifier.len() - suffix.len()]
        .trim()
        .parse::<i64>()
        .ok()
}

fn parse_sqlite_modifier_offset_millis(
    modifier: &str,
    suffix: &str,
    millis_per_unit: f64,
) -> Option<i64> {
    if !modifier.ends_with(suffix) {
        return None;
    }

    let value = modifier[..modifier.len() - suffix.len()]
        .trim()
        .parse::<f64>()
        .ok()?;
    if !value.is_finite() {
        return None;
    }
    let millis = (value * millis_per_unit).round();
    if millis < i64::MIN as f64 || millis > i64::MAX as f64 {
        return None;
    }
    Some(millis as i64)
}

fn parse_sqlite_weekday_modifier(modifier: &str) -> Option<i64> {
    let prefix = "weekday ";
    let suffix = modifier.strip_prefix(prefix)?;
    let weekday = suffix.trim().parse::<i64>().ok()?;
    if !(0..=6).contains(&weekday) {
        return None;
    }
    Some(weekday)
}

fn collect_date_time_modifiers(
    _function_name: &str,
    args: &[Value],
) -> Result<Option<Vec<String>>> {
    let mut modifiers = Vec::with_capacity(args.len());
    for value in args {
        match value {
            Value::Null => return Ok(None),
            value => modifiers.push(coerce_text_like_value_owned(value)),
        }
    }
    Ok(Some(modifiers))
}

fn coerce_text_like_value_owned(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Boolean(value) => {
            if *value {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        Value::Integer(value) => value.to_string(),
        Value::Real(value) => value.to_string(),
        Value::Blob(value) => String::from_utf8_lossy(value).into_owned(),
        Value::Text(value) => value.clone(),
    }
}

fn shift_parsed_date_time_parts_by_millis(
    parts: ParsedDateTimeParts,
    offset_millis: i64,
) -> Option<ParsedDateTimeParts> {
    let base_days = days_from_civil(parts.year, parts.month, parts.day);
    let base_millis = parts
        .hour
        .checked_mul(3_600_000)?
        .checked_add(parts.minute.checked_mul(60_000)?)?
        .checked_add(parts.second.checked_mul(1_000)?)?
        .checked_add(parts.millisecond)?;
    let shifted_millis = base_millis.checked_add(offset_millis)?;
    let shifted_days = base_days.checked_add(shifted_millis.div_euclid(86_400_000))?;
    let millis_of_day = shifted_millis.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(shifted_days);
    Some(ParsedDateTimeParts {
        year,
        month,
        day,
        hour: millis_of_day / 3_600_000,
        minute: (millis_of_day % 3_600_000) / 60_000,
        second: (millis_of_day % 60_000) / 1_000,
        millisecond: millis_of_day % 1_000,
    })
}

fn shift_parsed_date_time_parts_by_months(
    parts: ParsedDateTimeParts,
    offset_months: i64,
    rounding: DateShiftRounding,
) -> Option<ParsedDateTimeParts> {
    let zero_based_month = parts.month.checked_sub(1)?;
    let absolute_month = parts
        .year
        .checked_mul(12)?
        .checked_add(zero_based_month)?
        .checked_add(offset_months)?;
    let target_year = absolute_month.div_euclid(12);
    let target_month = absolute_month.rem_euclid(12) + 1;
    if matches!(rounding, DateShiftRounding::Floor)
        && parts.day > days_in_month(target_year, target_month)
    {
        return Some(ParsedDateTimeParts {
            year: target_year,
            month: target_month,
            day: days_in_month(target_year, target_month),
            hour: parts.hour,
            minute: parts.minute,
            second: parts.second,
            millisecond: parts.millisecond,
        });
    }
    let target_month_first_day = days_from_civil(target_year, target_month, 1);
    let shifted_days = target_month_first_day.checked_add(parts.day.checked_sub(1)?)?;
    let (year, month, day) = civil_from_days(shifted_days);
    Some(ParsedDateTimeParts {
        year,
        month,
        day,
        hour: parts.hour,
        minute: parts.minute,
        second: parts.second,
        millisecond: parts.millisecond,
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    let next_month = if month == 12 { 1 } else { month + 1 };
    let next_year = if month == 12 { year + 1 } else { year };
    days_from_civil(next_year, next_month, 1) - days_from_civil(year, month, 1)
}

fn shift_parsed_date_time_parts_to_weekday(
    parts: ParsedDateTimeParts,
    target_weekday: i64,
) -> Option<ParsedDateTimeParts> {
    let current_days = days_from_civil(parts.year, parts.month, parts.day);
    let current_weekday = (current_days + 4).rem_euclid(7);
    let delta_days = (target_weekday - current_weekday).rem_euclid(7);
    let shifted_days = current_days.checked_add(delta_days)?;
    let (year, month, day) = civil_from_days(shifted_days);
    Some(ParsedDateTimeParts {
        year,
        month,
        day,
        hour: parts.hour,
        minute: parts.minute,
        second: parts.second,
        millisecond: parts.millisecond,
    })
}

fn parse_sqlite_unixepoch_value(value: i64) -> Option<ParsedDateTimeParts> {
    let days = value.div_euclid(86_400);
    let seconds_of_day = value.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Some(ParsedDateTimeParts {
        year,
        month,
        day,
        hour: seconds_of_day / 3_600,
        minute: (seconds_of_day % 3_600) / 60,
        second: seconds_of_day % 60,
        millisecond: 0,
    })
}

fn parse_sqlite_unixepoch_real_value(value: f64) -> Option<ParsedDateTimeParts> {
    if !value.is_finite() || value < i64::MIN as f64 || value > i64::MAX as f64 {
        return None;
    }
    let whole_seconds = value.trunc() as i64;
    let parts = parse_sqlite_unixepoch_value(whole_seconds)?;
    let millis = ((value - whole_seconds as f64).abs() * 1000.0).round() as i64;
    if value.is_sign_negative() {
        shift_parsed_date_time_parts_by_millis(parts, -millis)
    } else {
        shift_parsed_date_time_parts_by_millis(parts, millis)
    }
}

fn parse_sqlite_auto_value(value: f64) -> Option<ParsedDateTimeParts> {
    // SQLite "auto" treats values in the valid julian-day range as JD, otherwise as unix seconds.
    if (0.0..=5_373_484.499_999).contains(&value) {
        return parse_sqlite_julian_day_value(value);
    }

    parse_sqlite_unixepoch_real_value(value)
}

fn parse_sqlite_julian_day_value(value: f64) -> Option<ParsedDateTimeParts> {
    if !value.is_finite() {
        return None;
    }

    let days_since_unix_epoch = (value + 0.5).floor() as i64 - 2_440_588;
    let fractional_day = value - (days_since_unix_epoch as f64 + 2_440_587.5);
    let mut millis_of_day = (fractional_day * 86_400_000.0).round() as i64;
    let extra_days = millis_of_day.div_euclid(86_400_000);
    millis_of_day = millis_of_day.rem_euclid(86_400_000);
    let shifted_days = days_since_unix_epoch.checked_add(extra_days)?;
    let (year, month, day) = civil_from_days(shifted_days);

    Some(ParsedDateTimeParts {
        year,
        month,
        day,
        hour: millis_of_day / 3_600_000,
        minute: (millis_of_day % 3_600_000) / 60_000,
        second: (millis_of_day % 60_000) / 1_000,
        millisecond: millis_of_day % 1_000,
    })
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    }
    .div_euclid(400);
    let yoe = adjusted_year - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2).div_euclid(5) + day - 1;
    let doe = yoe * 365 + yoe.div_euclid(4) - yoe.div_euclid(100) + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe.div_euclid(1_460) + doe.div_euclid(36_524) - doe.div_euclid(146_096))
        .div_euclid(365);
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe.div_euclid(4) - yoe.div_euclid(100));
    let mp = (5 * doy + 2).div_euclid(153);
    let day = doy - (153 * mp + 2).div_euclid(5) + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

fn sqlite_strftime_minimal(
    format: &str,
    parts: ParsedDateTimeParts,
    subsecond: bool,
) -> Option<String> {
    let mut rendered = String::with_capacity(format.len());
    let mut chars = format.chars();

    while let Some(ch) = chars.next() {
        if ch != '%' {
            rendered.push(ch);
            continue;
        }

        let directive = chars.next()?;
        match directive {
            '%' => rendered.push('%'),
            'Y' => rendered.push_str(&format!("{:04}", parts.year)),
            'm' => rendered.push_str(&format!("{:02}", parts.month)),
            'd' => rendered.push_str(&format!("{:02}", parts.day)),
            'e' => rendered.push_str(&format!("{:2}", parts.day)),
            'H' => rendered.push_str(&format!("{:02}", parts.hour)),
            'M' => rendered.push_str(&format!("{:02}", parts.minute)),
            'S' => rendered.push_str(&format!("{:02}", parts.second)),
            'F' => rendered.push_str(&format!(
                "{:04}-{:02}-{:02}",
                parts.year, parts.month, parts.day
            )),
            'T' => rendered.push_str(&format!(
                "{:02}:{:02}:{:02}",
                parts.hour, parts.minute, parts.second
            )),
            'J' => rendered.push_str(&sqlite_julianday(parts).to_string()),
            's' if subsecond => rendered.push_str(&format!(
                "{}.{:03}",
                sqlite_unixepoch(parts),
                parts.millisecond
            )),
            's' => rendered.push_str(&sqlite_unixepoch(parts).to_string()),
            'j' => rendered.push_str(&format!("{:03}", sqlite_day_of_year(parts))),
            'w' => rendered.push_str(&sqlite_sunday_weekday(parts).to_string()),
            'u' => rendered.push_str(&sqlite_monday_weekday(parts).to_string()),
            'U' => rendered.push_str(&format!("{:02}", sqlite_sunday_week_number(parts))),
            'W' => rendered.push_str(&format!("{:02}", sqlite_monday_week_number(parts))),
            'V' => rendered.push_str(&format!("{:02}", sqlite_iso_week(parts).1)),
            'G' => rendered.push_str(&format!("{:04}", sqlite_iso_week(parts).0)),
            'g' => rendered.push_str(&format!("{:02}", sqlite_iso_week(parts).0.rem_euclid(100))),
            'R' => rendered.push_str(&format!("{:02}:{:02}", parts.hour, parts.minute)),
            'f' => rendered.push_str(&format!("{:02}.{:03}", parts.second, parts.millisecond)),
            'I' => rendered.push_str(&format!("{:02}", sqlite_12_hour(parts.hour))),
            'p' => rendered.push_str(if parts.hour < 12 { "AM" } else { "PM" }),
            'P' => rendered.push_str(if parts.hour < 12 { "am" } else { "pm" }),
            'k' => rendered.push_str(&format!("{:2}", parts.hour)),
            'l' => rendered.push_str(&format!("{:2}", sqlite_12_hour(parts.hour))),
            _ => return None,
        }
    }

    Some(rendered)
}

fn sqlite_12_hour(hour: i64) -> i64 {
    let hour = hour.rem_euclid(12);
    if hour == 0 { 12 } else { hour }
}

fn sqlite_day_of_year(parts: ParsedDateTimeParts) -> i64 {
    days_from_civil(parts.year, parts.month, parts.day) - days_from_civil(parts.year, 1, 1) + 1
}

fn sqlite_sunday_weekday(parts: ParsedDateTimeParts) -> i64 {
    (days_from_civil(parts.year, parts.month, parts.day) + 4).rem_euclid(7)
}

fn sqlite_monday_weekday(parts: ParsedDateTimeParts) -> i64 {
    let weekday = sqlite_sunday_weekday(parts);
    if weekday == 0 { 7 } else { weekday }
}

fn sqlite_monday_week_number(parts: ParsedDateTimeParts) -> i64 {
    let yday = sqlite_day_of_year(parts) - 1;
    let monday_weekday = (sqlite_sunday_weekday(parts) + 6).rem_euclid(7);
    (yday + 7 - monday_weekday) / 7
}

fn sqlite_sunday_week_number(parts: ParsedDateTimeParts) -> i64 {
    let yday = sqlite_day_of_year(parts) - 1;
    let sunday_weekday = sqlite_sunday_weekday(parts);
    (yday + 7 - sunday_weekday) / 7
}

fn sqlite_iso_week(parts: ParsedDateTimeParts) -> (i64, i64) {
    let days = days_from_civil(parts.year, parts.month, parts.day);
    let monday_weekday = sqlite_monday_weekday(parts);
    let thursday_days = days + (4 - monday_weekday);
    let (iso_year, _, _) = civil_from_days(thursday_days);
    let week1_monday = days_from_civil(iso_year, 1, 4)
        - (sqlite_monday_weekday(ParsedDateTimeParts {
            year: iso_year,
            month: 1,
            day: 4,
            hour: 0,
            minute: 0,
            second: 0,
            millisecond: 0,
        }) - 1);
    let iso_week = ((days - week1_monday) / 7) + 1;
    (iso_year, iso_week)
}

fn sqlite_unistr(value: &str) -> Result<String> {
    let mut output = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some('\\') => {
                chars.next();
                output.push('\\');
            }
            Some('u') => {
                chars.next();
                output.push(parse_unistr_escape(&mut chars, 4)?);
            }
            Some('U') => {
                chars.next();
                output.push(parse_unistr_escape(&mut chars, 8)?);
            }
            Some('+') => {
                chars.next();
                output.push(parse_unistr_escape(&mut chars, 6)?);
            }
            Some(_) => output.push(parse_unistr_escape_with_first(&mut chars, 4)?),
            None => return Err(DbError::plan("invalid Unicode escape")),
        }
    }
    Ok(output)
}

fn sqlite_unistr_quote(value: &Value) -> String {
    let Value::Text(value) = value else {
        return sqlite_quote_value(value);
    };
    if !value
        .chars()
        .any(|ch| matches!(ch, '\u{0001}'..='\u{001f}'))
    {
        return sqlite_quote_text(value);
    }

    let mut quoted = String::from("unistr('");
    for ch in value.chars() {
        match ch {
            '\'' => quoted.push_str("''"),
            '\\' => quoted.push_str("\\\\"),
            '\u{0001}'..='\u{001f}' => {
                quoted.push_str(&format!("\\u{:04x}", u32::from(ch)));
            }
            ch => quoted.push(ch),
        }
    }
    quoted.push_str("')");
    quoted
}

fn sqlite_quote_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(value) => {
            if *value {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        Value::Integer(value) => value.to_string(),
        Value::Real(value) => sqlite_real_to_text_for_quote(*value),
        Value::Blob(value) => format!(
            "X'{}'",
            value
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
        Value::Text(value) => sqlite_quote_text(value),
    }
}

fn sqlite_quote_text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn sqlite_real_to_text_for_quote(value: f64) -> String {
    let rendered = value.to_string();
    if rendered.contains(['.', 'e', 'E']) {
        rendered
    } else {
        format!("{rendered}.0")
    }
}

fn json_path_lookup<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Result<Option<&'a serde_json::Value>> {
    let mut current = value;
    let mut remaining = path
        .strip_prefix('$')
        .ok_or_else(|| DbError::plan("JSON path must start with '$'"))?;
    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix('.') {
            let key_end = rest.find(['.', '[']).unwrap_or(rest.len());
            let key = &rest[..key_end];
            if key.is_empty() {
                return Err(DbError::plan("invalid JSON path"));
            }
            let Some(next) = current.get(key) else {
                return Ok(None);
            };
            current = next;
            remaining = &rest[key_end..];
            continue;
        }
        if let Some(rest) = remaining.strip_prefix('[') {
            let Some(index_end) = rest.find(']') else {
                return Err(DbError::plan("invalid JSON path"));
            };
            let Some(index) = json_path_array_index(current, &rest[..index_end])? else {
                return Ok(None);
            };
            let Some(next) = current.get(index) else {
                return Ok(None);
            };
            current = next;
            remaining = &rest[index_end + 1..];
            continue;
        }
        return Err(DbError::plan("invalid JSON path"));
    }
    Ok(Some(current))
}

fn json_path_array_index(value: &serde_json::Value, index: &str) -> Result<Option<usize>> {
    if index == "#" {
        return Ok(None);
    }
    if let Some(tail) = index.strip_prefix("#-") {
        let offset = tail
            .parse::<usize>()
            .map_err(|_| DbError::plan("invalid JSON array index"))?;
        let Some(length) = value.as_array().map(Vec::len) else {
            return Ok(None);
        };
        return Ok(length.checked_sub(offset));
    }
    index
        .parse::<usize>()
        .map(Some)
        .map_err(|_| DbError::plan("invalid JSON array index"))
}

fn parse_sqlite_json_value(
    json: &str,
) -> std::result::Result<serde_json::Value, serde_json::Error> {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(value) => Ok(value),
        Err(original) => match quote_json5_object_keys(json) {
            Some(normalized) => serde_json::from_str::<serde_json::Value>(&normalized),
            None => Err(original),
        },
    }
}

fn json_error_position(json: &str, error: &serde_json::Error) -> i64 {
    let line = error.line();
    let column = error.column();
    if line == 0 || column == 0 {
        return 1;
    }

    let mut current_line = 1;
    let mut current_column = 1;
    for (index, ch) in json.char_indices() {
        if current_line == line && current_column == column {
            return index as i64 + 1;
        }
        if ch == '\n' {
            current_line += 1;
            current_column = 1;
        } else {
            current_column += 1;
        }
    }
    json.len() as i64 + 1
}

fn json_pretty_render(value: &serde_json::Value, indent: &str) -> String {
    let mut output = String::new();
    json_pretty_render_into(value, indent, 0, &mut output);
    output
}

fn json_pretty_render_into(
    value: &serde_json::Value,
    indent: &str,
    depth: usize,
    output: &mut String,
) {
    match value {
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {
            output.push_str(
                &serde_json::to_string(value)
                    .expect("serde_json scalar values must serialize to JSON"),
            );
        }
        serde_json::Value::Array(values) => {
            if values.is_empty() {
                output.push_str("[]");
                return;
            }
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push('\n');
                for _ in 0..=depth {
                    output.push_str(indent);
                }
                json_pretty_render_into(value, indent, depth + 1, output);
            }
            output.push('\n');
            for _ in 0..depth {
                output.push_str(indent);
            }
            output.push(']');
        }
        serde_json::Value::Object(object) => {
            if object.is_empty() {
                output.push_str("{}");
                return;
            }
            output.push('{');
            for (index, (key, value)) in object.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push('\n');
                for _ in 0..=depth {
                    output.push_str(indent);
                }
                output.push_str(
                    &serde_json::to_string(key)
                        .expect("serde_json object keys must serialize to JSON strings"),
                );
                output.push_str(": ");
                json_pretty_render_into(value, indent, depth + 1, output);
            }
            output.push('\n');
            for _ in 0..depth {
                output.push_str(indent);
            }
            output.push('}');
        }
    }
}

fn quote_json5_object_keys(json: &str) -> Option<String> {
    let bytes = json.as_bytes();
    let mut output = String::with_capacity(json.len());
    let mut index = 0;
    let mut changed = false;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            output.push(byte as char);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        if byte == b'"' {
            in_string = true;
            output.push('"');
            index += 1;
            continue;
        }

        if byte == b'\'' {
            output.push('"');
            index += 1;
            changed = true;
            while index < bytes.len() {
                let byte = bytes[index];
                if byte == b'\'' {
                    output.push('"');
                    index += 1;
                    break;
                }
                if byte == b'\\' {
                    if index + 1 >= bytes.len() {
                        output.push('\\');
                        index += 1;
                        continue;
                    }
                    let escaped = bytes[index + 1];
                    match escaped {
                        b'\'' => output.push('\''),
                        b'"' => output.push_str("\\\""),
                        b'\\' => output.push_str("\\\\"),
                        b'/' => output.push('/'),
                        b'b' | b'f' | b'n' | b'r' | b't' | b'u' => {
                            output.push('\\');
                            output.push(escaped as char);
                        }
                        _ => {
                            output.push('\\');
                            output.push(escaped as char);
                        }
                    }
                    index += 2;
                    continue;
                }
                match byte {
                    b'"' => output.push_str("\\\""),
                    b'\n' => output.push_str("\\n"),
                    b'\r' => output.push_str("\\r"),
                    b'\t' => output.push_str("\\t"),
                    _ => output.push(byte as char),
                }
                index += 1;
            }
            continue;
        }

        if byte == b'/' && matches!(bytes.get(index + 1), Some(b'*')) {
            let Some(comment_end) = find_block_comment_end(bytes, index + 2) else {
                output.push('/');
                index += 1;
                continue;
            };
            output.push(' ');
            index = comment_end + 2;
            changed = true;
            continue;
        }

        if byte == b'/' && matches!(bytes.get(index + 1), Some(b'/')) {
            output.push(' ');
            index += 2;
            while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                index += 1;
            }
            changed = true;
            continue;
        }

        if byte == b'{' || byte == b',' {
            let delimiter_output_start = output.len();
            output.push(byte as char);
            index += 1;
            let (next_index, skipped_comment) =
                skip_json5_whitespace_and_comments(bytes, index, &mut output);
            index = next_index;
            changed |= skipped_comment;
            if byte == b',' && index < bytes.len() && matches!(bytes[index], b'}' | b']') {
                output.truncate(delimiter_output_start);
                changed = true;
                continue;
            }
            if let Some(key_end) = json5_unquoted_key_end(json, index) {
                let key_start = index;
                index = key_end;
                let key = &json[key_start..index];
                let mut lookahead = index;
                while lookahead < bytes.len() && bytes[lookahead].is_ascii_whitespace() {
                    lookahead += 1;
                }
                if lookahead < bytes.len() && bytes[lookahead] == b':' {
                    output.push('"');
                    output.push_str(key);
                    output.push('"');
                    changed = true;
                    continue;
                }
                output.push_str(&json[key_start..index]);
            }
            continue;
        }

        if let Some((normalized, next_index)) = normalize_json5_special_value_token(bytes, index) {
            output.push_str(&normalized);
            index = next_index;
            changed = true;
            continue;
        }

        if let Some((normalized, next_index)) = normalize_json5_number_token(bytes, index) {
            output.push_str(&normalized);
            index = next_index;
            changed = true;
            continue;
        }

        output.push(byte as char);
        index += 1;
    }

    changed.then_some(output)
}

fn json5_unquoted_key_end(json: &str, start: usize) -> Option<usize> {
    let mut chars = json[start..].char_indices();
    let (_, first) = chars.next()?;
    if !is_json5_unquoted_key_start(first) {
        return None;
    }
    let mut end = start + first.len_utf8();
    for (offset, ch) in chars {
        if !is_json5_unquoted_key_continue(ch) {
            break;
        }
        end = start + offset + ch.len_utf8();
    }
    Some(end)
}

fn is_json5_unquoted_key_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic() || (ch as u32) >= 0x80
}

fn is_json5_unquoted_key_continue(ch: char) -> bool {
    is_json5_unquoted_key_start(ch) || ch.is_ascii_digit()
}

fn skip_json5_whitespace_and_comments(
    bytes: &[u8],
    mut index: usize,
    output: &mut String,
) -> (usize, bool) {
    let mut skipped_comment = false;
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            output.push(bytes[index] as char);
            index += 1;
        }
        if bytes.get(index) == Some(&b'/') && matches!(bytes.get(index + 1), Some(b'*')) {
            let Some(comment_end) = find_block_comment_end(bytes, index + 2) else {
                return (index, skipped_comment);
            };
            output.push(' ');
            index = comment_end + 2;
            skipped_comment = true;
            continue;
        }
        if bytes.get(index) == Some(&b'/') && matches!(bytes.get(index + 1), Some(b'/')) {
            output.push(' ');
            index += 2;
            while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                index += 1;
            }
            skipped_comment = true;
            continue;
        }
        return (index, skipped_comment);
    }
}

fn find_block_comment_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index + 1 < bytes.len() {
        if bytes[index] == b'*' && bytes[index + 1] == b'/' {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn normalize_json5_special_value_token(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    let mut index = start;
    let sign = if matches!(bytes.get(index), Some(b'+') | Some(b'-')) {
        let sign = bytes[index] as char;
        index += 1;
        Some(sign)
    } else {
        None
    };

    let (literal, replacement) = if ascii_keyword_at(bytes, index, b"QNaN") {
        ("QNaN", "null")
    } else if ascii_keyword_at(bytes, index, b"SNaN") {
        ("SNaN", "null")
    } else if ascii_keyword_at(bytes, index, b"NaN") {
        ("NaN", "null")
    } else {
        return None;
    };

    index += literal.len();
    if sign == Some('-') || sign == Some('+') {
        return None;
    }
    if !json5_number_boundary(bytes, index) {
        return None;
    }
    Some((replacement.to_string(), index))
}

fn ascii_keyword_at(bytes: &[u8], start: usize, keyword: &[u8]) -> bool {
    bytes
        .get(start..start + keyword.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(keyword))
}

fn normalize_json5_number_token(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    let mut index = start;
    let sign = if matches!(bytes.get(index), Some(b'+') | Some(b'-')) {
        let sign = bytes[index] as char;
        index += 1;
        Some(sign)
    } else {
        None
    };

    if index >= bytes.len() {
        return None;
    }

    if bytes.get(index) == Some(&b'0') && matches!(bytes.get(index + 1), Some(b'x' | b'X')) {
        index += 2;
        let digits_start = index;
        while index < bytes.len() && bytes[index].is_ascii_hexdigit() {
            index += 1;
        }
        if digits_start == index || !json5_number_boundary(bytes, index) {
            return None;
        }
        let digits = std::str::from_utf8(&bytes[digits_start..index]).ok()?;
        let value = i128::from_str_radix(digits, 16).ok()?;
        let value = if sign == Some('-') { -value } else { value };
        return Some((value.to_string(), index));
    }

    let mut has_digits_before_dot = false;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
        has_digits_before_dot = true;
    }

    let mut has_dot = false;
    let mut has_digits_after_dot = false;
    if bytes.get(index) == Some(&b'.') {
        has_dot = true;
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
            has_digits_after_dot = true;
        }
    }

    if !has_digits_before_dot && !has_digits_after_dot {
        return None;
    }

    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        let exponent_marker = index;
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if exponent_start == index {
            index = exponent_marker;
        }
    }

    if !json5_number_boundary(bytes, index) {
        return None;
    }

    let token = std::str::from_utf8(&bytes[start..index]).ok()?;
    if !token.starts_with('+') && !token.starts_with('.') && !token.ends_with('.') {
        return None;
    }

    let mut normalized = token.trim_start_matches('+').to_string();
    if normalized.starts_with('.') {
        normalized.insert(0, '0');
    } else if normalized.starts_with("-.") {
        normalized.insert(1, '0');
    }
    if has_dot && normalized.ends_with('.') {
        normalized.push('0');
    }
    Some((normalized, index))
}

fn json5_number_boundary(bytes: &[u8], index: usize) -> bool {
    matches!(
        bytes.get(index),
        None | Some(b',' | b'}' | b']' | b':' | b' ' | b'\n' | b'\r' | b'\t')
    )
}

fn json_merge_patch(target: &mut serde_json::Value, patch: serde_json::Value) {
    let serde_json::Value::Object(patch_object) = patch else {
        *target = patch;
        return;
    };

    if !target.is_object() {
        *target = serde_json::Value::Object(serde_json::Map::new());
    }
    let serde_json::Value::Object(target_object) = target else {
        unreachable!("target was normalized to object");
    };

    for (key, value) in patch_object {
        if value.is_null() {
            target_object.remove(&key);
            continue;
        }
        match target_object.get_mut(&key) {
            Some(target_value) => json_merge_patch(target_value, value),
            None => {
                target_object.insert(key, value);
            }
        }
    }
}

fn json_remove_path(value: &mut serde_json::Value, path: &str) -> Result<()> {
    let remaining = path
        .strip_prefix('$')
        .ok_or_else(|| DbError::plan("JSON path must start with '$'"))?;
    if remaining.is_empty() {
        return Err(DbError::plan("root path is handled by caller"));
    }
    json_remove_path_tail(value, remaining)
}

fn json_remove_path_tail(value: &mut serde_json::Value, remaining: &str) -> Result<()> {
    if let Some(rest) = remaining.strip_prefix('.') {
        let key_end = rest.find(['.', '[']).unwrap_or(rest.len());
        let key = &rest[..key_end];
        if key.is_empty() {
            return Err(DbError::plan("invalid JSON path"));
        }
        let tail = &rest[key_end..];
        if tail.is_empty() {
            if let serde_json::Value::Object(object) = value {
                object.remove(key);
            }
            return Ok(());
        }
        let Some(next) = value.get_mut(key) else {
            return Ok(());
        };
        return json_remove_path_tail(next, tail);
    }

    if let Some(rest) = remaining.strip_prefix('[') {
        let Some(index_end) = rest.find(']') else {
            return Err(DbError::plan("invalid JSON path"));
        };
        let index = rest[..index_end]
            .parse::<usize>()
            .map_err(|_| DbError::plan("invalid JSON array index"))?;
        let tail = &rest[index_end + 1..];
        if tail.is_empty() {
            if let serde_json::Value::Array(values) = value {
                if index < values.len() {
                    values.remove(index);
                }
            }
            return Ok(());
        }
        let Some(next) = value.get_mut(index) else {
            return Ok(());
        };
        return json_remove_path_tail(next, tail);
    }

    Err(DbError::plan("invalid JSON path"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonWriteMode {
    Set,
    Insert,
    Replace,
}

fn json_write_path(
    value: &mut serde_json::Value,
    path: &str,
    replacement: serde_json::Value,
    mode: JsonWriteMode,
) -> Result<()> {
    let remaining = path
        .strip_prefix('$')
        .ok_or_else(|| DbError::plan("JSON path must start with '$'"))?;
    if remaining.is_empty() {
        return Err(DbError::plan("root path is handled by caller"));
    }
    json_write_path_tail(value, remaining, replacement, mode)
}

fn json_write_path_tail(
    value: &mut serde_json::Value,
    remaining: &str,
    replacement: serde_json::Value,
    mode: JsonWriteMode,
) -> Result<()> {
    if let Some(rest) = remaining.strip_prefix('.') {
        let key_end = rest.find(['.', '[']).unwrap_or(rest.len());
        let key = &rest[..key_end];
        if key.is_empty() {
            return Err(DbError::plan("invalid JSON path"));
        }
        let tail = &rest[key_end..];
        if tail.is_empty() {
            if !value.is_object() {
                if matches!(mode, JsonWriteMode::Replace) {
                    return Ok(());
                }
                *value = serde_json::Value::Object(serde_json::Map::new());
            }
            if let serde_json::Value::Object(object) = value {
                let exists = object.contains_key(key);
                if matches!(
                    (mode, exists),
                    (JsonWriteMode::Set, _)
                        | (JsonWriteMode::Insert, false)
                        | (JsonWriteMode::Replace, true)
                ) {
                    object.insert(key.to_string(), replacement);
                }
            }
            return Ok(());
        }
        if !value.is_object() {
            if matches!(mode, JsonWriteMode::Replace) {
                return Ok(());
            }
            *value = serde_json::Value::Object(serde_json::Map::new());
        }
        let serde_json::Value::Object(object) = value else {
            unreachable!("value was normalized to object");
        };
        let next = match object.get_mut(key) {
            Some(next) => next,
            None if matches!(mode, JsonWriteMode::Set | JsonWriteMode::Insert) => object
                .entry(key.to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new())),
            None => return Ok(()),
        };
        return json_write_path_tail(next, tail, replacement, mode);
    }

    if let Some(rest) = remaining.strip_prefix('[') {
        let Some(index_end) = rest.find(']') else {
            return Err(DbError::plan("invalid JSON path"));
        };
        let index = rest[..index_end]
            .parse::<usize>()
            .map_err(|_| DbError::plan("invalid JSON array index"))?;
        let tail = &rest[index_end + 1..];
        let serde_json::Value::Array(values) = value else {
            return Ok(());
        };
        if tail.is_empty() {
            if index < values.len() {
                if matches!(mode, JsonWriteMode::Set | JsonWriteMode::Replace) {
                    values[index] = replacement;
                }
            } else if index == values.len()
                && matches!(mode, JsonWriteMode::Set | JsonWriteMode::Insert)
            {
                values.push(replacement);
            }
            return Ok(());
        }
        let Some(next) = values.get_mut(index) else {
            return Ok(());
        };
        return json_write_path_tail(next, tail, replacement, mode);
    }

    Err(DbError::plan("invalid JSON path"))
}

fn json_value_to_sql(value: &serde_json::Value) -> Result<Value> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(value) => Ok(Value::Integer(i64::from(*value))),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(Value::Integer(value))
            } else if let Some(value) = value.as_f64() {
                Ok(Value::Real(value))
            } else {
                Ok(Value::Null)
            }
        }
        serde_json::Value::String(value) => Ok(Value::Text(value.clone())),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => serde_json::to_string(value)
            .map(Value::Text)
            .map_err(|error| DbError::plan(format!("failed to render JSON value: {error}"))),
    }
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(true) => "true",
        serde_json::Value::Bool(false) => "false",
        serde_json::Value::Number(value) if value.is_i64() || value.is_u64() => "integer",
        serde_json::Value::Number(_) => "real",
        serde_json::Value::String(_) => "text",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn parse_unistr_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    len: usize,
) -> Result<char> {
    let mut value = 0_u32;
    for _ in 0..len {
        let Some(ch) = chars.next() else {
            return Err(DbError::plan("invalid Unicode escape"));
        };
        let Some(digit) = ch.to_digit(16) else {
            return Err(DbError::plan("invalid Unicode escape"));
        };
        value = (value << 4) | digit;
    }
    char::from_u32(value).ok_or_else(|| DbError::plan("invalid Unicode escape"))
}

fn parse_unistr_escape_with_first(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    len: usize,
) -> Result<char> {
    parse_unistr_escape(chars, len)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn sqlite_ctas_column_dedup_base(name: &str) -> &str {
    let Some((base, suffix)) = name.rsplit_once(':') else {
        return name;
    };
    if suffix.is_empty() || suffix.bytes().any(|byte| !byte.is_ascii_digit()) {
        return name;
    }
    base
}

fn sqlite_ascii_lower(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii() {
                ch.to_ascii_lowercase()
            } else {
                ch
            }
        })
        .collect()
}

fn sqlite_ascii_upper(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii() {
                ch.to_ascii_uppercase()
            } else {
                ch
            }
        })
        .collect()
}

fn sqlite_substr_text(value: &str, start: i64, length: Option<i64>) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    let (begin, end) = sqlite_substr_bounds(characters.len(), start, length);
    characters[begin..end].iter().collect()
}

fn sqlite_substr_blob(value: &[u8], start: i64, length: Option<i64>) -> Vec<u8> {
    let (begin, end) = sqlite_substr_bounds(value.len(), start, length);
    value[begin..end].to_vec()
}

fn sqlite_substr_bounds(item_count: usize, start: i64, length: Option<i64>) -> (usize, usize) {
    let len = i64::try_from(item_count).unwrap_or(i64::MAX);
    let start_index = if start > 0 {
        start - 1
    } else if start < 0 {
        len.saturating_add(start)
    } else {
        0
    };

    let (begin, end) = match length {
        None => (start_index.clamp(0, len), len),
        Some(length) if length >= 0 => {
            let begin = start_index.clamp(0, len);
            let end = start_index.saturating_add(length).clamp(0, len);
            (begin, end)
        }
        Some(length) => {
            let begin = start_index.saturating_add(length).clamp(0, len);
            let end = start_index.clamp(0, len);
            (begin, end)
        }
    };

    if begin >= end {
        return (0, 0);
    }
    let begin = usize::try_from(begin).unwrap_or(usize::MAX);
    let end = usize::try_from(end).unwrap_or(usize::MAX);
    (begin, end)
}

fn sqlite_text_integer_prefix(value: &str) -> i64 {
    let trimmed = value.trim_start();
    let mut end = 0usize;
    for (index, ch) in trimmed.char_indices() {
        let allowed_sign = index == 0 && matches!(ch, '+' | '-');
        if allowed_sign || ch.is_ascii_digit() {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    let candidate = &trimmed[..end];
    if candidate.is_empty() || matches!(candidate, "+" | "-") {
        0
    } else {
        candidate.parse::<i64>().unwrap_or(0)
    }
}

fn sqlite_text_numeric_prefix(value: &str) -> Value {
    let Some((candidate, has_real_syntax)) = sqlite_numeric_text_prefix(value) else {
        return Value::Integer(0);
    };
    if !has_real_syntax && let Ok(integer) = candidate.parse::<i64>() {
        return Value::Integer(integer);
    }

    let real = candidate.parse::<f64>().unwrap_or(0.0);
    const MAX_EXACT_F64_INTEGER: f64 = 9_007_199_254_740_991.0;
    if real.is_finite() && real.fract() == 0.0 && real.abs() <= MAX_EXACT_F64_INTEGER {
        return Value::Integer(real as i64);
    }
    Value::Real(real)
}

fn sqlite_numeric_text_prefix(value: &str) -> Option<(&str, bool)> {
    let trimmed = value.trim_start();
    let mut chars = trimmed.char_indices().peekable();
    let mut end = 0usize;
    let mut saw_digit = false;
    let mut saw_dot = false;
    let mut saw_exp = false;
    if let Some((index, ch)) = chars.peek().copied()
        && index == 0
        && matches!(ch, '+' | '-')
    {
        end = ch.len_utf8();
        chars.next();
    }
    while let Some((index, ch)) = chars.peek().copied() {
        if ch.is_ascii_digit() {
            saw_digit = true;
            end = index + ch.len_utf8();
            chars.next();
        } else if ch == '.' && !saw_dot {
            saw_dot = true;
            end = index + ch.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    if !saw_digit {
        return None;
    }
    if let Some((exp_index, ch)) = chars.peek().copied()
        && matches!(ch, 'e' | 'E')
    {
        let mut exp_end = exp_index + ch.len_utf8();
        let mut lookahead = chars.clone();
        lookahead.next();
        if let Some((sign_index, sign)) = lookahead.peek().copied()
            && matches!(sign, '+' | '-')
        {
            exp_end = sign_index + sign.len_utf8();
            lookahead.next();
        }
        let mut saw_exp_digit = false;
        while let Some((index, digit)) = lookahead.peek().copied() {
            if digit.is_ascii_digit() {
                saw_exp_digit = true;
                exp_end = index + digit.len_utf8();
                lookahead.next();
            } else {
                break;
            }
        }
        if saw_exp_digit {
            saw_exp = true;
            end = exp_end;
        }
    }
    Some((&trimmed[..end], saw_dot || saw_exp))
}

fn sqlite_text_real_prefix(value: &str) -> f64 {
    let trimmed = value.trim_start();
    let mut chars = trimmed.char_indices().peekable();
    let mut end = 0usize;
    let mut saw_digit = false;
    let mut saw_dot = false;
    if let Some((index, ch)) = chars.peek().copied()
        && index == 0
        && matches!(ch, '+' | '-')
    {
        end = ch.len_utf8();
        chars.next();
    }
    while let Some((index, ch)) = chars.peek().copied() {
        if ch.is_ascii_digit() {
            saw_digit = true;
            end = index + ch.len_utf8();
            chars.next();
        } else if ch == '.' && !saw_dot {
            saw_dot = true;
            end = index + ch.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    if !saw_digit {
        return 0.0;
    }
    if let Some((exp_index, ch)) = chars.peek().copied()
        && matches!(ch, 'e' | 'E')
    {
        let mut exp_end = exp_index + ch.len_utf8();
        let mut lookahead = chars.clone();
        lookahead.next();
        if let Some((sign_index, sign)) = lookahead.peek().copied()
            && matches!(sign, '+' | '-')
        {
            exp_end = sign_index + sign.len_utf8();
            lookahead.next();
        }
        let mut saw_exp_digit = false;
        while let Some((index, digit)) = lookahead.peek().copied() {
            if digit.is_ascii_digit() {
                saw_exp_digit = true;
                exp_end = index + digit.len_utf8();
                lookahead.next();
            } else {
                break;
            }
        }
        if saw_exp_digit {
            end = exp_end;
        }
    }
    trimmed[..end].parse::<f64>().unwrap_or(0.0)
}

fn scalar_expr_collation(expr: &ScalarExpr) -> Option<&str> {
    match expr {
        ScalarExpr::Collate { collation, .. } => Some(collation.as_str()),
        _ => None,
    }
}

fn sqlite_nocase_cmp(left: &str, right: &str) -> Ordering {
    let mut left_chars = left.chars();
    let mut right_chars = right.chars();
    loop {
        match (left_chars.next(), right_chars.next()) {
            (Some(left), Some(right)) => {
                let left = if left.is_ascii() {
                    left.to_ascii_lowercase()
                } else {
                    left
                };
                let right = if right.is_ascii() {
                    right.to_ascii_lowercase()
                } else {
                    right
                };
                let ordering = left.cmp(&right);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (None, None) => return Ordering::Equal,
        }
    }
}

fn sqlite_rtrim_cmp(left: &str, right: &str) -> Ordering {
    left.trim_end_matches(' ').cmp(right.trim_end_matches(' '))
}

fn plan_writes_database(plan: &Plan) -> bool {
    matches!(
        plan,
        Plan::CreateTable { .. }
            | Plan::CreateTableAs { .. }
            | Plan::CreateIndex { .. }
            | Plan::DropTable { .. }
            | Plan::DropIndex { .. }
            | Plan::AlterTable { .. }
            | Plan::Insert { .. }
            | Plan::InsertReturning { .. }
            | Plan::InsertUpsert { .. }
            | Plan::InsertUpsertReturning { .. }
            | Plan::InsertMany { .. }
            | Plan::InsertManyUpsert { .. }
            | Plan::InsertManyUpsertReturning { .. }
            | Plan::InsertManyReturning { .. }
            | Plan::InsertDoNothing { .. }
            | Plan::InsertDoNothingReturning { .. }
            | Plan::InsertManyDoNothing { .. }
            | Plan::InsertManyDoNothingReturning { .. }
            | Plan::InsertExpr { .. }
            | Plan::InsertExprReturning { .. }
            | Plan::InsertExprUpsert { .. }
            | Plan::InsertExprUpsertReturning { .. }
            | Plan::InsertManyExpr { .. }
            | Plan::InsertManyExprUpsert { .. }
            | Plan::InsertManyExprUpsertReturning { .. }
            | Plan::InsertManyExprReturning { .. }
            | Plan::InsertExprDoNothing { .. }
            | Plan::InsertExprDoNothingReturning { .. }
            | Plan::InsertManyExprDoNothing { .. }
            | Plan::InsertManyExprDoNothingReturning { .. }
            | Plan::InsertSelect { .. }
            | Plan::InsertSelectReturning { .. }
            | Plan::InsertSelectUpsert { .. }
            | Plan::InsertSelectUpsertReturning { .. }
            | Plan::InsertSelectDoNothing { .. }
            | Plan::InsertSelectDoNothingReturning { .. }
            | Plan::Delete { .. }
            | Plan::DeleteLimited { .. }
            | Plan::DeleteReturning { .. }
            | Plan::DeleteReturningLimited { .. }
            | Plan::Update { .. }
            | Plan::UpdateLimited { .. }
            | Plan::UpdateReturning { .. }
            | Plan::UpdateReturningLimited { .. }
            | Plan::SetPragmaUserVersion { .. }
            | Plan::SetPragmaApplicationId { .. }
            | Plan::SetPragmaSchemaVersion { .. }
    )
}

fn sqlite_function_list_rows() -> Vec<Row> {
    const UTF8: &str = "utf8";
    const SCALAR: &str = "s";
    const AGGREGATE: &str = "w";
    const DEFAULT_FLAGS: i64 = 2_097_152;
    const DETERMINISTIC_FLAGS: i64 = 2_099_200;

    fn row(name: &str, kind: &str, narg: i64, flags: i64) -> Row {
        vec![
            Value::from(name),
            Value::Integer(1),
            Value::from(kind),
            Value::from(UTF8),
            Value::Integer(narg),
            Value::Integer(flags),
        ]
    }

    let mut rows = vec![
        row("abs", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("acos", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("acosh", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("asin", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("asinh", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("atan", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("atan2", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("atanh", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("avg", AGGREGATE, 1, DEFAULT_FLAGS),
        row("ceil", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("ceiling", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("char", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("changes", SCALAR, 0, DEFAULT_FLAGS),
        row("coalesce", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("concat", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("concat_ws", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("cos", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("count", AGGREGATE, 0, DEFAULT_FLAGS),
        row("count", AGGREGATE, 1, DEFAULT_FLAGS),
        row("date", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("datetime", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("degrees", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("exp", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("floor", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("format", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("glob", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("group_concat", AGGREGATE, 1, DEFAULT_FLAGS),
        row("group_concat", AGGREGATE, 2, DEFAULT_FLAGS),
        row("hex", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("if", SCALAR, -4, DETERMINISTIC_FLAGS),
        row("ifnull", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("iif", SCALAR, -4, DETERMINISTIC_FLAGS),
        row("instr", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("julianday", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("json", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("json_array", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("json_array_length", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("json_array_length", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("json_error_position", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("json_extract", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("json_insert", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("json_object", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("json_patch", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("json_pretty", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("json_pretty", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("json_quote", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("json_remove", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("json_replace", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("json_set", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("json_type", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("json_type", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("json_valid", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("json_valid", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("json_group_array", AGGREGATE, 1, DEFAULT_FLAGS),
        row("json_group_object", AGGREGATE, 2, DEFAULT_FLAGS),
        row("last_insert_rowid", SCALAR, 0, DEFAULT_FLAGS),
        row("length", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("like", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("like", SCALAR, 3, DETERMINISTIC_FLAGS),
        row("likelihood", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("likely", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("ln", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("log", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("log", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("log10", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("log2", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("lower", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("max", AGGREGATE, 1, DEFAULT_FLAGS),
        row("max", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("median", AGGREGATE, 1, DEFAULT_FLAGS),
        row("min", AGGREGATE, 1, DEFAULT_FLAGS),
        row("min", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("mod", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("nullif", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("octet_length", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("percentile", AGGREGATE, 2, DEFAULT_FLAGS),
        row("percentile_cont", AGGREGATE, 2, DEFAULT_FLAGS),
        row("percentile_disc", AGGREGATE, 2, DEFAULT_FLAGS),
        row("pi", SCALAR, 0, DETERMINISTIC_FLAGS),
        row("pow", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("power", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("printf", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("quote", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("radians", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("random", SCALAR, 0, DEFAULT_FLAGS),
        row("randomblob", SCALAR, 1, DEFAULT_FLAGS),
        row("replace", SCALAR, 3, DETERMINISTIC_FLAGS),
        row("round", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("round", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("rtrim", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("rtrim", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("sign", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("sinh", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("sin", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("sqlite_compileoption_get", SCALAR, 1, DEFAULT_FLAGS),
        row("sqlite_compileoption_used", SCALAR, 1, DEFAULT_FLAGS),
        row("sqlite_source_id", SCALAR, 0, DEFAULT_FLAGS),
        row("sqlite_version", SCALAR, 0, DEFAULT_FLAGS),
        row("sqrt", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("strftime", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("string_agg", AGGREGATE, 2, DEFAULT_FLAGS),
        row("substr", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("substr", SCALAR, 3, DETERMINISTIC_FLAGS),
        row("substring", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("substring", SCALAR, 3, DETERMINISTIC_FLAGS),
        row("subtype", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("sum", AGGREGATE, 1, DEFAULT_FLAGS),
        row("cosh", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("tan", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("tanh", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("time", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("timediff", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("total", AGGREGATE, 1, DEFAULT_FLAGS),
        row("total_changes", SCALAR, 0, DEFAULT_FLAGS),
        row("trim", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("trim", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("trunc", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("typeof", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("unicode", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("unixepoch", SCALAR, -1, DETERMINISTIC_FLAGS),
        row("unlikely", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("unhex", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("unhex", SCALAR, 2, DETERMINISTIC_FLAGS),
        row("unistr", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("unistr_quote", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("upper", SCALAR, 1, DETERMINISTIC_FLAGS),
        row("zeroblob", SCALAR, 1, DETERMINISTIC_FLAGS),
    ];
    rows.sort_by(|left, right| left[0].cmp(&right[0]).then_with(|| left[4].cmp(&right[4])));
    rows
}

fn sqlite_compile_option_used(requested: &str) -> bool {
    let requested = sqlite_compile_option_match_key(requested);
    SQLITE_COMPILE_OPTIONS.iter().any(|option| {
        let option = sqlite_compile_option_match_key(option);
        option == requested
            || option
                .strip_prefix(&requested)
                .is_some_and(|suffix| suffix.starts_with('='))
    })
}

fn sqlite_compile_option_match_key(value: &str) -> String {
    let upper = value.to_ascii_uppercase();
    upper.strip_prefix("SQLITE_").unwrap_or(&upper).to_string()
}

fn matches_ignore_conflict(or_conflict: Option<&str>, error: &DbError) -> bool {
    if !or_conflict.is_some_and(|mode| mode.eq_ignore_ascii_case("IGNORE")) {
        return false;
    }

    let message = error.to_string();
    message.contains("cannot be NULL")
        || message.contains("check constraint")
        || message.contains("duplicate primary key")
        || message.contains("unique index")
}

fn matches_replace_conflict(or_conflict: Option<&str>, error: &DbError) -> bool {
    if !or_conflict.is_some_and(|mode| mode.eq_ignore_ascii_case("REPLACE")) {
        return false;
    }

    let message = error.to_string();
    message.contains("duplicate primary key") || message.contains("unique index")
}

fn matches_rollback_conflict(or_conflict: Option<&str>, error: &DbError) -> bool {
    if !or_conflict.is_some_and(|mode| mode.eq_ignore_ascii_case("ROLLBACK")) {
        return false;
    }

    let message = error.to_string();
    message.contains("cannot be NULL")
        || message.contains("check constraint")
        || message.contains("duplicate primary key")
        || message.contains("unique index")
}

fn autoindex_name(table: &str, ordinal: usize) -> String {
    format!("sqlite_autoindex_{table}_{ordinal}")
}

fn sqlite_percentile_cont(values: &[f64], fraction: f64) -> f64 {
    if values.len() == 1 {
        return values[0];
    }
    let position = fraction * (values.len() as f64 - 1.0);
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        values[lower]
    } else {
        let weight = position - lower as f64;
        values[lower] + ((values[upper] - values[lower]) * weight)
    }
}

fn sqlite_percentile_disc(values: &[f64], fraction: f64) -> f64 {
    let index = (fraction * (values.len() as f64 - 1.0)).floor() as usize;
    values[index]
}
