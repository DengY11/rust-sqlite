use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::common::error::{DbError, Result};
use crate::common::types::{ColumnDef, IndexMeta, Row, RowId, Schema, Value};
use crate::engine::{PlanningStorageEngine, TransactionId};
use crate::sql::ast::{
    AggregateArg, AggregateFunc, AlterTableAction, CompareOp, Expr, JoinKind, NullOrder, OrderBy,
    OrderByExpr, ScalarBinaryOp, ScalarExpr, SelectItem, Statement, TableConstraint,
};
use crate::sql::optimizer::Optimizer;
use crate::sql::plan::{IndexScanMode, IndexScanSpec, JoinPlan, Plan};
use crate::sql::planner::Planner;

#[derive(Debug, Clone)]
struct ColumnMeta {
    table: Option<String>,
    alias: Option<String>,
    name: String,
    output_name: String,
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
    Sum { sum: i128, seen: bool },
    SumDistinct(BTreeSet<Value>),
    Avg { sum: i128, count: i64 },
    AvgDistinct(BTreeSet<Value>),
    Min(Option<Value>),
    Max(Option<Value>),
}

struct AggregateExecOptions<'a> {
    columns: &'a [SelectItem],
    group_by: &'a [String],
    having: Option<&'a Expr>,
    order_by: &'a [OrderBy],
    limit: Option<usize>,
}

pub struct Executor<'a, S: PlanningStorageEngine> {
    storage: &'a S,
    current_txn: &'a Cell<Option<TransactionId>>,
}

