use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::common::error::{DbError, Result};
use crate::common::types::{
    ColumnDef, ColumnType, IndexMeta, Row, RowId, Schema, Value, sqlite_round_f64,
};
use crate::engine::traits::{
    CatalogStore, IndexStore, PlanningStorageEngine, TableStore, TransactionManager,
};
use crate::engine::txn::TransactionId;
use crate::sql::ast::{
    AggregateArg, AggregateFunc, CompareOp, Expr, IsolationLevel, ScalarBinaryOp, ScalarExpr,
    ScalarFunc,
};
use crate::sql::parser::{parse_check_constraint_expression, parse_scalar_sql_expression};
use crate::sql::planner::PlanningContext;

use super::btree::{get_table_row, lookup_index_entries, scan_table_rows};
use super::index_expr::validate_index_term;
use super::pager::Pager;
use super::schema::{Catalog, load_catalog};
use super::writer::{WritableDatabase, WritableTable, write_database};

const SQLITE_TRIGGER_RAISE_IGNORE: &str = "__rustsql_sqlite_trigger_raise_ignore__";

#[derive(Debug)]
pub struct FileStorage {
    path: Option<PathBuf>,
    pager: RefCell<Option<Pager>>,
    catalog: RefCell<Catalog>,
    writable: RefCell<WritableDatabase>,
    txn_state: RefCell<TxnState>,
    ignore_check_constraints: RefCell<bool>,
    case_sensitive_like: RefCell<bool>,
}

fn validate_supported_text_encoding(text_encoding: u32) -> Result<()> {
    if text_encoding == 1 {
        Ok(())
    } else {
        Err(DbError::storage(format!(
            "unsupported sqlite text encoding {text_encoding}; only UTF-8 databases are supported"
        )))
    }
}

fn rename_trigger_target_table_sql(sql: &str, old_name: &str, new_name: &str) -> String {
    let quoted_new = format!("\"{}\"", new_name.replace('"', "\"\""));
    let replacements = [
        (
            format!(" ON \"{}\"", old_name.replace('"', "\"\"")),
            format!(" ON {quoted_new}"),
        ),
        (
            format!(" ON '{}'", old_name.replace('\'', "''")),
            format!(" ON {quoted_new}"),
        ),
        (
            format!(" ON [{}]", old_name.replace(']', "]]")),
            format!(" ON {quoted_new}"),
        ),
        (
            format!(" ON `{}`", old_name.replace('`', "``")),
            format!(" ON {quoted_new}"),
        ),
        (format!(" ON {old_name}"), format!(" ON {quoted_new}")),
    ];
    let mut renamed = sql.to_string();
    for (from, to) in replacements {
        renamed = renamed.replace(&from, &to);
    }
    renamed
}

fn rename_trigger_column_sql(sql: &str, old_name: &str, new_name: &str) -> String {
    let mut renamed = sql.to_string();
    for qualifier in ["new", "old"] {
        renamed = renamed.replace(
            &format!("{qualifier}.{old_name}"),
            &format!("{qualifier}.{new_name}"),
        );
        renamed = renamed.replace(
            &format!("{qualifier}.\"{}\"", old_name.replace('"', "\"\"")),
            &format!("{qualifier}.\"{}\"", new_name.replace('"', "\"\"")),
        );
        renamed = renamed.replace(
            &format!("{qualifier}.[{}]", old_name.replace(']', "]]")),
            &format!("{qualifier}.\"{}\"", new_name.replace('"', "\"\"")),
        );
        renamed = renamed.replace(
            &format!("{qualifier}.`{}`", old_name.replace('`', "``")),
            &format!("{qualifier}.\"{}\"", new_name.replace('"', "\"\"")),
        );
    }
    renamed
}

#[derive(Debug)]
struct SimpleTriggerAction {
    update_of_columns: Vec<String>,
    when: Option<ScalarExpr>,
    statements: Vec<SimpleTriggerStatement>,
}

#[derive(Debug)]
enum SimpleTriggerStatement {
    Insert(SimpleTriggerInsertAction),
    Delete(SimpleTriggerDeleteAction),
    Update(SimpleTriggerUpdateAction),
    RaiseError(ScalarExpr),
    RaiseIgnore,
}

#[derive(Debug)]
struct SimpleTriggerInsertAction {
    target_table: String,
    target_columns: Option<Vec<String>>,
    rows: Vec<Vec<ScalarExpr>>,
    select_star: bool,
    aggregate: Option<SimpleTriggerAggregate>,
    aggregate_filter: Option<ScalarExpr>,
    group_by: Vec<ScalarExpr>,
    group_having: Vec<Vec<ScalarExpr>>,
    group_select_items: Vec<SimpleTriggerGroupSelectItem>,
    where_groups: Vec<Vec<ScalarExpr>>,
    distinct: bool,
    order_by: Vec<SimpleTriggerOrderBy>,
    limit: Option<ScalarExpr>,
    offset: Option<ScalarExpr>,
    select_from: Option<SimpleTriggerSelectFrom>,
}

#[derive(Debug)]
enum SimpleTriggerAggregate {
    Error(String),
    CountStar,
    CountExpr {
        expr: ScalarExpr,
        distinct: bool,
    },
    Sum {
        expr: ScalarExpr,
        distinct: bool,
    },
    Avg {
        expr: ScalarExpr,
        distinct: bool,
    },
    Total {
        expr: ScalarExpr,
        distinct: bool,
    },
    Min(ScalarExpr),
    Max(ScalarExpr),
    GroupConcat {
        expr: ScalarExpr,
        separator: Option<ScalarExpr>,
        distinct: bool,
        order_by: Vec<SimpleTriggerOrderBy>,
    },
}

#[derive(Debug)]
enum SimpleTriggerGroupSelectItem {
    Scalar {
        expr: ScalarExpr,
        alias: Option<String>,
    },
    Aggregate {
        aggregate: SimpleTriggerAggregate,
        filter: Vec<Vec<SimpleTriggerWhere>>,
        alias: Option<String>,
    },
}

#[derive(Debug)]
struct SimpleTriggerSelectFrom {
    table: String,
    alias: Option<String>,
}

#[derive(Debug)]
struct SimpleTriggerOrderBy {
    expr: ScalarExpr,
    descending: bool,
    nulls_first: Option<bool>,
    collation: Option<String>,
}

#[derive(Debug)]
struct SimpleTriggerDeleteAction {
    target_table: String,
    where_groups: Vec<Vec<SimpleTriggerWhere>>,
}

#[derive(Debug)]
struct SimpleTriggerUpdateAction {
    target_table: String,
    assignments: Vec<(String, ScalarExpr)>,
    where_groups: Vec<Vec<SimpleTriggerWhere>>,
}

#[derive(Debug)]
struct SimpleTriggerWhere {
    left: ScalarExpr,
    expr: SimpleTriggerWhereExpr,
}

#[derive(Debug)]
enum SimpleTriggerWhereExpr {
    Compare {
        op: CompareOp,
        value: ScalarExpr,
    },
    IsNull,
    IsNotNull,
    Is {
        value: ScalarExpr,
        negated: bool,
    },
    Between {
        low: ScalarExpr,
        high: ScalarExpr,
        negated: bool,
    },
    InList {
        values: Vec<ScalarExpr>,
        negated: bool,
    },
    Like {
        pattern: ScalarExpr,
        escape: Option<ScalarExpr>,
        negated: bool,
    },
    Glob {
        pattern: ScalarExpr,
        negated: bool,
    },
}

struct SimpleTriggerEvalContext<'a> {
    source_schema: &'a Schema,
    old_row: Option<&'a Row>,
    new_row: Option<&'a Row>,
    select_table_name: Option<&'a str>,
    select_alias: Option<&'a str>,
    select_schema: Option<&'a Schema>,
    select_row_id: Option<RowId>,
    select_row: Option<&'a Row>,
}

fn parse_simple_after_insert_trigger(sql: &str) -> Option<SimpleTriggerAction> {
    parse_simple_trigger_action(sql, "insert")
}

fn parse_simple_after_update_trigger(sql: &str) -> Option<SimpleTriggerAction> {
    parse_simple_trigger_action(sql, "update")
}

fn parse_simple_after_delete_trigger(sql: &str) -> Option<SimpleTriggerAction> {
    parse_simple_trigger_action(sql, "delete")
}

fn parse_simple_before_insert_trigger(sql: &str) -> Option<SimpleTriggerAction> {
    parse_simple_timed_trigger_action(sql, "before", "insert")
}

fn parse_simple_before_update_trigger(sql: &str) -> Option<SimpleTriggerAction> {
    parse_simple_timed_trigger_action(sql, "before", "update")
}

fn parse_simple_before_delete_trigger(sql: &str) -> Option<SimpleTriggerAction> {
    parse_simple_timed_trigger_action(sql, "before", "delete")
}

fn parse_simple_trigger_action(sql: &str, event: &str) -> Option<SimpleTriggerAction> {
    parse_simple_timed_trigger_action(sql, "after", event)
}

fn parse_simple_timed_trigger_action(
    sql: &str,
    timing: &str,
    event: &str,
) -> Option<SimpleTriggerAction> {
    let lower = sql.to_ascii_lowercase();
    let trigger_marker = format!(" {timing} {event}");
    let event_start = lower.find(&trigger_marker)?;
    let after_event = event_start + trigger_marker.len();
    let on_index = lower[after_event..].find(" on ")? + after_event;
    let update_of_columns = if event == "update" {
        let between_event_and_on = sql[after_event..on_index].trim();
        let between_lower = between_event_and_on.to_ascii_lowercase();
        if let Some(columns) = between_lower.strip_prefix("of ") {
            let offset = between_event_and_on.len() - columns.len();
            between_event_and_on[offset..]
                .split(',')
                .map(|column| column.trim().trim_matches('"').to_string())
                .filter(|column| !column.is_empty())
                .collect::<Vec<_>>()
        } else if between_event_and_on.is_empty() {
            Vec::new()
        } else {
            return None;
        }
    } else {
        Vec::new()
    };
    let begin = lower[on_index..].find(" begin ")? + on_index;
    let header = &sql[after_event..begin];
    let header_lower = &lower[after_event..begin];
    let when = if let Some(when_index) = header_lower.find(" when ") {
        let expr = header[when_index + " when ".len()..].trim();
        Some(parse_scalar_sql_expression(expr).ok()?)
    } else {
        None
    };
    let body_start = begin + " begin ".len();
    let body_end = lower.rfind(" end")?;
    let body = sql[body_start..body_end]
        .trim()
        .trim_end_matches(';')
        .trim();
    let statements = split_trigger_body_statements(body)
        .into_iter()
        .map(parse_simple_trigger_statement)
        .collect::<Option<Vec<_>>>()?;
    Some(SimpleTriggerAction {
        update_of_columns,
        when,
        statements,
    })
}

fn parse_simple_trigger_statement(body: &str) -> Option<SimpleTriggerStatement> {
    parse_simple_trigger_insert_action(body)
        .map(SimpleTriggerStatement::Insert)
        .or_else(|| parse_simple_trigger_delete_action(body).map(SimpleTriggerStatement::Delete))
        .or_else(|| parse_simple_trigger_update_action(body).map(SimpleTriggerStatement::Update))
        .or_else(|| parse_simple_trigger_raise_error(body).map(SimpleTriggerStatement::RaiseError))
        .or_else(|| {
            parse_simple_trigger_raise_ignore(body).then_some(SimpleTriggerStatement::RaiseIgnore)
        })
}

fn parse_simple_trigger_insert_action(body: &str) -> Option<SimpleTriggerInsertAction> {
    let body_lower = body.to_ascii_lowercase();
    let values_index = body_lower.find(" values ");
    let select_index = body_lower.find(" select ");
    let body_split = values_index
        .map(|index| (index, " values "))
        .or_else(|| select_index.map(|index| (index, " select ")))?;
    let insert_prefix = body[..body_split.0].trim();
    let target_spec = insert_prefix
        .strip_prefix("INSERT INTO ")
        .or_else(|| insert_prefix.strip_prefix("insert into "))?
        .trim();
    let (target_table, target_columns) = parse_trigger_insert_target(target_spec)?;
    let values = body[body_split.0 + body_split.1.len()..].trim();
    let (
        rows,
        select_star,
        aggregate,
        aggregate_filter,
        group_by,
        group_having,
        group_select_items,
        where_groups,
        distinct,
        order_by,
        limit,
        offset,
        select_from,
    ) = if body_split.1.eq_ignore_ascii_case(" select ") {
        let (values, limit, offset) = split_trigger_select_limit(values)?;
        let (values, order_by) = split_trigger_select_order_by(values)?;
        let (values, group_by) = split_trigger_select_group_by(values)?;
        let (select_values, where_expr) = split_trigger_select_where(values)?;
        let (select_values, select_from) = split_trigger_select_from(select_values)?;
        let (select_values, distinct) = strip_trigger_select_distinct(select_values);
        let select_from = if let Some(select_from) = select_from {
            Some(parse_trigger_select_from_clause(select_from)?)
        } else {
            None
        };
        let mut row = Vec::new();
        let mut select_aliases = Vec::new();
        let has_group_by = group_by.is_some();
        let (select_values, aggregate_filter) = if has_group_by {
            (select_values, None)
        } else {
            split_trigger_aggregate_filter(select_values)?
        };
        let group_select_items = if !has_group_by {
            Vec::new()
        } else {
            let select_items = split_trigger_value_exprs(select_values);
            for select_expr in &select_items {
                let (expr, alias) = parse_trigger_select_alias(select_expr);
                if let Some(alias) = alias
                    && let Ok(expr) = parse_scalar_sql_expression(expr)
                {
                    select_aliases.push((alias, expr));
                }
            }
            select_items
                .into_iter()
                .map(parse_trigger_group_select_item)
                .collect::<Option<Vec<_>>>()?
        };
        let (group_by, group_having) = if let Some(group_by) = group_by {
            let (group_by, having) = split_trigger_group_by_having(group_by)?;
            split_trigger_value_exprs(group_by)
                .into_iter()
                .map(|expr| parse_trigger_group_by_expr(expr, &group_select_items))
                .collect::<Option<Vec<_>>>()
                .map(|group_by| {
                    (
                        group_by,
                        having
                            .and_then(parse_simple_trigger_select_where_groups)
                            .unwrap_or_default(),
                    )
                })?
        } else {
            (Vec::new(), Vec::new())
        };
        let select_star = is_trigger_select_star(select_values, select_from.as_ref())?;
        let aggregate = parse_trigger_select_aggregate(select_values);
        if !select_star && aggregate.is_none() {
            for select_expr in split_trigger_value_exprs(select_values) {
                let (expr, alias) = parse_trigger_select_alias(select_expr);
                let expr = parse_scalar_sql_expression(expr).ok()?;
                if let Some(alias) = alias {
                    select_aliases.push((alias, expr.clone()));
                }
                row.push(expr);
            }
        }
        let rows = vec![row.clone()];
        let where_groups = if let Some(where_expr) = where_expr {
            parse_simple_trigger_select_where_groups(where_expr)?
        } else {
            Vec::new()
        };
        let order_by = if let Some(order_by) = order_by {
            parse_trigger_select_order_by(order_by, &select_aliases, &row)?
        } else {
            Vec::new()
        };
        let limit = limit.map(parse_scalar_sql_expression).transpose().ok()?;
        let offset = offset.map(parse_scalar_sql_expression).transpose().ok()?;
        (
            rows,
            select_star,
            aggregate,
            aggregate_filter,
            group_by,
            group_having,
            group_select_items,
            where_groups,
            distinct,
            order_by,
            limit,
            offset,
            select_from,
        )
    } else {
        let rows = split_trigger_values_rows(values)
            .into_iter()
            .map(|values| {
                split_trigger_value_exprs(values)
                    .into_iter()
                    .map(parse_scalar_sql_expression)
                    .collect::<Result<Vec<_>>>()
                    .ok()
            })
            .collect::<Option<Vec<_>>>()?;
        (
            rows,
            false,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            Vec::new(),
            None,
            None,
            None,
        )
    };
    Some(SimpleTriggerInsertAction {
        target_table,
        target_columns,
        rows,
        select_star,
        aggregate,
        aggregate_filter,
        group_by,
        group_having,
        group_select_items,
        where_groups,
        distinct,
        order_by,
        limit,
        offset,
        select_from,
    })
}

fn parse_simple_trigger_raise_error(body: &str) -> Option<ScalarExpr> {
    let body = body.trim();
    let upper = body.to_ascii_uppercase();
    let prefix = if upper.starts_with("SELECT RAISE(ABORT,") {
        "SELECT RAISE(ABORT,"
    } else if upper.starts_with("SELECT RAISE(FAIL,") {
        "SELECT RAISE(FAIL,"
    } else if upper.starts_with("SELECT RAISE(ROLLBACK,") {
        "SELECT RAISE(ROLLBACK,"
    } else {
        return None;
    };
    if !body.ends_with(')') {
        return None;
    }
    parse_scalar_sql_expression(body[prefix.len()..body.len() - 1].trim()).ok()
}

fn parse_simple_trigger_raise_ignore(body: &str) -> bool {
    let body = body.trim();
    body.eq_ignore_ascii_case("SELECT RAISE(IGNORE)")
}

fn parse_simple_trigger_delete_action(body: &str) -> Option<SimpleTriggerDeleteAction> {
    let body = body.trim();
    let body_lower = body.to_ascii_lowercase();
    let where_index = body_lower.find(" where ");
    let delete_prefix = where_index
        .map(|index| body[..index].trim())
        .unwrap_or(body);
    let target_table = delete_prefix
        .strip_prefix("DELETE FROM ")
        .or_else(|| delete_prefix.strip_prefix("delete from "))?
        .trim()
        .trim_end_matches(';')
        .trim_matches('"')
        .to_string();
    let where_groups = if let Some(where_index) = where_index {
        let condition = body[where_index + " where ".len()..].trim();
        parse_simple_trigger_where_groups(condition)?
    } else {
        Vec::new()
    };
    Some(SimpleTriggerDeleteAction {
        target_table,
        where_groups,
    })
}

fn parse_simple_trigger_update_action(body: &str) -> Option<SimpleTriggerUpdateAction> {
    let body = body.trim();
    let body_lower = body.to_ascii_lowercase();
    let set_index = body_lower.find(" set ")?;
    let where_index = body_lower.find(" where ");
    let update_prefix = body[..set_index].trim();
    let target_table = update_prefix
        .strip_prefix("UPDATE ")
        .or_else(|| update_prefix.strip_prefix("update "))?
        .trim()
        .trim_matches('"')
        .to_string();
    let assignment_end = where_index.unwrap_or(body.len());
    if assignment_end <= set_index {
        return None;
    }
    let assignments =
        split_trigger_value_exprs(body[set_index + " set ".len()..assignment_end].trim())
            .into_iter()
            .map(|assignment| {
                let assignment_equals = assignment.find('=')?;
                let assignment_column = assignment[..assignment_equals]
                    .trim()
                    .trim_matches('"')
                    .to_string();
                let assignment_value =
                    parse_scalar_sql_expression(assignment[assignment_equals + 1..].trim()).ok()?;
                Some((assignment_column, assignment_value))
            })
            .collect::<Option<Vec<_>>>()?;
    let where_groups = if let Some(where_index) = where_index {
        let condition = body[where_index + " where ".len()..].trim();
        parse_simple_trigger_where_groups(condition)?
    } else {
        Vec::new()
    };
    Some(SimpleTriggerUpdateAction {
        target_table,
        assignments,
        where_groups,
    })
}

fn parse_simple_trigger_where_groups(condition: &str) -> Option<Vec<Vec<SimpleTriggerWhere>>> {
    split_trigger_or_terms(condition)
        .into_iter()
        .map(|or_term| {
            split_trigger_and_terms(or_term)
                .into_iter()
                .map(parse_simple_trigger_where)
                .collect::<Option<Vec<_>>>()
        })
        .collect::<Option<Vec<_>>>()
}

fn parse_simple_trigger_where(condition: &str) -> Option<SimpleTriggerWhere> {
    let upper = condition.to_ascii_uppercase();
    if let Some((index, rhs_start)) = find_trigger_operator_clause(condition, &["NOT", "LIKE"]) {
        let left = parse_trigger_where_left(&condition[..index])?;
        let (pattern, escape) = parse_trigger_like_pattern(condition[rhs_start..].trim())?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::Like {
                pattern,
                escape,
                negated: true,
            },
        });
    }
    if let Some((index, rhs_start)) = find_trigger_operator_clause(condition, &["NOT", "GLOB"]) {
        let left = parse_trigger_where_left(&condition[..index])?;
        let pattern = parse_scalar_sql_expression(condition[rhs_start..].trim()).ok()?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::Glob {
                pattern,
                negated: true,
            },
        });
    }
    if let Some((index, rhs_start)) = find_trigger_operator_clause(condition, &["LIKE"]) {
        let left = parse_trigger_where_left(&condition[..index])?;
        let (pattern, escape) = parse_trigger_like_pattern(condition[rhs_start..].trim())?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::Like {
                pattern,
                escape,
                negated: false,
            },
        });
    }
    if let Some((index, rhs_start)) = find_trigger_operator_clause(condition, &["GLOB"]) {
        let left = parse_trigger_where_left(&condition[..index])?;
        let pattern = parse_scalar_sql_expression(condition[rhs_start..].trim()).ok()?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::Glob {
                pattern,
                negated: false,
            },
        });
    }
    if let Some(index) = upper.find(" IS NOT NULL") {
        let left = parse_trigger_where_left(&condition[..index])?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::IsNotNull,
        });
    }
    if let Some(index) = upper.find(" IS NULL") {
        let left = parse_trigger_where_left(&condition[..index])?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::IsNull,
        });
    }
    if let Some((index, rhs_start)) = find_trigger_operator_clause(condition, &["IS", "NOT"]) {
        let left = parse_trigger_where_left(&condition[..index])?;
        let value = parse_scalar_sql_expression(condition[rhs_start..].trim()).ok()?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::Is {
                value,
                negated: true,
            },
        });
    }
    if let Some((index, rhs_start)) = find_trigger_operator_clause(condition, &["IS"]) {
        let left = parse_trigger_where_left(&condition[..index])?;
        let value = parse_scalar_sql_expression(condition[rhs_start..].trim()).ok()?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::Is {
                value,
                negated: false,
            },
        });
    }
    if let Some((index, rhs_start)) = find_trigger_operator_clause(condition, &["NOT", "BETWEEN"]) {
        let left = parse_trigger_where_left(&condition[..index])?;
        let (low, high) = parse_trigger_between_bounds(condition[rhs_start..].trim())?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::Between {
                low,
                high,
                negated: true,
            },
        });
    }
    if let Some((index, rhs_start)) = find_trigger_operator_clause(condition, &["BETWEEN"]) {
        let left = parse_trigger_where_left(&condition[..index])?;
        let (low, high) = parse_trigger_between_bounds(condition[rhs_start..].trim())?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::Between {
                low,
                high,
                negated: false,
            },
        });
    }
    if let Some((index, rhs_start)) = find_trigger_operator_clause(condition, &["NOT", "IN"]) {
        let left = parse_trigger_where_left(&condition[..index])?;
        let values = parse_trigger_in_values(condition[rhs_start..].trim())?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::InList {
                values,
                negated: true,
            },
        });
    }
    if let Some((index, rhs_start)) = find_trigger_operator_clause(condition, &["IN"]) {
        let left = parse_trigger_where_left(&condition[..index])?;
        let values = parse_trigger_in_values(condition[rhs_start..].trim())?;
        return Some(SimpleTriggerWhere {
            left,
            expr: SimpleTriggerWhereExpr::InList {
                values,
                negated: false,
            },
        });
    }
    let (operator, op, op_len) = find_trigger_compare_operator(condition)?;
    let left = parse_trigger_where_left(&condition[..operator])?;
    let value = parse_scalar_sql_expression(condition[operator + op_len..].trim()).ok()?;
    Some(SimpleTriggerWhere {
        left,
        expr: SimpleTriggerWhereExpr::Compare { op, value },
    })
}

fn parse_trigger_where_left(input: &str) -> Option<ScalarExpr> {
    parse_scalar_sql_expression(input.trim()).ok()
}

fn find_trigger_compare_operator(condition: &str) -> Option<(usize, CompareOp, usize)> {
    let mut depth = 0_i32;
    let mut in_string = false;
    let bytes = condition.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index] as char;
        if ch == '\'' {
            if in_string && index + 1 < bytes.len() && bytes[index + 1] == b'\'' {
                index += 2;
                continue;
            }
            in_string = !in_string;
            index += 1;
            continue;
        }
        if !in_string && ch == '(' {
            depth += 1;
            index += 1;
            continue;
        }
        if !in_string && ch == ')' {
            depth -= 1;
            index += 1;
            continue;
        }
        if !in_string && depth == 0 {
            let rest = &condition[index..];
            if rest.starts_with("!=") {
                return Some((index, CompareOp::Ne, 2));
            }
            if rest.starts_with("<>") {
                return Some((index, CompareOp::Ne, 2));
            }
            if rest.starts_with("<=") {
                return Some((index, CompareOp::Lte, 2));
            }
            if rest.starts_with(">=") {
                return Some((index, CompareOp::Gte, 2));
            }
            if ch == '<' {
                return Some((index, CompareOp::Lt, 1));
            }
            if ch == '>' {
                return Some((index, CompareOp::Gt, 1));
            }
            if ch == '=' {
                return Some((index, CompareOp::Eq, 1));
            }
        }
        index += 1;
    }
    None
}

fn parse_trigger_like_pattern(input: &str) -> Option<(ScalarExpr, Option<ScalarExpr>)> {
    let Some(escape_index) = find_trigger_escape_clause(input) else {
        return Some((parse_scalar_sql_expression(input.trim()).ok()?, None));
    };
    let pattern = parse_scalar_sql_expression(input[..escape_index].trim()).ok()?;
    let escape = parse_scalar_sql_expression(input[escape_index + "ESCAPE".len()..].trim()).ok()?;
    Some((pattern, Some(escape)))
}

fn parse_trigger_between_bounds(input: &str) -> Option<(ScalarExpr, ScalarExpr)> {
    let (and_index, high_start) = find_trigger_operator_clause(input, &["AND"])?;
    let low = parse_scalar_sql_expression(input[..and_index].trim()).ok()?;
    let high = parse_scalar_sql_expression(input[high_start..].trim()).ok()?;
    Some((low, high))
}

fn parse_trigger_in_values(input: &str) -> Option<Vec<ScalarExpr>> {
    let input = input.trim();
    if !input.starts_with('(') || !input.ends_with(')') {
        return None;
    }
    let inner = input[1..input.len() - 1].trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    split_trigger_value_exprs(inner)
        .into_iter()
        .map(|value| parse_scalar_sql_expression(value).ok())
        .collect()
}

fn find_trigger_operator_clause(input: &str, keywords: &[&str]) -> Option<(usize, usize)> {
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut chars = input.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\'' if in_string && chars.peek().is_some_and(|(_, next)| *next == '\'') => {
                chars.next();
            }
            '\'' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth -= 1,
            _ if !in_string && depth == 0 => {
                if let Some(end) = match_trigger_keyword_sequence(input, index, keywords) {
                    return Some((index, end));
                }
            }
            _ => {}
        }
    }
    None
}

fn match_trigger_keyword_sequence(input: &str, start: usize, keywords: &[&str]) -> Option<usize> {
    if input[..start]
        .chars()
        .next_back()
        .is_some_and(is_sql_identifier_char)
    {
        return None;
    }
    let mut cursor = start;
    for (keyword_index, keyword) in keywords.iter().enumerate() {
        if input[cursor..].len() < keyword.len() {
            return None;
        }
        let end = cursor + keyword.len();
        if !input[cursor..end].eq_ignore_ascii_case(keyword) {
            return None;
        }
        if input[end..]
            .chars()
            .next()
            .is_some_and(is_sql_identifier_char)
        {
            return None;
        }
        cursor = end;
        if keyword_index + 1 < keywords.len() {
            let next_cursor = skip_sql_whitespace(input, cursor);
            if next_cursor == cursor {
                return None;
            }
            cursor = next_cursor;
        }
    }
    Some(cursor)
}

