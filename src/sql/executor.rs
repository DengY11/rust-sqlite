use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Row, RowId, Schema, Value};
use crate::engine::{PlanningStorageEngine, TransactionId};
use crate::sql::ast::{
    AggregateArg, AggregateFunc, CompareOp, Expr, JoinKind, OrderBy, SelectItem, Statement,
};
use crate::sql::plan::{IndexScanSpec, JoinPlan, Plan};
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
    Sum { sum: i128, seen: bool },
    Avg { sum: i128, count: i64 },
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
            Plan::CreateTable { name, columns } => {
                self.storage
                    .create_schema(transaction_id, Schema::new(name, columns))?;
                Ok(Vec::new())
            }
            Plan::CreateIndex {
                name,
                table,
                columns,
            } => {
                self.storage.create_index(
                    transaction_id,
                    &table,
                    IndexMeta {
                        name,
                        columns,
                        unique: false,
                    },
                )?;
                Ok(Vec::new())
            }
            Plan::Insert { table, values } => {
                self.storage.insert_row(transaction_id, &table, values)?;
                Ok(Vec::new())
            }
            Plan::Delete { table, filter } => {
                let source = self.scan_table_rowset(transaction_id, &table, None, None)?;
                let rows = self.storage.scan_rows(transaction_id, &table)?;
                for ((row_id, _), row) in rows.iter().zip(source.rows.iter()) {
                    if self.matches_filter(transaction_id, &source, row, filter.as_ref())? {
                        self.storage.delete_row(transaction_id, &table, *row_id)?;
                    }
                }
                Ok(Vec::new())
            }
            Plan::Update {
                table,
                assignments,
                filter,
            } => {
                let schema = self.require_schema(transaction_id, &table)?;
                let source = self.scan_table_rowset(transaction_id, &table, None, None)?;
                let rows = self.storage.scan_rows(transaction_id, &table)?;
                for ((row_id, _), row) in rows.iter().zip(source.rows.iter()) {
                    if self.matches_filter(transaction_id, &source, row, filter.as_ref())? {
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
                        self.storage.delete_row(transaction_id, &table, *row_id)?;
                        self.storage.insert_row(transaction_id, &table, updated)?;
                    }
                }
                Ok(Vec::new())
            }
            query_plan => Ok(self.execute_query_plan(transaction_id, query_plan)?.rows),
        }
    }

    fn execute_query_plan(&self, transaction_id: TransactionId, plan: Plan) -> Result<RowSet> {
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
                )?;
                self.finish_projection(transaction_id, source, &columns, &order_by, limit, distinct)
            }
            Plan::IndexScan {
                table,
                table_alias,
                columns,
                index,
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
                let source = self.execute_query_plan(transaction_id, *source)?;
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
            Plan::BeginTxn | Plan::CommitTxn | Plan::RollbackTxn => Err(DbError::txn(
                "transaction control plan reached data execution path",
            )),
            other => Err(DbError::plan(format!("unexpected query plan: {other:?}"))),
        }
    }

    fn scan_table_rowset(
        &self,
        transaction_id: TransactionId,
        table: &str,
        table_alias: Option<&str>,
        filter: Option<&Expr>,
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
            if self.matches_filter(transaction_id, &rowset, &row, filter)? {
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
                && self.matches_filter(transaction_id, &rowset, &row, filter)?
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
    ) -> Result<RowSet> {
        let mut current = self.scan_table_rowset(transaction_id, table, table_alias, None)?;

        for join in joins {
            let right = self.scan_table_rowset(
                transaction_id,
                &join.table,
                join.table_alias.as_deref(),
                None,
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
                    if self.matches_filter(transaction_id, &candidate, &row, Some(&join.on))? {
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
                    match self.matches_filter(transaction_id, &current, row, Some(filter)) {
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
                SelectItem::Aggregate { func, .. } => Some(match func {
                    AggregateFunc::Count => AggregateState::Count(0),
                    AggregateFunc::Sum => AggregateState::Sum {
                        sum: 0,
                        seen: false,
                    },
                    AggregateFunc::Avg => AggregateState::Avg { sum: 0, count: 0 },
                    AggregateFunc::Min => AggregateState::Min(None),
                    AggregateFunc::Max => AggregateState::Max(None),
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
                    AggregateArg::Column(column),
                ) if self.lookup_value(&source.columns, row, column)? != &Value::Null => {
                    *count += 1;
                }
                (AggregateState::Count(_), AggregateFunc::Count, AggregateArg::Column(_)) => {}
                (
                    AggregateState::Sum { sum, seen },
                    AggregateFunc::Sum,
                    AggregateArg::Column(column),
                ) => {
                    if let Value::Integer(value) =
                        self.lookup_value(&source.columns, row, column)?
                    {
                        *sum += i128::from(*value);
                        *seen = true;
                    }
                }
                (
                    AggregateState::Avg { sum, count },
                    AggregateFunc::Avg,
                    AggregateArg::Column(column),
                ) => {
                    if let Value::Integer(value) =
                        self.lookup_value(&source.columns, row, column)?
                    {
                        *sum += i128::from(*value);
                        *count += 1;
                    }
                }
                (
                    AggregateState::Min(current),
                    AggregateFunc::Min,
                    AggregateArg::Column(column),
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
                    AggregateArg::Column(column),
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
            AggregateState::Sum { sum, seen } => {
                if *seen {
                    Value::Integer(
                        i64::try_from(*sum).map_err(|_| DbError::plan("SUM overflowed i64"))?,
                    )
                } else {
                    Value::Null
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
                &item.column,
            );
            let right_value = self.resolve_order_value(
                projected_columns,
                right_projected,
                full_columns,
                right_full,
                &item.column,
            );
            let ordering = match (left_value, right_value) {
                (Some(left), Some(right)) => self
                    .compare(left, right)
                    .unwrap_or(None)
                    .unwrap_or(Ordering::Equal),
                _ => Ordering::Equal,
            };
            if ordering != Ordering::Equal {
                return if item.descending {
                    ordering.reverse()
                } else {
                    ordering
                };
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
            for (index, select_item) in columns.iter().enumerate() {
                let output_name = self.output_name(select_item);
                let matches = match select_item {
                    SelectItem::Column(name) => name == &item.column || output_name == item.column,
                    SelectItem::AliasedColumn { name, alias } => {
                        name == &item.column || alias == &item.column || output_name == item.column
                    }
                    SelectItem::Aggregate { alias, .. } => {
                        alias.as_ref().is_some_and(|alias| alias == &item.column)
                            || output_name == item.column
                    }
                    SelectItem::Wildcard => false,
                };
                if matches {
                    ordering = match (left.get(index), right.get(index)) {
                        (Some(left), Some(right)) => self
                            .compare(left, right)
                            .unwrap_or(None)
                            .unwrap_or(Ordering::Equal),
                        _ => Ordering::Equal,
                    };
                    break;
                }
            }
            if ordering != Ordering::Equal {
                return if item.descending {
                    ordering.reverse()
                } else {
                    ordering
                };
            }
        }
        Ordering::Equal
    }

    fn scan_index_spec(
        &self,
        transaction_id: TransactionId,
        table: &str,
        scan: &IndexScanSpec,
    ) -> Result<Vec<RowId>> {
        match &scan.range {
            Some(range) => self.storage.scan_index_range(
                transaction_id,
                table,
                &scan.index,
                &scan.key_prefix,
                range.lower.as_ref().map(|bound| (bound.op, &bound.value)),
                range.upper.as_ref().map(|bound| (bound.op, &bound.value)),
            ),
            None => {
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
        column: &str,
    ) -> Option<&'b Value> {
        self.try_lookup_value(projected_columns, projected, column)
            .or_else(|| self.try_lookup_value(full_columns, full, column))
    }

    fn matches_filter(
        &self,
        transaction_id: TransactionId,
        rowset: &RowSet,
        row: &Row,
        filter: Option<&Expr>,
    ) -> Result<bool> {
        let Some(filter) = filter else {
            return Ok(true);
        };

        match filter {
            Expr::Compare { column, op, value } => {
                let left = self.lookup_value(&rowset.columns, row, column)?;
                self.compare_with_operator(left, op, value)
            }
            Expr::CompareColumns { left, op, right } => {
                let left = self.lookup_value(&rowset.columns, row, left)?;
                let right = self.lookup_value(&rowset.columns, row, right)?;
                self.compare_with_operator(left, op, right)
            }
            Expr::IsNull { column, negated } => {
                let left = self.lookup_value(&rowset.columns, row, column)?;
                Ok((left == &Value::Null) ^ *negated)
            }
            Expr::InSubquery {
                column,
                query,
                negated,
            } => {
                let left = self.lookup_value(&rowset.columns, row, column)?;
                let rows = self.execute_subquery(transaction_id, query)?;
                let contains = rows.rows.iter().any(|row| row.first() == Some(left));
                Ok(contains ^ *negated)
            }
            Expr::CompareSubquery { column, op, query } => {
                let left = self.lookup_value(&rowset.columns, row, column)?;
                let right = self.scalar_subquery_value(transaction_id, query)?;
                self.compare_with_operator(left, op, &right)
            }
            Expr::Not(expr) => {
                Ok(!self.matches_filter(transaction_id, rowset, row, Some(expr.as_ref()))?)
            }
            Expr::And(left, right) => {
                Ok(
                    self.matches_filter(transaction_id, rowset, row, Some(left.as_ref()))?
                        && self.matches_filter(
                            transaction_id,
                            rowset,
                            row,
                            Some(right.as_ref()),
                        )?,
                )
            }
            Expr::Or(left, right) => {
                Ok(
                    self.matches_filter(transaction_id, rowset, row, Some(left.as_ref()))?
                        || self.matches_filter(
                            transaction_id,
                            rowset,
                            row,
                            Some(right.as_ref()),
                        )?,
                )
            }
        }
    }

    fn execute_subquery(
        &self,
        transaction_id: TransactionId,
        query: &crate::sql::ast::SelectStatement,
    ) -> Result<RowSet> {
        let planner = Planner::new();
        let context = self
            .storage
            .planning_context_snapshot(Some(transaction_id))?;
        let plan = planner.plan_statement(&Statement::Select(query.clone()), &context)?;
        let rows = self.execute_query_plan(transaction_id, plan)?;
        if rows.columns.len() != 1 {
            return Err(DbError::plan("subquery must return exactly one column"));
        }
        Ok(rows)
    }

    fn scalar_subquery_value(
        &self,
        transaction_id: TransactionId,
        query: &crate::sql::ast::SelectStatement,
    ) -> Result<Value> {
        let rows = self.execute_subquery(transaction_id, query)?;
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

    fn output_name(&self, item: &SelectItem) -> String {
        match item {
            SelectItem::Wildcard => "*".to_string(),
            SelectItem::Column(name) => name.clone(),
            SelectItem::AliasedColumn { alias, .. } => alias.clone(),
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
                        AggregateArg::Column(name) => name.clone(),
                    }
                )
            }),
        }
    }
}