impl<'a, S: PlanningStorageEngine> Executor<'a, S> {
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
            Plan::CreateTable {
                name,
                columns,
                constraints,
            } => {
                let mut schema = Schema::new(name, columns);
                for constraint in constraints {
                    match constraint {
                        TableConstraint::Check(check) => schema = schema.with_check(check),
                        TableConstraint::ForeignKey(foreign_key) => {
                            schema = schema.with_foreign_key(foreign_key);
                        }
                    }
                }
                self.validate_create_table_foreign_key_metadata(transaction_id, &schema)?;
                self.storage.create_schema(transaction_id, schema)?;
                Ok(Vec::new())
            }
            Plan::CreateIndex {
                name,
                table,
                columns,
                unique,
            } => {
                self.storage.create_index(
                    transaction_id,
                    &table,
                    IndexMeta {
                        name,
                        columns,
                        unique,
                    },
                )?;
                Ok(Vec::new())
            }
            Plan::DropTable { name } => {
                self.storage.drop_schema(transaction_id, &name)?;
                Ok(Vec::new())
            }
            Plan::DropIndex { table, name } => {
                self.storage.drop_index(transaction_id, &table, &name)?;
                Ok(Vec::new())
            }
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
                }
                Ok(Vec::new())
            }
            Plan::Insert { table, values } => {
                let schema = self.require_schema(transaction_id, &table)?;
                self.validate_foreign_key_references(transaction_id, &schema, &values)?;
                self.storage.insert_row(transaction_id, &table, values)?;
                Ok(Vec::new())
            }
            Plan::Delete { table, filter } => {
                let source = self.scan_table_rowset(transaction_id, &table, None, None, None)?;
                let rows = self.storage.scan_rows(transaction_id, &table)?;
                let mut pending_deletes = Vec::new();
                for ((row_id, stored_row), source_row) in rows.iter().zip(source.rows.iter()) {
                    if self.matches_filter(
                        transaction_id,
                        &source,
                        source_row,
                        filter.as_ref(),
                        None,
                    )? {
                        pending_deletes.push((*row_id, stored_row.clone()));
                    }
                }

                for (_, stored_row) in &pending_deletes {
                    self.validate_no_foreign_key_dependents(transaction_id, &table, stored_row)?;
                }

                for (row_id, _) in pending_deletes {
                    self.storage.delete_row(transaction_id, &table, row_id)?;
                }
                Ok(Vec::new())
            }
            Plan::Update {
                table,
                assignments,
                filter,
            } => {
                let schema = self.require_schema(transaction_id, &table)?;
                let source = self.scan_table_rowset(transaction_id, &table, None, None, None)?;
                let rows = self.storage.scan_rows(transaction_id, &table)?;
                let indexes = self.storage.list_indexes(transaction_id, &table)?;
                let mut pending_updates = Vec::new();
                let mut final_rows = Vec::with_capacity(source.rows.len());

                for ((row_id, _), row) in rows.iter().zip(source.rows.iter()) {
                    if self.matches_filter(transaction_id, &source, row, filter.as_ref(), None)? {
                        let mut updated = row.clone();
                        for assignment in &assignments {
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
                            updated[position] = assignment.value.clone();
                        }
                        final_rows.push(updated.clone());
                        pending_updates.push((*row_id, row.clone(), updated));
                    } else {
                        final_rows.push(row.clone());
                    }
                }

                self.validate_update_result_constraints(
                    transaction_id,
                    &schema,
                    &indexes,
                    &final_rows,
                )?;
                self.validate_update_parent_key_changes(
                    transaction_id,
                    &table,
                    &schema,
                    &pending_updates,
                )?;

                for (row_id, _, updated) in pending_updates {
                    self.storage.delete_row(transaction_id, &table, row_id)?;
                    self.storage.insert_row(transaction_id, &table, updated)?;
                }
                Ok(Vec::new())
            }
            query_plan => Ok(self.execute_query_plan(transaction_id, query_plan)?.rows),
        }
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
            schema.validate_check_constraints(row)?;
            self.validate_foreign_key_references(transaction_id, schema, row)?;
        }

        self.validate_update_primary_key_uniqueness(schema, final_rows)?;
        self.validate_update_unique_index_constraints(schema, indexes, final_rows)
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
            parent_schema.column_index(&foreign_key.ref_column)?;
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
        let default_value = column.default_value.clone().unwrap_or(Value::Null);
        updated_schema.columns.push(column.clone());
        updated_schema.validate_constraints_metadata()?;

        for (_, row) in self.storage.scan_rows(transaction_id, table)? {
            let mut candidate = row;
            candidate.push(default_value.clone());
            updated_schema.validate_row_values(&candidate)?;
            updated_schema.validate_check_constraints(&candidate)?;
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
        for foreign_key in schema.all_foreign_keys() {
            let child_value = schema.value_for_column(row, &foreign_key.column)?;
            if matches!(child_value, Value::Null) {
                continue;
            }

            let parent_schema = self.require_schema(transaction_id, &foreign_key.ref_table)?;
            let parent_column = parent_schema.column_index(&foreign_key.ref_column)?;
            let parent_rows = self
                .storage
                .scan_rows(transaction_id, &foreign_key.ref_table)?;
            let found = parent_rows
                .iter()
                .any(|(_, parent_row)| parent_row.get(parent_column) == Some(child_value));

            if !found {
                return Err(DbError::storage(format!(
                    "foreign key constraint failed: {} references {}({})",
                    foreign_key.column, foreign_key.ref_table, foreign_key.ref_column
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
        let parent_schema = self.require_schema(transaction_id, parent_table)?;
        for child_schema in self.storage.list_schemas(transaction_id)? {
            for foreign_key in child_schema
                .all_foreign_keys()
                .into_iter()
                .filter(|foreign_key| foreign_key.ref_table == parent_table)
            {
                let parent_value =
                    parent_schema.value_for_column(parent_row, &foreign_key.ref_column)?;
                if matches!(parent_value, Value::Null) {
                    continue;
                }

                self.validate_no_foreign_key_dependents_for_key(
                    transaction_id,
                    &child_schema,
                    &foreign_key.column,
                    parent_table,
                    &foreign_key.ref_column,
                    parent_value,
                )?;
            }
        }

        Ok(())
    }

    fn validate_no_foreign_key_dependents_for_key(
        &self,
        transaction_id: TransactionId,
        child_schema: &Schema,
        child_column: &str,
        parent_table: &str,
        parent_column: &str,
        parent_value: &Value,
    ) -> Result<()> {
        let child_rows = self.storage.scan_rows(transaction_id, &child_schema.name)?;
        for (_, child_row) in child_rows {
            let child_value = child_schema.value_for_column(&child_row, child_column)?;
            if matches!(child_value, Value::Null) {
                continue;
            }
            if child_value == parent_value {
                return Err(DbError::storage(format!(
                    "foreign key constraint failed: {}.{} references {}({})",
                    child_schema.name, child_column, parent_table, parent_column
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
        if pending_updates.is_empty() {
            return Ok(());
        }

        for child_schema in self.storage.list_schemas(transaction_id)? {
            for foreign_key in child_schema
                .all_foreign_keys()
                .into_iter()
                .filter(|foreign_key| foreign_key.ref_table == parent_table)
            {
                for (_, old_row, updated_row) in pending_updates {
                    let old_parent_value =
                        parent_schema.value_for_column(old_row, &foreign_key.ref_column)?;
                    let updated_parent_value =
                        parent_schema.value_for_column(updated_row, &foreign_key.ref_column)?;

                    if old_parent_value == updated_parent_value
                        || matches!(old_parent_value, Value::Null)
                    {
                        continue;
                    }

                    self.validate_no_foreign_key_dependents_for_key(
                        transaction_id,
                        &child_schema,
                        &foreign_key.column,
                        parent_table,
                        &foreign_key.ref_column,
                        old_parent_value,
                    )?;
                }
            }
        }

        Ok(())
    }

    fn validate_update_primary_key_uniqueness(
        &self,
        schema: &Schema,
        final_rows: &[Row],
    ) -> Result<()> {
        for (column_index, column) in schema.columns.iter().enumerate() {
            if !column.primary_key {
                continue;
            }

            let mut seen = BTreeSet::new();
            for row in final_rows {
                let Some(value) = row.get(column_index) else {
                    continue;
                };
                if matches!(value, Value::Null) {
                    continue;
                }
                if !seen.insert(value.clone()) {
                    return Err(DbError::storage(format!(
                        "duplicate primary key value for column '{}': {}",
                        column.name, value
                    )));
                }
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
                let key = Self::project_index_key(schema, index, row)?;
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

    fn project_index_key(schema: &Schema, index: &IndexMeta, row: &Row) -> Result<Vec<Value>> {
        index
            .columns
            .iter()
            .map(|column| {
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
                row.get(position).cloned().ok_or_else(|| {
                    DbError::storage(format!(
                        "row for table {} is missing column {column}",
                        schema.name
                    ))
                })
            })
            .collect()
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
            Plan::SeqScan {
                table,
                table_alias,
                columns,
                filter,
                order_by,
                limit,
                distinct,
            } => {
                let source = self.scan_table_rowset(
                    transaction_id,
                    &table,
                    table_alias.as_deref(),
                    filter.as_ref(),
                    outer,
                )?;
                self.finish_projection(transaction_id, source, &columns, &order_by, limit, distinct)
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
                self.finish_projection(transaction_id, source, &columns, &order_by, limit, distinct)
            }
            Plan::IndexUnion {
                table,
                table_alias,
                columns,
                scans,
                filter,
                order_by,
                limit,
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
                self.finish_projection(transaction_id, source, &columns, &order_by, limit, distinct)
            }
            Plan::NestedLoopJoin {
                table,
                table_alias,
                joins,
                columns,
                filter,
                order_by,
                limit,
                distinct,
            } => {
                let source = self.execute_join_plan(
                    transaction_id,
                    &table,
                    table_alias.as_deref(),
                    &joins,
                    filter.as_ref(),
                    outer,
                )?;
                self.finish_projection(transaction_id, source, &columns, &order_by, limit, distinct)
            }
            Plan::Aggregate {
                source,
                columns,
                group_by,
                having,
                order_by,
                limit,
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
                    },
                )
            }
            Plan::ExplainQueryPlan { plan } => Ok(self.explain_query_plan(&plan)),
            Plan::BeginTxn | Plan::CommitTxn | Plan::RollbackTxn => Err(DbError::txn(
                "transaction control plan reached data execution path",
            )),
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
                },
                ColumnMeta {
                    table: None,
                    alias: None,
                    name: "detail".to_string(),
                    output_name: "detail".to_string(),
                },
            ],
            rows,
        }
    }

    fn collect_plan_rows(plan: &Plan, depth: usize, rows: &mut Vec<Row>) {
        let indent = "  ".repeat(depth);
        match plan {
            Plan::SeqScan { table, .. } => rows.push(vec![
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
            Plan::NestedLoopJoin { table, joins, .. } => rows.push(vec![
                Value::from(format!("{indent}NestedLoopJoin")),
                Value::from(format!("table={table} joins={}", joins.len())),
            ]),
            Plan::Aggregate { source, .. } => {
                rows.push(vec![
                    Value::from(format!("{indent}Aggregate")),
                    Value::from("grouped"),
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
            Plan::CreateIndex { .. } => "CreateIndex",
            Plan::DropTable { .. } => "DropTable",
            Plan::DropIndex { .. } => "DropIndex",
            Plan::AlterTable { .. } => "AlterTable",
            Plan::Insert { .. } => "Insert",
            Plan::Delete { .. } => "Delete",
            Plan::Update { .. } => "Update",
            Plan::SeqScan { .. } => "SeqScan",
            Plan::IndexScan { .. } => "IndexScan",
            Plan::IndexUnion { .. } => "IndexUnion",
            Plan::NestedLoopJoin { .. } => "NestedLoopJoin",
            Plan::Aggregate { .. } => "Aggregate",
            Plan::ExplainQueryPlan { .. } => "ExplainQueryPlan",
            Plan::BeginTxn => "BeginTxn",
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
        let schema = self.require_schema(transaction_id, table)?;
        let mut rowset = RowSet {
            columns: schema
                .columns
                .iter()
                .map(|column| ColumnMeta {
                    table: Some(table.to_string()),
                    alias: table_alias.map(str::to_string),
                    name: column.name.clone(),
                    output_name: column.name.clone(),
                })
                .collect(),
            rows: Vec::new(),
        };

        for (_, row) in self.storage.scan_rows(transaction_id, table)? {
            if self.matches_filter(transaction_id, &rowset, &row, filter, outer)? {
                rowset.rows.push(row);
            }
        }

        Ok(rowset)
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
        let mut rowset = RowSet {
            columns: schema
                .columns
                .iter()
                .map(|column| ColumnMeta {
                    table: Some(table.to_string()),
                    alias: table_alias.map(str::to_string),
                    name: column.name.clone(),
                    output_name: column.name.clone(),
                })
                .collect(),
            rows: Vec::new(),
        };

        for row_id in row_ids {
            if let Some(row) = self.storage.get_row(transaction_id, table, *row_id)?
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
        table: &str,
        table_alias: Option<&str>,
        joins: &[JoinPlan],
        filter: Option<&Expr>,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<RowSet> {
        let mut current =
            self.scan_table_rowset(transaction_id, table, table_alias, None, outer)?;

        for join in joins {
            let right = self.scan_table_rowset(
                transaction_id,
                &join.table,
                join.table_alias.as_deref(),
                None,
                outer,
            )?;
            let joined_columns = current
                .columns
                .iter()
                .cloned()
                .chain(right.columns.iter().cloned())
                .collect::<Vec<_>>();
            let right_width = right.columns.len();
            let mut joined_rows = Vec::new();

            for left_row in &current.rows {
                let mut matched = false;
                for right_row in &right.rows {
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
                    }
                }
                if !matched && matches!(join.kind, JoinKind::Left) {
                    let mut row = left_row.clone();
                    row.extend(std::iter::repeat_n(Value::Null, right_width));
                    joined_rows.push(row);
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
        _transaction_id: TransactionId,
        source: RowSet,
        columns: &[SelectItem],
        order_by: &[OrderBy],
        limit: Option<usize>,
        distinct: bool,
    ) -> Result<RowSet> {
        if columns.len() == 1 && matches!(columns.first(), Some(SelectItem::Wildcard)) {
            let mut rowset = self.sort_and_limit_rows(source, order_by, limit);
            if distinct {
                rowset.rows = Self::deduplicate_rows(rowset.rows);
            }
            return Ok(rowset);
        }

        let projected_columns = self.projected_columns(&source.columns, columns)?;
        let mut entries = source
            .rows
            .iter()
            .map(|row| Ok((self.project_row(&source, row, columns)?, row.clone())))
            .collect::<Result<Vec<(Row, Row)>>>()?;

        if !order_by.is_empty() {
            entries.sort_by(
                |(left_projected, left_full), (right_projected, right_full)| {
                    self.compare_ordering(
                        &projected_columns,
                        (left_projected, right_projected),
                        &source.columns,
                        (left_full, right_full),
                        order_by,
                    )
                },
            );
        }

        let mut rows = entries
            .into_iter()
            .map(|(projected, _)| projected)
            .collect::<Vec<_>>();
        if distinct {
            rows = Self::deduplicate_rows(rows);
        }
        if let Some(limit) = limit {
            rows.truncate(limit);
        }

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
            Value::Text(t) => (3, t.clone()),
        }
    }

    fn execute_aggregate(
        &self,
        _transaction_id: TransactionId,
        source: RowSet,
        options: AggregateExecOptions<'_>,
    ) -> Result<RowSet> {
        let AggregateExecOptions {
            columns,
            group_by,
            having,
            order_by,
            limit,
        } = options;
        let output_columns = self.aggregate_output_columns(columns);
        let has_aggregates = columns
            .iter()
            .any(|item| matches!(item, SelectItem::Aggregate { .. }));

        let mut groups = BTreeMap::<Vec<Value>, Vec<AggregateState>>::new();
        if source.rows.is_empty() && group_by.is_empty() {
            groups.insert(Vec::new(), self.initial_aggregate_states(columns));
        }

        for row in &source.rows {
            let key = group_by
                .iter()
                .map(|column| self.lookup_value(&source.columns, row, column).cloned())
                .collect::<Result<Vec<_>>>()?;
            let states = groups
                .entry(key)
                .or_insert_with(|| self.initial_aggregate_states(columns));
            self.update_aggregate_states(&source, row, columns, states)?;
        }

        let mut rows = Vec::new();
        for (key, states) in groups {
            if !has_aggregates && !group_by.is_empty() && key.is_empty() {
                continue;
            }
            let aggregate_row =
                self.finalize_aggregate_row(&source, columns, group_by, &key, &states)?;
            if let Some(having) = having {
                let aggregate_rowset = RowSet {
                    columns: output_columns.clone(),
                    rows: Vec::new(),
                };
                if !self.matches_filter(
                    _transaction_id,
                    &aggregate_rowset,
                    &aggregate_row,
                    Some(having),
                    None,
                )? {
                    continue;
                }
            }
            rows.push(aggregate_row);
        }

        let mut output = RowSet {
            columns: output_columns,
            rows,
        };

        if !order_by.is_empty() {
            output.rows.sort_by(|left, right| {
                self.compare_aggregate_order(columns, order_by, left, right)
            });
        }
        if let Some(limit) = limit {
            output.rows.truncate(limit);
        }
        Ok(output)
    }

    fn initial_aggregate_states(&self, columns: &[SelectItem]) -> Vec<AggregateState> {
        columns
            .iter()
            .filter_map(|item| match item {
                SelectItem::Aggregate { func, arg, .. } => Some(match (func, arg) {
                    (AggregateFunc::Count, AggregateArg::Column { distinct: true, .. }) => {
                        AggregateState::CountDistinct(BTreeSet::new())
                    }
                    (AggregateFunc::Count, _) => AggregateState::Count(0),
                    (AggregateFunc::Sum, AggregateArg::Column { distinct: true, .. }) => {
                        AggregateState::SumDistinct(BTreeSet::new())
                    }
                    (AggregateFunc::Sum, _) => AggregateState::Sum {
                        sum: 0,
                        seen: false,
                    },
                    (AggregateFunc::Avg, AggregateArg::Column { distinct: true, .. }) => {
                        AggregateState::AvgDistinct(BTreeSet::new())
                    }
                    (AggregateFunc::Avg, _) => AggregateState::Avg { sum: 0, count: 0 },
                    (AggregateFunc::Min, _) => AggregateState::Min(None),
                    (AggregateFunc::Max, _) => AggregateState::Max(None),
                }),
                _ => None,
            })
            .collect()
    }

    fn update_aggregate_states(
        &self,
        source: &RowSet,
        row: &Row,
        columns: &[SelectItem],
        states: &mut [AggregateState],
    ) -> Result<()> {
        let mut state_index = 0;
        for item in columns {
            let SelectItem::Aggregate { func, arg, .. } = item else {
                continue;
            };

            match (&mut states[state_index], func, arg) {
                (AggregateState::Count(count), AggregateFunc::Count, AggregateArg::Wildcard) => {
                    *count += 1;
                }
                (
                    AggregateState::Count(count),
                    AggregateFunc::Count,
                    AggregateArg::Column { name: column, .. },
                ) if self.lookup_value(&source.columns, row, column)? != &Value::Null => {
                    *count += 1;
                }
                (
                    AggregateState::CountDistinct(values),
                    AggregateFunc::Count,
                    AggregateArg::Column { name: column, .. },
                ) => {
                    let value = self.lookup_value(&source.columns, row, column)?;
                    if value != &Value::Null {
                        values.insert(value.clone());
                    }
                }
                (AggregateState::Count(_), AggregateFunc::Count, AggregateArg::Column { .. }) => {}
                (
                    AggregateState::Sum { sum, seen },
                    AggregateFunc::Sum,
                    AggregateArg::Column { name: column, .. },
                ) => {
                    if let Value::Integer(value) =
                        self.lookup_value(&source.columns, row, column)?
                    {
                        *sum += i128::from(*value);
                        *seen = true;
                    }
                }
                (
                    AggregateState::SumDistinct(values),
                    AggregateFunc::Sum,
                    AggregateArg::Column { name: column, .. },
                ) => {
                    let value = self.lookup_value(&source.columns, row, column)?;
                    if matches!(value, Value::Integer(_)) {
                        values.insert(value.clone());
                    }
                }
                (
                    AggregateState::Avg { sum, count },
                    AggregateFunc::Avg,
                    AggregateArg::Column { name: column, .. },
                ) => {
                    if let Value::Integer(value) =
                        self.lookup_value(&source.columns, row, column)?
                    {
                        *sum += i128::from(*value);
                        *count += 1;
                    }
                }
                (
                    AggregateState::AvgDistinct(values),
                    AggregateFunc::Avg,
                    AggregateArg::Column { name: column, .. },
                ) => {
                    let value = self.lookup_value(&source.columns, row, column)?;
                    if matches!(value, Value::Integer(_)) {
                        values.insert(value.clone());
                    }
                }
                (
                    AggregateState::Min(current),
                    AggregateFunc::Min,
                    AggregateArg::Column { name: column, .. },
                ) => {
                    let value = self.lookup_value(&source.columns, row, column)?;
                    if value != &Value::Null {
                        match current.as_ref() {
                            None => *current = Some(value.clone()),
                            Some(existing)
                                if self.compare(existing, value)? == Some(Ordering::Greater) =>
                            {
                                *current = Some(value.clone())
                            }
                            _ => {}
                        }
                    }
                }
                (
                    AggregateState::Max(current),
                    AggregateFunc::Max,
                    AggregateArg::Column { name: column, .. },
                ) => {
                    let value = self.lookup_value(&source.columns, row, column)?;
                    if value != &Value::Null {
                        match current.as_ref() {
                            None => *current = Some(value.clone()),
                            Some(existing)
                                if self.compare(existing, value)? == Some(Ordering::Less) =>
                            {
                                *current = Some(value.clone())
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }

            state_index += 1;
        }
        Ok(())
    }

    fn finalize_aggregate_row(
        &self,
        source: &RowSet,
        columns: &[SelectItem],
        group_by: &[String],
        key: &[Value],
        states: &[AggregateState],
    ) -> Result<Row> {
        let mut row = Vec::with_capacity(columns.len());
        let mut state_index = 0;
        for item in columns {
            match item {
                SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
                    let key_index = self.group_key_index(source, group_by, name)?;
                    row.push(key[key_index].clone());
                }
                SelectItem::Expr { .. } => {
                    return Err(DbError::plan(
                        "scalar expressions cannot be used with GROUP BY or aggregate projections",
                    ));
                }
                SelectItem::Aggregate { .. } => {
                    row.push(self.aggregate_state_value(&states[state_index])?);
                    state_index += 1;
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
            AggregateState::Sum { sum, seen } => {
                if *seen {
                    Value::Integer(
                        i64::try_from(*sum).map_err(|_| DbError::plan("SUM overflowed i64"))?,
                    )
                } else {
                    Value::Null
                }
            }
            AggregateState::SumDistinct(values) => {
                if values.is_empty() {
                    Value::Null
                } else {
                    let sum = values.iter().try_fold(0_i128, |sum, value| match value {
                        Value::Integer(value) => Ok(sum + i128::from(*value)),
                        _ => Err(DbError::plan("SUM only supports INTEGER columns")),
                    })?;
                    Value::Integer(
                        i64::try_from(sum).map_err(|_| DbError::plan("SUM overflowed i64"))?,
                    )
                }
            }
            AggregateState::Avg { sum, count } => {
                if *count == 0 {
                    Value::Null
                } else {
                    Value::Integer(
                        i64::try_from(*sum / i128::from(*count))
                            .map_err(|_| DbError::plan("AVG overflowed i64"))?,
                    )
                }
            }
            AggregateState::AvgDistinct(values) => {
                if values.is_empty() {
                    Value::Null
                } else {
                    let sum = values.iter().try_fold(0_i128, |sum, value| match value {
                        Value::Integer(value) => Ok(sum + i128::from(*value)),
                        _ => Err(DbError::plan("AVG only supports INTEGER columns")),
                    })?;
                    let count = i128::try_from(values.len())
                        .map_err(|_| DbError::plan("AVG overflowed i64"))?;
                    Value::Integer(
                        i64::try_from(sum / count)
                            .map_err(|_| DbError::plan("AVG overflowed i64"))?,
                    )
                }
            }
            AggregateState::Min(value) | AggregateState::Max(value) => {
                value.clone().unwrap_or(Value::Null)
            }
        })
    }

    fn aggregate_output_columns(&self, columns: &[SelectItem]) -> Vec<ColumnMeta> {
        columns
            .iter()
            .map(|item| ColumnMeta {
                table: None,
                alias: None,
                name: self.output_name(item),
                output_name: self.output_name(item),
            })
            .collect()
    }

    fn group_key_index(&self, source: &RowSet, group_by: &[String], name: &str) -> Result<usize> {
        if let Some(index) = group_by.iter().position(|column| column == name) {
            return Ok(index);
        }
        let target = self.resolve_column_index(&source.columns, name)?;
        for (index, column) in group_by.iter().enumerate() {
            if self.resolve_column_index(&source.columns, column)? == target {
                return Ok(index);
            }
        }
        Err(DbError::plan(format!(
            "non-aggregate column {name} must appear in GROUP BY"
        )))
    }

    fn sort_and_limit_rows(
        &self,
        mut rowset: RowSet,
        order_by: &[OrderBy],
        limit: Option<usize>,
    ) -> RowSet {
        if !order_by.is_empty() {
            rowset.rows.sort_by(|left, right| {
                self.compare_ordering(
                    &rowset.columns,
                    (left, right),
                    &rowset.columns,
                    (left, right),
                    order_by,
                )
            });
        }
        if let Some(limit) = limit {
            rowset.rows.truncate(limit);
        }
        rowset
    }

    fn compare_ordering(
        &self,
        projected_columns: &[ColumnMeta],
        projected_rows: (&Row, &Row),
        full_columns: &[ColumnMeta],
        full_rows: (&Row, &Row),
        order_by: &[OrderBy],
    ) -> Ordering {
        let (left_projected, right_projected) = projected_rows;
        let (left_full, right_full) = full_rows;
        for item in order_by {
            let left_value = self.resolve_order_value(
                projected_columns,
                left_projected,
                full_columns,
                left_full,
                &item.expr,
            );
            let right_value = self.resolve_order_value(
                projected_columns,
                right_projected,
                full_columns,
                right_full,
                &item.expr,
            );
            let ordering = match (left_value, right_value) {
                (Some(left), Some(right)) => {
                    self.compare_order_values(left, right, item.nulls, item.descending)
                }
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
        columns: &[SelectItem],
        order_by: &[OrderBy],
        left: &Row,
        right: &Row,
    ) -> Ordering {
        for item in order_by {
            let mut ordering = Ordering::Equal;
            if let OrderByExpr::Position(position) = item.expr {
                let index = position.saturating_sub(1);
                ordering = match (left.get(index), right.get(index)) {
                    (Some(left), Some(right)) => {
                        self.compare_order_values(left, right, item.nulls, item.descending)
                    }
                    _ => Ordering::Equal,
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
                continue;
            }
            let OrderByExpr::Column(order_column) = &item.expr else {
                unreachable!();
            };
            for (index, select_item) in columns.iter().enumerate() {
                let output_name = self.output_name(select_item);
                let matches = match select_item {
                    SelectItem::Column(name) => {
                        name == order_column || output_name == *order_column
                    }
                    SelectItem::AliasedColumn { name, alias } => {
                        name == order_column
                            || alias == order_column
                            || output_name == *order_column
                    }
                    SelectItem::Expr { alias, .. } => {
                        alias.as_ref().is_some_and(|alias| alias == order_column)
                            || output_name == *order_column
                    }
                    SelectItem::Aggregate { alias, .. } => {
                        alias.as_ref().is_some_and(|alias| alias == order_column)
                            || output_name == *order_column
                    }
                    SelectItem::Wildcard => false,
                };
                if matches {
                    ordering = match (left.get(index), right.get(index)) {
                        (Some(left), Some(right)) => {
                            self.compare_order_values(left, right, item.nulls, item.descending)
                        }
                        _ => Ordering::Equal,
                    };
                    break;
                }
            }
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    fn compare_order_values(
        &self,
        left: &Value,
        right: &Value,
        nulls: Option<NullOrder>,
        descending: bool,
    ) -> Ordering {
        let ordering = match (nulls, left, right) {
            (Some(_), Value::Null, Value::Null) => Ordering::Equal,
            (Some(NullOrder::First), Value::Null, _) => Ordering::Less,
            (Some(NullOrder::First), _, Value::Null) => Ordering::Greater,
            (Some(NullOrder::Last), Value::Null, _) => Ordering::Greater,
            (Some(NullOrder::Last), _, Value::Null) => Ordering::Less,
            _ => self
                .compare(left, right)
                .unwrap_or(None)
                .unwrap_or(Ordering::Equal),
        };

        let should_reverse = descending
            && (nulls.is_none() || !matches!((left, right), (Value::Null, _) | (_, Value::Null)));

        if should_reverse {
            ordering.reverse()
        } else {
            ordering
        }
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

    fn project_row(&self, source: &RowSet, row: &Row, columns: &[SelectItem]) -> Result<Row> {
        columns
            .iter()
            .map(|column| match column {
                SelectItem::Wildcard => Err(DbError::plan(
                    "wildcard cannot be mixed with explicit projections",
                )),
                SelectItem::Column(name) | SelectItem::AliasedColumn { name, .. } => {
                    self.lookup_value(&source.columns, row, name).cloned()
                }
                SelectItem::Expr { expr, .. } => self.evaluate_scalar_expr(source, row, expr),
                SelectItem::Aggregate { .. } => Err(DbError::plan(
                    "aggregate projection requires aggregate execution path",
                )),
            })
            .collect()
    }

    fn projected_columns(
        &self,
        source_columns: &[ColumnMeta],
        columns: &[SelectItem],
    ) -> Result<Vec<ColumnMeta>> {
        columns
            .iter()
            .map(|column| match column {
                SelectItem::Wildcard => Err(DbError::plan(
                    "wildcard cannot be mixed with explicit projections",
                )),
                SelectItem::Column(name) => {
                    let source = self.resolve_column_index(source_columns, name)?;
                    let mut meta = source_columns[source].clone();
                    meta.output_name = name.clone();
                    Ok(meta)
                }
                SelectItem::AliasedColumn { name, alias } => {
                    let source = self.resolve_column_index(source_columns, name)?;
                    let mut meta = source_columns[source].clone();
                    meta.output_name = alias.clone();
                    meta.name = alias.clone();
                    Ok(meta)
                }
                SelectItem::Expr { expr, alias } => {
                    let output_name = alias.clone().unwrap_or_else(|| self.scalar_expr_name(expr));
                    Ok(ColumnMeta {
                        table: None,
                        alias: None,
                        name: output_name.clone(),
                        output_name,
                    })
                }
                SelectItem::Aggregate { .. } => Err(DbError::plan(
                    "aggregate projection requires aggregate execution path",
                )),
            })
            .collect()
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
                let left = self.lookup_filter_value(rowset, row, outer, column)?;
                self.compare_with_operator(&left, op, value)
            }
            Expr::CompareColumns { left, op, right } => {
                let left = self.lookup_filter_value(rowset, row, outer, left)?;
                let right = self.lookup_filter_value(rowset, row, outer, right)?;
                self.compare_with_operator(&left, op, &right)
            }
            Expr::IsNull { column, negated } => {
                let left = self.lookup_filter_value(rowset, row, outer, column)?;
                Ok((left == Value::Null) ^ *negated)
            }
            Expr::InSubquery {
                column,
                query,
                negated,
            } => {
                let left = self.lookup_filter_value(rowset, row, outer, column)?;
                let rows = self.execute_subquery(transaction_id, query, Some((rowset, row)))?;
                let contains = rows.rows.iter().any(|row| row.first() == Some(&left));
                Ok(contains ^ *negated)
            }
            Expr::CompareSubquery { column, op, query } => {
                let left = self.lookup_filter_value(rowset, row, outer, column)?;
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
                negated,
            } => {
                let Value::Text(value) = self.lookup_filter_value(rowset, row, outer, column)?
                else {
                    return Ok(false);
                };
                Ok(Self::matches_like_pattern(&value, pattern) ^ *negated)
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
        let planner = Planner::with_unresolved_outer_refs();
        let context = self
            .storage
            .planning_context_snapshot(Some(transaction_id))?;
        let plan = planner.plan_statement(&Statement::Select(query.clone()), &context)?;
        let plan = Optimizer::new().optimize_with_context(plan, &context)?;
        let rows = self.execute_query_plan_with_outer(transaction_id, plan, outer)?;
        if rows.columns.len() != 1 {
            return Err(DbError::plan("subquery must return exactly one column"));
        }
        Ok(rows)
    }

    fn scalar_subquery_value(
        &self,
        transaction_id: TransactionId,
        query: &crate::sql::ast::SelectStatement,
        outer: Option<(&RowSet, &Row)>,
    ) -> Result<Value> {
        let rows = self.execute_subquery(transaction_id, query, outer)?;
        match rows.rows.as_slice() {
            [] => Ok(Value::Null),
            [row] => Ok(row.first().cloned().unwrap_or(Value::Null)),
            _ => Err(DbError::plan("scalar subquery returned more than one row")),
        }
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
        if let Some(value) = self.try_lookup_value(&rowset.columns, row, column) {
            return Ok(value.clone());
        }
        if let Some((outer_rowset, outer_row)) = outer
            && let Some(value) = self.try_lookup_value(&outer_rowset.columns, outer_row, column)
        {
            return Ok(value.clone());
        }
        self.lookup_value(&rowset.columns, row, column).cloned()
    }

    fn resolve_column_index(&self, columns: &[ColumnMeta], column: &str) -> Result<usize> {
        if let Some((prefix, suffix)) = column.split_once('.') {
            return columns
                .iter()
                .position(|entry| {
                    entry.name == suffix
                        && (entry.table.as_deref() == Some(prefix)
                            || entry.alias.as_deref() == Some(prefix)
                            || entry.output_name == column)
                })
                .ok_or_else(|| DbError::plan(format!("unknown column {column}")));
        }

        let matches = columns
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.output_name == column || entry.name == column)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => Err(DbError::plan(format!("unknown column {column}"))),
            [index] => Ok(*index),
            _ => Err(DbError::plan(format!(
                "ambiguous column reference: {column}"
            ))),
        }
    }

    fn compare_with_operator(&self, left: &Value, op: &CompareOp, right: &Value) -> Result<bool> {
        match op {
            CompareOp::Eq => Ok(left == right),
            CompareOp::Ne => Ok(left != right),
            CompareOp::Gt => Ok(self.compare(left, right)? == Some(Ordering::Greater)),
            CompareOp::Gte => Ok(matches!(
                self.compare(left, right)?,
                Some(Ordering::Greater | Ordering::Equal)
            )),
            CompareOp::Lt => Ok(self.compare(left, right)? == Some(Ordering::Less)),
            CompareOp::Lte => Ok(matches!(
                self.compare(left, right)?,
                Some(Ordering::Less | Ordering::Equal)
            )),
        }
    }

    fn evaluate_scalar_expr(&self, source: &RowSet, row: &Row, expr: &ScalarExpr) -> Result<Value> {
        Ok(match expr {
            ScalarExpr::Literal(value) => value.clone(),
            ScalarExpr::Column(name) => self.lookup_value(&source.columns, row, name)?.clone(),
            ScalarExpr::UnaryMinus(expr) => match self.evaluate_scalar_expr(source, row, expr)? {
                Value::Integer(value) => Value::Integer(-value),
                Value::Null => Value::Null,
                value => {
                    return Err(DbError::plan(format!(
                        "unary - expects INTEGER but got {}",
                        value.type_name()
                    )));
                }
            },
            ScalarExpr::Binary { left, op, right } => {
                let left = self.evaluate_scalar_expr(source, row, left)?;
                let right = self.evaluate_scalar_expr(source, row, right)?;
                self.evaluate_binary_scalar(*op, left, right)?
            }
        })
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
            ScalarBinaryOp::Add => Self::integer_binary_op(left, right, "+", |l, r| l + r),
            ScalarBinaryOp::Subtract => Self::integer_binary_op(left, right, "-", |l, r| l - r),
            ScalarBinaryOp::Multiply => Self::integer_binary_op(left, right, "*", |l, r| l * r),
            ScalarBinaryOp::Divide => match (left, right) {
                (Value::Integer(_), Value::Integer(0)) => Err(DbError::plan("division by zero")),
                (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(left / right)),
                (left, right) => Err(DbError::plan(format!(
                    "/ expects INTEGER operands but got {} and {}",
                    left.type_name(),
                    right.type_name()
                ))),
            },
            ScalarBinaryOp::Concat => match (left, right) {
                (Value::Text(left), Value::Text(right)) => {
                    Ok(Value::Text(format!("{left}{right}")))
                }
                (left, right) => Err(DbError::plan(format!(
                    "|| expects TEXT operands but got {} and {}",
                    left.type_name(),
                    right.type_name()
                ))),
            },
        }
    }

    fn integer_binary_op(
        left: Value,
        right: Value,
        op: &str,
        f: impl FnOnce(i64, i64) -> i64,
    ) -> Result<Value> {
        match (left, right) {
            (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(f(left, right))),
            (left, right) => Err(DbError::plan(format!(
                "{op} expects INTEGER operands but got {} and {}",
                left.type_name(),
                right.type_name()
            ))),
        }
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

    fn matches_like_pattern(value: &str, pattern: &str) -> bool {
        let value = value.chars().collect::<Vec<_>>();
        let pattern = pattern.chars().collect::<Vec<_>>();
        let mut dp = vec![vec![false; pattern.len() + 1]; value.len() + 1];
        dp[0][0] = true;
        for pattern_index in 1..=pattern.len() {
            if pattern[pattern_index - 1] == '%' {
                dp[0][pattern_index] = dp[0][pattern_index - 1];
            }
        }
        for value_index in 1..=value.len() {
            for pattern_index in 1..=pattern.len() {
                dp[value_index][pattern_index] = match pattern[pattern_index - 1] {
                    '%' => dp[value_index][pattern_index - 1] || dp[value_index - 1][pattern_index],
                    '_' => dp[value_index - 1][pattern_index - 1],
                    ch => dp[value_index - 1][pattern_index - 1] && value[value_index - 1] == ch,
                };
            }
        }
        dp[value.len()][pattern.len()]
    }

    fn output_name(&self, item: &SelectItem) -> String {
        match item {
            SelectItem::Wildcard => "*".to_string(),
            SelectItem::Column(name) => name.clone(),
            SelectItem::AliasedColumn { alias, .. } => alias.clone(),
            SelectItem::Expr { expr, alias } => {
                alias.clone().unwrap_or_else(|| self.scalar_expr_name(expr))
            }
            SelectItem::Aggregate { func, arg, alias } => alias.clone().unwrap_or_else(|| {
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
                        AggregateArg::Column { name, distinct } => {
                            if *distinct {
                                format!("DISTINCT {name}")
                            } else {
                                name.clone()
                            }
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
            ScalarExpr::Binary { left, op, right } => format!(
                "{} {} {}",
                self.scalar_expr_name(left),
                match op {
                    ScalarBinaryOp::Add => "+",
                    ScalarBinaryOp::Subtract => "-",
                    ScalarBinaryOp::Multiply => "*",
                    ScalarBinaryOp::Divide => "/",
                    ScalarBinaryOp::Concat => "||",
                },
                self.scalar_expr_name(right)
            ),
        }
    }
}