fn find_trigger_escape_clause(input: &str) -> Option<usize> {
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut chars = input.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\'' if in_string && chars.peek().is_some_and(|(_, next)| *next == '\'') => {
                chars.next();
            }
            '\'' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth -= 1,
            _ if !in_string && depth == 0 && input[index..].len() >= "ESCAPE".len() => {
                let end = index + "ESCAPE".len();
                if input[index..end].eq_ignore_ascii_case("ESCAPE")
                    && input[..index]
                        .chars()
                        .next_back()
                        .is_some_and(|before| !is_sql_identifier_char(before))
                    && input[end..]
                        .chars()
                        .next()
                        .is_none_or(|after| !is_sql_identifier_char(after))
                {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn skip_sql_whitespace(input: &str, start: usize) -> usize {
    let mut cursor = start;
    while let Some(ch) = input[cursor..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        cursor += ch.len_utf8();
    }
    cursor
}

fn is_sql_identifier_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
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

fn trigger_abs_value(value: &Value) -> Result<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Boolean(value) => Ok(Value::Integer(if *value { 1 } else { 0 })),
        Value::Integer(value) => value
            .checked_abs()
            .map(Value::Integer)
            .ok_or_else(|| DbError::storage("ABS overflowed i64")),
        Value::Real(value) => Ok(Value::Real(value.abs())),
        Value::Text(value) => Ok(trigger_numeric_prefix(value).map_abs()),
        Value::Blob(value) => Ok(trigger_numeric_prefix(&String::from_utf8_lossy(value)).map_abs()),
    }
}

trait TriggerNumericAbs {
    fn map_abs(self) -> Value;
}

impl TriggerNumericAbs for Value {
    fn map_abs(self) -> Value {
        match self {
            Value::Integer(value) => value
                .checked_abs()
                .map(Value::Integer)
                .unwrap_or_else(|| Value::Real((value as f64).abs())),
            Value::Real(value) => Value::Real(value.abs()),
            _ => Value::Integer(0),
        }
    }
}

fn trigger_numeric_prefix(value: &str) -> Value {
    let Some((candidate, has_real_syntax)) = trigger_numeric_text_prefix(value) else {
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

fn trigger_numeric_text_prefix(value: &str) -> Option<(&str, bool)> {
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

fn trigger_is_true_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Boolean(value) => *value,
        Value::Integer(value) => *value != 0,
        Value::Real(value) => *value != 0.0,
        Value::Text(value) => match trigger_numeric_prefix(value) {
            Value::Integer(value) => value != 0,
            Value::Real(value) => value != 0.0,
            _ => false,
        },
        Value::Blob(value) => match trigger_numeric_prefix(&String::from_utf8_lossy(value)) {
            Value::Integer(value) => value != 0,
            Value::Real(value) => value != 0.0,
            _ => false,
        },
    }
}

fn trigger_value_to_i64(value: &Value) -> Result<i64> {
    match value {
        Value::Null => Err(DbError::storage("trigger integer argument is NULL")),
        Value::Boolean(value) => Ok(if *value { 1 } else { 0 }),
        Value::Integer(value) => Ok(*value),
        Value::Real(value) => Ok(*value as i64),
        Value::Text(value) => Ok(match trigger_numeric_prefix(value) {
            Value::Integer(value) => value,
            Value::Real(value) => value as i64,
            _ => 0,
        }),
        Value::Blob(value) => Ok(
            match trigger_numeric_prefix(&String::from_utf8_lossy(value)) {
                Value::Integer(value) => value,
                Value::Real(value) => value as i64,
                _ => 0,
            },
        ),
    }
}

fn trigger_cast_value(value: Value, ty: ColumnType) -> Result<Value> {
    Ok(match ty {
        ColumnType::Any => value,
        ColumnType::Boolean => match value {
            Value::Null => Value::Null,
            value => Value::Boolean(trigger_value_to_i64(&value)? != 0),
        },
        ColumnType::Integer => match value {
            Value::Null => Value::Null,
            value => Value::Integer(trigger_value_to_i64(&value)?),
        },
        ColumnType::Numeric => match value {
            Value::Null => Value::Null,
            Value::Text(value) => trigger_numeric_prefix(&value),
            Value::Blob(value) => trigger_numeric_prefix(&String::from_utf8_lossy(&value)),
            value => value,
        },
        ColumnType::Real => match value {
            Value::Null => Value::Null,
            value => Value::Real(trigger_value_to_f64(&value)?),
        },
        ColumnType::Blob => match value {
            Value::Null => Value::Null,
            Value::Blob(value) => Value::Blob(value),
            value => Value::Blob(trigger_value_to_text_owned(&value).into_bytes()),
        },
        ColumnType::Text => match value {
            Value::Null => Value::Null,
            value => Value::Text(trigger_value_to_text_owned(&value)),
        },
    })
}

fn trigger_value_to_text_owned(value: &Value) -> String {
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

fn trigger_value_to_f64(value: &Value) -> Result<f64> {
    match value {
        Value::Null => Err(DbError::storage("trigger numeric argument is NULL")),
        Value::Boolean(value) => Ok(if *value { 1.0 } else { 0.0 }),
        Value::Integer(value) => Ok(*value as f64),
        Value::Real(value) => Ok(*value),
        Value::Text(value) => Ok(match trigger_numeric_prefix(value) {
            Value::Integer(value) => value as f64,
            Value::Real(value) => value,
            _ => 0.0,
        }),
        Value::Blob(value) => Ok(
            match trigger_numeric_prefix(&String::from_utf8_lossy(value)) {
                Value::Integer(value) => value as f64,
                Value::Real(value) => value,
                _ => 0.0,
            },
        ),
    }
}

fn trigger_substr_text(value: &str, start: i64, length: Option<i64>) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    let (begin, end) = trigger_substr_bounds(characters.len(), start, length);
    characters[begin..end].iter().collect()
}

fn trigger_substr_blob(value: &[u8], start: i64, length: Option<i64>) -> Vec<u8> {
    let (begin, end) = trigger_substr_bounds(value.len(), start, length);
    value[begin..end].to_vec()
}

fn trigger_instr_blob(haystack: &[u8], needle: &[u8]) -> i64 {
    if needle.is_empty() {
        return 1;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| index as i64 + 1)
        .unwrap_or(0)
}

fn trigger_quote_value(value: &Value) -> String {
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
        Value::Real(value) => value.to_string(),
        Value::Blob(value) => format!(
            "X'{}'",
            value
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
        Value::Text(value) => format!("'{}'", value.replace('\'', "''")),
    }
}

fn trigger_hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

fn trigger_date_value(value: &Value) -> Value {
    if matches!(value, Value::Null) {
        return Value::Null;
    }
    let text = trigger_value_to_text_owned(value);
    let Some(date) = text.get(..10) else {
        return Value::Null;
    };
    if date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        Value::Text(date.to_string())
    } else {
        Value::Null
    }
}

fn trigger_time_value(value: &Value) -> Value {
    if matches!(value, Value::Null) {
        return Value::Null;
    }
    let text = trigger_value_to_text_owned(value);
    let time = text
        .get(11..19)
        .filter(|_| text.as_bytes().get(10) == Some(&b' '))
        .or_else(|| text.get(..8));
    let Some(time) = time else {
        return Value::Null;
    };
    if time.len() == 8
        && time.as_bytes()[2] == b':'
        && time.as_bytes()[5] == b':'
        && time
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 2 | 5) || byte.is_ascii_digit())
    {
        Value::Text(time.to_string())
    } else {
        Value::Null
    }
}

fn trigger_datetime_value(value: &Value) -> Value {
    if matches!(value, Value::Null) {
        return Value::Null;
    }
    let text = trigger_value_to_text_owned(value);
    let Some(date) = text.get(..10) else {
        return Value::Null;
    };
    if !matches!(
        trigger_date_value(&Value::Text(date.to_string())),
        Value::Text(_)
    ) {
        return Value::Null;
    }
    if text.len() >= 19 && text.as_bytes().get(10) == Some(&b' ') {
        let Some(time) = text.get(11..19) else {
            return Value::Null;
        };
        if matches!(
            trigger_time_value(&Value::Text(time.to_string())),
            Value::Text(_)
        ) {
            Value::Text(text[..19].to_string())
        } else {
            Value::Null
        }
    } else {
        Value::Text(format!("{date} 00:00:00"))
    }
}

fn trigger_substr_bounds(item_count: usize, start: i64, length: Option<i64>) -> (usize, usize) {
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
            let effective_length = if start == 0 {
                length.saturating_sub(1)
            } else {
                length
            };
            let end = start_index.saturating_add(effective_length).clamp(0, len);
            (begin, end)
        }
        Some(length) => {
            let begin = start_index.saturating_add(length).clamp(0, len);
            let end = start_index.clamp(0, len);
            (begin, end)
        }
    };

    (
        usize::try_from(begin).unwrap_or(0),
        usize::try_from(end).unwrap_or(0),
    )
}

fn split_trigger_and_terms(condition: &str) -> Vec<&str> {
    split_trigger_terms(condition, " AND ")
}

fn split_trigger_or_terms(condition: &str) -> Vec<&str> {
    split_trigger_terms(condition, " OR ")
}

fn split_trigger_terms<'a>(condition: &'a str, separator: &str) -> Vec<&'a str> {
    let mut terms = Vec::new();
    let mut start = 0;
    let mut depth = 0_i32;
    let mut in_string = false;
    let bytes = condition.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index] as char;
        if ch == '\'' {
            if in_string && index + 1 < bytes.len() && bytes[index + 1] == b'\'' {
                index += 2;
                continue;
            }
            in_string = !in_string;
            index += 1;
            continue;
        }
        if !in_string && ch == '(' {
            depth += 1;
            index += 1;
            continue;
        }
        if !in_string && ch == ')' {
            depth -= 1;
            index += 1;
            continue;
        }
        if !in_string
            && depth == 0
            && index + separator.len() <= bytes.len()
            && condition[index..index + separator.len()].eq_ignore_ascii_case(separator)
            && !is_trigger_between_and(condition, start, index, separator)
        {
            terms.push(condition[start..index].trim());
            start = index + separator.len();
            index = start;
            continue;
        }
        index += 1;
    }
    terms.push(condition[start..].trim());
    terms
}

fn is_trigger_between_and(
    condition: &str,
    term_start: usize,
    and_index: usize,
    separator: &str,
) -> bool {
    separator.eq_ignore_ascii_case(" AND ")
        && find_trigger_operator_clause(condition[term_start..and_index].trim(), &["BETWEEN"])
            .is_some()
}

fn split_trigger_values_rows(values: &str) -> Vec<&str> {
    let mut rows = Vec::new();
    let mut row_start = None;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut chars = values.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\'' if in_string && chars.peek().is_some_and(|(_, next)| *next == '\'') => {
                chars.next();
            }
            '\'' => in_string = !in_string,
            '(' if !in_string => {
                if depth == 0 {
                    row_start = Some(index + ch.len_utf8());
                }
                depth += 1;
            }
            ')' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = row_start.take() {
                        rows.push(values[start..index].trim());
                    }
                }
            }
            _ => {}
        }
    }
    rows
}

fn split_trigger_body_statements(body: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_string = false;
    let mut chars = body.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\'' if in_string && chars.peek().is_some_and(|(_, next)| *next == '\'') => {
                chars.next();
            }
            '\'' => in_string = !in_string,
            ';' if !in_string => {
                let statement = body[start..index].trim();
                if !statement.is_empty() {
                    parts.push(statement);
                }
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    let statement = body[start..].trim();
    if !statement.is_empty() {
        parts.push(statement);
    }
    parts
}

fn parse_trigger_insert_target(target_spec: &str) -> Option<(String, Option<Vec<String>>)> {
    let Some(columns_start) = target_spec.find('(') else {
        return Some((target_spec.trim().trim_matches('"').to_string(), None));
    };
    let columns_end = target_spec.rfind(')')?;
    let target_table = target_spec[..columns_start]
        .trim()
        .trim_matches('"')
        .to_string();
    let columns = target_spec[columns_start + 1..columns_end]
        .split(',')
        .map(|column| column.trim().trim_matches('"').to_string())
        .collect::<Vec<_>>();
    Some((target_table, Some(columns)))
}

fn split_trigger_select_where(select_body: &str) -> Option<(&str, Option<&str>)> {
    let Some((where_index, condition_start)) =
        find_trigger_operator_clause(select_body, &["WHERE"])
    else {
        return Some((select_body.trim(), None));
    };
    let select_values = select_body[..where_index].trim();
    let condition = select_body[condition_start..].trim();
    if select_values.is_empty() || condition.is_empty() {
        return None;
    }
    Some((select_values, Some(condition)))
}

fn split_trigger_select_order_by(select_body: &str) -> Option<(&str, Option<&str>)> {
    let Some((order_index, order_by_start)) =
        find_trigger_operator_clause(select_body, &["ORDER", "BY"])
    else {
        return Some((select_body.trim(), None));
    };
    let before_order = select_body[..order_index].trim();
    let order_by = select_body[order_by_start..].trim();
    if before_order.is_empty() || order_by.is_empty() {
        return None;
    }
    Some((before_order, Some(order_by)))
}

fn split_trigger_select_limit(select_body: &str) -> Option<(&str, Option<&str>, Option<&str>)> {
    let Some((limit_index, limit_start)) = find_trigger_operator_clause(select_body, &["LIMIT"])
    else {
        return Some((select_body.trim(), None, None));
    };
    let before_limit = select_body[..limit_index].trim();
    let limit_clause = select_body[limit_start..].trim();
    let (limit, offset) = if let Some((offset_index, offset_start)) =
        find_trigger_operator_clause(limit_clause, &["OFFSET"])
    {
        (
            limit_clause[..offset_index].trim(),
            Some(limit_clause[offset_start..].trim()),
        )
    } else if let Some(comma_index) = find_trigger_top_level_comma(limit_clause) {
        (
            limit_clause[comma_index + 1..].trim(),
            Some(limit_clause[..comma_index].trim()),
        )
    } else {
        (limit_clause, None)
    };
    if before_limit.is_empty() || limit.is_empty() {
        return None;
    }
    if offset.is_some_and(str::is_empty) {
        return None;
    }
    Some((before_limit, Some(limit), offset))
}

fn split_trigger_select_group_by(select_body: &str) -> Option<(&str, Option<&str>)> {
    let Some((group_index, group_by_start)) =
        find_trigger_operator_clause(select_body, &["GROUP", "BY"])
    else {
        return Some((select_body.trim(), None));
    };
    let before_group = select_body[..group_index].trim();
    let group_by = select_body[group_by_start..].trim();
    if before_group.is_empty() || group_by.is_empty() {
        return None;
    }
    Some((before_group, Some(group_by)))
}

fn split_trigger_group_by_having(group_by: &str) -> Option<(&str, Option<&str>)> {
    let Some((having_index, having_start)) = find_trigger_operator_clause(group_by, &["HAVING"])
    else {
        return Some((group_by.trim(), None));
    };
    let group_by_exprs = group_by[..having_index].trim();
    let having = group_by[having_start..].trim();
    if group_by_exprs.is_empty() || having.is_empty() {
        return None;
    }
    Some((group_by_exprs, Some(having)))
}

fn find_trigger_top_level_comma(input: &str) -> Option<usize> {
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut chars = input.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\'' if in_string && chars.peek().is_some_and(|(_, next)| *next == '\'') => {
                chars.next();
            }
            '\'' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth -= 1,
            ',' if !in_string && depth == 0 => return Some(index),
            _ => {}
        }
    }
    None
}

fn parse_trigger_select_order_by(
    order_by: &str,
    select_aliases: &[(String, ScalarExpr)],
    select_exprs: &[ScalarExpr],
) -> Option<Vec<SimpleTriggerOrderBy>> {
    split_trigger_value_exprs(order_by)
        .into_iter()
        .map(|term| parse_trigger_select_order_by_term(term, select_aliases, select_exprs))
        .collect()
}

fn parse_trigger_select_order_by_term(
    order_by: &str,
    select_aliases: &[(String, ScalarExpr)],
    select_exprs: &[ScalarExpr],
) -> Option<SimpleTriggerOrderBy> {
    let mut order_by = order_by.trim();
    if order_by.is_empty() {
        return None;
    }
    let mut descending = false;
    let mut nulls_first = None;
    let upper = order_by.to_ascii_uppercase();
    if upper.ends_with(" NULLS FIRST") {
        nulls_first = Some(true);
        order_by = order_by[..order_by.len() - " NULLS FIRST".len()].trim();
    } else if upper.ends_with(" NULLS LAST") {
        nulls_first = Some(false);
        order_by = order_by[..order_by.len() - " NULLS LAST".len()].trim();
    }
    let upper = order_by.to_ascii_uppercase();
    if upper.ends_with(" DESC") {
        descending = true;
        order_by = order_by[..order_by.len() - " DESC".len()].trim();
    } else if upper.ends_with(" ASC") {
        order_by = order_by[..order_by.len() - " ASC".len()].trim();
    }
    if order_by.is_empty() {
        return None;
    }
    let expr = order_by
        .parse::<usize>()
        .ok()
        .and_then(|position| position.checked_sub(1))
        .and_then(|index| select_exprs.get(index).cloned())
        .or_else(|| {
            select_aliases
                .iter()
                .find(|(alias, _)| alias.eq_ignore_ascii_case(order_by))
                .map(|(_, expr)| expr.clone())
        })
        .or_else(|| parse_scalar_sql_expression(order_by).ok())?;
    let (expr, collation) = match expr {
        ScalarExpr::Collate { expr, collation } => (*expr, Some(collation)),
        expr => (expr, None),
    };
    Some(SimpleTriggerOrderBy {
        expr,
        descending,
        nulls_first,
        collation,
    })
}

fn parse_simple_trigger_select_where_groups(condition: &str) -> Option<Vec<Vec<ScalarExpr>>> {
    split_trigger_or_terms(condition)
        .into_iter()
        .map(|or_term| {
            split_trigger_and_terms(or_term)
                .into_iter()
                .map(|and_term| parse_scalar_sql_expression(and_term).ok())
                .collect::<Option<Vec<_>>>()
        })
        .collect::<Option<Vec<_>>>()
}

fn split_trigger_aggregate_order_by(aggregate_args: &str) -> Option<(&str, Option<&str>)> {
    let Some((order_index, order_by_start)) =
        find_trigger_operator_clause(aggregate_args, &["ORDER", "BY"])
    else {
        return Some((aggregate_args.trim(), None));
    };
    let args = aggregate_args[..order_index].trim();
    let order_by = aggregate_args[order_by_start..].trim();
    if args.is_empty() || order_by.is_empty() {
        return None;
    }
    Some((args, Some(order_by)))
}

fn split_trigger_select_from(select_values: &str) -> Option<(&str, Option<&str>)> {
    let Some((from_index, table_start)) = find_trigger_operator_clause(select_values, &["FROM"])
    else {
        return Some((select_values.trim(), None));
    };
    let values = select_values[..from_index].trim();
    let from = select_values[table_start..].trim();
    if values.is_empty() || from.is_empty() {
        return None;
    }
    Some((values, Some(from)))
}

fn split_trigger_aggregate_filter(select_values: &str) -> Option<(&str, Option<ScalarExpr>)> {
    let Some((filter_index, filter_start)) =
        find_trigger_operator_clause(select_values, &["FILTER"])
    else {
        return Some((select_values.trim(), None));
    };
    let aggregate = select_values[..filter_index].trim();
    let filter = select_values[filter_start..].trim();
    let filter_lower = filter.to_ascii_lowercase();
    if !filter_lower.starts_with("(where ") || !filter.ends_with(')') {
        return None;
    }
    let condition = filter["(where ".len()..filter.len() - 1].trim();
    if aggregate.is_empty() || condition.is_empty() {
        return None;
    }
    Some((
        aggregate,
        Some(parse_scalar_sql_expression(condition).ok()?),
    ))
}

fn split_trigger_aggregate_filter_groups(
    select_values: &str,
) -> Option<(&str, Vec<Vec<SimpleTriggerWhere>>)> {
    let Some((filter_index, filter_start)) =
        find_trigger_operator_clause(select_values, &["FILTER"])
    else {
        return Some((select_values.trim(), Vec::new()));
    };
    let aggregate = select_values[..filter_index].trim();
    let filter = select_values[filter_start..].trim();
    let filter_lower = filter.to_ascii_lowercase();
    if !filter_lower.starts_with("(where ") || !filter.ends_with(')') {
        return None;
    }
    let condition = filter["(where ".len()..filter.len() - 1].trim();
    if aggregate.is_empty() || condition.is_empty() {
        return None;
    }
    Some((aggregate, parse_simple_trigger_where_groups(condition)?))
}

fn strip_trigger_select_distinct(select_values: &str) -> (&str, bool) {
    let trimmed = select_values.trim();
    if trimmed.len() >= "DISTINCT".len()
        && trimmed[.."DISTINCT".len()].eq_ignore_ascii_case("DISTINCT")
        && trimmed["DISTINCT".len()..]
            .chars()
            .next()
            .is_none_or(|ch| !is_sql_identifier_char(ch))
    {
        (trimmed["DISTINCT".len()..].trim(), true)
    } else {
        (trimmed, false)
    }
}

fn parse_trigger_select_aggregate(select_values: &str) -> Option<SimpleTriggerAggregate> {
    let trimmed = select_values.trim();
    if trimmed.eq_ignore_ascii_case("count(*)") {
        return Some(SimpleTriggerAggregate::CountStar);
    }
    if trimmed.len() >= "count(".len() + 1
        && trimmed[.."count(".len()].eq_ignore_ascii_case("count(")
        && trimmed.ends_with(')')
    {
        let mut inner = trimmed["count(".len()..trimmed.len() - 1].trim();
        if inner.is_empty() || inner == "*" {
            return None;
        }
        let (stripped, _) = split_trigger_aggregate_order_by(inner)?;
        inner = stripped;
        let (stripped, distinct) = strip_trigger_aggregate_quantifier(inner);
        inner = stripped;
        if inner.is_empty() {
            return None;
        }
        let args = split_trigger_value_exprs(inner);
        if args.len() != 1 {
            return Some(SimpleTriggerAggregate::Error(
                "wrong number of arguments to function count()".to_string(),
            ));
        }
        return Some(SimpleTriggerAggregate::CountExpr {
            expr: parse_scalar_sql_expression(args[0]).ok()?,
            distinct,
        });
    }
    if trimmed.len() >= "group_concat(".len() + 1
        && trimmed[.."group_concat(".len()].eq_ignore_ascii_case("group_concat(")
        && trimmed.ends_with(')')
    {
        let mut inner = trimmed["group_concat(".len()..trimmed.len() - 1].trim();
        let (stripped, order_by) = split_trigger_aggregate_order_by(inner)?;
        inner = stripped;
        let (stripped, distinct) = strip_trigger_aggregate_quantifier(inner);
        inner = stripped;
        let args = split_trigger_value_exprs(inner);
        if args.is_empty() || args.len() > 2 || args[0].is_empty() {
            return None;
        }
        if distinct && args.len() != 1 {
            return Some(SimpleTriggerAggregate::Error(
                "DISTINCT aggregates must have exactly one argument".to_string(),
            ));
        }
        return Some(SimpleTriggerAggregate::GroupConcat {
            expr: parse_scalar_sql_expression(args[0]).ok()?,
            separator: if args.len() == 2 {
                Some(parse_scalar_sql_expression(args[1]).ok()?)
            } else {
                None
            },
            distinct,
            order_by: if let Some(order_by) = order_by {
                parse_trigger_select_order_by(order_by, &[], &[])?
            } else {
                Vec::new()
            },
        });
    }
    for prefix in ["sum(", "avg(", "total(", "min(", "max("] {
        if trimmed.len() >= prefix.len() + 1
            && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix)
            && trimmed.ends_with(')')
        {
            let mut inner = trimmed[prefix.len()..trimmed.len() - 1].trim();
            if inner.is_empty() || inner == "*" {
                return None;
            }
            let (stripped, _) = split_trigger_aggregate_order_by(inner)?;
            inner = stripped;
            let (stripped, distinct) = strip_trigger_aggregate_quantifier(inner);
            inner = stripped;
            if inner.is_empty() {
                return None;
            }
            let args = split_trigger_value_exprs(inner);
            if args.len() != 1 {
                if prefix.eq_ignore_ascii_case("sum(")
                    || prefix.eq_ignore_ascii_case("avg(")
                    || prefix.eq_ignore_ascii_case("total(")
                {
                    let function_name = &prefix[..prefix.len() - 1];
                    return Some(SimpleTriggerAggregate::Error(format!(
                        "wrong number of arguments to function {function_name}()"
                    )));
                }
                return None;
            }
            let expr = parse_scalar_sql_expression(args[0]).ok()?;
            return Some(if prefix.eq_ignore_ascii_case("sum(") {
                SimpleTriggerAggregate::Sum { expr, distinct }
            } else if prefix.eq_ignore_ascii_case("avg(") {
                SimpleTriggerAggregate::Avg { expr, distinct }
            } else if prefix.eq_ignore_ascii_case("total(") {
                SimpleTriggerAggregate::Total { expr, distinct }
            } else if prefix.eq_ignore_ascii_case("min(") {
                SimpleTriggerAggregate::Min(expr)
            } else {
                SimpleTriggerAggregate::Max(expr)
            });
        }
    }
    None
}

fn parse_trigger_group_select_item(select_value: &str) -> Option<SimpleTriggerGroupSelectItem> {
    let (select_value, alias) = parse_trigger_select_alias(select_value);
    let (select_value, filter) = split_trigger_aggregate_filter_groups(select_value)?;
    if let Some(aggregate) = parse_trigger_select_aggregate(select_value) {
        Some(SimpleTriggerGroupSelectItem::Aggregate {
            aggregate,
            filter,
            alias,
        })
    } else {
        Some(SimpleTriggerGroupSelectItem::Scalar {
            expr: parse_scalar_sql_expression(select_value).ok()?,
            alias,
        })
    }
}

fn parse_trigger_group_by_expr(
    expr: &str,
    select_items: &[SimpleTriggerGroupSelectItem],
) -> Option<ScalarExpr> {
    let expr = expr.trim();
    if let Some(index) = expr
        .parse::<usize>()
        .ok()
        .and_then(|position| position.checked_sub(1))
        && let Some(SimpleTriggerGroupSelectItem::Scalar { expr, .. }) = select_items.get(index)
    {
        return Some(expr.clone());
    }
    for item in select_items {
        if let SimpleTriggerGroupSelectItem::Scalar {
            expr: select_expr,
            alias: Some(alias),
        } = item
            && alias.eq_ignore_ascii_case(expr)
        {
            return Some(select_expr.clone());
        }
    }
    parse_scalar_sql_expression(expr).ok()
}

fn strip_trigger_aggregate_quantifier(input: &str) -> (&str, bool) {
    let trimmed = input.trim();
    if trimmed.len() >= "distinct".len()
        && trimmed[.."distinct".len()].eq_ignore_ascii_case("distinct")
        && trimmed["distinct".len()..]
            .chars()
            .next()
            .is_none_or(|ch| !is_sql_identifier_char(ch))
    {
        (trimmed["distinct".len()..].trim(), true)
    } else if trimmed.len() >= "all".len()
        && trimmed[.."all".len()].eq_ignore_ascii_case("all")
        && trimmed["all".len()..]
            .chars()
            .next()
            .is_none_or(|ch| !is_sql_identifier_char(ch))
    {
        (trimmed["all".len()..].trim(), false)
    } else {
        (trimmed, false)
    }
}

fn is_trigger_select_star(
    select_values: &str,
    select_from: Option<&SimpleTriggerSelectFrom>,
) -> Option<bool> {
    let select_values = select_values.trim();
    if select_values == "*" {
        return Some(true);
    }
    let Some(qualifier) = select_values.strip_suffix(".*") else {
        return Some(false);
    };
    let qualifier = qualifier.trim().trim_matches('"');
    let select_from = select_from?;
    Some(
        qualifier.eq_ignore_ascii_case(&select_from.table)
            || select_from
                .alias
                .as_deref()
                .is_some_and(|alias| qualifier.eq_ignore_ascii_case(alias)),
    )
}

fn parse_trigger_select_from_clause(from: &str) -> Option<SimpleTriggerSelectFrom> {
    let parts = from.split_whitespace().collect::<Vec<_>>();
    let (table, alias) = match parts.as_slice() {
        [table] => (*table, None),
        [table, alias] => (*table, Some(*alias)),
        [table, as_keyword, alias] if as_keyword.eq_ignore_ascii_case("AS") => {
            (*table, Some(*alias))
        }
        _ => return None,
    };
    Some(SimpleTriggerSelectFrom {
        table: table.trim_matches('"').to_string(),
        alias: alias.map(|alias| alias.trim_matches('"').to_string()),
    })
}

fn parse_trigger_select_alias(expr: &str) -> (&str, Option<String>) {
    let mut depth = 0_i32;
    let mut in_string = false;
    let bytes = expr.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index] as char;
        if ch == '\'' {
            if in_string && index + 1 < bytes.len() && bytes[index + 1] == b'\'' {
                index += 2;
                continue;
            }
            in_string = !in_string;
            index += 1;
            continue;
        }
        if !in_string && ch == '(' {
            depth += 1;
            index += 1;
            continue;
        }
        if !in_string && ch == ')' {
            depth -= 1;
            index += 1;
            continue;
        }
        if !in_string
            && depth == 0
            && index + " AS ".len() <= bytes.len()
            && expr[index..index + " AS ".len()].eq_ignore_ascii_case(" AS ")
        {
            let alias = expr[index + " AS ".len()..]
                .trim()
                .trim_matches('"')
                .to_string();
            return (expr[..index].trim(), (!alias.is_empty()).then_some(alias));
        }
        index += 1;
    }
    if let Some((expr, alias)) = split_trigger_bare_select_alias(expr) {
        return (expr, Some(alias));
    }
    (expr, None)
}

fn split_trigger_bare_select_alias(expr: &str) -> Option<(&str, String)> {
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut alias_start = None;
    let mut chars = expr.char_indices().rev().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\'' if in_string && chars.peek().is_some_and(|(_, next)| *next == '\'') => {
                chars.next();
            }
            '\'' => in_string = !in_string,
            ')' if !in_string => depth += 1,
            '(' if !in_string => depth -= 1,
            ch if !in_string && depth == 0 && ch.is_whitespace() => {
                alias_start = Some(index + ch.len_utf8());
                break;
            }
            _ => {}
        }
    }
    let alias_start = alias_start?;
    let base = expr[..alias_start].trim();
    let alias = expr[alias_start..].trim().trim_matches('"');
    if base.is_empty()
        || alias.is_empty()
        || !alias.chars().all(is_sql_identifier_char)
        || parse_scalar_sql_expression(base).is_err()
    {
        return None;
    }
    Some((base, alias.to_string()))
}

fn split_trigger_value_exprs(values: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut chars = values.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\'' if in_string && chars.peek().is_some_and(|(_, next)| *next == '\'') => {
                chars.next();
            }
            '\'' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth -= 1,
            ',' if !in_string && depth == 0 => {
                parts.push(values[start..index].trim());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(values[start..].trim());
    parts
}

fn trigger_matches_update_of(action: &SimpleTriggerAction, updated_columns: &[String]) -> bool {
    action.update_of_columns.is_empty()
        || action.update_of_columns.iter().any(|trigger_column| {
            updated_columns
                .iter()
                .any(|updated_column| trigger_column.eq_ignore_ascii_case(updated_column))
        })
}

#[derive(Debug)]
struct TxnState {
    next_txn_id: u64,
    active_txn: Option<TransactionId>,
    pending_writable: Option<WritableDatabase>,
    savepoints: Vec<Savepoint>,
}

#[derive(Debug, Clone)]
struct Savepoint {
    name: String,
    snapshot: WritableDatabase,
}

impl Default for FileStorage {
    fn default() -> Self {
        Self {
            path: None,
            pager: RefCell::new(None),
            catalog: RefCell::new(Catalog::default()),
            writable: RefCell::new(WritableDatabase::default()),
            txn_state: RefCell::new(TxnState {
                next_txn_id: 1,
                active_txn: None,
                pending_writable: None,
                savepoints: Vec::new(),
            }),
            ignore_check_constraints: RefCell::new(false),
            case_sensitive_like: RefCell::new(false),
        }
    }
}

impl FileStorage {
    fn without_rowid_synthetic_row_id(schema: &Schema, row: &Row) -> Result<RowId> {
        let primary_key = schema.primary_key_constraint.as_ref().ok_or_else(|| {
            DbError::storage(format!(
                "WITHOUT ROWID table {} is missing PRIMARY KEY metadata",
                schema.name
            ))
        })?;
        let key = primary_key
            .columns
            .iter()
            .map(|column| {
                let index = schema.column_index(column)?;
                row.get(index).cloned().ok_or_else(|| {
                    DbError::storage(format!(
                        "row for table {} is missing column {column}",
                        schema.name
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Self::hash_without_rowid_key(&key)
    }

    fn hash_without_rowid_key(key: &[Value]) -> Result<RowId> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let value = hasher.finish();
        if value == 0 {
            return Ok(RowId(1));
        }
        Ok(RowId(value))
    }

    fn without_rowid_primary_key_index_name(schema_name: &str) -> String {
        format!("sqlite_autoindex_{schema_name}_1")
    }

    fn is_without_rowid_primary_key_index(
        schema: &Schema,
        index: &IndexMeta,
        schema_name: &str,
    ) -> bool {
        schema.without_rowid
            && index.name == Self::without_rowid_primary_key_index_name(schema_name)
            && schema
                .primary_key_constraint
                .as_ref()
                .is_some_and(|primary_key| primary_key.columns == index.columns)
    }

    fn without_rowid_lookup_row_ids(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        key_prefix: &[Value],
        require_full_key: bool,
    ) -> Result<Vec<RowId>> {
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        let rows = if self.txn_state.borrow().pending_writable.is_some() {
            self.writable_view()
                .tables
                .get(schema_name)
                .map(|table| table.rows.clone())
                .unwrap_or_default()
        } else {
            let (_, root_page) = self.require_schema_and_root_page(schema_name)?;
            let pager = self.pager.borrow();
            let pager = pager.as_ref().ok_or_else(|| {
                DbError::storage("sqlite3 FileStorage is not backed by a database file")
            })?;
            scan_table_rows(pager, root_page, &schema)?
        };

        if let Some(primary_key) = &schema.primary_key_constraint {
            let expected = primary_key.columns.len();
            if require_full_key && key_prefix.len() != expected {
                return Err(DbError::storage(format!(
                    "index {} expected {} key values but got {}",
                    Self::without_rowid_primary_key_index_name(schema_name),
                    expected,
                    key_prefix.len()
                )));
            }
            if !require_full_key && key_prefix.len() > expected {
                return Err(DbError::storage(format!(
                    "index {} expected at most {} key values but got {}",
                    Self::without_rowid_primary_key_index_name(schema_name),
                    expected,
                    key_prefix.len()
                )));
            }
        }

        let primary_key_columns = schema
            .primary_key_constraint
            .as_ref()
            .ok_or_else(|| {
                DbError::storage(format!(
                    "WITHOUT ROWID table {schema_name} is missing PRIMARY KEY metadata"
                ))
            })?
            .columns
            .clone();

        let mut row_ids = Vec::new();
        for (_row_id, row) in rows {
            let key = primary_key_columns
                .iter()
                .map(|column| {
                    let index = schema.column_index(column)?;
                    row.get(index).cloned().ok_or_else(|| {
                        DbError::storage(format!(
                            "row for table {} is missing column {column}",
                            schema.name
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if key.starts_with(key_prefix) {
                row_ids.push(Self::without_rowid_synthetic_row_id(&schema, &row)?);
            }
        }
        Ok(row_ids)
    }

    fn without_rowid_scan_rows_internal(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        schema: &Schema,
    ) -> Result<Vec<(RowId, Row)>> {
        if self.txn_state.borrow().pending_writable.is_some() {
            let rows = self
                .writable_view()
                .tables
                .get(schema_name)
                .map(|table| table.rows.clone())
                .unwrap_or_default();
            return Self::materialize_without_rowid_rows(schema, rows);
        }

        let (_, root_page) = self.require_schema_and_root_page(schema_name)?;
        let pager = self.pager.borrow();
        let pager = pager.as_ref().ok_or_else(|| {
            DbError::storage("sqlite3 FileStorage is not backed by a database file")
        })?;
        let _ = transaction_id;
        let rows = scan_table_rows(pager, root_page, schema)?;
        Self::materialize_without_rowid_rows(schema, rows)
    }

    fn materialize_without_rowid_rows(
        schema: &Schema,
        rows: Vec<(RowId, Row)>,
    ) -> Result<Vec<(RowId, Row)>> {
        rows.into_iter()
            .map(|(_row_id, row)| Ok((Self::without_rowid_synthetic_row_id(schema, &row)?, row)))
            .collect()
    }

    fn without_rowid_row_position(table: &WritableTable, row_id: RowId) -> Result<Option<usize>> {
        for (index, (_stored_row_id, row)) in table.rows.iter().enumerate() {
            if Self::without_rowid_synthetic_row_id(&table.schema, row)? == row_id {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let (pager, catalog, writable) = match Pager::open(&path) {
            Ok(pager) => {
                validate_supported_text_encoding(pager.header().text_encoding)?;
                let catalog = load_catalog(&pager)?;
                let writable = Self::load_writable_database(&pager, &catalog)?;
                (Some(pager), catalog, writable)
            }
            Err(DbError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                (None, Catalog::default(), WritableDatabase::default())
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            path: Some(path),
            pager: RefCell::new(pager),
            catalog: RefCell::new(catalog),
            writable: RefCell::new(writable),
            txn_state: RefCell::new(TxnState {
                next_txn_id: 1,
                active_txn: None,
                pending_writable: None,
                savepoints: Vec::new(),
            }),
            ignore_check_constraints: RefCell::new(false),
            case_sensitive_like: RefCell::new(false),
        })
    }

    fn load_writable_database(pager: &Pager, catalog: &Catalog) -> Result<WritableDatabase> {
        let mut database = WritableDatabase::default();
        database.schema_version = pager.header().schema_version;
        database.user_version = pager.header().user_version;
        database.application_id = pager.header().application_id;
        for (table_name, schema) in catalog.schemas() {
            let rows = if schema.is_view() {
                Vec::new()
            } else {
                let root_page = catalog.table_root_page(table_name).ok_or_else(|| {
                    DbError::storage(format!(
                        "sqlite catalog is missing root page for table {table_name}",
                    ))
                })?;
                scan_table_rows(pager, root_page, schema)?
            };
            if schema.without_rowid {
                database.contains_without_rowid_tables = true;
            }
            database.tables.insert(
                table_name.clone(),
                WritableTable {
                    schema: schema.clone(),
                    rows,
                },
            );
        }
        for (table_name, indexes) in catalog.indexes() {
            database.indexes.insert(table_name.clone(), indexes.clone());
        }
        database.extra_schema_objects = catalog.extra_schema_objects().to_vec();
        if let Some(root_page) = catalog.sqlite_sequence_root_page() {
            database.sqlite_sequence_exists = true;
            let schema = Schema::new(
                "sqlite_sequence",
                vec![
                    ColumnDef::new("name", ColumnType::Text),
                    ColumnDef::new("seq", ColumnType::Integer),
                ],
            );
            let rows = scan_table_rows(pager, root_page, &schema)?;
            for (_, row) in rows {
                let [Value::Text(name), Value::Integer(seq)] = row.as_slice() else {
                    return Err(DbError::storage(
                        "sqlite_sequence row did not contain expected (name TEXT, seq INTEGER)",
                    ));
                };
                let seq = u64::try_from(*seq).map_err(|_| {
                    DbError::storage("sqlite_sequence seq must be a non-negative INTEGER")
                })?;
                database.sqlite_sequence.insert(name.clone(), seq);
            }
        }
        Ok(database)
    }

    fn validate_transaction(&self, transaction_id: TransactionId) -> Result<()> {
        match self.txn_state.borrow().active_txn {
            Some(active) if active == transaction_id => Ok(()),
            Some(active) => Err(DbError::txn(format!(
                "transaction {} is not active; current transaction is {}",
                transaction_id.0, active.0
            ))),
            None => Err(DbError::txn("no active transaction")),
        }
    }

    fn unsupported(&self, operation: &str) -> DbError {
        DbError::storage(format!(
            "sqlite3 FileStorage does not support {operation} in this phase"
        ))
    }

    fn require_schema_and_root_page(&self, schema_name: &str) -> Result<(Schema, u32)> {
        let catalog = self.catalog.borrow();
        let schema = catalog
            .schemas()
            .get(schema_name)
            .cloned()
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        let root_page = catalog.table_root_page(schema_name).ok_or_else(|| {
            DbError::storage(format!(
                "sqlite catalog is missing root page for table {schema_name}",
            ))
        })?;
        Ok((schema, root_page))
    }

    fn require_index_and_root_page(
        &self,
        schema_name: &str,
        index_name: &str,
    ) -> Result<(IndexMeta, u32)> {
        let catalog = self.catalog.borrow();
        let index = catalog
            .indexes()
            .get(schema_name)
            .and_then(|indexes| indexes.get(index_name))
            .cloned()
            .ok_or_else(|| {
                DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
            })?;
        let root_page = catalog
            .index_root_page(schema_name, index_name)
            .ok_or_else(|| {
                DbError::storage(format!(
                    "sqlite catalog is missing root page for index {index_name} on table {schema_name}",
                ))
            })?;
        Ok((index, root_page))
    }

    fn writable_view(&self) -> WritableDatabase {
        let txn_state = self.txn_state.borrow();
        txn_state
            .pending_writable
            .clone()
            .unwrap_or_else(|| self.writable.borrow().clone())
    }

    fn with_pending_writable_mut<T>(
        &self,
        transaction_id: TransactionId,
        f: impl FnOnce(&mut WritableDatabase) -> Result<T>,
    ) -> Result<T> {
        self.validate_transaction(transaction_id)?;
        let base = self.writable.borrow().clone();
        let mut txn_state = self.txn_state.borrow_mut();
        let pending = txn_state.pending_writable.get_or_insert(base);
        f(pending)
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
                    *self.case_sensitive_like.borrow(),
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
            *self.case_sensitive_like.borrow(),
        )
    }

    fn integer_primary_key_column_index(schema: &Schema) -> Option<usize> {
        if schema.without_rowid {
            return None;
        }
        let primary_key_columns = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.primary_key)
            .collect::<Vec<_>>();
        let [(index, column)] = primary_key_columns.as_slice() else {
            return None;
        };
        (matches!(column.column_type, ColumnType::Integer)
            && !matches!(
                column.primary_key_sort_order,
                Some(crate::common::types::SortOrder::Desc)
            ))
        .then_some(*index)
    }

    fn next_row_id_for_insert(table: &WritableTable, sqlite_sequence: Option<u64>) -> u64 {
        if table
            .schema
            .columns
            .iter()
            .any(|column| column.primary_key && column.autoincrement)
        {
            sqlite_sequence.unwrap_or(0).saturating_add(1)
        } else {
            table
                .rows
                .iter()
                .map(|(row_id, _)| row_id.0)
                .max()
                .unwrap_or(0)
                .saturating_add(1)
        }
    }

    fn insert_trigger_row(
        &self,
        database: &mut WritableDatabase,
        schema_name: &str,
        row: Row,
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<RowId> {
        let table = database
            .tables
            .get_mut(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;

        let mut row = row;
        let row_id_column_index = Self::integer_primary_key_column_index(&table.schema);
        let row_id = if let Some(index) = row_id_column_index {
            match row.get(index) {
                Some(Value::Integer(value)) => RowId(u64::try_from(*value).map_err(|_| {
                    DbError::storage("sqlite rowid must be a non-negative INTEGER")
                })?),
                Some(Value::Null) => {
                    let next = Self::next_row_id_for_insert(
                        table,
                        database.sqlite_sequence.get(schema_name).copied(),
                    );
                    let row_id = RowId(next);
                    row[index] = Value::Integer(
                        i64::try_from(row_id.0)
                            .map_err(|_| DbError::storage("sqlite rowid does not fit in i64"))?,
                    );
                    row_id
                }
                Some(value) => {
                    return Err(DbError::storage(format!(
                        "sqlite rowid column must be INTEGER, got {}",
                        value.type_name()
                    )));
                }
                None => return Err(DbError::storage("sqlite row is missing rowid column")),
            }
        } else {
            let next = table
                .rows
                .iter()
                .map(|(row_id, _)| row_id.0)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
            RowId(next)
        };

        table.schema.validate_row_values(&row)?;
        if !ignore_check_constraints {
            table
                .schema
                .validate_check_constraints_with_like_mode(&row, case_sensitive_like)?;
        }
        let existing_rows = table.rows.iter().map(|(_, row)| row).collect::<Vec<_>>();
        table
            .schema
            .validate_primary_key_uniqueness(&row, &existing_rows)?;
        self.validate_unique_indexes_for_row(
            &table.schema,
            database.indexes.get(schema_name),
            &table.rows,
            &row,
        )?;

        table.rows.push((row_id, row));
        table.rows.sort_by_key(|(row_id, _)| row_id.0);
        if table
            .schema
            .columns
            .iter()
            .any(|column| column.primary_key && column.autoincrement)
        {
            let entry = database
                .sqlite_sequence
                .entry(schema_name.to_string())
                .or_insert(0);
            *entry = (*entry).max(row_id.0);
        }
        Ok(row_id)
    }

    fn delete_trigger_rows(
        database: &mut WritableDatabase,
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        action: &SimpleTriggerDeleteAction,
    ) -> Result<()> {
        let table = database
            .tables
            .get_mut(&action.target_table)
            .ok_or_else(|| DbError::storage(format!("unknown table: {}", action.target_table)))?;
        if action.where_groups.is_empty() {
            table.rows.clear();
            return Ok(());
        }
        let mut positions = Vec::new();
        for (position, (_, row)) in table.rows.iter().enumerate() {
            if Self::trigger_where_groups_match(
                &table.schema,
                row,
                source_schema,
                old_row,
                new_row,
                &action.where_groups,
            )? {
                positions.push(position);
            }
        }
        for position in positions.into_iter().rev() {
            table.rows.remove(position);
        }
        Ok(())
    }

    fn update_trigger_rows(
        &self,
        database: &mut WritableDatabase,
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        action: &SimpleTriggerUpdateAction,
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<()> {
        let table = database
            .tables
            .get_mut(&action.target_table)
            .ok_or_else(|| DbError::storage(format!("unknown table: {}", action.target_table)))?;
        let mut assignment_indexes = Vec::new();
        for (assignment_column, _) in &action.assignments {
            let assignment_index = table.schema.column_index(assignment_column)?;
            if table.schema.columns[assignment_index]
                .generated_expr
                .is_some()
            {
                return Err(DbError::storage(format!(
                    "cannot UPDATE generated column {assignment_column}"
                )));
            }
            assignment_indexes.push(assignment_index);
        }
        let mut updates = Vec::new();
        for (position, (_, row)) in table.rows.iter().enumerate() {
            let matches_where = action.where_groups.is_empty()
                || Self::trigger_where_groups_match(
                    &table.schema,
                    row,
                    source_schema,
                    old_row,
                    new_row,
                    &action.where_groups,
                )?;
            if matches_where {
                let evaluated = action
                    .assignments
                    .iter()
                    .map(|(_, expr)| {
                        Self::evaluate_trigger_target_expr(
                            &table.schema,
                            row,
                            source_schema,
                            old_row,
                            new_row,
                            expr,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut updated = row.clone();
                for (assignment_index, value) in assignment_indexes.iter().zip(evaluated) {
                    updated[*assignment_index] = value;
                }
                updates.push((position, updated));
            }
        }
        for (position, updated) in updates {
            table.schema.validate_row_values(&updated)?;
            if !ignore_check_constraints {
                table
                    .schema
                    .validate_check_constraints_with_like_mode(&updated, case_sensitive_like)?;
            }
            let existing_rows = table
                .rows
                .iter()
                .enumerate()
                .filter_map(|(candidate_position, (_, row))| {
                    (candidate_position != position).then_some(row)
                })
                .collect::<Vec<_>>();
            table
                .schema
                .validate_primary_key_uniqueness(&updated, &existing_rows)?;
            self.validate_unique_indexes_for_row(
                &table.schema,
                database.indexes.get(&action.target_table),
                &table.rows,
                &updated,
            )?;
            table.rows[position].1 = updated;
        }
        Ok(())
    }

    fn trigger_where_groups_match(
        target_schema: &Schema,
        target_row: &Row,
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        where_groups: &[Vec<SimpleTriggerWhere>],
    ) -> Result<bool> {
        for group in where_groups {
            let mut group_matches = true;
            for clause in group {
                if !Self::trigger_where_matches(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    clause,
                )? {
                    group_matches = false;
                    break;
                }
            }
            if group_matches {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn trigger_where_matches(
        target_schema: &Schema,
        target_row: &Row,
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        clause: &SimpleTriggerWhere,
    ) -> Result<bool> {
        let column_value = Self::evaluate_trigger_target_expr(
            target_schema,
            target_row,
            source_schema,
            old_row,
            new_row,
            &clause.left,
        )?;
        match &clause.expr {
            SimpleTriggerWhereExpr::IsNull => Ok(matches!(column_value, Value::Null)),
            SimpleTriggerWhereExpr::IsNotNull => Ok(!matches!(column_value, Value::Null)),
            SimpleTriggerWhereExpr::Is { value, negated } => {
                let value = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    value,
                )?;
                Ok(Self::is_trigger_is_match(&column_value, &value) ^ *negated)
            }
            SimpleTriggerWhereExpr::Between { low, high, negated } => {
                if matches!(column_value, Value::Null) {
                    return Ok(false);
                }
                let low = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    low,
                )?;
                let high = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    high,
                )?;
                if matches!(low, Value::Null) || matches!(high, Value::Null) {
                    return Ok(false);
                }
                let lower_match = Self::compare_with_operator(&column_value, CompareOp::Gte, &low)?;
                let upper_match =
                    Self::compare_with_operator(&column_value, CompareOp::Lte, &high)?;
                Ok((lower_match && upper_match) ^ *negated)
            }
            SimpleTriggerWhereExpr::InList { values, negated } => {
                let values = values
                    .iter()
                    .map(|value| {
                        Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            value,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Self::trigger_in_membership(&column_value, &values)
                    .map(|matches| matches ^ *negated)
                    .unwrap_or(false))
            }
            SimpleTriggerWhereExpr::Like {
                pattern,
                escape,
                negated,
            } => {
                if matches!(column_value, Value::Null) {
                    return Ok(false);
                }
                let pattern = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    pattern,
                )?;
                if matches!(pattern, Value::Null) {
                    return Ok(false);
                }
                let escape_value = escape
                    .as_ref()
                    .map(|escape| {
                        Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            escape,
                        )
                    })
                    .transpose()?;
                let escape_char = match escape_value {
                    Some(Value::Null) => return Ok(false),
                    Some(escape) => Some(Self::trigger_escape_char(&escape)?),
                    None => None,
                };
                Ok(Self::trigger_like_matches(
                    &Self::trigger_value_to_text(&column_value),
                    &Self::trigger_value_to_text(&pattern),
                    escape_char,
                ) ^ *negated)
            }
            SimpleTriggerWhereExpr::Glob { pattern, negated } => {
                if matches!(column_value, Value::Null) {
                    return Ok(false);
                }
                let pattern = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    pattern,
                )?;
                if matches!(pattern, Value::Null) {
                    return Ok(false);
                }
                Ok(Self::trigger_glob_matches(
                    &Self::trigger_value_to_text(&column_value),
                    &Self::trigger_value_to_text(&pattern),
                ) ^ *negated)
            }
            SimpleTriggerWhereExpr::Compare { op, value } => {
                let value = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    value,
                )?;
                if matches!(column_value, Value::Null) || matches!(value, Value::Null) {
                    Ok(false)
                } else {
                    Self::compare_with_operator(&column_value, *op, &value)
                }
            }
        }
    }

    fn trigger_in_membership(left: &Value, values: &[Value]) -> Option<bool> {
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
            if Self::compare_values(left, value).ok().flatten() == Some(Ordering::Equal) {
                return Some(true);
            }
        }
        if saw_null { None } else { Some(false) }
    }

    fn trigger_like_matches(value: &str, pattern: &str, escape: Option<char>) -> bool {
        fn chars_equal(left: char, right: char) -> bool {
            if left.is_ascii() && right.is_ascii() {
                left.eq_ignore_ascii_case(&right)
            } else {
                left == right
            }
        }

        fn inner(value: &[char], pattern: &[char], escape: Option<char>) -> bool {
            if pattern.is_empty() {
                return value.is_empty();
            }
            if escape.is_some_and(|escape| pattern[0] == escape) {
                return pattern.len() > 1
                    && !value.is_empty()
                    && chars_equal(value[0], pattern[1])
                    && inner(value.get(1..).unwrap_or_default(), &pattern[2..], escape);
            }
            match pattern[0] {
                '%' => {
                    inner(value, &pattern[1..], escape)
                        || (!value.is_empty() && inner(&value[1..], pattern, escape))
                }
                '_' => !value.is_empty() && inner(&value[1..], &pattern[1..], escape),
                literal => {
                    !value.is_empty()
                        && chars_equal(value[0], literal)
                        && inner(&value[1..], &pattern[1..], escape)
                }
            }
        }

        let value = value.chars().collect::<Vec<_>>();
        let pattern = pattern.chars().collect::<Vec<_>>();
        inner(&value, &pattern, escape)
    }

    fn trigger_glob_matches(value: &str, pattern: &str) -> bool {
        fn inner(value: &[char], pattern: &[char]) -> bool {
            if pattern.is_empty() {
                return value.is_empty();
            }
            match pattern[0] {
                '*' => {
                    inner(value, &pattern[1..])
                        || (!value.is_empty() && inner(&value[1..], pattern))
                }
                '?' => !value.is_empty() && inner(&value[1..], &pattern[1..]),
                '[' => {
                    let Some(end) = pattern.iter().position(|ch| *ch == ']') else {
                        return !value.is_empty()
                            && value[0] == '['
                            && inner(&value[1..], &pattern[1..]);
                    };
                    if value.is_empty() {
                        return false;
                    }
                    let class = &pattern[1..end];
                    let negated = class.first().is_some_and(|ch| *ch == '^');
                    let class = if negated { &class[1..] } else { class };
                    let mut contains = false;
                    let mut class_index = 0;
                    while class_index < class.len() {
                        if class_index + 2 < class.len() && class[class_index + 1] == '-' {
                            let start = class[class_index];
                            let end = class[class_index + 2];
                            if start <= value[0] && value[0] <= end {
                                contains = true;
                                break;
                            }
                            class_index += 3;
                        } else {
                            if class[class_index] == value[0] {
                                contains = true;
                                break;
                            }
                            class_index += 1;
                        }
                    }
                    (contains ^ negated) && inner(&value[1..], &pattern[end + 1..])
                }
                literal => {
                    !value.is_empty() && value[0] == literal && inner(&value[1..], &pattern[1..])
                }
            }
        }

        let value = value.chars().collect::<Vec<_>>();
        let pattern = pattern.chars().collect::<Vec<_>>();
        inner(&value, &pattern)
    }

    fn evaluate_trigger_target_expr(
        target_schema: &Schema,
        target_row: &Row,
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        expr: &ScalarExpr,
    ) -> Result<Value> {
        match expr {
            ScalarExpr::Literal(value) => Ok(value.clone()),
            ScalarExpr::Column(name) if name.starts_with("old.") || name.starts_with("new.") => {
                Self::evaluate_trigger_column_expr(source_schema, old_row, new_row, expr)
            }
            ScalarExpr::Column(name) => {
                let index = target_schema.column_index(name)?;
                target_row
                    .get(index)
                    .cloned()
                    .ok_or_else(|| DbError::storage(format!("row is missing column {name}")))
            }
            ScalarExpr::UnaryPlus(expr) | ScalarExpr::Collate { expr, .. } => {
                Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    expr,
                )
            }
            ScalarExpr::Cast { expr, ty } => {
                let value = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    expr,
                )?;
                trigger_cast_value(value, *ty)
            }
            ScalarExpr::UnaryMinus(expr) => {
                let value = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    expr,
                )?;
                match value {
                    Value::Null => Ok(Value::Null),
                    Value::Integer(value) => value
                        .checked_neg()
                        .map(Value::Integer)
                        .ok_or_else(|| DbError::storage("integer overflow")),
                    Value::Real(value) => Ok(Value::Real(-value)),
                    value => Ok(Value::Real(-trigger_value_to_f64(&value)?)),
                }
            }
            ScalarExpr::Not(expr) => {
                let value = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    expr,
                )?;
                Ok(match value {
                    Value::Null => Value::Null,
                    value => Value::Integer(if Self::sqlite_truthy(&value) { 0 } else { 1 }),
                })
            }
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                Ok(Value::Integer(
                    if Self::is_trigger_is_match(&left, &right) ^ *negated {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => {
                let evaluated = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    expr,
                )?;
                let matches = !matches!(evaluated, Value::Null)
                    && trigger_is_true_value(&evaluated) == *value;
                Ok(Value::Integer(if matches ^ *negated { 1 } else { 0 }))
            }
            ScalarExpr::Compare { left, op, right } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                if matches!(left, Value::Null) || matches!(right, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(Value::Integer(
                    if Self::compare_with_operator(&left, *op, &right)? {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let value = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    expr,
                )?;
                let low = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    low,
                )?;
                let high = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    high,
                )?;
                if matches!(value, Value::Null)
                    || matches!(low, Value::Null)
                    || matches!(high, Value::Null)
                {
                    return Ok(Value::Null);
                }
                let lower_match = Self::compare_with_operator(&value, CompareOp::Gte, &low)?;
                let upper_match = Self::compare_with_operator(&value, CompareOp::Lte, &high)?;
                Ok(Value::Integer(if (lower_match && upper_match) ^ *negated {
                    1
                } else {
                    0
                }))
            }
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    expr,
                )?;
                let values = values
                    .iter()
                    .map(|value| {
                        Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            value,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Self::trigger_in_membership(&left, &values)
                    .map(|matches| Value::Integer(if matches ^ *negated { 1 } else { 0 }))
                    .unwrap_or(Value::Null))
            }
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => {
                let value = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    expr,
                )?;
                let pattern = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    pattern,
                )?;
                if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                let escape_value = escape
                    .as_ref()
                    .map(|escape| {
                        Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            escape,
                        )
                    })
                    .transpose()?;
                let escape_char = match escape_value {
                    Some(Value::Null) => return Ok(Value::Null),
                    Some(escape) => Some(Self::trigger_escape_char(&escape)?),
                    None => None,
                };
                Ok(Value::Integer(
                    if Self::trigger_like_matches(
                        &Self::trigger_value_to_text(&value),
                        &Self::trigger_value_to_text(&pattern),
                        escape_char,
                    ) ^ *negated
                    {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => {
                let value = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    expr,
                )?;
                let pattern = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    pattern,
                )?;
                if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(Value::Integer(
                    if Self::trigger_glob_matches(
                        &Self::trigger_value_to_text(&value),
                        &Self::trigger_value_to_text(&pattern),
                    ) ^ *negated
                    {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                let base = base
                    .as_ref()
                    .map(|base| {
                        Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            base,
                        )
                    })
                    .transpose()?;
                for (when_expr, then_expr) in when_then_clauses {
                    let when_value = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        when_expr,
                    )?;
                    let matches = if let Some(base) = &base {
                        Self::is_trigger_is_match(base, &when_value)
                    } else {
                        !matches!(when_value, Value::Null) && Self::sqlite_truthy(&when_value)
                    };
                    if matches {
                        return Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            then_expr,
                        );
                    }
                }
                else_expr
                    .as_ref()
                    .map(|else_expr| {
                        Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            else_expr,
                        )
                    })
                    .unwrap_or(Ok(Value::Null))
            }
            ScalarExpr::BitNot(expr) => {
                let value = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    expr,
                )?;
                match value {
                    Value::Integer(value) => Ok(Value::Integer(!value)),
                    Value::Null => Ok(Value::Null),
                    value => Err(DbError::storage(format!(
                        "cannot bitwise-not {} in trigger UPDATE",
                        value.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Add,
                right,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(
                        left.checked_add(right)
                            .ok_or_else(|| DbError::storage("integer overflow"))?,
                    )),
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot add {} and {} in trigger UPDATE",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Subtract,
                right,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(
                        left.checked_sub(right)
                            .ok_or_else(|| DbError::storage("integer overflow"))?,
                    )),
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot subtract {} and {} in trigger UPDATE",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Multiply,
                right,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(
                        left.checked_mul(right)
                            .ok_or_else(|| DbError::storage("integer overflow"))?,
                    )),
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot multiply {} and {} in trigger UPDATE",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Divide,
                right,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                match (left, right) {
                    (Value::Integer(_), Value::Integer(0)) => Ok(Value::Null),
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left / right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot divide {} and {} in trigger UPDATE",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Modulo,
                right,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                match (left, right) {
                    (Value::Integer(_), Value::Integer(0)) => Ok(Value::Null),
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left % right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot modulo {} and {} in trigger UPDATE",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::BitAnd,
                right,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left & right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot bitwise-and {} and {} in trigger UPDATE",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::BitOr,
                right,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left | right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot bitwise-or {} and {} in trigger UPDATE",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::ShiftLeft,
                right,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(Self::trigger_shift_op(left, right, true)))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot shift {} and {} in trigger UPDATE",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::ShiftRight,
                right,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(Self::trigger_shift_op(left, right, false)))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot shift {} and {} in trigger UPDATE",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Concat,
                right,
            } => {
                let left = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    left,
                )?;
                let right = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    right,
                )?;
                if matches!(left, Value::Null) || matches!(right, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(Value::Text(format!(
                    "{}{}",
                    Self::trigger_value_to_text(&left),
                    Self::trigger_value_to_text(&right)
                )))
            }
            ScalarExpr::Function { func, args }
                if matches!(
                    func,
                    ScalarFunc::Lower
                        | ScalarFunc::Upper
                        | ScalarFunc::Abs
                        | ScalarFunc::Coalesce
                        | ScalarFunc::IfNull
                        | ScalarFunc::NullIf
                        | ScalarFunc::Length
                        | ScalarFunc::Substr
                        | ScalarFunc::Trim
                        | ScalarFunc::LTrim
                        | ScalarFunc::RTrim
                        | ScalarFunc::Replace
                        | ScalarFunc::Instr
                        | ScalarFunc::Round
                        | ScalarFunc::TypeOf
                        | ScalarFunc::Quote
                        | ScalarFunc::Unicode
                        | ScalarFunc::Char
                        | ScalarFunc::Hex
                        | ScalarFunc::ZeroBlob
                ) =>
            {
                if matches!(func, ScalarFunc::ZeroBlob) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "ZEROBLOB trigger expressions require one argument",
                        ));
                    }
                    let length = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    let length = if matches!(length, Value::Null) {
                        0
                    } else {
                        trigger_value_to_i64(&length)?.max(0)
                    };
                    let length = usize::try_from(length)
                        .map_err(|_| DbError::storage("ZEROBLOB length is too large"))?;
                    return Ok(Value::Blob(vec![0; length]));
                }
                if matches!(func, ScalarFunc::Hex) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "HEX trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    return Ok(Value::Text(match value {
                        Value::Null => String::new(),
                        Value::Blob(value) => trigger_hex_bytes(&value),
                        value => trigger_hex_bytes(Self::trigger_value_to_text(&value).as_bytes()),
                    }));
                }
                if matches!(func, ScalarFunc::Char) {
                    let mut result = String::new();
                    for arg in args {
                        let value = Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            arg,
                        )?;
                        if matches!(value, Value::Null) {
                            continue;
                        }
                        let code_point = trigger_value_to_i64(&value)?;
                        let ch = u32::try_from(code_point)
                            .ok()
                            .and_then(char::from_u32)
                            .unwrap_or(char::REPLACEMENT_CHARACTER);
                        result.push(ch);
                    }
                    return Ok(Value::Text(result));
                }
                if matches!(func, ScalarFunc::Unicode) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "UNICODE trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    if matches!(value, Value::Null) {
                        return Ok(Value::Null);
                    }
                    return Ok(Self::trigger_value_to_text(&value)
                        .chars()
                        .next()
                        .map(|ch| Value::Integer(i64::from(u32::from(ch))))
                        .unwrap_or(Value::Null));
                }
                if matches!(func, ScalarFunc::Quote) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "QUOTE trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    return Ok(Value::Text(trigger_quote_value(&value)));
                }
                if matches!(func, ScalarFunc::TypeOf) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "TYPEOF trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    return Ok(Value::Text(
                        match value {
                            Value::Null => "null",
                            Value::Boolean(_) | Value::Integer(_) => "integer",
                            Value::Real(_) => "real",
                            Value::Blob(_) => "blob",
                            Value::Text(_) => "text",
                        }
                        .to_string(),
                    ));
                }
                if matches!(func, ScalarFunc::Round) {
                    if !matches!(args.len(), 1 | 2) {
                        return Err(DbError::storage(
                            "ROUND trigger expressions require one or two arguments",
                        ));
                    }
                    let value = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    if matches!(value, Value::Null) {
                        return Ok(Value::Null);
                    }
                    let value = trigger_value_to_f64(&value)?;
                    let precision = if args.len() == 2 {
                        let precision = Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            &args[1],
                        )?;
                        if matches!(precision, Value::Null) {
                            return Ok(Value::Null);
                        }
                        i32::try_from(trigger_value_to_i64(&precision)?)
                            .map_err(|_| DbError::storage("ROUND precision does not fit in i32"))?
                    } else {
                        0
                    };
                    return Ok(Value::Real(sqlite_round_f64(value, precision)));
                }
                if matches!(func, ScalarFunc::Instr) {
                    if args.len() != 2 {
                        return Err(DbError::storage(
                            "INSTR trigger expressions require two arguments",
                        ));
                    }
                    let haystack = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    let needle = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[1],
                    )?;
                    if matches!(haystack, Value::Null) || matches!(needle, Value::Null) {
                        return Ok(Value::Null);
                    }
                    return Ok(match (&haystack, &needle) {
                        (Value::Blob(haystack), Value::Blob(needle)) => {
                            Value::Integer(trigger_instr_blob(haystack, needle))
                        }
                        _ => {
                            let haystack = Self::trigger_value_to_text(&haystack);
                            let needle = Self::trigger_value_to_text(&needle);
                            if needle.is_empty() {
                                Value::Integer(1)
                            } else {
                                Value::Integer(
                                    haystack
                                        .find(&needle)
                                        .map(|byte_index| {
                                            haystack[..byte_index].chars().count() as i64 + 1
                                        })
                                        .unwrap_or(0),
                                )
                            }
                        }
                    });
                }
                if matches!(func, ScalarFunc::Replace) {
                    if args.len() != 3 {
                        return Err(DbError::storage(
                            "REPLACE trigger expressions require three arguments",
                        ));
                    }
                    let value = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    let pattern = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[1],
                    )?;
                    let replacement = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[2],
                    )?;
                    if matches!(value, Value::Null)
                        || matches!(pattern, Value::Null)
                        || matches!(replacement, Value::Null)
                    {
                        return Ok(Value::Null);
                    }
                    let value = Self::trigger_value_to_text(&value);
                    let pattern = Self::trigger_value_to_text(&pattern);
                    if pattern.is_empty() {
                        return Ok(Value::Text(value));
                    }
                    return Ok(Value::Text(
                        value.replace(&pattern, &Self::trigger_value_to_text(&replacement)),
                    ));
                }
                if matches!(
                    func,
                    ScalarFunc::Trim | ScalarFunc::LTrim | ScalarFunc::RTrim
                ) {
                    if !matches!(args.len(), 1 | 2) {
                        return Err(DbError::storage(
                            "TRIM trigger expressions require one or two arguments",
                        ));
                    }
                    let value = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    if matches!(value, Value::Null) {
                        return Ok(Value::Null);
                    }
                    let characters = if args.len() == 2 {
                        let characters = Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            &args[1],
                        )?;
                        if matches!(characters, Value::Null) {
                            return Ok(Value::Null);
                        }
                        Self::trigger_value_to_text(&characters)
                    } else {
                        " ".to_string()
                    };
                    let value = Self::trigger_value_to_text(&value);
                    return Ok(Value::Text(match func {
                        ScalarFunc::LTrim => value
                            .trim_start_matches(|ch| characters.contains(ch))
                            .to_string(),
                        ScalarFunc::RTrim => value
                            .trim_end_matches(|ch| characters.contains(ch))
                            .to_string(),
                        _ => value.trim_matches(|ch| characters.contains(ch)).to_string(),
                    }));
                }
                if matches!(func, ScalarFunc::Substr) {
                    if !matches!(args.len(), 2 | 3) {
                        return Err(DbError::storage(
                            "SUBSTR trigger expressions require two or three arguments",
                        ));
                    }
                    let value = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    if matches!(value, Value::Null) {
                        return Ok(Value::Null);
                    }
                    let start = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[1],
                    )?;
                    let start = trigger_value_to_i64(&start)?;
                    let length = if args.len() == 3 {
                        let length = Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            &args[2],
                        )?;
                        Some(trigger_value_to_i64(&length)?)
                    } else {
                        None
                    };
                    return Ok(match value {
                        Value::Blob(value) => {
                            Value::Blob(trigger_substr_blob(&value, start, length))
                        }
                        value => Value::Text(trigger_substr_text(
                            &Self::trigger_value_to_text(&value),
                            start,
                            length,
                        )),
                    });
                }
                if matches!(func, ScalarFunc::Length) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "LENGTH trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    return Ok(match value {
                        Value::Null => Value::Null,
                        Value::Blob(value) => Value::Integer(value.len() as i64),
                        value => Value::Integer(
                            Self::trigger_value_to_text(&value).chars().count() as i64,
                        ),
                    });
                }
                if matches!(func, ScalarFunc::NullIf) {
                    if args.len() != 2 {
                        return Err(DbError::storage(
                            "NULLIF trigger expressions require two arguments",
                        ));
                    }
                    let left = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    let right = Self::evaluate_trigger_target_expr(
                        target_schema,
                        target_row,
                        source_schema,
                        old_row,
                        new_row,
                        &args[1],
                    )?;
                    if Self::compare_values(&left, &right).ok().flatten() == Some(Ordering::Equal) {
                        return Ok(Value::Null);
                    }
                    return Ok(left);
                }
                if matches!(func, ScalarFunc::Coalesce | ScalarFunc::IfNull) {
                    if matches!(func, ScalarFunc::Coalesce) && args.len() < 2 {
                        return Err(DbError::storage(
                            "COALESCE trigger expressions require at least two arguments",
                        ));
                    }
                    if matches!(func, ScalarFunc::IfNull) && args.len() != 2 {
                        return Err(DbError::storage(
                            "IFNULL trigger expressions require two arguments",
                        ));
                    }
                    for arg in args {
                        let value = Self::evaluate_trigger_target_expr(
                            target_schema,
                            target_row,
                            source_schema,
                            old_row,
                            new_row,
                            arg,
                        )?;
                        if !matches!(value, Value::Null) {
                            return Ok(value);
                        }
                    }
                    return Ok(Value::Null);
                }
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "LOWER/UPPER/ABS trigger expressions require one argument",
                    ));
                }
                let value = Self::evaluate_trigger_target_expr(
                    target_schema,
                    target_row,
                    source_schema,
                    old_row,
                    new_row,
                    &args[0],
                )?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                if matches!(func, ScalarFunc::Abs) {
                    return trigger_abs_value(&value);
                }
                let text = Self::trigger_value_to_text(&value);
                Ok(Value::Text(if matches!(func, ScalarFunc::Upper) {
                    sqlite_ascii_upper(&text)
                } else {
                    sqlite_ascii_lower(&text)
                }))
            }
            _ => Err(DbError::storage(
                "only simple trigger UPDATE assignment expressions are supported",
            )),
        }
    }

    fn trigger_value_to_text(value: &Value) -> String {
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
            Value::Text(value) => value.clone(),
            Value::Blob(value) => String::from_utf8_lossy(value).to_string(),
        }
    }

    fn trigger_escape_char(value: &Value) -> Result<char> {
        let text = Self::trigger_value_to_text(value);
        let mut chars = text.chars();
        let Some(escape) = chars.next() else {
            return Err(DbError::storage(
                "LIKE ESCAPE expression must be a single character",
            ));
        };
        if chars.next().is_some() {
            return Err(DbError::storage(
                "LIKE ESCAPE expression must be a single character",
            ));
        }
        Ok(escape)
    }

    fn trigger_shift_op(left: i64, right: i64, left_shift: bool) -> i64 {
        let shift_left = if right < 0 { !left_shift } else { left_shift };
        let amount = right.unsigned_abs();
        if amount >= 64 {
            if shift_left || left >= 0 { 0 } else { -1 }
        } else {
            let amount = u32::try_from(amount).expect("shift amount < 64 fits in u32");
            if shift_left {
                left.wrapping_shl(amount)
            } else {
                left.wrapping_shr(amount)
            }
        }
    }

    fn build_trigger_insert_row(
        database: &WritableDatabase,
        action: &SimpleTriggerInsertAction,
        values: Row,
    ) -> Result<Row> {
        let Some(columns) = action.target_columns.as_deref() else {
            return Ok(values);
        };
        if columns.is_empty() {
            return Err(DbError::storage("insert column list cannot be empty"));
        }
        if columns.len() != values.len() {
            return Err(DbError::storage(format!(
                "insert into {} specified {} columns but got {} values",
                action.target_table,
                columns.len(),
                values.len()
            )));
        }
        let table = database
            .tables
            .get(&action.target_table)
            .ok_or_else(|| DbError::storage(format!("unknown table: {}", action.target_table)))?;
        let mut row = table
            .schema
            .columns
            .iter()
            .map(|column| {
                column
                    .default_value
                    .as_ref()
                    .map_or(Ok(Value::Null), |default| default.evaluate())
            })
            .collect::<Result<Vec<_>>>()?;
        let mut seen = BTreeSet::new();
        for (column, value) in columns.iter().zip(values.into_iter()) {
            if !seen.insert(column.clone()) {
                return Err(DbError::storage(format!(
                    "duplicate insert column: {column}"
                )));
            }
            let position = table
                .schema
                .columns
                .iter()
                .position(|entry| entry.name == *column)
                .ok_or_else(|| {
                    DbError::storage(format!(
                        "unknown column {column} on table {}",
                        table.schema.name
                    ))
                })?;
            if table.schema.columns[position].generated_expr.is_some() {
                return Err(DbError::storage(format!(
                    "cannot INSERT into generated column {column}"
                )));
            }
            row[position] = value;
        }
        Ok(row)
    }

    fn execute_simple_trigger_insert(
        &self,
        database: &mut WritableDatabase,
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        insert: &SimpleTriggerInsertAction,
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<()> {
        if let Some(select_from) = &insert.select_from {
            let select_table = database
                .tables
                .get(&select_from.table)
                .ok_or_else(|| DbError::storage(format!("unknown table: {}", select_from.table)))?;
            let select_schema = select_table.schema.clone();
            let select_rows = select_table
                .rows
                .iter()
                .map(|(row_id, row)| (*row_id, row.clone()))
                .collect::<Vec<_>>();
            let mut ordered_rows = Vec::new();
            for (select_row_id, select_row) in select_rows {
                let context = SimpleTriggerEvalContext {
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name: Some(&select_from.table),
                    select_alias: select_from.alias.as_deref(),
                    select_schema: Some(&select_schema),
                    select_row_id: Some(select_row_id),
                    select_row: Some(&select_row),
                };
                if !Self::simple_trigger_select_where_groups_match(&context, &insert.where_groups)?
                {
                    continue;
                }
                let order_key = if insert.group_by.is_empty() {
                    insert
                        .order_by
                        .iter()
                        .map(|order_by| {
                            Self::evaluate_simple_trigger_expr_with_context(
                                &context,
                                &order_by.expr,
                            )
                        })
                        .collect::<Result<Vec<_>>>()?
                } else {
                    Vec::new()
                };
                ordered_rows.push((select_row_id, select_row, order_key));
            }
            let aggregate_rows = ordered_rows.clone();
            if let Some(SimpleTriggerAggregate::Error(message)) = &insert.aggregate {
                return Err(DbError::storage(message.clone()));
            }
            if !insert.order_by.is_empty() {
                Self::sort_simple_trigger_rows_by_order_keys(&mut ordered_rows, &insert.order_by);
            }
            let limit = if let Some(limit_expr) = &insert.limit {
                let context = SimpleTriggerEvalContext {
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name: None,
                    select_alias: None,
                    select_schema: None,
                    select_row_id: None,
                    select_row: None,
                };
                let limit = Self::evaluate_simple_trigger_expr_with_context(&context, limit_expr)?;
                let limit = trigger_value_to_i64(&limit)?;
                if limit < 0 {
                    None
                } else {
                    Some(
                        usize::try_from(limit)
                            .map_err(|_| DbError::storage("trigger SELECT LIMIT is too large"))?,
                    )
                }
            } else {
                None
            };
            let offset = if let Some(offset_expr) = &insert.offset {
                let context = SimpleTriggerEvalContext {
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name: None,
                    select_alias: None,
                    select_schema: None,
                    select_row_id: None,
                    select_row: None,
                };
                let offset =
                    Self::evaluate_simple_trigger_expr_with_context(&context, offset_expr)?;
                usize::try_from(trigger_value_to_i64(&offset)?.max(0))
                    .map_err(|_| DbError::storage("trigger SELECT OFFSET is too large"))?
            } else {
                0
            };
            if !insert.group_by.is_empty() {
                let grouped_rows = Self::build_simple_trigger_group_rows(
                    source_schema,
                    old_row,
                    new_row,
                    &select_from.table,
                    select_from.alias.as_deref(),
                    &select_schema,
                    &aggregate_rows,
                    insert,
                )?;
                for (_, values, _) in grouped_rows
                    .iter()
                    .skip(offset)
                    .take(limit.unwrap_or(usize::MAX))
                {
                    let row = Self::build_trigger_insert_row(database, insert, values.clone())?;
                    self.insert_trigger_row(
                        database,
                        &insert.target_table,
                        row,
                        ignore_check_constraints,
                        case_sensitive_like,
                    )?;
                }
                return Ok(());
            }
            if let Some(aggregate) = &insert.aggregate {
                if offset == 0 && limit != Some(0) {
                    let aggregate_rows = if let Some(filter) = &insert.aggregate_filter {
                        aggregate_rows
                            .iter()
                            .filter_map(|(row_id, row, order_key)| {
                                let context = SimpleTriggerEvalContext {
                                    source_schema,
                                    old_row,
                                    new_row,
                                    select_table_name: Some(&select_from.table),
                                    select_alias: select_from.alias.as_deref(),
                                    select_schema: Some(&select_schema),
                                    select_row_id: Some(*row_id),
                                    select_row: Some(row),
                                };
                                match Self::evaluate_simple_trigger_expr_with_context(
                                    &context, filter,
                                ) {
                                    Ok(value)
                                        if !matches!(value, Value::Null)
                                            && Self::sqlite_truthy(&value) =>
                                    {
                                        Some(Ok((*row_id, row.clone(), order_key.clone())))
                                    }
                                    Ok(_) => None,
                                    Err(error) => Some(Err(error)),
                                }
                            })
                            .collect::<Result<Vec<_>>>()?
                    } else {
                        aggregate_rows
                    };
                    let value = Self::evaluate_simple_trigger_aggregate(
                        source_schema,
                        old_row,
                        new_row,
                        &select_from.table,
                        select_from.alias.as_deref(),
                        &select_schema,
                        &aggregate_rows,
                        aggregate,
                    )?;
                    let row = Self::build_trigger_insert_row(database, insert, vec![value])?;
                    self.insert_trigger_row(
                        database,
                        &insert.target_table,
                        row,
                        ignore_check_constraints,
                        case_sensitive_like,
                    )?;
                }
                return Ok(());
            }
            let mut distinct_rows = insert.distinct.then(BTreeSet::new);
            for (select_row_id, select_row, _) in ordered_rows
                .iter()
                .skip(offset)
                .take(limit.unwrap_or(usize::MAX))
            {
                let context = SimpleTriggerEvalContext {
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name: Some(&select_from.table),
                    select_alias: select_from.alias.as_deref(),
                    select_schema: Some(&select_schema),
                    select_row_id: Some(*select_row_id),
                    select_row: Some(select_row),
                };
                self.execute_simple_trigger_insert_rows(
                    database,
                    insert,
                    &context,
                    distinct_rows.as_mut(),
                    ignore_check_constraints,
                    case_sensitive_like,
                )?;
            }
            return Ok(());
        }

        let context = SimpleTriggerEvalContext {
            source_schema,
            old_row,
            new_row,
            select_table_name: None,
            select_alias: None,
            select_schema: None,
            select_row_id: None,
            select_row: None,
        };
        self.execute_simple_trigger_insert_rows(
            database,
            insert,
            &context,
            None,
            ignore_check_constraints,
            case_sensitive_like,
        )
    }

    fn build_simple_trigger_group_rows(
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        select_table_name: &str,
        select_alias: Option<&str>,
        select_schema: &Schema,
        rows: &[(RowId, Row, Vec<Value>)],
        insert: &SimpleTriggerInsertAction,
    ) -> Result<Vec<(Row, Row, Vec<Value>)>> {
        let mut groups: BTreeMap<Row, Vec<(RowId, Row, Vec<Value>)>> = BTreeMap::new();
        for (row_id, row, order_key) in rows {
            let context = SimpleTriggerEvalContext {
                source_schema,
                old_row,
                new_row,
                select_table_name: Some(select_table_name),
                select_alias,
                select_schema: Some(select_schema),
                select_row_id: Some(*row_id),
                select_row: Some(row),
            };
            let key = insert
                .group_by
                .iter()
                .map(|expr| Self::evaluate_simple_trigger_expr_with_context(&context, expr))
                .collect::<Result<Row>>()?;
            groups
                .entry(key)
                .or_default()
                .push((*row_id, row.clone(), order_key.clone()));
        }

        let mut result = Vec::new();
        for (_key, group_rows) in groups {
            let first = group_rows
                .first()
                .ok_or_else(|| DbError::storage("empty trigger SELECT group"))?;
            let first_context = SimpleTriggerEvalContext {
                source_schema,
                old_row,
                new_row,
                select_table_name: Some(select_table_name),
                select_alias,
                select_schema: Some(select_schema),
                select_row_id: Some(first.0),
                select_row: Some(&first.1),
            };
            let mut values = Vec::new();
            let mut select_alias_values = Vec::new();
            for item in &insert.group_select_items {
                match item {
                    SimpleTriggerGroupSelectItem::Scalar { expr, alias } => {
                        let value =
                            Self::evaluate_simple_trigger_expr_with_context(&first_context, expr)?;
                        if let Some(alias) = alias {
                            select_alias_values.push((alias.clone(), value.clone()));
                        }
                        values.push(value);
                    }
                    SimpleTriggerGroupSelectItem::Aggregate {
                        aggregate,
                        filter,
                        alias,
                    } => {
                        let aggregate_rows = if !filter.is_empty() {
                            group_rows
                                .iter()
                                .filter_map(|(row_id, row, order_key)| {
                                    match Self::trigger_where_groups_match(
                                        select_schema,
                                        row,
                                        source_schema,
                                        old_row,
                                        new_row,
                                        filter,
                                    ) {
                                        Ok(true) => {
                                            Some(Ok((*row_id, row.clone(), order_key.clone())))
                                        }
                                        Ok(false) => None,
                                        Err(error) => Some(Err(error)),
                                    }
                                })
                                .collect::<Result<Vec<_>>>()?
                        } else {
                            group_rows.clone()
                        };
                        let value = Self::evaluate_simple_trigger_aggregate(
                            source_schema,
                            old_row,
                            new_row,
                            select_table_name,
                            select_alias,
                            select_schema,
                            &aggregate_rows,
                            aggregate,
                        )?;
                        if let Some(alias) = alias {
                            select_alias_values.push((alias.clone(), value.clone()));
                        }
                        values.push(value);
                    }
                }
            }
            if !insert.group_having.is_empty()
                && !Self::simple_trigger_group_having_matches(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    &group_rows,
                    &first_context,
                    &select_alias_values,
                    &insert.group_having,
                )?
            {
                continue;
            }
            let order_key = insert
                .order_by
                .iter()
                .map(|order_by| {
                    if let ScalarExpr::Column(name) = &order_by.expr
                        && let Some((_, value)) = select_alias_values
                            .iter()
                            .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
                    {
                        return Ok(value.clone());
                    }
                    Self::evaluate_simple_trigger_group_expr(
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name,
                        select_alias,
                        select_schema,
                        &group_rows,
                        &first_context,
                        &select_alias_values,
                        &order_by.expr,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            result.push((Vec::new(), values, order_key));
        }

        if !insert.order_by.is_empty() {
            result.sort_by(|(_, _, left), (_, _, right)| {
                Self::compare_simple_trigger_order_keys(left, right, &insert.order_by)
            });
        }
        Ok(result)
    }

    fn simple_trigger_group_having_matches(
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        select_table_name: &str,
        select_alias: Option<&str>,
        select_schema: &Schema,
        group_rows: &[(RowId, Row, Vec<Value>)],
        first_context: &SimpleTriggerEvalContext<'_>,
        select_alias_values: &[(String, Value)],
        having_groups: &[Vec<ScalarExpr>],
    ) -> Result<bool> {
        for group in having_groups {
            let mut group_matches = true;
            for expr in group {
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    expr,
                )?;
                if matches!(value, Value::Null) || !Self::sqlite_truthy(&value) {
                    group_matches = false;
                    break;
                }
            }
            if group_matches {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn evaluate_simple_trigger_group_expr(
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        select_table_name: &str,
        select_alias: Option<&str>,
        select_schema: &Schema,
        group_rows: &[(RowId, Row, Vec<Value>)],
        first_context: &SimpleTriggerEvalContext<'_>,
        select_alias_values: &[(String, Value)],
        expr: &ScalarExpr,
    ) -> Result<Value> {
        match expr {
            ScalarExpr::Column(name) => {
                if let Some((_, value)) = select_alias_values
                    .iter()
                    .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
                {
                    Ok(value.clone())
                } else {
                    Self::evaluate_simple_trigger_expr_with_context(first_context, expr)
                }
            }
            ScalarExpr::UnaryPlus(inner) | ScalarExpr::Collate { expr: inner, .. } => {
                Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    inner,
                )
            }
            ScalarExpr::UnaryMinus(inner) => {
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    inner,
                )?;
                match value {
                    Value::Null => Ok(Value::Null),
                    Value::Integer(value) => value
                        .checked_neg()
                        .map(Value::Integer)
                        .ok_or_else(|| DbError::storage("integer overflow")),
                    Value::Real(value) => Ok(Value::Real(-value)),
                    value => Ok(Value::Real(-trigger_value_to_f64(&value)?)),
                }
            }
            ScalarExpr::BitNot(inner) => {
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    inner,
                )?;
                match value {
                    Value::Integer(value) => Ok(Value::Integer(!value)),
                    Value::Null => Ok(Value::Null),
                    value => Err(DbError::storage(format!(
                        "cannot bitwise-not {} in trigger GROUP BY expression",
                        value.type_name()
                    ))),
                }
            }
            ScalarExpr::Not(inner) => {
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    inner,
                )?;
                Ok(match value {
                    Value::Null => Value::Null,
                    value => Value::Integer(if Self::sqlite_truthy(&value) { 0 } else { 1 }),
                })
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                let base = base
                    .as_ref()
                    .map(|base| {
                        Self::evaluate_simple_trigger_group_expr(
                            source_schema,
                            old_row,
                            new_row,
                            select_table_name,
                            select_alias,
                            select_schema,
                            group_rows,
                            first_context,
                            select_alias_values,
                            base,
                        )
                    })
                    .transpose()?;
                for (when_expr, then_expr) in when_then_clauses {
                    let when_value = Self::evaluate_simple_trigger_group_expr(
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name,
                        select_alias,
                        select_schema,
                        group_rows,
                        first_context,
                        select_alias_values,
                        when_expr,
                    )?;
                    let matches = if let Some(base) = &base {
                        Self::is_trigger_is_match(base, &when_value)
                    } else {
                        !matches!(when_value, Value::Null) && Self::sqlite_truthy(&when_value)
                    };
                    if matches {
                        return Self::evaluate_simple_trigger_group_expr(
                            source_schema,
                            old_row,
                            new_row,
                            select_table_name,
                            select_alias,
                            select_schema,
                            group_rows,
                            first_context,
                            select_alias_values,
                            then_expr,
                        );
                    }
                }
                else_expr
                    .as_ref()
                    .map(|else_expr| {
                        Self::evaluate_simple_trigger_group_expr(
                            source_schema,
                            old_row,
                            new_row,
                            select_table_name,
                            select_alias,
                            select_schema,
                            group_rows,
                            first_context,
                            select_alias_values,
                            else_expr,
                        )
                    })
                    .unwrap_or(Ok(Value::Null))
            }
            ScalarExpr::Cast { expr, ty } => {
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    expr,
                )?;
                trigger_cast_value(value, *ty)
            }
            ScalarExpr::Function {
                func: ScalarFunc::Date,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "DATE trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                Ok(trigger_date_value(&value))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Time,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "TIME trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                Ok(trigger_time_value(&value))
            }
            ScalarExpr::Function {
                func: ScalarFunc::DateTime,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "DATETIME trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                Ok(trigger_datetime_value(&value))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Printf,
                args,
            } => {
                if args.is_empty() {
                    return Err(DbError::storage(
                        "PRINTF trigger GROUP BY expressions require at least one argument",
                    ));
                }
                let values = args
                    .iter()
                    .map(|arg| {
                        Self::evaluate_simple_trigger_group_expr(
                            source_schema,
                            old_row,
                            new_row,
                            select_table_name,
                            select_alias,
                            select_schema,
                            group_rows,
                            first_context,
                            select_alias_values,
                            arg,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                Self::evaluate_simple_trigger_group_printf(&values)
            }
            ScalarExpr::Function {
                func: ScalarFunc::TypeOf,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "TYPEOF trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                Ok(Value::Text(
                    match value {
                        Value::Null => "null",
                        Value::Boolean(_) | Value::Integer(_) => "integer",
                        Value::Real(_) => "real",
                        Value::Blob(_) => "blob",
                        Value::Text(_) => "text",
                    }
                    .to_string(),
                ))
            }
            ScalarExpr::Function { func, args }
                if matches!(func, ScalarFunc::MinScalar | ScalarFunc::MaxScalar) =>
            {
                if args.is_empty() {
                    return Err(DbError::storage(
                        "MIN/MAX trigger GROUP BY expressions require at least one argument",
                    ));
                }
                let values = args
                    .iter()
                    .map(|arg| {
                        Self::evaluate_simple_trigger_group_expr(
                            source_schema,
                            old_row,
                            new_row,
                            select_table_name,
                            select_alias,
                            select_schema,
                            group_rows,
                            first_context,
                            select_alias_values,
                            arg,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                Self::evaluate_trigger_min_max_scalar(
                    &values,
                    matches!(func, ScalarFunc::MinScalar),
                )
            }
            ScalarExpr::Function {
                func: ScalarFunc::Abs,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "ABS trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                if matches!(value, Value::Null) {
                    Ok(Value::Null)
                } else {
                    trigger_abs_value(&value)
                }
            }
            ScalarExpr::Function {
                func: ScalarFunc::Sign,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "SIGN trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                Ok(match value {
                    Value::Null => Value::Null,
                    Value::Boolean(value) => Value::Integer(if value { 1 } else { 0 }),
                    Value::Integer(value) => Value::Integer(value.signum()),
                    Value::Real(value) => Value::Integer(if value > 0.0 {
                        1
                    } else if value < 0.0 {
                        -1
                    } else {
                        0
                    }),
                    Value::Text(value) => {
                        let value = value.trim();
                        if let Ok(value) = value.parse::<i64>() {
                            Value::Integer(value.signum())
                        } else if let Ok(value) = value.parse::<f64>() {
                            Value::Integer(if value > 0.0 {
                                1
                            } else if value < 0.0 {
                                -1
                            } else {
                                0
                            })
                        } else {
                            Value::Null
                        }
                    }
                    Value::Blob(_) => Value::Null,
                })
            }
            ScalarExpr::Function { func, args }
                if matches!(func, ScalarFunc::Length | ScalarFunc::OctetLength) =>
            {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "LENGTH/OCTET_LENGTH trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                Ok(match value {
                    Value::Null => Value::Null,
                    Value::Blob(value) => Value::Integer(value.len() as i64),
                    value if matches!(func, ScalarFunc::OctetLength) => {
                        Value::Integer(Self::trigger_value_to_text(&value).len() as i64)
                    }
                    value => {
                        Value::Integer(Self::trigger_value_to_text(&value).chars().count() as i64)
                    }
                })
            }
            ScalarExpr::Function {
                func: ScalarFunc::ZeroBlob,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "ZEROBLOB trigger GROUP BY expressions require one argument",
                    ));
                }
                let length = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                let length = if matches!(length, Value::Null) {
                    0
                } else {
                    trigger_value_to_i64(&length)?.max(0)
                };
                let length = usize::try_from(length)
                    .map_err(|_| DbError::storage("ZEROBLOB length is too large"))?;
                Ok(Value::Blob(vec![0; length]))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Quote,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "QUOTE trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                Ok(Value::Text(trigger_quote_value(&value)))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Hex,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "HEX trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                Ok(Value::Text(match value {
                    Value::Null => String::new(),
                    Value::Blob(value) => trigger_hex_bytes(&value),
                    value => trigger_hex_bytes(Self::trigger_value_to_text(&value).as_bytes()),
                }))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Unicode,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "UNICODE trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(Self::trigger_value_to_text(&value)
                    .chars()
                    .next()
                    .map(|ch| Value::Integer(i64::from(u32::from(ch))))
                    .unwrap_or(Value::Null))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Char,
                args,
            } => {
                let mut result = String::new();
                for arg in args {
                    let value = Self::evaluate_simple_trigger_group_expr(
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name,
                        select_alias,
                        select_schema,
                        group_rows,
                        first_context,
                        select_alias_values,
                        arg,
                    )?;
                    if matches!(value, Value::Null) {
                        continue;
                    }
                    let code_point = trigger_value_to_i64(&value)?;
                    let ch = u32::try_from(code_point)
                        .ok()
                        .and_then(char::from_u32)
                        .unwrap_or(char::REPLACEMENT_CHARACTER);
                    result.push(ch);
                }
                Ok(Value::Text(result))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Substr,
                args,
            } => {
                if !matches!(args.len(), 2 | 3) {
                    return Err(DbError::storage(
                        "SUBSTR trigger GROUP BY expressions require two or three arguments",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let start = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[1],
                )?;
                let start = trigger_value_to_i64(&start)?;
                let length = if args.len() == 3 {
                    let length = Self::evaluate_simple_trigger_group_expr(
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name,
                        select_alias,
                        select_schema,
                        group_rows,
                        first_context,
                        select_alias_values,
                        &args[2],
                    )?;
                    Some(trigger_value_to_i64(&length)?)
                } else {
                    None
                };
                Ok(match value {
                    Value::Blob(value) => Value::Blob(trigger_substr_blob(&value, start, length)),
                    value => Value::Text(trigger_substr_text(
                        &Self::trigger_value_to_text(&value),
                        start,
                        length,
                    )),
                })
            }
            ScalarExpr::Function { func, args }
                if matches!(func, ScalarFunc::Lower | ScalarFunc::Upper) =>
            {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "LOWER/UPPER trigger GROUP BY expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let text = Self::trigger_value_to_text(&value);
                Ok(Value::Text(if matches!(func, ScalarFunc::Upper) {
                    sqlite_ascii_upper(&text)
                } else {
                    sqlite_ascii_lower(&text)
                }))
            }
            ScalarExpr::Function { func, args }
                if matches!(
                    func,
                    ScalarFunc::Trim | ScalarFunc::LTrim | ScalarFunc::RTrim
                ) =>
            {
                if !matches!(args.len(), 1 | 2) {
                    return Err(DbError::storage(
                        "TRIM trigger GROUP BY expressions require one or two arguments",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let characters = if args.len() == 2 {
                    let characters = Self::evaluate_simple_trigger_group_expr(
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name,
                        select_alias,
                        select_schema,
                        group_rows,
                        first_context,
                        select_alias_values,
                        &args[1],
                    )?;
                    if matches!(characters, Value::Null) {
                        return Ok(Value::Null);
                    }
                    Self::trigger_value_to_text(&characters)
                } else {
                    " ".to_string()
                };
                let value = Self::trigger_value_to_text(&value);
                Ok(Value::Text(match func {
                    ScalarFunc::LTrim => value
                        .trim_start_matches(|ch| characters.contains(ch))
                        .to_string(),
                    ScalarFunc::RTrim => value
                        .trim_end_matches(|ch| characters.contains(ch))
                        .to_string(),
                    _ => value.trim_matches(|ch| characters.contains(ch)).to_string(),
                }))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Replace,
                args,
            } => {
                if args.len() != 3 {
                    return Err(DbError::storage(
                        "REPLACE trigger GROUP BY expressions require three arguments",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                let pattern = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[1],
                )?;
                let replacement = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[2],
                )?;
                if matches!(value, Value::Null)
                    || matches!(pattern, Value::Null)
                    || matches!(replacement, Value::Null)
                {
                    return Ok(Value::Null);
                }
                let value = Self::trigger_value_to_text(&value);
                let pattern = Self::trigger_value_to_text(&pattern);
                if pattern.is_empty() {
                    return Ok(Value::Text(value));
                }
                Ok(Value::Text(value.replace(
                    &pattern,
                    &Self::trigger_value_to_text(&replacement),
                )))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Instr,
                args,
            } => {
                if args.len() != 2 {
                    return Err(DbError::storage(
                        "INSTR trigger GROUP BY expressions require two arguments",
                    ));
                }
                let haystack = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                let needle = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[1],
                )?;
                if matches!(haystack, Value::Null) || matches!(needle, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(match (&haystack, &needle) {
                    (Value::Blob(haystack), Value::Blob(needle)) => {
                        Value::Integer(trigger_instr_blob(haystack, needle))
                    }
                    _ => {
                        let haystack = Self::trigger_value_to_text(&haystack);
                        let needle = Self::trigger_value_to_text(&needle);
                        if needle.is_empty() {
                            Value::Integer(1)
                        } else {
                            Value::Integer(
                                haystack
                                    .find(&needle)
                                    .map(|byte_index| {
                                        haystack[..byte_index].chars().count() as i64 + 1
                                    })
                                    .unwrap_or(0),
                            )
                        }
                    }
                })
            }
            ScalarExpr::Function {
                func: ScalarFunc::Round,
                args,
            } => {
                if !matches!(args.len(), 1 | 2) {
                    return Err(DbError::storage(
                        "ROUND trigger GROUP BY expressions require one or two arguments",
                    ));
                }
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let value = trigger_value_to_f64(&value)?;
                let precision = if args.len() == 2 {
                    let precision = Self::evaluate_simple_trigger_group_expr(
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name,
                        select_alias,
                        select_schema,
                        group_rows,
                        first_context,
                        select_alias_values,
                        &args[1],
                    )?;
                    if matches!(precision, Value::Null) {
                        return Ok(Value::Null);
                    }
                    i32::try_from(trigger_value_to_i64(&precision)?)
                        .map_err(|_| DbError::storage("ROUND precision does not fit in i32"))?
                } else {
                    0
                };
                Ok(Value::Real(sqlite_round_f64(value, precision)))
            }
            ScalarExpr::Function { func, args }
                if matches!(func, ScalarFunc::Concat | ScalarFunc::ConcatWs) =>
            {
                if matches!(func, ScalarFunc::Concat) && args.is_empty() {
                    return Err(DbError::storage(
                        "CONCAT trigger GROUP BY expressions require at least one argument",
                    ));
                }
                if matches!(func, ScalarFunc::ConcatWs) && args.len() < 2 {
                    return Err(DbError::storage(
                        "CONCAT_WS trigger GROUP BY expressions require at least two arguments",
                    ));
                }
                let mut result = String::new();
                let mut separator = String::new();
                let mut start_index = 0;
                if matches!(func, ScalarFunc::ConcatWs) {
                    let value = Self::evaluate_simple_trigger_group_expr(
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name,
                        select_alias,
                        select_schema,
                        group_rows,
                        first_context,
                        select_alias_values,
                        &args[0],
                    )?;
                    if matches!(value, Value::Null) {
                        return Ok(Value::Null);
                    }
                    separator = Self::trigger_value_to_text(&value);
                    start_index = 1;
                }
                let mut appended = false;
                for arg in &args[start_index..] {
                    let value = Self::evaluate_simple_trigger_group_expr(
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name,
                        select_alias,
                        select_schema,
                        group_rows,
                        first_context,
                        select_alias_values,
                        arg,
                    )?;
                    if matches!(value, Value::Null) {
                        continue;
                    }
                    if appended && matches!(func, ScalarFunc::ConcatWs) {
                        result.push_str(&separator);
                    }
                    result.push_str(&Self::trigger_value_to_text(&value));
                    appended = true;
                }
                Ok(Value::Text(result))
            }
            ScalarExpr::Function { func, args }
                if matches!(func, ScalarFunc::Coalesce | ScalarFunc::IfNull) =>
            {
                if matches!(func, ScalarFunc::Coalesce) && args.len() < 2 {
                    return Err(DbError::storage(
                        "COALESCE trigger GROUP BY expressions require at least two arguments",
                    ));
                }
                if matches!(func, ScalarFunc::IfNull) && args.len() != 2 {
                    return Err(DbError::storage(
                        "IFNULL trigger GROUP BY expressions require two arguments",
                    ));
                }
                for arg in args {
                    let value = Self::evaluate_simple_trigger_group_expr(
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name,
                        select_alias,
                        select_schema,
                        group_rows,
                        first_context,
                        select_alias_values,
                        arg,
                    )?;
                    if !matches!(value, Value::Null) {
                        return Ok(value);
                    }
                }
                Ok(Value::Null)
            }
            ScalarExpr::Function {
                func: ScalarFunc::NullIf,
                args,
            } => {
                if args.len() != 2 {
                    return Err(DbError::storage(
                        "NULLIF trigger GROUP BY expressions require two arguments",
                    ));
                }
                let left = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[0],
                )?;
                let right = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    &args[1],
                )?;
                if Self::compare_values(&left, &right).ok().flatten() == Some(Ordering::Equal) {
                    Ok(Value::Null)
                } else {
                    Ok(left)
                }
            }
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => {
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    expr,
                )?;
                let pattern = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    pattern,
                )?;
                if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                let escape_value = escape
                    .as_ref()
                    .map(|escape| {
                        Self::evaluate_simple_trigger_group_expr(
                            source_schema,
                            old_row,
                            new_row,
                            select_table_name,
                            select_alias,
                            select_schema,
                            group_rows,
                            first_context,
                            select_alias_values,
                            escape,
                        )
                    })
                    .transpose()?;
                let escape_char = match escape_value {
                    Some(Value::Null) => return Ok(Value::Null),
                    Some(escape) => Some(Self::trigger_escape_char(&escape)?),
                    None => None,
                };
                Ok(Value::Integer(
                    if Self::trigger_like_matches(
                        &Self::trigger_value_to_text(&value),
                        &Self::trigger_value_to_text(&pattern),
                        escape_char,
                    ) ^ *negated
                    {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => {
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    expr,
                )?;
                let pattern = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    pattern,
                )?;
                if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(Value::Integer(
                    if Self::trigger_glob_matches(
                        &Self::trigger_value_to_text(&value),
                        &Self::trigger_value_to_text(&pattern),
                    ) ^ *negated
                    {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::Aggregate { func, arg, filter } => {
                let aggregate = Self::simple_trigger_aggregate_from_ast(*func, arg)?;
                let aggregate_rows = if let Some(filter) = filter {
                    group_rows
                        .iter()
                        .filter_map(|(row_id, row, order_key)| {
                            let context = SimpleTriggerEvalContext {
                                source_schema,
                                old_row,
                                new_row,
                                select_table_name: Some(select_table_name),
                                select_alias,
                                select_schema: Some(select_schema),
                                select_row_id: Some(*row_id),
                                select_row: Some(row),
                            };
                            match Self::evaluate_simple_trigger_filter_expr(&context, filter) {
                                Ok(true) => Some(Ok((*row_id, row.clone(), order_key.clone()))),
                                Ok(false) => None,
                                Err(error) => Some(Err(error)),
                            }
                        })
                        .collect::<Result<Vec<_>>>()?
                } else {
                    group_rows.to_vec()
                };
                Self::evaluate_simple_trigger_aggregate(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    &aggregate_rows,
                    &aggregate,
                )
            }
            ScalarExpr::Binary { left, op, right } => {
                let left = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    left,
                )?;
                let right = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    right,
                )?;
                Self::evaluate_simple_trigger_integer_binary(left, *op, right)
            }
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    expr,
                )?;
                let low = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    low,
                )?;
                let high = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    high,
                )?;
                let lower_match = Self::compare_with_operator(&value, CompareOp::Gte, &low)?;
                let upper_match = Self::compare_with_operator(&value, CompareOp::Lte, &high)?;
                Ok(Value::Integer(if (lower_match && upper_match) ^ *negated {
                    1
                } else {
                    0
                }))
            }
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => {
                let value = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    expr,
                )?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let mut saw_null = false;
                for candidate in values {
                    let candidate = Self::evaluate_simple_trigger_group_expr(
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name,
                        select_alias,
                        select_schema,
                        group_rows,
                        first_context,
                        select_alias_values,
                        candidate,
                    )?;
                    if matches!(candidate, Value::Null) {
                        saw_null = true;
                        continue;
                    }
                    if Self::compare_values(&value, &candidate)?.unwrap_or(Ordering::Equal)
                        == Ordering::Equal
                    {
                        return Ok(Value::Integer(if *negated { 0 } else { 1 }));
                    }
                }
                if saw_null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Integer(if *negated { 1 } else { 0 }))
                }
            }
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => {
                let left = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    left,
                )?;
                let right = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    right,
                )?;
                Ok(Value::Integer(
                    if Self::is_trigger_is_match(&left, &right) ^ *negated {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => {
                let evaluated = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    expr,
                )?;
                let truthy = !matches!(evaluated, Value::Null) && Self::sqlite_truthy(&evaluated);
                Ok(Value::Integer(if (truthy == *value) ^ *negated {
                    1
                } else {
                    0
                }))
            }
            ScalarExpr::Compare { left, op, right } => {
                let left = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    left,
                )?;
                let right = Self::evaluate_simple_trigger_group_expr(
                    source_schema,
                    old_row,
                    new_row,
                    select_table_name,
                    select_alias,
                    select_schema,
                    group_rows,
                    first_context,
                    select_alias_values,
                    right,
                )?;
                Ok(Value::Integer(
                    if Self::compare_with_operator(&left, *op, &right)? {
                        1
                    } else {
                        0
                    },
                ))
            }
            expr => Self::evaluate_simple_trigger_expr_with_context(first_context, expr),
        }
    }

    fn simple_trigger_aggregate_from_ast(
        func: AggregateFunc,
        arg: &AggregateArg,
    ) -> Result<SimpleTriggerAggregate> {
        match (func, arg) {
            (AggregateFunc::Count, AggregateArg::Wildcard) => Ok(SimpleTriggerAggregate::CountStar),
            (
                AggregateFunc::Count,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by: _,
                },
            ) => Ok(SimpleTriggerAggregate::CountExpr {
                expr: expr.clone(),
                distinct: *distinct,
            }),
            (
                AggregateFunc::Sum,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by: _,
                },
            ) => Ok(SimpleTriggerAggregate::Sum {
                expr: expr.clone(),
                distinct: *distinct,
            }),
            (
                AggregateFunc::Avg,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by: _,
                },
            ) => Ok(SimpleTriggerAggregate::Avg {
                expr: expr.clone(),
                distinct: *distinct,
            }),
            (
                AggregateFunc::Total,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by: _,
                },
            ) => Ok(SimpleTriggerAggregate::Total {
                expr: expr.clone(),
                distinct: *distinct,
            }),
            (
                AggregateFunc::Min,
                AggregateArg::Expr {
                    expr,
                    distinct: _,
                    order_by: _,
                },
            ) => Ok(SimpleTriggerAggregate::Min(expr.clone())),
            (
                AggregateFunc::Max,
                AggregateArg::Expr {
                    expr,
                    distinct: _,
                    order_by: _,
                },
            ) => Ok(SimpleTriggerAggregate::Max(expr.clone())),
            (
                AggregateFunc::GroupConcat,
                AggregateArg::GroupConcat {
                    expr,
                    separator,
                    distinct,
                    order_by: _,
                },
            ) => Ok(SimpleTriggerAggregate::GroupConcat {
                expr: expr.clone(),
                separator: separator.clone(),
                distinct: *distinct,
                order_by: Vec::new(),
            }),
            _ => Err(DbError::storage(format!(
                "unsupported trigger GROUP BY HAVING aggregate: {func:?}"
            ))),
        }
    }

    fn evaluate_simple_trigger_filter_expr(
        context: &SimpleTriggerEvalContext<'_>,
        expr: &Expr,
    ) -> Result<bool> {
        match expr {
            Expr::Compare { column, op, value } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(
                    context,
                    &ScalarExpr::Column(column.clone()),
                )?;
                Self::compare_with_operator(&left, *op, value)
            }
            Expr::CompareScalar { left, op, right } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                Self::compare_with_operator(&left, *op, &right)
            }
            Expr::CompareColumns { left, op, right } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(
                    context,
                    &ScalarExpr::Column(left.clone()),
                )?;
                let right = Self::evaluate_simple_trigger_expr_with_context(
                    context,
                    &ScalarExpr::Column(right.clone()),
                )?;
                Self::compare_with_operator(&left, *op, &right)
            }
            Expr::IsNull { column, negated } => {
                let value = Self::evaluate_simple_trigger_expr_with_context(
                    context,
                    &ScalarExpr::Column(column.clone()),
                )?;
                Ok(matches!(value, Value::Null) ^ *negated)
            }
            Expr::IsNullScalar { expr, negated } => {
                let value = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                Ok(matches!(value, Value::Null) ^ *negated)
            }
            expr => Err(DbError::storage(format!(
                "unsupported trigger aggregate FILTER expression: {expr:?}"
            ))),
        }
    }

    fn evaluate_simple_trigger_integer_binary(
        left: Value,
        op: ScalarBinaryOp,
        right: Value,
    ) -> Result<Value> {
        match (left, op, right) {
            (Value::Null, _, _) | (_, _, Value::Null) => Ok(Value::Null),
            (Value::Integer(left), ScalarBinaryOp::Add, Value::Integer(right)) => left
                .checked_add(right)
                .map(Value::Integer)
                .ok_or_else(|| DbError::storage("integer overflow")),
            (Value::Integer(left), ScalarBinaryOp::Subtract, Value::Integer(right)) => left
                .checked_sub(right)
                .map(Value::Integer)
                .ok_or_else(|| DbError::storage("integer overflow")),
            (Value::Integer(left), ScalarBinaryOp::Multiply, Value::Integer(right)) => left
                .checked_mul(right)
                .map(Value::Integer)
                .ok_or_else(|| DbError::storage("integer overflow")),
            (Value::Integer(_), ScalarBinaryOp::Divide, Value::Integer(0))
            | (Value::Integer(_), ScalarBinaryOp::Modulo, Value::Integer(0)) => Ok(Value::Null),
            (Value::Integer(left), ScalarBinaryOp::Divide, Value::Integer(right)) => {
                Ok(Value::Integer(left / right))
            }
            (Value::Integer(left), ScalarBinaryOp::Modulo, Value::Integer(right)) => {
                Ok(Value::Integer(left % right))
            }
            (Value::Integer(left), ScalarBinaryOp::BitAnd, Value::Integer(right)) => {
                Ok(Value::Integer(left & right))
            }
            (Value::Integer(left), ScalarBinaryOp::BitOr, Value::Integer(right)) => {
                Ok(Value::Integer(left | right))
            }
            (left, ScalarBinaryOp::Concat, right) => Ok(Value::Text(format!(
                "{}{}",
                Self::trigger_value_to_text(&left),
                Self::trigger_value_to_text(&right)
            ))),
            (left, op, right) => Err(DbError::storage(format!(
                "cannot apply {op:?} to {} and {} in trigger GROUP BY expression",
                left.type_name(),
                right.type_name()
            ))),
        }
    }

    fn evaluate_simple_trigger_group_printf(args: &[Value]) -> Result<Value> {
        let Some(format) = args.first() else {
            return Err(DbError::storage(
                "PRINTF trigger GROUP BY expressions require at least one argument",
            ));
        };
        if matches!(format, Value::Null) {
            return Ok(Value::Null);
        }
        let format = Self::trigger_value_to_text(format);
        let mut rendered = String::new();
        let mut chars = format.chars().peekable();
        let mut arg_index = 1_usize;
        while let Some(ch) = chars.next() {
            if ch != '%' {
                rendered.push(ch);
                continue;
            }
            match chars.next() {
                Some('%') => rendered.push('%'),
                Some('d') | Some('i') => {
                    let value = args.get(arg_index).unwrap_or(&Value::Null);
                    arg_index += 1;
                    rendered.push_str(&trigger_value_to_i64(value).unwrap_or(0).to_string());
                }
                Some('s') => {
                    let value = args.get(arg_index).unwrap_or(&Value::Null);
                    arg_index += 1;
                    if !matches!(value, Value::Null) {
                        rendered.push_str(&Self::trigger_value_to_text(value));
                    }
                }
                Some(specifier) => {
                    return Err(DbError::storage(format!(
                        "unsupported trigger GROUP BY printf specifier %{specifier}"
                    )));
                }
                None => rendered.push('%'),
            }
        }
        Ok(Value::Text(rendered))
    }

    fn evaluate_simple_trigger_aggregate(
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        select_table_name: &str,
        select_alias: Option<&str>,
        select_schema: &Schema,
        rows: &[(RowId, Row, Vec<Value>)],
        aggregate: &SimpleTriggerAggregate,
    ) -> Result<Value> {
        match aggregate {
            SimpleTriggerAggregate::Error(message) => Err(DbError::storage(message.clone())),
            SimpleTriggerAggregate::CountStar => i64::try_from(rows.len())
                .map(Value::Integer)
                .map_err(|_| DbError::storage("trigger SELECT count is too large")),
            SimpleTriggerAggregate::CountExpr { expr, distinct } => {
                let mut count = 0_i64;
                let mut seen = BTreeSet::new();
                for (row_id, row, _) in rows {
                    let context = SimpleTriggerEvalContext {
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name: Some(select_table_name),
                        select_alias,
                        select_schema: Some(select_schema),
                        select_row_id: Some(*row_id),
                        select_row: Some(row),
                    };
                    let value = Self::evaluate_simple_trigger_expr_with_context(&context, expr)?;
                    if matches!(value, Value::Null) {
                        continue;
                    }
                    if *distinct && !seen.insert(value) {
                        continue;
                    }
                    {
                        count += 1;
                    }
                }
                Ok(Value::Integer(count))
            }
            SimpleTriggerAggregate::Sum { expr, distinct } => {
                let mut integer_sum = 0_i64;
                let mut real_sum = 0.0_f64;
                let mut saw_real = false;
                let mut saw_value = false;
                let mut seen = BTreeSet::new();
                for (row_id, row, _) in rows {
                    let context = SimpleTriggerEvalContext {
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name: Some(select_table_name),
                        select_alias,
                        select_schema: Some(select_schema),
                        select_row_id: Some(*row_id),
                        select_row: Some(row),
                    };
                    match Self::evaluate_simple_trigger_expr_with_context(&context, expr)? {
                        Value::Null => {}
                        value if *distinct && !seen.insert(value.clone()) => {}
                        Value::Integer(value) => {
                            if saw_real {
                                real_sum += value as f64;
                            } else {
                                integer_sum = integer_sum
                                    .checked_add(value)
                                    .ok_or_else(|| DbError::storage("integer overflow"))?;
                            }
                            saw_value = true;
                        }
                        Value::Real(value) => {
                            if !saw_real {
                                real_sum = integer_sum as f64;
                                saw_real = true;
                            }
                            real_sum += value;
                            saw_value = true;
                        }
                        value => {
                            return Err(DbError::storage(format!(
                                "cannot sum {} in trigger expression",
                                value.type_name()
                            )));
                        }
                    }
                }
                if saw_value {
                    if saw_real {
                        Ok(Value::Real(real_sum))
                    } else {
                        Ok(Value::Integer(integer_sum))
                    }
                } else {
                    Ok(Value::Null)
                }
            }
            SimpleTriggerAggregate::Avg { expr, distinct } => {
                let mut sum = 0.0_f64;
                let mut count = 0_i64;
                let mut seen = BTreeSet::new();
                for (row_id, row, _) in rows {
                    let context = SimpleTriggerEvalContext {
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name: Some(select_table_name),
                        select_alias,
                        select_schema: Some(select_schema),
                        select_row_id: Some(*row_id),
                        select_row: Some(row),
                    };
                    match Self::evaluate_simple_trigger_expr_with_context(&context, expr)? {
                        Value::Null => {}
                        value if *distinct && !seen.insert(value.clone()) => {}
                        Value::Integer(value) => {
                            sum += value as f64;
                            count += 1;
                        }
                        Value::Real(value) => {
                            sum += value;
                            count += 1;
                        }
                        value => {
                            return Err(DbError::storage(format!(
                                "cannot average {} in trigger expression",
                                value.type_name()
                            )));
                        }
                    }
                }
                if count == 0 {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Real(sum / count as f64))
                }
            }
            SimpleTriggerAggregate::Total { expr, distinct } => {
                let mut total = 0.0_f64;
                let mut seen = BTreeSet::new();
                for (row_id, row, _) in rows {
                    let context = SimpleTriggerEvalContext {
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name: Some(select_table_name),
                        select_alias,
                        select_schema: Some(select_schema),
                        select_row_id: Some(*row_id),
                        select_row: Some(row),
                    };
                    match Self::evaluate_simple_trigger_expr_with_context(&context, expr)? {
                        Value::Null => {}
                        value if *distinct && !seen.insert(value.clone()) => {}
                        Value::Integer(value) => total += value as f64,
                        Value::Real(value) => total += value,
                        value => {
                            return Err(DbError::storage(format!(
                                "cannot total {} in trigger expression",
                                value.type_name()
                            )));
                        }
                    }
                }
                Ok(Value::Real(total))
            }
            SimpleTriggerAggregate::Min(expr) | SimpleTriggerAggregate::Max(expr) => {
                let mut best: Option<Value> = None;
                for (row_id, row, _) in rows {
                    let context = SimpleTriggerEvalContext {
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name: Some(select_table_name),
                        select_alias,
                        select_schema: Some(select_schema),
                        select_row_id: Some(*row_id),
                        select_row: Some(row),
                    };
                    let value = Self::evaluate_simple_trigger_expr_with_context(&context, expr)?;
                    if matches!(value, Value::Null) {
                        continue;
                    }
                    let replace = match &best {
                        None => true,
                        Some(best) => {
                            let ordering =
                                Self::compare_values(&value, best)?.unwrap_or(Ordering::Equal);
                            match aggregate {
                                SimpleTriggerAggregate::Min(_) => ordering == Ordering::Less,
                                SimpleTriggerAggregate::Max(_) => ordering == Ordering::Greater,
                                _ => unreachable!(),
                            }
                        }
                    };
                    if replace {
                        best = Some(value);
                    }
                }
                Ok(best.unwrap_or(Value::Null))
            }
            SimpleTriggerAggregate::GroupConcat {
                expr,
                separator,
                distinct,
                order_by,
            } => {
                let mut result = String::new();
                let mut saw_value = false;
                let mut seen = BTreeSet::new();
                let rows = if order_by.is_empty() {
                    rows.to_vec()
                } else {
                    let mut rows = rows
                        .iter()
                        .map(|(row_id, row, _)| {
                            let context = SimpleTriggerEvalContext {
                                source_schema,
                                old_row,
                                new_row,
                                select_table_name: Some(select_table_name),
                                select_alias,
                                select_schema: Some(select_schema),
                                select_row_id: Some(*row_id),
                                select_row: Some(row),
                            };
                            let order_key = order_by
                                .iter()
                                .map(|order_by| {
                                    Self::evaluate_simple_trigger_expr_with_context(
                                        &context,
                                        &order_by.expr,
                                    )
                                })
                                .collect::<Result<Vec<_>>>()?;
                            Ok((*row_id, row.clone(), order_key))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Self::sort_simple_trigger_rows_by_order_keys(&mut rows, order_by);
                    rows
                };
                for (row_id, row, _) in &rows {
                    let context = SimpleTriggerEvalContext {
                        source_schema,
                        old_row,
                        new_row,
                        select_table_name: Some(select_table_name),
                        select_alias,
                        select_schema: Some(select_schema),
                        select_row_id: Some(*row_id),
                        select_row: Some(row),
                    };
                    let value = Self::evaluate_simple_trigger_expr_with_context(&context, expr)?;
                    if matches!(value, Value::Null) {
                        continue;
                    }
                    if *distinct && !seen.insert(value.clone()) {
                        continue;
                    }
                    if saw_value {
                        let separator = if let Some(separator) = separator {
                            let separator = Self::evaluate_simple_trigger_expr_with_context(
                                &context, separator,
                            )?;
                            if matches!(separator, Value::Null) {
                                String::new()
                            } else {
                                Self::trigger_value_to_text(&separator)
                            }
                        } else {
                            ",".to_string()
                        };
                        result.push_str(&separator);
                    }
                    result.push_str(&Self::trigger_value_to_text(&value));
                    saw_value = true;
                }
                if saw_value {
                    Ok(Value::Text(result))
                } else {
                    Ok(Value::Null)
                }
            }
        }
    }

    fn execute_simple_trigger_insert_rows(
        &self,
        database: &mut WritableDatabase,
        insert: &SimpleTriggerInsertAction,
        context: &SimpleTriggerEvalContext<'_>,
        mut distinct_rows: Option<&mut BTreeSet<Row>>,
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<()> {
        for values in &insert.rows {
            if !Self::simple_trigger_select_where_groups_match(context, &insert.where_groups)? {
                continue;
            }
            let values = if insert.select_star {
                context
                    .select_row
                    .cloned()
                    .ok_or_else(|| DbError::storage("trigger SELECT * requires a source row"))?
            } else {
                values
                    .iter()
                    .map(|expr| Self::evaluate_simple_trigger_expr_with_context(context, expr))
                    .collect::<Result<Vec<_>>>()?
            };
            if let Some(seen) = distinct_rows.as_deref_mut()
                && !seen.insert(values.clone())
            {
                continue;
            }
            let row = Self::build_trigger_insert_row(database, insert, values)?;
            self.insert_trigger_row(
                database,
                &insert.target_table,
                row,
                ignore_check_constraints,
                case_sensitive_like,
            )?;
        }
        Ok(())
    }

    fn sort_simple_trigger_rows_by_order_keys(
        rows: &mut [(RowId, Row, Vec<Value>)],
        order_by: &[SimpleTriggerOrderBy],
    ) {
        rows.sort_by(|(_, _, left), (_, _, right)| {
            Self::compare_simple_trigger_order_keys(left, right, order_by)
        });
    }

    fn compare_simple_trigger_order_keys(
        left: &[Value],
        right: &[Value],
        order_by: &[SimpleTriggerOrderBy],
    ) -> Ordering {
        for (index, order_by) in order_by.iter().enumerate() {
            let nulls_first = order_by.nulls_first.unwrap_or(!order_by.descending);
            let ordering = match (left.get(index), right.get(index)) {
                (Some(Value::Null), Some(Value::Null)) => Ordering::Equal,
                (Some(Value::Null), Some(_)) => {
                    if nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (Some(_), Some(Value::Null)) => {
                    if nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (Some(left), Some(right)) => {
                    if order_by
                        .collation
                        .as_deref()
                        .is_some_and(|collation| collation.eq_ignore_ascii_case("NOCASE"))
                    {
                        Self::compare_values_nocase(left, right)
                            .ok()
                            .flatten()
                            .unwrap_or(Ordering::Equal)
                    } else {
                        Self::compare_values(left, right)
                            .ok()
                            .flatten()
                            .unwrap_or(Ordering::Equal)
                    }
                }
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
            };
            let ordering = if order_by.descending
                && !matches!(
                    (left.get(index), right.get(index)),
                    (Some(Value::Null), _) | (_, Some(Value::Null))
                ) {
                ordering.reverse()
            } else {
                ordering
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    fn simple_trigger_select_where_groups_match(
        context: &SimpleTriggerEvalContext<'_>,
        where_groups: &[Vec<ScalarExpr>],
    ) -> Result<bool> {
        if where_groups.is_empty() {
            return Ok(true);
        }
        for group in where_groups {
            let mut group_matches = true;
            for expr in group {
                let value = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                if matches!(value, Value::Null) || !Self::sqlite_truthy(&value) {
                    group_matches = false;
                    break;
                }
            }
            if group_matches {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn execute_simple_trigger_inserts(
        &self,
        database: &mut WritableDatabase,
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        action: &SimpleTriggerAction,
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<()> {
        for statement in &action.statements {
            match statement {
                SimpleTriggerStatement::Insert(insert) => {
                    self.execute_simple_trigger_insert(
                        database,
                        source_schema,
                        old_row,
                        new_row,
                        insert,
                        ignore_check_constraints,
                        case_sensitive_like,
                    )?;
                }
                SimpleTriggerStatement::Delete(delete) => {
                    Self::delete_trigger_rows(database, source_schema, old_row, new_row, delete)?;
                }
                SimpleTriggerStatement::Update(update) => {
                    self.update_trigger_rows(
                        database,
                        source_schema,
                        old_row,
                        new_row,
                        update,
                        ignore_check_constraints,
                        case_sensitive_like,
                    )?;
                }
                SimpleTriggerStatement::RaiseError(message) => {
                    let message = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        message,
                    )?;
                    return Err(DbError::storage(Self::trigger_value_to_text(&message)));
                }
                SimpleTriggerStatement::RaiseIgnore => {
                    return Err(DbError::storage(SQLITE_TRIGGER_RAISE_IGNORE));
                }
            }
        }
        Ok(())
    }

    fn execute_simple_after_insert_triggers(
        &self,
        database: &mut WritableDatabase,
        schema_name: &str,
        inserted_row: &Row,
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<()> {
        let source_schema = database
            .tables
            .get(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?
            .schema
            .clone();
        let triggers = database.extra_schema_objects.clone();
        for trigger in triggers {
            if trigger.entry_type != "trigger" || trigger.table_name != schema_name {
                continue;
            }
            let Some(sql) = trigger.sql.as_deref() else {
                continue;
            };
            let Some(action) = parse_simple_after_insert_trigger(sql) else {
                continue;
            };
            if !Self::should_execute_simple_trigger(
                &source_schema,
                None,
                Some(inserted_row),
                action.when.as_ref(),
            )? {
                continue;
            }
            self.execute_simple_trigger_inserts(
                database,
                &source_schema,
                None,
                Some(inserted_row),
                &action,
                ignore_check_constraints,
                case_sensitive_like,
            )?;
        }
        Ok(())
    }

    fn execute_simple_before_insert_triggers(
        &self,
        database: &mut WritableDatabase,
        schema_name: &str,
        inserted_row: &Row,
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<()> {
        let source_schema = database
            .tables
            .get(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?
            .schema
            .clone();
        let triggers = database.extra_schema_objects.clone();
        for trigger in triggers {
            if trigger.entry_type != "trigger" || trigger.table_name != schema_name {
                continue;
            }
            let Some(sql) = trigger.sql.as_deref() else {
                continue;
            };
            let Some(action) = parse_simple_before_insert_trigger(sql) else {
                continue;
            };
            if !Self::should_execute_simple_trigger(
                &source_schema,
                None,
                Some(inserted_row),
                action.when.as_ref(),
            )? {
                continue;
            }
            self.execute_simple_trigger_inserts(
                database,
                &source_schema,
                None,
                Some(inserted_row),
                &action,
                ignore_check_constraints,
                case_sensitive_like,
            )?;
        }
        Ok(())
    }

    fn execute_simple_after_update_triggers(
        &self,
        database: &mut WritableDatabase,
        schema_name: &str,
        old_row: &Row,
        new_row: &Row,
        updated_columns: &[String],
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<()> {
        let source_schema = database
            .tables
            .get(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?
            .schema
            .clone();
        let triggers = database.extra_schema_objects.clone();
        for trigger in triggers {
            if trigger.entry_type != "trigger" || trigger.table_name != schema_name {
                continue;
            }
            let Some(sql) = trigger.sql.as_deref() else {
                continue;
            };
            let Some(action) = parse_simple_after_update_trigger(sql) else {
                continue;
            };
            if !trigger_matches_update_of(&action, updated_columns) {
                continue;
            }
            if !Self::should_execute_simple_trigger(
                &source_schema,
                Some(old_row),
                Some(new_row),
                action.when.as_ref(),
            )? {
                continue;
            }
            self.execute_simple_trigger_inserts(
                database,
                &source_schema,
                Some(old_row),
                Some(new_row),
                &action,
                ignore_check_constraints,
                case_sensitive_like,
            )?;
        }
        Ok(())
    }

    fn execute_simple_before_update_triggers(
        &self,
        database: &mut WritableDatabase,
        schema_name: &str,
        old_row: &Row,
        new_row: &Row,
        updated_columns: &[String],
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<()> {
        let source_schema = database
            .tables
            .get(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?
            .schema
            .clone();
        let triggers = database.extra_schema_objects.clone();
        for trigger in triggers {
            if trigger.entry_type != "trigger" || trigger.table_name != schema_name {
                continue;
            }
            let Some(sql) = trigger.sql.as_deref() else {
                continue;
            };
            let Some(action) = parse_simple_before_update_trigger(sql) else {
                continue;
            };
            if !trigger_matches_update_of(&action, updated_columns) {
                continue;
            }
            if !Self::should_execute_simple_trigger(
                &source_schema,
                Some(old_row),
                Some(new_row),
                action.when.as_ref(),
            )? {
                continue;
            }
            self.execute_simple_trigger_inserts(
                database,
                &source_schema,
                Some(old_row),
                Some(new_row),
                &action,
                ignore_check_constraints,
                case_sensitive_like,
            )?;
        }
        Ok(())
    }

    fn execute_simple_after_delete_triggers(
        &self,
        database: &mut WritableDatabase,
        schema_name: &str,
        old_row: &Row,
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<()> {
        let source_schema = database
            .tables
            .get(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?
            .schema
            .clone();
        let triggers = database.extra_schema_objects.clone();
        for trigger in triggers {
            if trigger.entry_type != "trigger" || trigger.table_name != schema_name {
                continue;
            }
            let Some(sql) = trigger.sql.as_deref() else {
                continue;
            };
            let Some(action) = parse_simple_after_delete_trigger(sql) else {
                continue;
            };
            if !Self::should_execute_simple_trigger(
                &source_schema,
                Some(old_row),
                None,
                action.when.as_ref(),
            )? {
                continue;
            }
            self.execute_simple_trigger_inserts(
                database,
                &source_schema,
                Some(old_row),
                None,
                &action,
                ignore_check_constraints,
                case_sensitive_like,
            )?;
        }
        Ok(())
    }

    fn execute_simple_before_delete_triggers(
        &self,
        database: &mut WritableDatabase,
        schema_name: &str,
        old_row: &Row,
        ignore_check_constraints: bool,
        case_sensitive_like: bool,
    ) -> Result<()> {
        let source_schema = database
            .tables
            .get(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?
            .schema
            .clone();
        let triggers = database.extra_schema_objects.clone();
        for trigger in triggers {
            if trigger.entry_type != "trigger" || trigger.table_name != schema_name {
                continue;
            }
            let Some(sql) = trigger.sql.as_deref() else {
                continue;
            };
            let Some(action) = parse_simple_before_delete_trigger(sql) else {
                continue;
            };
            if !Self::should_execute_simple_trigger(
                &source_schema,
                Some(old_row),
                None,
                action.when.as_ref(),
            )? {
                continue;
            }
            self.execute_simple_trigger_inserts(
                database,
                &source_schema,
                Some(old_row),
                None,
                &action,
                ignore_check_constraints,
                case_sensitive_like,
            )?;
        }
        Ok(())
    }

    fn should_execute_simple_trigger(
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        when: Option<&ScalarExpr>,
    ) -> Result<bool> {
        let Some(when) = when else {
            return Ok(true);
        };
        let value = Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, when)?;
        Ok(!matches!(value, Value::Null) && Self::sqlite_truthy(&value))
    }

    fn evaluate_simple_trigger_expr_with_context(
        context: &SimpleTriggerEvalContext<'_>,
        expr: &ScalarExpr,
    ) -> Result<Value> {
        match expr {
            ScalarExpr::Column(_) => Self::evaluate_trigger_column_expr_with_context(context, expr),
            ScalarExpr::UnaryPlus(expr) | ScalarExpr::Collate { expr, .. } => {
                Self::evaluate_simple_trigger_expr_with_context(context, expr)
            }
            ScalarExpr::UnaryMinus(expr) => {
                let value = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                match value {
                    Value::Null => Ok(Value::Null),
                    Value::Integer(value) => value
                        .checked_neg()
                        .map(Value::Integer)
                        .ok_or_else(|| DbError::storage("integer overflow")),
                    Value::Real(value) => Ok(Value::Real(-value)),
                    value => Ok(Value::Real(-trigger_value_to_f64(&value)?)),
                }
            }
            ScalarExpr::BitNot(expr) => {
                let value = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                match value {
                    Value::Integer(value) => Ok(Value::Integer(!value)),
                    Value::Null => Ok(Value::Null),
                    value => Err(DbError::storage(format!(
                        "cannot bitwise-not {} in trigger expression",
                        value.type_name()
                    ))),
                }
            }
            ScalarExpr::Cast { expr, ty } => {
                let value = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                trigger_cast_value(value, *ty)
            }
            ScalarExpr::Not(expr) => {
                let value = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                Ok(match value {
                    Value::Null => Value::Null,
                    value => Value::Integer(if Self::sqlite_truthy(&value) { 0 } else { 1 }),
                })
            }
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                Ok(Value::Integer(
                    if Self::is_trigger_is_match(&left, &right) ^ *negated {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => {
                let evaluated = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                let matches = !matches!(evaluated, Value::Null)
                    && trigger_is_true_value(&evaluated) == *value;
                Ok(Value::Integer(if matches ^ *negated { 1 } else { 0 }))
            }
            ScalarExpr::Compare { left, op, right } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                if matches!(left, Value::Null) || matches!(right, Value::Null) {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Integer(
                        if Self::compare_with_operator(&left, *op, &right)? {
                            1
                        } else {
                            0
                        },
                    ))
                }
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                let base = base
                    .as_ref()
                    .map(|base| Self::evaluate_simple_trigger_expr_with_context(context, base))
                    .transpose()?;
                for (when_expr, then_expr) in when_then_clauses {
                    let when_value =
                        Self::evaluate_simple_trigger_expr_with_context(context, when_expr)?;
                    let matches = if let Some(base) = &base {
                        Self::is_trigger_is_match(base, &when_value)
                    } else {
                        !matches!(when_value, Value::Null) && Self::sqlite_truthy(&when_value)
                    };
                    if matches {
                        return Self::evaluate_simple_trigger_expr_with_context(context, then_expr);
                    }
                }
                else_expr
                    .as_ref()
                    .map(|else_expr| {
                        Self::evaluate_simple_trigger_expr_with_context(context, else_expr)
                    })
                    .unwrap_or(Ok(Value::Null))
            }
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => {
                let value = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                let pattern = Self::evaluate_simple_trigger_expr_with_context(context, pattern)?;
                if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                let escape_value = escape
                    .as_ref()
                    .map(|escape| Self::evaluate_simple_trigger_expr_with_context(context, escape))
                    .transpose()?;
                let escape_char = match escape_value {
                    Some(Value::Null) => return Ok(Value::Null),
                    Some(escape) => Some(Self::trigger_escape_char(&escape)?),
                    None => None,
                };
                Ok(Value::Integer(
                    if Self::trigger_like_matches(
                        &Self::trigger_value_to_text(&value),
                        &Self::trigger_value_to_text(&pattern),
                        escape_char,
                    ) ^ *negated
                    {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => {
                let value = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                let pattern = Self::evaluate_simple_trigger_expr_with_context(context, pattern)?;
                if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(Value::Integer(
                    if Self::trigger_glob_matches(
                        &Self::trigger_value_to_text(&value),
                        &Self::trigger_value_to_text(&pattern),
                    ) ^ *negated
                    {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => {
                let value = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let mut saw_null = false;
                for candidate in values {
                    let candidate =
                        Self::evaluate_simple_trigger_expr_with_context(context, candidate)?;
                    if matches!(candidate, Value::Null) {
                        saw_null = true;
                        continue;
                    }
                    if Self::compare_values(&value, &candidate)? == Some(Ordering::Equal) {
                        return Ok(Value::Integer(if *negated { 0 } else { 1 }));
                    }
                }
                if saw_null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Integer(if *negated { 1 } else { 0 }))
                }
            }
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let value = Self::evaluate_simple_trigger_expr_with_context(context, expr)?;
                let low = Self::evaluate_simple_trigger_expr_with_context(context, low)?;
                let high = Self::evaluate_simple_trigger_expr_with_context(context, high)?;
                if matches!(value, Value::Null)
                    || matches!(low, Value::Null)
                    || matches!(high, Value::Null)
                {
                    return Ok(Value::Null);
                }
                let lower_match = Self::compare_with_operator(&value, CompareOp::Gte, &low)?;
                let upper_match = Self::compare_with_operator(&value, CompareOp::Lte, &high)?;
                Ok(Value::Integer(if (lower_match && upper_match) ^ *negated {
                    1
                } else {
                    0
                }))
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Concat,
                right,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                if matches!(left, Value::Null) || matches!(right, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(Value::Text(format!(
                    "{}{}",
                    Self::trigger_value_to_text(&left),
                    Self::trigger_value_to_text(&right)
                )))
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Add,
                right,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(
                        left.checked_add(right)
                            .ok_or_else(|| DbError::storage("integer overflow"))?,
                    )),
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot add {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Subtract,
                right,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(
                        left.checked_sub(right)
                            .ok_or_else(|| DbError::storage("integer overflow"))?,
                    )),
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot subtract {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Multiply,
                right,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(
                        left.checked_mul(right)
                            .ok_or_else(|| DbError::storage("integer overflow"))?,
                    )),
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot multiply {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Modulo,
                right,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                match (left, right) {
                    (Value::Integer(_), Value::Integer(0)) => Ok(Value::Null),
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left % right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot modulo {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Divide,
                right,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                match (left, right) {
                    (Value::Integer(_), Value::Integer(0)) => Ok(Value::Null),
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left / right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot divide {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::BitAnd,
                right,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left & right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot bitwise-and {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::BitOr,
                right,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left | right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot bitwise-or {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::ShiftLeft,
                right,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(Self::trigger_shift_op(left, right, true)))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot shift {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::ShiftRight,
                right,
            } => {
                let left = Self::evaluate_simple_trigger_expr_with_context(context, left)?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(Self::trigger_shift_op(left, right, false)))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot shift {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Function { func, args }
                if matches!(func, ScalarFunc::Lower | ScalarFunc::Upper) =>
            {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "LOWER/UPPER trigger expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let text = Self::trigger_value_to_text(&value);
                Ok(Value::Text(if matches!(func, ScalarFunc::Upper) {
                    sqlite_ascii_upper(&text)
                } else {
                    sqlite_ascii_lower(&text)
                }))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Length,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "LENGTH trigger expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                Ok(match value {
                    Value::Null => Value::Null,
                    Value::Blob(value) => Value::Integer(value.len() as i64),
                    value => {
                        Value::Integer(Self::trigger_value_to_text(&value).chars().count() as i64)
                    }
                })
            }
            ScalarExpr::Function {
                func: ScalarFunc::Substr,
                args,
            } => {
                if !matches!(args.len(), 2 | 3) {
                    return Err(DbError::storage(
                        "SUBSTR trigger expressions require two or three arguments",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let start = Self::evaluate_simple_trigger_expr_with_context(context, &args[1])?;
                let start = trigger_value_to_i64(&start)?;
                let length = if args.len() == 3 {
                    let length =
                        Self::evaluate_simple_trigger_expr_with_context(context, &args[2])?;
                    Some(trigger_value_to_i64(&length)?)
                } else {
                    None
                };
                Ok(match value {
                    Value::Blob(value) => Value::Blob(trigger_substr_blob(&value, start, length)),
                    value => Value::Text(trigger_substr_text(
                        &Self::trigger_value_to_text(&value),
                        start,
                        length,
                    )),
                })
            }
            ScalarExpr::Function {
                func: ScalarFunc::ZeroBlob,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "ZEROBLOB trigger expressions require one argument",
                    ));
                }
                let length = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                let length = if matches!(length, Value::Null) {
                    0
                } else {
                    trigger_value_to_i64(&length)?.max(0)
                };
                let length = usize::try_from(length)
                    .map_err(|_| DbError::storage("ZEROBLOB length is too large"))?;
                Ok(Value::Blob(vec![0; length]))
            }
            ScalarExpr::Function {
                func: ScalarFunc::NullIf,
                args,
            } => {
                if args.len() != 2 {
                    return Err(DbError::storage(
                        "NULLIF trigger expressions require two arguments",
                    ));
                }
                let left = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                let right = Self::evaluate_simple_trigger_expr_with_context(context, &args[1])?;
                if Self::compare_values(&left, &right).ok().flatten() == Some(Ordering::Equal) {
                    Ok(Value::Null)
                } else {
                    Ok(left)
                }
            }
            ScalarExpr::Function { func, args }
                if matches!(func, ScalarFunc::Coalesce | ScalarFunc::IfNull) =>
            {
                if matches!(func, ScalarFunc::Coalesce) && args.len() < 2 {
                    return Err(DbError::storage(
                        "COALESCE trigger expressions require at least two arguments",
                    ));
                }
                if matches!(func, ScalarFunc::IfNull) && args.len() != 2 {
                    return Err(DbError::storage(
                        "IFNULL trigger expressions require two arguments",
                    ));
                }
                for arg in args {
                    let value = Self::evaluate_simple_trigger_expr_with_context(context, arg)?;
                    if !matches!(value, Value::Null) {
                        return Ok(value);
                    }
                }
                Ok(Value::Null)
            }
            ScalarExpr::Function {
                func: ScalarFunc::Replace,
                args,
            } => {
                if args.len() != 3 {
                    return Err(DbError::storage(
                        "REPLACE trigger expressions require three arguments",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                let pattern = Self::evaluate_simple_trigger_expr_with_context(context, &args[1])?;
                let replacement =
                    Self::evaluate_simple_trigger_expr_with_context(context, &args[2])?;
                if matches!(value, Value::Null)
                    || matches!(pattern, Value::Null)
                    || matches!(replacement, Value::Null)
                {
                    return Ok(Value::Null);
                }
                let value = Self::trigger_value_to_text(&value);
                let pattern = Self::trigger_value_to_text(&pattern);
                if pattern.is_empty() {
                    return Ok(Value::Text(value));
                }
                Ok(Value::Text(value.replace(
                    &pattern,
                    &Self::trigger_value_to_text(&replacement),
                )))
            }
            ScalarExpr::Function { func, args }
                if matches!(
                    func,
                    ScalarFunc::Trim | ScalarFunc::LTrim | ScalarFunc::RTrim
                ) =>
            {
                if !matches!(args.len(), 1 | 2) {
                    return Err(DbError::storage(
                        "TRIM trigger expressions require one or two arguments",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let characters = if args.len() == 2 {
                    let characters =
                        Self::evaluate_simple_trigger_expr_with_context(context, &args[1])?;
                    if matches!(characters, Value::Null) {
                        return Ok(Value::Null);
                    }
                    Self::trigger_value_to_text(&characters)
                } else {
                    " ".to_string()
                };
                let value = Self::trigger_value_to_text(&value);
                Ok(Value::Text(match func {
                    ScalarFunc::LTrim => value
                        .trim_start_matches(|ch| characters.contains(ch))
                        .to_string(),
                    ScalarFunc::RTrim => value
                        .trim_end_matches(|ch| characters.contains(ch))
                        .to_string(),
                    _ => value.trim_matches(|ch| characters.contains(ch)).to_string(),
                }))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Abs,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "ABS trigger expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                trigger_abs_value(&value)
            }
            ScalarExpr::Function {
                func: ScalarFunc::Instr,
                args,
            } => {
                if args.len() != 2 {
                    return Err(DbError::storage(
                        "INSTR trigger expressions require two arguments",
                    ));
                }
                let haystack = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                let needle = Self::evaluate_simple_trigger_expr_with_context(context, &args[1])?;
                if matches!(haystack, Value::Null) || matches!(needle, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(match (&haystack, &needle) {
                    (Value::Blob(haystack), Value::Blob(needle)) => {
                        Value::Integer(trigger_instr_blob(haystack, needle))
                    }
                    _ => {
                        let haystack = Self::trigger_value_to_text(&haystack);
                        let needle = Self::trigger_value_to_text(&needle);
                        if needle.is_empty() {
                            Value::Integer(1)
                        } else {
                            Value::Integer(
                                haystack
                                    .find(&needle)
                                    .map(|byte_index| {
                                        haystack[..byte_index].chars().count() as i64 + 1
                                    })
                                    .unwrap_or(0),
                            )
                        }
                    }
                })
            }
            ScalarExpr::Function {
                func: ScalarFunc::TypeOf,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "TYPEOF trigger expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                Ok(Value::Text(
                    match value {
                        Value::Null => "null",
                        Value::Boolean(_) | Value::Integer(_) => "integer",
                        Value::Real(_) => "real",
                        Value::Blob(_) => "blob",
                        Value::Text(_) => "text",
                    }
                    .to_string(),
                ))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Round,
                args,
            } => {
                if !matches!(args.len(), 1 | 2) {
                    return Err(DbError::storage(
                        "ROUND trigger expressions require one or two arguments",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let value = trigger_value_to_f64(&value)?;
                let precision = if args.len() == 2 {
                    let precision =
                        Self::evaluate_simple_trigger_expr_with_context(context, &args[1])?;
                    if matches!(precision, Value::Null) {
                        return Ok(Value::Null);
                    }
                    i32::try_from(trigger_value_to_i64(&precision)?)
                        .map_err(|_| DbError::storage("ROUND precision does not fit in i32"))?
                } else {
                    0
                };
                Ok(Value::Real(sqlite_round_f64(value, precision)))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Quote,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "QUOTE trigger expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                Ok(Value::Text(trigger_quote_value(&value)))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Hex,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "HEX trigger expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                Ok(Value::Text(match value {
                    Value::Null => String::new(),
                    Value::Blob(value) => trigger_hex_bytes(&value),
                    value => trigger_hex_bytes(Self::trigger_value_to_text(&value).as_bytes()),
                }))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Char,
                args,
            } => {
                let mut result = String::new();
                for arg in args {
                    let value = Self::evaluate_simple_trigger_expr_with_context(context, arg)?;
                    if matches!(value, Value::Null) {
                        continue;
                    }
                    let code_point = trigger_value_to_i64(&value)?;
                    let ch = u32::try_from(code_point)
                        .ok()
                        .and_then(char::from_u32)
                        .unwrap_or(char::REPLACEMENT_CHARACTER);
                    result.push(ch);
                }
                Ok(Value::Text(result))
            }
            ScalarExpr::Function {
                func: ScalarFunc::Unicode,
                args,
            } => {
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "UNICODE trigger expressions require one argument",
                    ));
                }
                let value = Self::evaluate_simple_trigger_expr_with_context(context, &args[0])?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(Self::trigger_value_to_text(&value)
                    .chars()
                    .next()
                    .map(|ch| Value::Integer(i64::from(u32::from(ch))))
                    .unwrap_or(Value::Null))
            }
            ScalarExpr::Function { func, args }
                if matches!(func, ScalarFunc::MinScalar | ScalarFunc::MaxScalar) =>
            {
                if args.is_empty() {
                    return Err(DbError::storage(
                        "MIN/MAX trigger expressions require at least one argument",
                    ));
                }
                let values = args
                    .iter()
                    .map(|arg| Self::evaluate_simple_trigger_expr_with_context(context, arg))
                    .collect::<Result<Vec<_>>>()?;
                Self::evaluate_trigger_min_max_scalar(
                    &values,
                    matches!(func, ScalarFunc::MinScalar),
                )
            }
            _ => Self::evaluate_simple_trigger_expr(
                context.source_schema,
                context.old_row,
                context.new_row,
                expr,
            ),
        }
    }

    fn evaluate_trigger_min_max_scalar(args: &[Value], want_min: bool) -> Result<Value> {
        if args.iter().any(|value| matches!(value, Value::Null)) {
            return Ok(Value::Null);
        }

        let mut best = args
            .first()
            .cloned()
            .ok_or_else(|| DbError::storage("MIN/MAX expects at least 1 argument"))?;
        for candidate in args.iter().skip(1) {
            let ordering = Self::compare_trigger_min_max_scalar_values(candidate, &best);
            let replace = if want_min {
                matches!(ordering, Ordering::Less | Ordering::Equal)
            } else {
                ordering == Ordering::Greater
            };
            if replace {
                best = candidate.clone();
            }
        }

        Ok(match best {
            Value::Boolean(value) => Value::Integer(if value { 1 } else { 0 }),
            value => value,
        })
    }

    fn compare_trigger_min_max_scalar_values(left: &Value, right: &Value) -> Ordering {
        if let Some(ordering) = Self::compare_trigger_same_storage_class(left, right) {
            return ordering;
        }
        Self::trigger_min_max_storage_class_rank(left)
            .cmp(&Self::trigger_min_max_storage_class_rank(right))
    }

    fn compare_trigger_same_storage_class(left: &Value, right: &Value) -> Option<Ordering> {
        match (left, right) {
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
        }
    }

    fn trigger_min_max_storage_class_rank(value: &Value) -> u8 {
        match value {
            Value::Null => 0,
            Value::Boolean(_) | Value::Integer(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
        }
    }

    fn evaluate_simple_trigger_expr(
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        expr: &ScalarExpr,
    ) -> Result<Value> {
        match expr {
            ScalarExpr::Literal(value) => Ok(value.clone()),
            ScalarExpr::Column(_) => {
                Self::evaluate_trigger_column_expr(source_schema, old_row, new_row, expr)
            }
            ScalarExpr::UnaryPlus(expr) | ScalarExpr::Collate { expr, .. } => {
                Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, expr)
            }
            ScalarExpr::UnaryMinus(expr) => {
                let value =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, expr)?;
                match value {
                    Value::Null => Ok(Value::Null),
                    Value::Integer(value) => value
                        .checked_neg()
                        .map(Value::Integer)
                        .ok_or_else(|| DbError::storage("integer overflow")),
                    Value::Real(value) => Ok(Value::Real(-value)),
                    value => Ok(Value::Real(-trigger_value_to_f64(&value)?)),
                }
            }
            ScalarExpr::BitNot(expr) => {
                let value =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, expr)?;
                match value {
                    Value::Integer(value) => Ok(Value::Integer(!value)),
                    Value::Null => Ok(Value::Null),
                    value => Err(DbError::storage(format!(
                        "cannot bitwise-not {} in trigger expression",
                        value.type_name()
                    ))),
                }
            }
            ScalarExpr::Cast { expr, ty } => {
                let value =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, expr)?;
                trigger_cast_value(value, *ty)
            }
            ScalarExpr::Not(expr) => {
                let value =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, expr)?;
                Ok(match value {
                    Value::Null => Value::Null,
                    value => Value::Integer(if Self::sqlite_truthy(&value) { 0 } else { 1 }),
                })
            }
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                Ok(Value::Integer(
                    if Self::is_trigger_is_match(&left, &right) ^ *negated {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => {
                let evaluated =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, expr)?;
                let matches = !matches!(evaluated, Value::Null)
                    && trigger_is_true_value(&evaluated) == *value;
                Ok(Value::Integer(if matches ^ *negated { 1 } else { 0 }))
            }
            ScalarExpr::Compare { left, op, right } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                if matches!(left, Value::Null) || matches!(right, Value::Null) {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Integer(
                        if Self::compare_with_operator(&left, *op, &right)? {
                            1
                        } else {
                            0
                        },
                    ))
                }
            }
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                let base = base
                    .as_ref()
                    .map(|base| {
                        Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, base)
                    })
                    .transpose()?;
                for (when_expr, then_expr) in when_then_clauses {
                    let when_value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        when_expr,
                    )?;
                    let matches = if let Some(base) = &base {
                        Self::is_trigger_is_match(base, &when_value)
                    } else {
                        !matches!(when_value, Value::Null) && Self::sqlite_truthy(&when_value)
                    };
                    if matches {
                        return Self::evaluate_simple_trigger_expr(
                            source_schema,
                            old_row,
                            new_row,
                            then_expr,
                        );
                    }
                }
                else_expr
                    .as_ref()
                    .map(|else_expr| {
                        Self::evaluate_simple_trigger_expr(
                            source_schema,
                            old_row,
                            new_row,
                            else_expr,
                        )
                    })
                    .unwrap_or(Ok(Value::Null))
            }
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, expr)?;
                let values = values
                    .iter()
                    .map(|value| {
                        Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, value)
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Self::trigger_in_membership(&left, &values)
                    .map(|matches| Value::Integer(if matches ^ *negated { 1 } else { 0 }))
                    .unwrap_or(Value::Null))
            }
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let value =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, expr)?;
                let low = Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, low)?;
                let high =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, high)?;
                if matches!(value, Value::Null)
                    || matches!(low, Value::Null)
                    || matches!(high, Value::Null)
                {
                    return Ok(Value::Null);
                }
                let lower_match = Self::compare_with_operator(&value, CompareOp::Gte, &low)?;
                let upper_match = Self::compare_with_operator(&value, CompareOp::Lte, &high)?;
                Ok(Value::Integer(if (lower_match && upper_match) ^ *negated {
                    1
                } else {
                    0
                }))
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Add,
                right,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(
                        left.checked_add(right)
                            .ok_or_else(|| DbError::storage("integer overflow"))?,
                    )),
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot add {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Subtract,
                right,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(
                        left.checked_sub(right)
                            .ok_or_else(|| DbError::storage("integer overflow"))?,
                    )),
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot subtract {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Multiply,
                right,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(
                        left.checked_mul(right)
                            .ok_or_else(|| DbError::storage("integer overflow"))?,
                    )),
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot multiply {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Divide,
                right,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                match (left, right) {
                    (Value::Integer(_), Value::Integer(0)) => Ok(Value::Null),
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left / right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot divide {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Modulo,
                right,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                match (left, right) {
                    (Value::Integer(_), Value::Integer(0)) => Ok(Value::Null),
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left % right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot modulo {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::BitAnd,
                right,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left & right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot bitwise-and {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::BitOr,
                right,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(left | right))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot bitwise-or {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::ShiftLeft,
                right,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(Self::trigger_shift_op(left, right, true)))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot shift {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::ShiftRight,
                right,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                match (left, right) {
                    (Value::Integer(left), Value::Integer(right)) => {
                        Ok(Value::Integer(Self::trigger_shift_op(left, right, false)))
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (left, right) => Err(DbError::storage(format!(
                        "cannot shift {} and {} in trigger expression",
                        left.type_name(),
                        right.type_name()
                    ))),
                }
            }
            ScalarExpr::Binary {
                left,
                op: ScalarBinaryOp::Concat,
                right,
            } => {
                let left =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, left)?;
                let right =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, right)?;
                if matches!(left, Value::Null) || matches!(right, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(Value::Text(format!(
                    "{}{}",
                    Self::trigger_value_to_text(&left),
                    Self::trigger_value_to_text(&right)
                )))
            }
            ScalarExpr::Function { func, args }
                if matches!(
                    func,
                    ScalarFunc::Lower
                        | ScalarFunc::Upper
                        | ScalarFunc::Coalesce
                        | ScalarFunc::IfNull
                        | ScalarFunc::Abs
                        | ScalarFunc::Length
                        | ScalarFunc::Substr
                        | ScalarFunc::Trim
                        | ScalarFunc::LTrim
                        | ScalarFunc::RTrim
                        | ScalarFunc::Replace
                        | ScalarFunc::Instr
                        | ScalarFunc::Round
                        | ScalarFunc::TypeOf
                        | ScalarFunc::Quote
                        | ScalarFunc::Unicode
                        | ScalarFunc::Char
                        | ScalarFunc::Hex
                        | ScalarFunc::NullIf
                        | ScalarFunc::ZeroBlob
                ) =>
            {
                if matches!(func, ScalarFunc::ZeroBlob) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "ZEROBLOB trigger expressions require one argument",
                        ));
                    }
                    let length = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    let length = if matches!(length, Value::Null) {
                        0
                    } else {
                        trigger_value_to_i64(&length)?.max(0)
                    };
                    let length = usize::try_from(length)
                        .map_err(|_| DbError::storage("ZEROBLOB length is too large"))?;
                    return Ok(Value::Blob(vec![0; length]));
                }
                if matches!(func, ScalarFunc::NullIf) {
                    if args.len() != 2 {
                        return Err(DbError::storage(
                            "NULLIF trigger expressions require two arguments",
                        ));
                    }
                    let left = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    let right = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[1],
                    )?;
                    if Self::compare_values(&left, &right).ok().flatten() == Some(Ordering::Equal) {
                        return Ok(Value::Null);
                    }
                    return Ok(left);
                }
                if matches!(func, ScalarFunc::Hex) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "HEX trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    return Ok(Value::Text(match value {
                        Value::Null => String::new(),
                        Value::Blob(value) => trigger_hex_bytes(&value),
                        value => trigger_hex_bytes(Self::trigger_value_to_text(&value).as_bytes()),
                    }));
                }
                if matches!(func, ScalarFunc::Char) {
                    let mut result = String::new();
                    for arg in args {
                        let value = Self::evaluate_simple_trigger_expr(
                            source_schema,
                            old_row,
                            new_row,
                            arg,
                        )?;
                        if matches!(value, Value::Null) {
                            continue;
                        }
                        let code_point = trigger_value_to_i64(&value)?;
                        let ch = u32::try_from(code_point)
                            .ok()
                            .and_then(char::from_u32)
                            .unwrap_or(char::REPLACEMENT_CHARACTER);
                        result.push(ch);
                    }
                    return Ok(Value::Text(result));
                }
                if matches!(func, ScalarFunc::Unicode) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "UNICODE trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    if matches!(value, Value::Null) {
                        return Ok(Value::Null);
                    }
                    return Ok(Self::trigger_value_to_text(&value)
                        .chars()
                        .next()
                        .map(|ch| Value::Integer(i64::from(u32::from(ch))))
                        .unwrap_or(Value::Null));
                }
                if matches!(func, ScalarFunc::Quote) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "QUOTE trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    return Ok(Value::Text(trigger_quote_value(&value)));
                }
                if matches!(func, ScalarFunc::TypeOf) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "TYPEOF trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    return Ok(Value::Text(
                        match value {
                            Value::Null => "null",
                            Value::Boolean(_) | Value::Integer(_) => "integer",
                            Value::Real(_) => "real",
                            Value::Blob(_) => "blob",
                            Value::Text(_) => "text",
                        }
                        .to_string(),
                    ));
                }
                if matches!(func, ScalarFunc::Round) {
                    if !matches!(args.len(), 1 | 2) {
                        return Err(DbError::storage(
                            "ROUND trigger expressions require one or two arguments",
                        ));
                    }
                    let value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    if matches!(value, Value::Null) {
                        return Ok(Value::Null);
                    }
                    let value = trigger_value_to_f64(&value)?;
                    let precision = if args.len() == 2 {
                        let precision = Self::evaluate_simple_trigger_expr(
                            source_schema,
                            old_row,
                            new_row,
                            &args[1],
                        )?;
                        if matches!(precision, Value::Null) {
                            return Ok(Value::Null);
                        }
                        i32::try_from(trigger_value_to_i64(&precision)?)
                            .map_err(|_| DbError::storage("ROUND precision does not fit in i32"))?
                    } else {
                        0
                    };
                    return Ok(Value::Real(sqlite_round_f64(value, precision)));
                }
                if matches!(func, ScalarFunc::Instr) {
                    if args.len() != 2 {
                        return Err(DbError::storage(
                            "INSTR trigger expressions require two arguments",
                        ));
                    }
                    let haystack = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    let needle = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[1],
                    )?;
                    if matches!(haystack, Value::Null) || matches!(needle, Value::Null) {
                        return Ok(Value::Null);
                    }
                    return Ok(match (&haystack, &needle) {
                        (Value::Blob(haystack), Value::Blob(needle)) => {
                            Value::Integer(trigger_instr_blob(haystack, needle))
                        }
                        _ => {
                            let haystack = Self::trigger_value_to_text(&haystack);
                            let needle = Self::trigger_value_to_text(&needle);
                            if needle.is_empty() {
                                Value::Integer(1)
                            } else {
                                Value::Integer(
                                    haystack
                                        .find(&needle)
                                        .map(|byte_index| {
                                            haystack[..byte_index].chars().count() as i64 + 1
                                        })
                                        .unwrap_or(0),
                                )
                            }
                        }
                    });
                }
                if matches!(func, ScalarFunc::Replace) {
                    if args.len() != 3 {
                        return Err(DbError::storage(
                            "REPLACE trigger expressions require three arguments",
                        ));
                    }
                    let value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    let pattern = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[1],
                    )?;
                    let replacement = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[2],
                    )?;
                    if matches!(value, Value::Null)
                        || matches!(pattern, Value::Null)
                        || matches!(replacement, Value::Null)
                    {
                        return Ok(Value::Null);
                    }
                    let value = Self::trigger_value_to_text(&value);
                    let pattern = Self::trigger_value_to_text(&pattern);
                    if pattern.is_empty() {
                        return Ok(Value::Text(value));
                    }
                    return Ok(Value::Text(
                        value.replace(&pattern, &Self::trigger_value_to_text(&replacement)),
                    ));
                }
                if matches!(
                    func,
                    ScalarFunc::Trim | ScalarFunc::LTrim | ScalarFunc::RTrim
                ) {
                    if !matches!(args.len(), 1 | 2) {
                        return Err(DbError::storage(
                            "TRIM trigger expressions require one or two arguments",
                        ));
                    }
                    let value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    if matches!(value, Value::Null) {
                        return Ok(Value::Null);
                    }
                    let characters = if args.len() == 2 {
                        let characters = Self::evaluate_simple_trigger_expr(
                            source_schema,
                            old_row,
                            new_row,
                            &args[1],
                        )?;
                        if matches!(characters, Value::Null) {
                            return Ok(Value::Null);
                        }
                        Self::trigger_value_to_text(&characters)
                    } else {
                        " ".to_string()
                    };
                    let value = Self::trigger_value_to_text(&value);
                    return Ok(Value::Text(match func {
                        ScalarFunc::LTrim => value
                            .trim_start_matches(|ch| characters.contains(ch))
                            .to_string(),
                        ScalarFunc::RTrim => value
                            .trim_end_matches(|ch| characters.contains(ch))
                            .to_string(),
                        _ => value.trim_matches(|ch| characters.contains(ch)).to_string(),
                    }));
                }
                if matches!(func, ScalarFunc::Substr) {
                    if !matches!(args.len(), 2 | 3) {
                        return Err(DbError::storage(
                            "SUBSTR trigger expressions require two or three arguments",
                        ));
                    }
                    let value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    if matches!(value, Value::Null) {
                        return Ok(Value::Null);
                    }
                    let start = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[1],
                    )?;
                    let start = trigger_value_to_i64(&start)?;
                    let length = if args.len() == 3 {
                        let length = Self::evaluate_simple_trigger_expr(
                            source_schema,
                            old_row,
                            new_row,
                            &args[2],
                        )?;
                        Some(trigger_value_to_i64(&length)?)
                    } else {
                        None
                    };
                    return Ok(match value {
                        Value::Blob(value) => {
                            Value::Blob(trigger_substr_blob(&value, start, length))
                        }
                        value => Value::Text(trigger_substr_text(
                            &Self::trigger_value_to_text(&value),
                            start,
                            length,
                        )),
                    });
                }
                if matches!(func, ScalarFunc::Length) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "LENGTH trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    return Ok(match value {
                        Value::Null => Value::Null,
                        Value::Blob(value) => Value::Integer(value.len() as i64),
                        value => Value::Integer(
                            Self::trigger_value_to_text(&value).chars().count() as i64,
                        ),
                    });
                }
                if matches!(func, ScalarFunc::Abs) {
                    if args.len() != 1 {
                        return Err(DbError::storage(
                            "ABS trigger expressions require one argument",
                        ));
                    }
                    let value = Self::evaluate_simple_trigger_expr(
                        source_schema,
                        old_row,
                        new_row,
                        &args[0],
                    )?;
                    if matches!(value, Value::Null) {
                        return Ok(Value::Null);
                    }
                    return trigger_abs_value(&value);
                }
                if matches!(func, ScalarFunc::Coalesce | ScalarFunc::IfNull) {
                    if matches!(func, ScalarFunc::Coalesce) && args.len() < 2 {
                        return Err(DbError::storage(
                            "COALESCE trigger expressions require at least two arguments",
                        ));
                    }
                    if matches!(func, ScalarFunc::IfNull) && args.len() != 2 {
                        return Err(DbError::storage(
                            "IFNULL trigger expressions require two arguments",
                        ));
                    }
                    for arg in args {
                        let value = Self::evaluate_simple_trigger_expr(
                            source_schema,
                            old_row,
                            new_row,
                            arg,
                        )?;
                        if !matches!(value, Value::Null) {
                            return Ok(value);
                        }
                    }
                    return Ok(Value::Null);
                }
                if args.len() != 1 {
                    return Err(DbError::storage(
                        "LOWER/UPPER trigger expressions require one argument",
                    ));
                }
                let value =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, &args[0])?;
                if matches!(value, Value::Null) {
                    return Ok(Value::Null);
                }
                let text = Self::trigger_value_to_text(&value);
                Ok(Value::Text(if matches!(func, ScalarFunc::Upper) {
                    sqlite_ascii_upper(&text)
                } else {
                    sqlite_ascii_lower(&text)
                }))
            }
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => {
                let value =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, expr)?;
                let pattern =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, pattern)?;
                if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                let escape_value = escape
                    .as_ref()
                    .map(|escape| {
                        Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, escape)
                    })
                    .transpose()?;
                let escape_char = match escape_value {
                    Some(Value::Null) => return Ok(Value::Null),
                    Some(escape) => Some(Self::trigger_escape_char(&escape)?),
                    None => None,
                };
                Ok(Value::Integer(
                    if Self::trigger_like_matches(
                        &Self::trigger_value_to_text(&value),
                        &Self::trigger_value_to_text(&pattern),
                        escape_char,
                    ) ^ *negated
                    {
                        1
                    } else {
                        0
                    },
                ))
            }
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => {
                let value =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, expr)?;
                let pattern =
                    Self::evaluate_simple_trigger_expr(source_schema, old_row, new_row, pattern)?;
                if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                Ok(Value::Integer(
                    if Self::trigger_glob_matches(
                        &Self::trigger_value_to_text(&value),
                        &Self::trigger_value_to_text(&pattern),
                    ) ^ *negated
                    {
                        1
                    } else {
                        0
                    },
                ))
            }
            _ => Err(DbError::storage(
                "only simple OLD/NEW trigger WHEN expressions are supported",
            )),
        }
    }

    fn evaluate_trigger_column_expr(
        source_schema: &Schema,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
        expr: &ScalarExpr,
    ) -> Result<Value> {
        let ScalarExpr::Column(name) = expr else {
            return Err(DbError::storage("expected OLD/NEW trigger column"));
        };
        let (row, column) = if let Some(column) = name.strip_prefix("new.") {
            let new_row = new_row
                .ok_or_else(|| DbError::storage("NEW column trigger values require a new row"))?;
            (new_row, column)
        } else if let Some(column) = name.strip_prefix("old.") {
            let old_row = old_row
                .ok_or_else(|| DbError::storage("OLD column trigger values require an old row"))?;
            (old_row, column)
        } else {
            return Err(DbError::storage(
                "only OLD/NEW column trigger values are supported",
            ));
        };
        let index = source_schema.column_index(column)?;
        row.get(index)
            .cloned()
            .ok_or_else(|| DbError::storage(format!("row is missing column {column}")))
    }

    fn evaluate_trigger_column_expr_with_context(
        context: &SimpleTriggerEvalContext<'_>,
        expr: &ScalarExpr,
    ) -> Result<Value> {
        let ScalarExpr::Column(name) = expr else {
            return Err(DbError::storage("expected trigger column"));
        };
        if name.starts_with("new.") || name.starts_with("old.") {
            return Self::evaluate_trigger_column_expr(
                context.source_schema,
                context.old_row,
                context.new_row,
                expr,
            );
        }
        let Some(select_schema) = context.select_schema else {
            return Self::evaluate_trigger_column_expr(
                context.source_schema,
                context.old_row,
                context.new_row,
                expr,
            );
        };
        let select_row = context
            .select_row
            .ok_or_else(|| DbError::storage("trigger SELECT row is missing"))?;
        let column = if let Some((qualifier, column)) = name.split_once('.') {
            let matches_table = context
                .select_table_name
                .is_some_and(|table| qualifier.eq_ignore_ascii_case(table));
            let matches_alias = context
                .select_alias
                .is_some_and(|alias| qualifier.eq_ignore_ascii_case(alias));
            if matches_table || matches_alias {
                column
            } else {
                name
            }
        } else {
            name
        };
        let index = match select_schema.column_index(column) {
            Ok(index) => index,
            Err(_error)
                if matches!(
                    column.to_ascii_lowercase().as_str(),
                    "rowid" | "_rowid_" | "oid"
                ) =>
            {
                let row_id = context
                    .select_row_id
                    .ok_or_else(|| DbError::storage("trigger SELECT rowid is missing"))?;
                return i64::try_from(row_id.0)
                    .map(Value::Integer)
                    .map_err(|_| DbError::storage("sqlite rowid does not fit in i64"));
            }
            Err(error) => return Err(error),
        };
        select_row
            .get(index)
            .cloned()
            .ok_or_else(|| DbError::storage(format!("row is missing column {column}")))
    }

    fn sqlite_truthy(value: &Value) -> bool {
        match value {
            Value::Null => false,
            Value::Boolean(value) => *value,
            Value::Integer(value) => *value != 0,
            Value::Real(value) => *value != 0.0,
            Value::Text(value) => !value.is_empty() && value != "0",
            Value::Blob(value) => !value.is_empty() && value != b"0",
        }
    }

    fn is_trigger_is_match(left: &Value, right: &Value) -> bool {
        matches!((left, right), (Value::Null, Value::Null))
            || (!matches!(left, Value::Null)
                && !matches!(right, Value::Null)
                && Self::compare_values(left, right).ok().flatten() == Some(Ordering::Equal))
    }

    fn compare_values(left: &Value, right: &Value) -> Result<Option<Ordering>> {
        Ok(match (left, right) {
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
            (Value::Null, _) | (_, Value::Null) => None,
            _ => {
                return Err(DbError::storage(format!(
                    "cannot compare {} with {} in sqlite index range scan",
                    left.type_name(),
                    right.type_name()
                )));
            }
        })
    }

    fn compare_values_nocase(left: &Value, right: &Value) -> Result<Option<Ordering>> {
        Ok(match (left, right) {
            (Value::Text(left), Value::Text(right)) => {
                Some(sqlite_ascii_lower(left).cmp(&sqlite_ascii_lower(right)))
            }
            _ => Self::compare_values(left, right)?,
        })
    }

    fn compare_with_operator(left: &Value, op: CompareOp, right: &Value) -> Result<bool> {
        let Some(ordering) = Self::compare_values(left, right)? else {
            return Ok(false);
        };
        Ok(match op {
            CompareOp::Eq => ordering == Ordering::Equal,
            CompareOp::Ne => ordering != Ordering::Equal,
            CompareOp::Gt => ordering == Ordering::Greater,
            CompareOp::Gte => matches!(ordering, Ordering::Greater | Ordering::Equal),
            CompareOp::Lt => ordering == Ordering::Less,
            CompareOp::Lte => matches!(ordering, Ordering::Less | Ordering::Equal),
        })
    }

    fn row_matches_index_range(
        &self,
        schema: &Schema,
        index: &IndexMeta,
        row: &Row,
        key_prefix: &[Value],
        lower: Option<(CompareOp, &Value)>,
        upper: Option<(CompareOp, &Value)>,
    ) -> Result<bool> {
        let key = self.project_index_key(schema, index, row)?;
        if !key.starts_with(key_prefix) {
            return Ok(false);
        }

        let range_value = key.get(key_prefix.len()).ok_or_else(|| {
            DbError::storage(format!(
                "index {} has no range column after prefix of length {}",
                index.name,
                key_prefix.len()
            ))
        })?;
        if let Some((op, value)) = lower {
            if !Self::compare_with_operator(range_value, op, value)? {
                return Ok(false);
            }
        }
        if let Some((op, value)) = upper {
            if !Self::compare_with_operator(range_value, op, value)? {
                return Ok(false);
            }
        }

        Ok(true)
    }

    fn validate_unique_indexes_for_row(
        &self,
        schema: &Schema,
        indexes: Option<&std::collections::BTreeMap<String, IndexMeta>>,
        existing_rows: &[(RowId, Row)],
        candidate_row: &Row,
    ) -> Result<()> {
        let Some(indexes) = indexes else {
            return Ok(());
        };

        let inline_primary_key_columns = schema
            .columns
            .iter()
            .filter(|column| column.primary_key)
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        for index in indexes.values().filter(|index| {
            index.unique
                && !index
                    .name
                    .starts_with(&format!("sqlite_autoindex_{}_", schema.name))
                || (index.unique
                    && !schema
                        .primary_key_constraint
                        .as_ref()
                        .is_some_and(|primary_key| primary_key.columns == index.columns)
                    && (inline_primary_key_columns.is_empty()
                        || index.columns != inline_primary_key_columns))
        }) {
            if !self.row_matches_partial_index(schema, index, candidate_row)? {
                continue;
            }
            let candidate_key = self.project_index_key(schema, index, candidate_row)?;
            if !index.enforces_unique_key(&candidate_key) {
                continue;
            }

            for (_, existing_row) in existing_rows {
                if !self.row_matches_partial_index(schema, index, existing_row)? {
                    continue;
                }
                let existing_key = self.project_index_key(schema, index, existing_row)?;
                if existing_key == candidate_key {
                    return Err(DbError::storage(format!(
                        "unique index {} constraint failed",
                        index.name
                    )));
                }
            }
        }

        Ok(())
    }
}

impl PlanningStorageEngine for FileStorage {
    fn planning_context_snapshot(
        &self,
        transaction_id: Option<TransactionId>,
    ) -> Result<PlanningContext> {
        if let Some(transaction_id) = transaction_id {
            self.validate_transaction(transaction_id)?;
        }

        let writable = self.writable_view();
        let schemas = writable
            .tables
            .iter()
            .map(|(name, table)| (name.clone(), table.schema.clone()))
            .collect::<HashMap<_, _>>();
        let indexes = self
            .writable_view()
            .indexes
            .iter()
            .map(|(table, entries)| {
                (
                    table.clone(),
                    entries.values().cloned().collect::<Vec<IndexMeta>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        Ok(PlanningContext::new(schemas, indexes))
    }

    fn database_path(&self) -> Option<PathBuf> {
        self.path
            .as_ref()
            .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
    }

    fn journal_mode(&self) -> &'static str {
        "delete"
    }

    fn ignore_check_constraints(&self) -> bool {
        *self.ignore_check_constraints.borrow()
    }

    fn set_ignore_check_constraints(&self, enabled: bool) -> Result<()> {
        *self.ignore_check_constraints.borrow_mut() = enabled;
        Ok(())
    }

    fn case_sensitive_like(&self) -> bool {
        *self.case_sensitive_like.borrow()
    }

    fn set_case_sensitive_like(&self, enabled: bool) -> Result<()> {
        *self.case_sensitive_like.borrow_mut() = enabled;
        Ok(())
    }

    fn database_page_size(&self) -> u32 {
        self.pager
            .borrow()
            .as_ref()
            .map(|pager| pager.header().page_size)
            .unwrap_or(4096)
    }

    fn database_page_count(&self) -> Result<u32> {
        self.pager
            .borrow()
            .as_ref()
            .map_or(Ok(0), |pager| pager.page_count())
    }

    fn database_freelist_count(&self) -> Result<u32> {
        Ok(self
            .pager
            .borrow()
            .as_ref()
            .map(|pager| pager.header().freelist_count)
            .unwrap_or(0))
    }

    fn user_version(&self) -> Result<u32> {
        Ok(self.writable_view().user_version)
    }

    fn set_user_version(&self, version: u32) -> Result<()> {
        let active_txn = self.txn_state.borrow().active_txn;
        if let Some(transaction_id) = active_txn {
            self.with_pending_writable_mut(transaction_id, |database| {
                database.user_version = version;
                Ok(())
            })
        } else {
            let mut database = self.writable.borrow().clone();
            database.user_version = version;
            let path = self.path.clone().ok_or_else(|| {
                DbError::storage("sqlite3 FileStorage is not backed by a database file")
            })?;
            write_database(&path, &database)?;
            let pager = Pager::open(&path)?;
            let catalog = load_catalog(&pager)?;
            *self.catalog.borrow_mut() = catalog;
            *self.pager.borrow_mut() = Some(pager);
            *self.writable.borrow_mut() = database;
            Ok(())
        }
    }

    fn application_id(&self) -> Result<u32> {
        Ok(self.writable_view().application_id)
    }

    fn set_application_id(&self, application_id: u32) -> Result<()> {
        let active_txn = self.txn_state.borrow().active_txn;
        if let Some(transaction_id) = active_txn {
            self.with_pending_writable_mut(transaction_id, |database| {
                database.application_id = application_id;
                Ok(())
            })
        } else {
            let mut database = self.writable.borrow().clone();
            database.application_id = application_id;
            let path = self.path.clone().ok_or_else(|| {
                DbError::storage("sqlite3 FileStorage is not backed by a database file")
            })?;
            write_database(&path, &database)?;
            let pager = Pager::open(&path)?;
            let catalog = load_catalog(&pager)?;
            *self.catalog.borrow_mut() = catalog;
            *self.pager.borrow_mut() = Some(pager);
            *self.writable.borrow_mut() = database;
            Ok(())
        }
    }

    fn schema_version(&self) -> Result<u32> {
        Ok(self.writable_view().schema_version)
    }

    fn set_schema_version(&self, schema_version: u32) -> Result<()> {
        let active_txn = self.txn_state.borrow().active_txn;
        if let Some(transaction_id) = active_txn {
            self.with_pending_writable_mut(transaction_id, |database| {
                database.schema_version = schema_version;
                Ok(())
            })
        } else {
            let mut database = self.writable.borrow().clone();
            database.schema_version = schema_version;
            let path = self.path.clone().ok_or_else(|| {
                DbError::storage("sqlite3 FileStorage is not backed by a database file")
            })?;
            write_database(&path, &database)?;
            let pager = Pager::open(&path)?;
            let catalog = load_catalog(&pager)?;
            *self.catalog.borrow_mut() = catalog;
            *self.pager.borrow_mut() = Some(pager);
            *self.writable.borrow_mut() = database;
            Ok(())
        }
    }
}

impl CatalogStore for FileStorage {
    fn create_schema(&self, transaction_id: TransactionId, schema: Schema) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            schema.validate_constraints_metadata()?;
            if database.tables.contains_key(&schema.name) {
                return Err(DbError::storage(format!(
                    "table {} already exists",
                    schema.name
                )));
            }
            let schema_name = schema.name.clone();
            let rowid_primary_key_index = if !schema.without_rowid {
                let primary_key_columns = schema
                    .columns
                    .iter()
                    .filter(|column| column.primary_key)
                    .map(|column| column.name.clone())
                    .collect::<Vec<_>>();
                let needs_inline_primary_key_autoindex = primary_key_columns.len() == 1
                    && schema
                        .columns
                        .iter()
                        .find(|column| column.name == primary_key_columns[0])
                        .is_some_and(|column| !matches!(column.column_type, ColumnType::Integer));
                needs_inline_primary_key_autoindex.then(|| IndexMeta {
                    name: format!("sqlite_autoindex_{schema_name}_1"),
                    columns: primary_key_columns,
                    decorated_columns: None,
                    unique: true,
                    predicate: None,
                })
            } else {
                None
            };
            let has_autoincrement = schema
                .columns
                .iter()
                .any(|column| column.primary_key && column.autoincrement);
            database.tables.insert(
                schema_name.clone(),
                WritableTable {
                    schema,
                    rows: Vec::new(),
                },
            );
            if has_autoincrement {
                database.sqlite_sequence_exists = true;
                database.sqlite_sequence.insert(schema_name.clone(), 0);
            }
            if let Some(index) = rowid_primary_key_index {
                database
                    .indexes
                    .entry(schema_name)
                    .or_default()
                    .insert(index.name.clone(), index);
            }
            Ok(())
        })
    }

    fn create_trigger(
        &self,
        transaction_id: TransactionId,
        name: &str,
        table: &str,
        sql: &str,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            if !database.tables.contains_key(table) {
                return Err(DbError::storage(format!("unknown table: {table}")));
            }
            if database
                .extra_schema_objects
                .iter()
                .any(|object| object.entry_type == "trigger" && object.name == name)
            {
                return Err(DbError::storage(format!("trigger already exists: {name}")));
            }
            database.extra_schema_objects.push(
                crate::storage::sqlite3::writer::WritableSchemaObject {
                    entry_type: "trigger".to_string(),
                    name: name.to_string(),
                    table_name: table.to_string(),
                    root_page: 0,
                    sql: Some(sql.to_string()),
                },
            );
            Ok(())
        })
    }

    fn drop_schema(&self, transaction_id: TransactionId, _name: &str) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let name = _name;
            if database.tables.remove(name).is_none() {
                return Err(DbError::storage(format!("unknown table: {name}")));
            }
            database.indexes.remove(name);
            database.sqlite_sequence.remove(name);
            database
                .extra_schema_objects
                .retain(|object| !(object.entry_type == "trigger" && object.table_name == name));
            Ok(())
        })
    }

    fn drop_trigger(&self, transaction_id: TransactionId, name: &str) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let original_len = database.extra_schema_objects.len();
            database
                .extra_schema_objects
                .retain(|object| !(object.entry_type == "trigger" && object.name == name));
            if database.extra_schema_objects.len() == original_len {
                return Err(DbError::storage(format!("unknown trigger: {name}")));
            }
            Ok(())
        })
    }

    fn replace_schema(&self, transaction_id: TransactionId, _schema: Schema) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        Err(self.unsupported("ALTER TABLE"))
    }

    fn rename_schema(
        &self,
        transaction_id: TransactionId,
        old_name: &str,
        new_name: &str,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            if database.tables.contains_key(new_name) {
                return Err(DbError::storage(format!(
                    "table already exists: {new_name}"
                )));
            }

            let mut table = database
                .tables
                .remove(old_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {old_name}")))?;
            table.schema.name = new_name.to_string();
            for (name, other_table) in &mut database.tables {
                if name != new_name {
                    other_table
                        .schema
                        .rename_foreign_key_ref_table(old_name, new_name);
                }
            }
            table
                .schema
                .rename_foreign_key_ref_table(old_name, new_name);
            database.tables.insert(new_name.to_string(), table);

            if let Some(indexes) = database.indexes.remove(old_name) {
                database.indexes.insert(new_name.to_string(), indexes);
            }
            if let Some(seq) = database.sqlite_sequence.remove(old_name) {
                database.sqlite_sequence.insert(new_name.to_string(), seq);
            }
            for object in &mut database.extra_schema_objects {
                if object.entry_type == "trigger" && object.table_name == old_name {
                    object.table_name = new_name.to_string();
                    if let Some(sql) = &mut object.sql {
                        *sql = rename_trigger_target_table_sql(sql, old_name, new_name);
                    }
                }
            }
            Ok(())
        })
    }

    fn add_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        column: ColumnDef,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get_mut(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            if table
                .schema
                .columns
                .iter()
                .any(|entry| entry.name == column.name)
            {
                return Err(DbError::storage(format!(
                    "column already exists on table {schema_name}: {}",
                    column.name
                )));
            }

            let default_value = column
                .default_value
                .as_ref()
                .map_or(Ok(Value::Null), |default| default.evaluate())?;
            let mut updated_schema = table.schema.clone();
            updated_schema.columns.push(column);
            updated_schema.validate_constraints_metadata()?;

            let mut updated_rows = Vec::with_capacity(table.rows.len());
            for (row_id, row) in &table.rows {
                let mut candidate = row.clone();
                candidate.push(default_value.clone());
                let candidate = updated_schema.normalize_strict_row_values(candidate)?;
                updated_schema.validate_row_values(&candidate)?;
                updated_schema.validate_check_constraints(&candidate)?;
                updated_rows.push((*row_id, candidate));
            }

            table.schema = updated_schema;
            table.rows = updated_rows;
            Ok(())
        })
    }

    fn rename_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get_mut(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            if !table
                .schema
                .columns
                .iter()
                .any(|entry| entry.name == old_name)
            {
                return Err(DbError::storage(format!(
                    "unknown column {old_name} on table {schema_name}"
                )));
            }
            if table
                .schema
                .columns
                .iter()
                .any(|entry| entry.name == new_name)
            {
                return Err(DbError::storage(format!(
                    "column already exists on table {schema_name}: {new_name}"
                )));
            }

            table.schema.rename_column_references(old_name, new_name);
            table
                .schema
                .rename_foreign_key_ref_column(schema_name, old_name, new_name);
            table.schema.validate_constraints_metadata()?;

            if let Some(indexes) = database.indexes.get_mut(schema_name) {
                for index in indexes.values_mut() {
                    for column in &mut index.columns {
                        if column == old_name {
                            *column = new_name.to_string();
                        }
                    }
                }
            }
            for (name, other_table) in &mut database.tables {
                if name != schema_name {
                    other_table.schema.rename_foreign_key_ref_column(
                        schema_name,
                        old_name,
                        new_name,
                    );
                }
            }
            for object in &mut database.extra_schema_objects {
                if object.entry_type == "trigger"
                    && object.table_name == schema_name
                    && let Some(sql) = &mut object.sql
                {
                    *sql = rename_trigger_column_sql(sql, old_name, new_name);
                }
            }
            Ok(())
        })
    }

    fn drop_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        old_name: &str,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get_mut(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            let (updated_schema, removed_index) = table.schema.drop_column(old_name)?;
            table.schema = updated_schema;
            for (_, row) in &mut table.rows {
                row.remove(removed_index);
            }

            if let Some(indexes) = database.indexes.get_mut(schema_name) {
                indexes.retain(|_, index| !index.columns.iter().any(|column| column == old_name));
            }
            Ok(())
        })
    }

    fn get_schema(&self, transaction_id: TransactionId, name: &str) -> Result<Option<Schema>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .writable_view()
            .tables
            .get(name)
            .map(|table| table.schema.clone()))
    }

    fn list_schemas(&self, transaction_id: TransactionId) -> Result<Vec<Schema>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .writable_view()
            .tables
            .values()
            .map(|table| table.schema.clone())
            .collect())
    }
}

impl TableStore for FileStorage {
    fn insert_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row: Row,
    ) -> Result<RowId> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let mut row = row;
            let row_id = {
                let table = database
                    .tables
                    .get_mut(schema_name)
                    .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;

                let row_id_column_index = Self::integer_primary_key_column_index(&table.schema);
                let row_id = if let Some(index) = row_id_column_index {
                    match row.get(index) {
                        Some(Value::Integer(value)) => {
                            RowId(u64::try_from(*value).map_err(|_| {
                                DbError::storage("sqlite rowid must be a non-negative INTEGER")
                            })?)
                        }
                        Some(Value::Null) => {
                            let next = Self::next_row_id_for_insert(
                                table,
                                database.sqlite_sequence.get(schema_name).copied(),
                            );
                            let row_id = RowId(next);
                            row[index] = Value::Integer(i64::try_from(row_id.0).map_err(|_| {
                                DbError::storage("sqlite rowid does not fit in i64")
                            })?);
                            row_id
                        }
                        Some(value) => {
                            return Err(DbError::storage(format!(
                                "sqlite rowid column must be INTEGER, got {}",
                                value.type_name()
                            )));
                        }
                        None => {
                            return Err(DbError::storage("sqlite row is missing rowid column"));
                        }
                    }
                } else {
                    let next = table
                        .rows
                        .last()
                        .map(|(row_id, _)| row_id.0.saturating_add(1))
                        .unwrap_or(1);
                    RowId(next)
                };

                table.schema.validate_row_values(&row)?;
                if !*self.ignore_check_constraints.borrow() {
                    table.schema.validate_check_constraints_with_like_mode(
                        &row,
                        *self.case_sensitive_like.borrow(),
                    )?;
                }
                let existing_rows = table.rows.iter().map(|(_, row)| row).collect::<Vec<_>>();
                table
                    .schema
                    .validate_primary_key_uniqueness(&row, &existing_rows)?;
                self.validate_unique_indexes_for_row(
                    &table.schema,
                    database.indexes.get(schema_name),
                    &table.rows,
                    &row,
                )?;
                row_id
            };

            let inserted_row = row.clone();
            self.execute_simple_before_insert_triggers(
                database,
                schema_name,
                &inserted_row,
                *self.ignore_check_constraints.borrow(),
                *self.case_sensitive_like.borrow(),
            )?;

            let table = database
                .tables
                .get_mut(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            table.schema.validate_row_values(&row)?;
            if !*self.ignore_check_constraints.borrow() {
                table.schema.validate_check_constraints_with_like_mode(
                    &row,
                    *self.case_sensitive_like.borrow(),
                )?;
            }
            let existing_rows = table.rows.iter().map(|(_, row)| row).collect::<Vec<_>>();
            table
                .schema
                .validate_primary_key_uniqueness(&row, &existing_rows)?;
            self.validate_unique_indexes_for_row(
                &table.schema,
                database.indexes.get(schema_name),
                &table.rows,
                &row,
            )?;
            table.rows.push((row_id, row));
            table.rows.sort_by_key(|(row_id, _)| row_id.0);
            if table
                .schema
                .columns
                .iter()
                .any(|column| column.primary_key && column.autoincrement)
            {
                let entry = database
                    .sqlite_sequence
                    .entry(schema_name.to_string())
                    .or_insert(0);
                *entry = (*entry).max(row_id.0);
            }
            self.execute_simple_after_insert_triggers(
                database,
                schema_name,
                &inserted_row,
                *self.ignore_check_constraints.borrow(),
                *self.case_sensitive_like.borrow(),
            )?;
            Ok(row_id)
        })
    }

    fn get_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
    ) -> Result<Option<Row>> {
        self.validate_transaction(transaction_id)?;
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        if schema.without_rowid {
            return Ok(self
                .without_rowid_scan_rows_internal(transaction_id, schema_name, &schema)?
                .into_iter()
                .find(|(candidate, _)| *candidate == row_id)
                .map(|(_, row)| row));
        }
        if self.txn_state.borrow().pending_writable.is_some() {
            return Ok(self
                .writable_view()
                .tables
                .get(schema_name)
                .and_then(|table| {
                    table
                        .rows
                        .iter()
                        .find(|(candidate, _)| *candidate == row_id)
                        .map(|(_, row)| row.clone())
                }));
        }
        let (_, root_page) = self.require_schema_and_root_page(schema_name)?;
        let pager = self.pager.borrow();
        let pager = pager.as_ref().ok_or_else(|| {
            DbError::storage("sqlite3 FileStorage is not backed by a database file")
        })?;
        get_table_row(pager, root_page, &schema, row_id)
    }

    fn scan_rows(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<(RowId, Row)>> {
        self.validate_transaction(transaction_id)?;
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        if schema.without_rowid {
            return self.without_rowid_scan_rows_internal(transaction_id, schema_name, &schema);
        }
        if self.txn_state.borrow().pending_writable.is_some() {
            return Ok(self
                .writable_view()
                .tables
                .get(schema_name)
                .map(|table| table.rows.clone())
                .unwrap_or_default());
        }
        let (_, root_page) = self.require_schema_and_root_page(schema_name)?;
        let pager = self.pager.borrow();
        let pager = pager.as_ref().ok_or_else(|| {
            DbError::storage("sqlite3 FileStorage is not backed by a database file")
        })?;
        scan_table_rows(pager, root_page, &schema)
    }

    fn delete_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let old_row = {
                let table = database
                    .tables
                    .get(schema_name)
                    .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
                let position = if table.schema.without_rowid {
                    Self::without_rowid_row_position(table, row_id)?.ok_or_else(|| {
                        DbError::storage(format!(
                            "unknown rowid {} on table {schema_name}",
                            row_id.0
                        ))
                    })?
                } else {
                    table
                        .rows
                        .iter()
                        .position(|(candidate, _)| *candidate == row_id)
                        .ok_or_else(|| {
                            DbError::storage(format!(
                                "unknown rowid {} on table {schema_name}",
                                row_id.0
                            ))
                        })?
                };
                table.rows[position].1.clone()
            };
            self.execute_simple_before_delete_triggers(
                database,
                schema_name,
                &old_row,
                *self.ignore_check_constraints.borrow(),
                *self.case_sensitive_like.borrow(),
            )?;
            {
                let table = database
                    .tables
                    .get_mut(schema_name)
                    .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
                let position = if table.schema.without_rowid {
                    Self::without_rowid_row_position(table, row_id)?.ok_or_else(|| {
                        DbError::storage(format!(
                            "unknown rowid {} on table {schema_name}",
                            row_id.0
                        ))
                    })?
                } else {
                    table
                        .rows
                        .iter()
                        .position(|(candidate, _)| *candidate == row_id)
                        .ok_or_else(|| {
                            DbError::storage(format!(
                                "unknown rowid {} on table {schema_name}",
                                row_id.0
                            ))
                        })?
                };
                table.rows.remove(position);
            };
            self.execute_simple_after_delete_triggers(
                database,
                schema_name,
                &old_row,
                *self.ignore_check_constraints.borrow(),
                *self.case_sensitive_like.borrow(),
            )?;
            Ok(())
        })
    }

    fn update_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
        row: Row,
    ) -> Result<()> {
        self.update_row_with_columns(transaction_id, schema_name, row_id, row, &[])
    }

    fn update_row_with_columns(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
        row: Row,
        updated_columns: &[String],
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let new_row = row.clone();
            let (old_row, new_row_id) = {
                let table = database
                    .tables
                    .get_mut(schema_name)
                    .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
                table.schema.validate_row_values(&row)?;
                if !*self.ignore_check_constraints.borrow() {
                    table.schema.validate_check_constraints_with_like_mode(
                        &row,
                        *self.case_sensitive_like.borrow(),
                    )?;
                }

                let position = if table.schema.without_rowid {
                    Self::without_rowid_row_position(table, row_id)?.ok_or_else(|| {
                        DbError::storage(format!(
                            "unknown rowid {} on table {schema_name}",
                            row_id.0
                        ))
                    })?
                } else {
                    table
                        .rows
                        .iter()
                        .position(|(candidate, _)| *candidate == row_id)
                        .ok_or_else(|| {
                            DbError::storage(format!(
                                "unknown rowid {} on table {schema_name}",
                                row_id.0
                            ))
                        })?
                };

                let new_row_id = table
                    .schema
                    .columns
                    .iter()
                    .position(|column| {
                        column.primary_key
                            && matches!(
                                column.column_type,
                                crate::common::types::ColumnType::Integer
                            )
                    })
                    .map(|index| match row.get(index) {
                        Some(Value::Integer(value)) => {
                            u64::try_from(*value).map(RowId).map_err(|_| {
                                DbError::storage("sqlite rowid must be a non-negative INTEGER")
                            })
                        }
                        Some(value) => Err(DbError::storage(format!(
                            "sqlite rowid column must be INTEGER, got {}",
                            value.type_name()
                        ))),
                        None => Err(DbError::storage("sqlite row is missing rowid column")),
                    })
                    .transpose()?
                    .unwrap_or(row_id);

                (table.rows[position].1.clone(), new_row_id)
            };
            self.execute_simple_before_update_triggers(
                database,
                schema_name,
                &old_row,
                &new_row,
                updated_columns,
                *self.ignore_check_constraints.borrow(),
                *self.case_sensitive_like.borrow(),
            )?;
            {
                let table = database
                    .tables
                    .get_mut(schema_name)
                    .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
                table.schema.validate_row_values(&row)?;
                if !*self.ignore_check_constraints.borrow() {
                    table.schema.validate_check_constraints_with_like_mode(
                        &row,
                        *self.case_sensitive_like.borrow(),
                    )?;
                }
                let position = if table.schema.without_rowid {
                    Self::without_rowid_row_position(table, row_id)?.ok_or_else(|| {
                        DbError::storage(format!(
                            "unknown rowid {} on table {schema_name}",
                            row_id.0
                        ))
                    })?
                } else {
                    table
                        .rows
                        .iter()
                        .position(|(candidate, _)| *candidate == row_id)
                        .ok_or_else(|| {
                            DbError::storage(format!(
                                "unknown rowid {} on table {schema_name}",
                                row_id.0
                            ))
                        })?
                };
                table.rows[position] = (new_row_id, row);
                if !table.schema.without_rowid {
                    table
                        .rows
                        .sort_by_key(|(candidate_row_id, _)| candidate_row_id.0);
                }
            }
            self.execute_simple_after_update_triggers(
                database,
                schema_name,
                &old_row,
                &new_row,
                updated_columns,
                *self.ignore_check_constraints.borrow(),
                *self.case_sensitive_like.borrow(),
            )?;
            Ok(())
        })
    }
}

impl IndexStore for FileStorage {
    fn create_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index: IndexMeta,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let table = database
                .tables
                .get(schema_name)
                .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
            if index.columns.is_empty() {
                return Err(DbError::storage("index must define at least one column"));
            }
            for column in &index.columns {
                validate_index_term(&table.schema, column)?;
            }
            if let Some(predicate_sql) = index.predicate.as_deref() {
                let predicate = parse_check_constraint_expression(predicate_sql)?;
                table.schema.validate_check_expr_metadata(&predicate)?;
            }
            if index.unique {
                let mut seen = std::collections::BTreeSet::new();
                for (_, row) in &table.rows {
                    if !self.row_matches_partial_index(&table.schema, &index, row)? {
                        continue;
                    }
                    let key = self.project_index_key(&table.schema, &index, row)?;
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

            let table_indexes = database.indexes.entry(schema_name.to_string()).or_default();
            if table_indexes.contains_key(&index.name) {
                return Err(DbError::storage(format!(
                    "index already exists on table {schema_name}: {}",
                    index.name
                )));
            }
            table_indexes.insert(index.name.clone(), index);
            Ok(())
        })
    }

    fn drop_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<()> {
        self.with_pending_writable_mut(transaction_id, |database| {
            let indexes = database.indexes.get_mut(schema_name).ok_or_else(|| {
                DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
            })?;
            if indexes.remove(index_name).is_none() {
                return Err(DbError::storage(format!(
                    "unknown index {index_name} on table {schema_name}"
                )));
            }
            if indexes.is_empty() {
                database.indexes.remove(schema_name);
            }
            Ok(())
        })
    }

    fn get_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<Option<IndexMeta>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .writable_view()
            .indexes
            .get(schema_name)
            .and_then(|indexes| indexes.get(index_name))
            .cloned())
    }

    fn list_indexes(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<IndexMeta>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .writable_view()
            .indexes
            .get(schema_name)
            .map(|indexes| {
                indexes
                    .values()
                    .filter(|index| index.is_usable())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    fn list_all_indexes(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<IndexMeta>> {
        self.validate_transaction(transaction_id)?;
        Ok(self
            .writable_view()
            .indexes
            .get(schema_name)
            .map(|indexes| indexes.values().cloned().collect())
            .unwrap_or_default())
    }

    fn lookup_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key: &[Value],
    ) -> Result<Vec<RowId>> {
        self.validate_transaction(transaction_id)?;
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        let (index, root_page) = self.require_index_and_root_page(schema_name, index_name)?;
        if Self::is_without_rowid_primary_key_index(&schema, &index, schema_name) {
            return self.without_rowid_lookup_row_ids(transaction_id, schema_name, key, true);
        }
        if schema.without_rowid {
            let rows =
                self.without_rowid_scan_rows_internal(transaction_id, schema_name, &schema)?;
            let synthetic_ids = rows
                .into_iter()
                .filter_map(|(_row_id, row)| {
                    let index_key = self.project_index_key(&schema, &index, &row).ok()?;
                    if index_key != key {
                        return None;
                    }
                    Self::without_rowid_synthetic_row_id(&schema, &row).ok()
                })
                .collect::<Vec<_>>();
            return Ok(synthetic_ids);
        }
        if key.len() != index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} expected {} key values but got {}",
                index.name,
                index.columns.len(),
                key.len()
            )));
        }
        let pager = self.pager.borrow();
        let pager = pager.as_ref().ok_or_else(|| {
            DbError::storage("sqlite3 FileStorage is not backed by a database file")
        })?;
        lookup_index_entries(pager, root_page, &index, key)
    }

    fn scan_index_prefix(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key_prefix: &[Value],
    ) -> Result<Vec<RowId>> {
        self.validate_transaction(transaction_id)?;
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        let (index, root_page) = self.require_index_and_root_page(schema_name, index_name)?;
        if Self::is_without_rowid_primary_key_index(&schema, &index, schema_name) {
            return self.without_rowid_lookup_row_ids(
                transaction_id,
                schema_name,
                key_prefix,
                false,
            );
        }
        if schema.without_rowid {
            let rows =
                self.without_rowid_scan_rows_internal(transaction_id, schema_name, &schema)?;
            let synthetic_ids = rows
                .into_iter()
                .filter_map(|(_row_id, row)| {
                    let index_key = self.project_index_key(&schema, &index, &row).ok()?;
                    if !index_key.starts_with(key_prefix) {
                        return None;
                    }
                    Self::without_rowid_synthetic_row_id(&schema, &row).ok()
                })
                .collect::<Vec<_>>();
            return Ok(synthetic_ids);
        }
        if key_prefix.len() > index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} expected at most {} key values but got {}",
                index.name,
                index.columns.len(),
                key_prefix.len()
            )));
        }
        let pager = self.pager.borrow();
        let pager = pager.as_ref().ok_or_else(|| {
            DbError::storage("sqlite3 FileStorage is not backed by a database file")
        })?;
        lookup_index_entries(pager, root_page, &index, key_prefix)
    }

    fn scan_index_range(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key_prefix: &[Value],
        lower: Option<(CompareOp, &Value)>,
        upper: Option<(CompareOp, &Value)>,
    ) -> Result<Vec<RowId>> {
        self.validate_transaction(transaction_id)?;
        let schema = self
            .get_schema(transaction_id, schema_name)?
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        if let Ok((index, _root_page)) = self.require_index_and_root_page(schema_name, index_name)
            && Self::is_without_rowid_primary_key_index(&schema, &index, schema_name)
        {
            if key_prefix.len() >= index.columns.len() {
                return Err(DbError::storage(format!(
                    "index {} has no range column after prefix of length {}",
                    index.name,
                    key_prefix.len()
                )));
            }

            let rows =
                self.without_rowid_scan_rows_internal(transaction_id, schema_name, &schema)?;
            let mut row_ids = BTreeSet::new();
            for (row_id, row) in rows {
                if self.row_matches_index_range(&schema, &index, &row, key_prefix, lower, upper)? {
                    row_ids.insert(row_id);
                }
            }
            return Ok(row_ids.into_iter().collect());
        }
        if schema.without_rowid {
            let database = self.writable_view();
            let index = database
                .indexes
                .get(schema_name)
                .and_then(|indexes| indexes.get(index_name))
                .ok_or_else(|| {
                    DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
                })?;

            if key_prefix.len() >= index.columns.len() {
                return Err(DbError::storage(format!(
                    "index {} has no range column after prefix of length {}",
                    index.name,
                    key_prefix.len()
                )));
            }

            let rows =
                self.without_rowid_scan_rows_internal(transaction_id, schema_name, &schema)?;
            let mut row_ids = BTreeSet::new();
            for (row_id, row) in rows {
                if self.row_matches_index_range(&schema, index, &row, key_prefix, lower, upper)? {
                    row_ids.insert(row_id);
                }
            }
            return Ok(row_ids.into_iter().collect());
        }
        let database = self.writable_view();
        let table = database
            .tables
            .get(schema_name)
            .ok_or_else(|| DbError::storage(format!("unknown table: {schema_name}")))?;
        let index = database
            .indexes
            .get(schema_name)
            .and_then(|indexes| indexes.get(index_name))
            .ok_or_else(|| {
                DbError::storage(format!("unknown index {index_name} on table {schema_name}"))
            })?;

        if key_prefix.len() >= index.columns.len() {
            return Err(DbError::storage(format!(
                "index {} has no range column after prefix of length {}",
                index.name,
                key_prefix.len()
            )));
        }

        let mut row_ids = BTreeSet::new();
        for (row_id, row) in &table.rows {
            if self.row_matches_index_range(&table.schema, index, row, key_prefix, lower, upper)? {
                row_ids.insert(*row_id);
            }
        }

        Ok(row_ids.into_iter().collect())
    }
}

impl TransactionManager for FileStorage {
    fn begin(&self) -> Result<TransactionId> {
        let mut txn_state = self.txn_state.borrow_mut();
        if let Some(active) = txn_state.active_txn {
            return Err(DbError::txn(format!(
                "transaction {} is already active",
                active.0
            )));
        }

        let transaction_id = TransactionId(txn_state.next_txn_id);
        txn_state.next_txn_id += 1;
        txn_state.active_txn = Some(transaction_id);
        txn_state.savepoints.clear();
        Ok(transaction_id)
    }

    fn begin_with_isolation(&self, _isolation_level: IsolationLevel) -> Result<TransactionId> {
        self.begin()
    }

    fn commit(&self, transaction_id: TransactionId) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let pending = {
            let mut txn_state = self.txn_state.borrow_mut();
            let pending = txn_state.pending_writable.take();
            txn_state.active_txn = None;
            txn_state.savepoints.clear();
            pending
        };
        if let Some(database) = pending {
            let path = self.path.clone().ok_or_else(|| {
                DbError::storage("sqlite3 FileStorage is not backed by a database file")
            })?;
            write_database(&path, &database)?;
            let pager = Pager::open(&path)?;
            let catalog = load_catalog(&pager)?;
            *self.catalog.borrow_mut() = catalog;
            *self.pager.borrow_mut() = Some(pager);
            *self.writable.borrow_mut() = database;
        }
        Ok(())
    }

    fn rollback(&self, transaction_id: TransactionId) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let mut txn_state = self.txn_state.borrow_mut();
        txn_state.pending_writable = None;
        txn_state.active_txn = None;
        txn_state.savepoints.clear();
        Ok(())
    }

    fn savepoint(&self, transaction_id: TransactionId, name: &str) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let snapshot = self.writable_view();
        self.txn_state.borrow_mut().savepoints.push(Savepoint {
            name: name.to_string(),
            snapshot,
        });
        Ok(())
    }

    fn rollback_to_savepoint(&self, transaction_id: TransactionId, name: &str) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let (savepoint_index, snapshot) = {
            let txn_state = self.txn_state.borrow();
            txn_state
                .savepoints
                .iter()
                .rposition(|savepoint| savepoint.name.eq_ignore_ascii_case(name))
                .map(|index| (index, txn_state.savepoints[index].snapshot.clone()))
                .ok_or_else(|| DbError::txn(format!("no such savepoint: {name}")))?
        };
        let mut txn_state = self.txn_state.borrow_mut();
        txn_state.pending_writable = Some(snapshot);
        txn_state.savepoints.truncate(savepoint_index + 1);
        Ok(())
    }

    fn release_savepoint(&self, transaction_id: TransactionId, name: &str) -> Result<()> {
        self.validate_transaction(transaction_id)?;
        let mut txn_state = self.txn_state.borrow_mut();
        let savepoint_index = txn_state
            .savepoints
            .iter()
            .rposition(|savepoint| savepoint.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| DbError::txn(format!("no such savepoint: {name}")))?;
        txn_state.savepoints.truncate(savepoint_index);
        Ok(())
    }
}
