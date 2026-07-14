use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::common::error::{DbError, Result};
use crate::common::types::{ColumnType, Row, Schema, Value, sqlite_round_f64};
use crate::sql::ast::{CompareOp, ScalarBinaryOp, ScalarExpr, ScalarFunc};
use crate::sql::parser::parse_scalar_sql_expression;

pub fn evaluate_constant_expr(expr: &ScalarExpr) -> Option<Value> {
    if !is_constant_scalar_expr(expr) || contains_current_time_scalar_expr(expr) {
        return None;
    }

    let schema = Schema::new("__constant__", vec![]);
    let row = Vec::new();
    evaluate_scalar_expr(&schema, &row, expr).ok()
}

pub fn evaluate_index_term(schema: &Schema, row: &Row, term: &str) -> Result<Value> {
    evaluate_index_term_with_like_mode(schema, row, term, false)
}

pub fn evaluate_index_term_with_like_mode(
    schema: &Schema,
    row: &Row,
    term: &str,
    case_sensitive_like: bool,
) -> Result<Value> {
    if let Ok(index) = schema.column_index(term) {
        return row.get(index).cloned().ok_or_else(|| {
            DbError::storage(format!(
                "row for table {} is missing column {term}",
                schema.name
            ))
        });
    }

    let expr = parse_scalar_sql_expression(term)?;
    evaluate_scalar_expr_with_like_mode(schema, row, &expr, case_sensitive_like)
}

pub fn validate_index_term(schema: &Schema, term: &str) -> Result<()> {
    if schema.column_index(term).is_ok() {
        return Ok(());
    }

    let expr = parse_scalar_sql_expression(term)?;
    require_scalar_expr_columns(schema, &expr)
}

fn require_scalar_expr_columns(schema: &Schema, expr: &ScalarExpr) -> Result<()> {
    match expr {
        ScalarExpr::Literal(_) => Ok(()),
        ScalarExpr::Tuple(items) => {
            for item in items {
                require_scalar_expr_columns(schema, item)?;
            }
            Ok(())
        }
        ScalarExpr::Column(name) => {
            schema.column_index(name)?;
            Ok(())
        }
        ScalarExpr::UnaryPlus(expr) => require_scalar_expr_columns(schema, expr),
        ScalarExpr::UnaryMinus(expr) => require_scalar_expr_columns(schema, expr),
        ScalarExpr::BitNot(expr) => require_scalar_expr_columns(schema, expr),
        ScalarExpr::Not(expr) => require_scalar_expr_columns(schema, expr),
        ScalarExpr::Collate { expr, .. } => require_scalar_expr_columns(schema, expr),
        ScalarExpr::Cast { expr, .. } => require_scalar_expr_columns(schema, expr),
        ScalarExpr::Is { left, right, .. } | ScalarExpr::Compare { left, right, .. } => {
            require_scalar_expr_columns(schema, left)?;
            require_scalar_expr_columns(schema, right)
        }
        ScalarExpr::IsBool { expr, .. } => require_scalar_expr_columns(schema, expr),
        ScalarExpr::Glob { expr, pattern, .. } => {
            require_scalar_expr_columns(schema, expr)?;
            require_scalar_expr_columns(schema, pattern)
        }
        ScalarExpr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            require_scalar_expr_columns(schema, expr)?;
            require_scalar_expr_columns(schema, pattern)?;
            if let Some(escape) = escape {
                require_scalar_expr_columns(schema, escape)?;
            }
            Ok(())
        }
        ScalarExpr::InList { expr, values, .. } => {
            require_scalar_expr_columns(schema, expr)?;
            for value in values {
                require_scalar_expr_columns(schema, value)?;
            }
            Ok(())
        }
        ScalarExpr::InSubquery { .. }
        | ScalarExpr::Subquery { .. }
        | ScalarExpr::CompareSubquery { .. }
        | ScalarExpr::WindowFunction { .. } => Err(DbError::plan(
            "subqueries are not allowed in index expressions",
        )),
        ScalarExpr::Between {
            expr, low, high, ..
        } => {
            require_scalar_expr_columns(schema, expr)?;
            require_scalar_expr_columns(schema, low)?;
            require_scalar_expr_columns(schema, high)
        }
        ScalarExpr::Case {
            base,
            when_then_clauses,
            else_expr,
        } => {
            if let Some(base) = base {
                require_scalar_expr_columns(schema, base)?;
            }
            for (when_expr, then_expr) in when_then_clauses {
                require_scalar_expr_columns(schema, when_expr)?;
                require_scalar_expr_columns(schema, then_expr)?;
            }
            if let Some(else_expr) = else_expr {
                require_scalar_expr_columns(schema, else_expr)?;
            }
            Ok(())
        }
        ScalarExpr::Binary { left, right, .. } => {
            require_scalar_expr_columns(schema, left)?;
            require_scalar_expr_columns(schema, right)
        }
        ScalarExpr::Function { args, .. } => {
            for arg in args {
                require_scalar_expr_columns(schema, arg)?;
            }
            Ok(())
        }
        ScalarExpr::Aggregate { .. } => Err(DbError::plan(
            "aggregate functions are not allowed in index expressions",
        )),
    }
}

fn is_constant_scalar_expr(expr: &ScalarExpr) -> bool {
    match expr {
        ScalarExpr::Literal(_) => true,
        ScalarExpr::Tuple(items) => items.iter().all(is_constant_scalar_expr),
        ScalarExpr::Column(_) => false,
        ScalarExpr::UnaryPlus(expr) => is_constant_scalar_expr(expr),
        ScalarExpr::UnaryMinus(expr) => is_constant_scalar_expr(expr),
        ScalarExpr::BitNot(expr) => is_constant_scalar_expr(expr),
        ScalarExpr::Not(expr) => is_constant_scalar_expr(expr),
        ScalarExpr::Collate { expr, .. } => is_constant_scalar_expr(expr),
        ScalarExpr::Cast { expr, .. } => is_constant_scalar_expr(expr),
        ScalarExpr::Is { left, right, .. } | ScalarExpr::Compare { left, right, .. } => {
            is_constant_scalar_expr(left) && is_constant_scalar_expr(right)
        }
        ScalarExpr::IsBool { expr, .. } => is_constant_scalar_expr(expr),
        ScalarExpr::Glob { expr, pattern, .. } => {
            is_constant_scalar_expr(expr) && is_constant_scalar_expr(pattern)
        }
        ScalarExpr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            is_constant_scalar_expr(expr)
                && is_constant_scalar_expr(pattern)
                && escape
                    .as_ref()
                    .is_none_or(|expr| is_constant_scalar_expr(expr))
        }
        ScalarExpr::InList { expr, values, .. } => {
            is_constant_scalar_expr(expr) && values.iter().all(is_constant_scalar_expr)
        }
        ScalarExpr::InSubquery { .. }
        | ScalarExpr::Subquery { .. }
        | ScalarExpr::CompareSubquery { .. }
        | ScalarExpr::WindowFunction { .. } => false,
        ScalarExpr::Between {
            expr, low, high, ..
        } => {
            is_constant_scalar_expr(expr)
                && is_constant_scalar_expr(low)
                && is_constant_scalar_expr(high)
        }
        ScalarExpr::Case {
            base,
            when_then_clauses,
            else_expr,
        } => {
            base.as_ref()
                .is_none_or(|expr| is_constant_scalar_expr(expr))
                && when_then_clauses.iter().all(|(when_expr, then_expr)| {
                    is_constant_scalar_expr(when_expr) && is_constant_scalar_expr(then_expr)
                })
                && else_expr
                    .as_ref()
                    .is_none_or(|expr| is_constant_scalar_expr(expr))
        }
        ScalarExpr::Binary { left, right, .. } => {
            is_constant_scalar_expr(left) && is_constant_scalar_expr(right)
        }
        ScalarExpr::Function { func, args } => {
            is_deterministic_constant_func(*func) && args.iter().all(is_constant_scalar_expr)
        }
        ScalarExpr::Aggregate { .. } => false,
    }
}

fn is_deterministic_constant_func(func: ScalarFunc) -> bool {
    !matches!(
        func,
        ScalarFunc::Changes
            | ScalarFunc::TotalChanges
            | ScalarFunc::Random
            | ScalarFunc::RandomBlob
            | ScalarFunc::SqliteSourceId
            | ScalarFunc::SqliteVersion
            | ScalarFunc::LastInsertRowId
    )
}

fn contains_current_time_scalar_expr(expr: &ScalarExpr) -> bool {
    match expr {
        ScalarExpr::Literal(_) | ScalarExpr::Column(_) => false,
        ScalarExpr::Tuple(items) => items.iter().any(contains_current_time_scalar_expr),
        ScalarExpr::UnaryPlus(expr)
        | ScalarExpr::UnaryMinus(expr)
        | ScalarExpr::BitNot(expr)
        | ScalarExpr::Not(expr)
        | ScalarExpr::Collate { expr, .. }
        | ScalarExpr::Cast { expr, .. } => {
            contains_current_time_scalar_expr(expr)
        }
        ScalarExpr::Is { left, right, .. } | ScalarExpr::Compare { left, right, .. } => {
            contains_current_time_scalar_expr(left) || contains_current_time_scalar_expr(right)
        }
        ScalarExpr::IsBool { expr, .. } => {
            contains_current_time_scalar_expr(expr)
        }
        ScalarExpr::Glob { expr, pattern, .. } => {
            contains_current_time_scalar_expr(expr) || contains_current_time_scalar_expr(pattern)
        }
        ScalarExpr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            contains_current_time_scalar_expr(expr)
                || contains_current_time_scalar_expr(pattern)
                || escape
                    .as_ref()
                    .is_some_and(|expr| contains_current_time_scalar_expr(expr))
        }
        ScalarExpr::InList { expr, values, .. } => {
            contains_current_time_scalar_expr(expr)
                || values.iter().any(contains_current_time_scalar_expr)
        }
        ScalarExpr::InSubquery { expr, .. }
        | ScalarExpr::CompareSubquery { left: expr, .. } => {
            contains_current_time_scalar_expr(expr)
        }
        ScalarExpr::Subquery { .. } | ScalarExpr::WindowFunction { .. } => false,
        ScalarExpr::Between {
            expr, low, high, ..
        } => {
            contains_current_time_scalar_expr(expr)
                || contains_current_time_scalar_expr(low)
                || contains_current_time_scalar_expr(high)
        }
        ScalarExpr::Case {
            base,
            when_then_clauses,
            else_expr,
        } => {
            base.as_ref().is_some_and(|expr| contains_current_time_scalar_expr(expr))
                || when_then_clauses.iter().any(|(when_expr, then_expr)| {
                    contains_current_time_scalar_expr(when_expr)
                        || contains_current_time_scalar_expr(then_expr)
                })
                || else_expr
                    .as_ref()
                    .is_some_and(|expr| contains_current_time_scalar_expr(expr))
        }
        ScalarExpr::Binary { left, right, .. } => {
            contains_current_time_scalar_expr(left) || contains_current_time_scalar_expr(right)
        }
        ScalarExpr::Function { func, args } => {
            matches!(
                func,
                ScalarFunc::Date
                    | ScalarFunc::Time
                    | ScalarFunc::DateTime
                    | ScalarFunc::Strftime
                    | ScalarFunc::JulianDay
                    | ScalarFunc::UnixEpoch
            ) && args
                .first()
                .is_some_and(|arg| matches!(arg, ScalarExpr::Literal(Value::Text(value)) if value.eq_ignore_ascii_case("now")))
                || args.iter().any(contains_current_time_scalar_expr)
        }
        ScalarExpr::Aggregate { .. } => false,
    }
}

fn evaluate_scalar_expr(schema: &Schema, row: &Row, expr: &ScalarExpr) -> Result<Value> {
    evaluate_scalar_expr_with_like_mode(schema, row, expr, false)
}

fn evaluate_scalar_expr_with_like_mode(
    schema: &Schema,
    row: &Row,
    expr: &ScalarExpr,
    case_sensitive_like: bool,
) -> Result<Value> {
    Ok(match expr {
        ScalarExpr::Literal(value) => value.clone(),
        ScalarExpr::Tuple(_) => {
            return Err(DbError::storage(
                "row value expressions cannot be evaluated as sqlite index values",
            ));
        }
        ScalarExpr::Column(name) => {
            let index = schema.column_index(name)?;
            row.get(index).cloned().ok_or_else(|| {
                DbError::storage(format!(
                    "row for table {} is missing column {name}",
                    schema.name
                ))
            })?
        }
        ScalarExpr::UnaryPlus(expr) => {
            evaluate_scalar_expr_with_like_mode(schema, row, expr, case_sensitive_like)?
        }
        ScalarExpr::UnaryMinus(expr) => {
            match evaluate_scalar_expr_with_like_mode(schema, row, expr, case_sensitive_like)? {
                Value::Integer(value) => Value::Integer(
                    value
                        .checked_neg()
                        .ok_or_else(|| DbError::storage("integer overflow"))?,
                ),
                Value::Real(value) => Value::Real(-value),
                Value::Null => Value::Null,
                value => match coerce_arithmetic_value(&value)? {
                    Value::Integer(value) => Value::Integer(
                        value
                            .checked_neg()
                            .ok_or_else(|| DbError::storage("integer overflow"))?,
                    ),
                    Value::Real(value) => Value::Real(-value),
                    Value::Null => Value::Null,
                    _ => unreachable!("sqlite arithmetic coercion only returns numeric values"),
                },
            }
        }
        ScalarExpr::BitNot(expr) => {
            match evaluate_scalar_expr_with_like_mode(schema, row, expr, case_sensitive_like)? {
                Value::Null => Value::Null,
                value => Value::Integer(!sqlite_bitwise_integer_arg(&value)?),
            }
        }
        ScalarExpr::Not(expr) => {
            match evaluate_scalar_expr_with_like_mode(schema, row, expr, case_sensitive_like)? {
                value => sqlite_not_value(&value),
            }
        }
        ScalarExpr::Collate { expr, .. } => {
            evaluate_scalar_expr_with_like_mode(schema, row, expr, case_sensitive_like)?
        }
        ScalarExpr::Cast { expr, ty } => cast_value(
            evaluate_scalar_expr_with_like_mode(schema, row, expr, case_sensitive_like)?,
            *ty,
        )?,
        ScalarExpr::Is {
            left,
            right,
            negated,
        } => {
            let left = evaluate_scalar_expr_with_like_mode(schema, row, left, case_sensitive_like)?;
            let right =
                evaluate_scalar_expr_with_like_mode(schema, row, right, case_sensitive_like)?;
            Value::Boolean(is_with_negation(&left, &right, *negated))
        }
        ScalarExpr::IsBool {
            expr,
            value,
            negated,
        } => {
            let matches = match evaluate_scalar_expr_with_like_mode(
                schema,
                row,
                expr,
                case_sensitive_like,
            )? {
                Value::Null => false,
                evaluated => sqlite_is_true_value(&evaluated) == *value,
            };
            Value::Boolean(matches ^ *negated)
        }
        ScalarExpr::InList {
            expr,
            values,
            negated,
        } => {
            let left = evaluate_scalar_expr_with_like_mode(schema, row, expr, case_sensitive_like)?;
            let values = values
                .iter()
                .map(|value| {
                    evaluate_scalar_expr_with_like_mode(schema, row, value, case_sensitive_like)
                })
                .collect::<Result<Vec<_>>>()?;
            in_result_value(&left, &values, *negated)?
        }
        ScalarExpr::InSubquery { .. }
        | ScalarExpr::Subquery { .. }
        | ScalarExpr::CompareSubquery { .. }
        | ScalarExpr::WindowFunction { .. } => {
            return Err(DbError::plan(
                "subqueries are not allowed in index expressions",
            ));
        }
        ScalarExpr::Like {
            expr,
            pattern,
            escape,
            negated,
        } => {
            let pattern = evaluate_like_pattern(schema, row, pattern, case_sensitive_like)?;
            if matches!(pattern, LikeEscapeValue::Null) {
                return Ok(Value::Null);
            }
            let escape = evaluate_like_escape(schema, row, escape, case_sensitive_like)?;
            if matches!(escape, LikeEscapeValue::Null) {
                return Ok(Value::Null);
            }
            let pattern = pattern.as_option_string().unwrap_or_default();
            let escape = escape.as_option_string();
            match evaluate_scalar_expr_with_like_mode(schema, row, expr, case_sensitive_like)? {
                Value::Null => Value::Null,
                value => Value::Boolean(
                    matches_like_pattern(
                        &coerce_text_like_value(&value),
                        pattern,
                        escape,
                        case_sensitive_like,
                    )? ^ *negated,
                ),
            }
        }
        ScalarExpr::Glob {
            expr,
            pattern,
            negated,
        } => {
            let pattern = evaluate_like_pattern(schema, row, pattern, case_sensitive_like)?;
            if matches!(pattern, LikeEscapeValue::Null) {
                return Ok(Value::Null);
            }
            let pattern = pattern.as_option_string().unwrap_or_default();
            match evaluate_scalar_expr_with_like_mode(schema, row, expr, case_sensitive_like)? {
                Value::Null => Value::Null,
                value => Value::Boolean(
                    matches_glob_pattern(&coerce_text_like_value(&value), pattern) ^ *negated,
                ),
            }
        }
        ScalarExpr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let value =
                evaluate_scalar_expr_with_like_mode(schema, row, expr, case_sensitive_like)?;
            let low = evaluate_scalar_expr_with_like_mode(schema, row, low, case_sensitive_like)?;
            let high = evaluate_scalar_expr_with_like_mode(schema, row, high, case_sensitive_like)?;
            let Some(low_cmp) = compare(&value, &low)? else {
                return Ok(Value::Null);
            };
            let Some(high_cmp) = compare(&value, &high)? else {
                return Ok(Value::Null);
            };
            let matches = matches!(
                low_cmp,
                std::cmp::Ordering::Greater | std::cmp::Ordering::Equal
            ) && matches!(
                high_cmp,
                std::cmp::Ordering::Less | std::cmp::Ordering::Equal
            );
            Value::Boolean(matches ^ *negated)
        }
        ScalarExpr::Compare { left, op, right } => {
            let left = evaluate_scalar_expr_with_like_mode(schema, row, left, case_sensitive_like)?;
            let right =
                evaluate_scalar_expr_with_like_mode(schema, row, right, case_sensitive_like)?;
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Value::Null
            } else {
                Value::Boolean(compare_with_operator(&left, op, &right)?)
            }
        }
        ScalarExpr::Case {
            base,
            when_then_clauses,
            else_expr,
        } => evaluate_case_scalar_expr(
            schema,
            row,
            base.as_deref(),
            when_then_clauses,
            else_expr.as_deref(),
            case_sensitive_like,
        )?,
        ScalarExpr::Binary { left, op, right } => {
            let left = evaluate_scalar_expr_with_like_mode(schema, row, left, case_sensitive_like)?;
            let right =
                evaluate_scalar_expr_with_like_mode(schema, row, right, case_sensitive_like)?;
            evaluate_binary_scalar(*op, left, right)?
        }
        ScalarExpr::Function { func, args } => {
            if matches!(func, ScalarFunc::IIf | ScalarFunc::If) {
                if args.len() < 2 {
                    return Err(DbError::storage(format!(
                        "{} expects at least 2 arguments but got {}",
                        match func {
                            ScalarFunc::IIf => "IIF",
                            ScalarFunc::If => "IF",
                            _ => unreachable!(),
                        },
                        args.len()
                    )));
                }

                let pair_count = args.len() / 2;
                for pair_index in 0..pair_count {
                    let condition_index = pair_index * 2;
                    let condition = cast_value(
                        evaluate_scalar_expr_with_like_mode(
                            schema,
                            row,
                            &args[condition_index],
                            case_sensitive_like,
                        )?,
                        ColumnType::Boolean,
                    )?;
                    if matches!(condition, Value::Boolean(true)) {
                        return evaluate_scalar_expr_with_like_mode(
                            schema,
                            row,
                            &args[condition_index + 1],
                            case_sensitive_like,
                        );
                    }
                }
                if args.len() % 2 == 1 {
                    return evaluate_scalar_expr_with_like_mode(
                        schema,
                        row,
                        args.last().expect("odd IIF arity has default argument"),
                        case_sensitive_like,
                    );
                }
                return Ok(Value::Null);
            }

            let args = args
                .iter()
                .map(|arg| {
                    evaluate_scalar_expr_with_like_mode(schema, row, arg, case_sensitive_like)
                })
                .collect::<Result<Vec<_>>>()?;
            evaluate_scalar_function(*func, args, case_sensitive_like)?
        }
        ScalarExpr::Aggregate { .. } => {
            return Err(DbError::plan(
                "aggregate functions are not allowed in index expressions",
            ));
        }
    })
}

fn evaluate_case_scalar_expr(
    schema: &Schema,
    row: &Row,
    base: Option<&ScalarExpr>,
    when_then_clauses: &[(ScalarExpr, ScalarExpr)],
    else_expr: Option<&ScalarExpr>,
    case_sensitive_like: bool,
) -> Result<Value> {
    if let Some(base) = base {
        let base_value =
            evaluate_scalar_expr_with_like_mode(schema, row, base, case_sensitive_like)?;
        for (when_expr, then_expr) in when_then_clauses {
            let when_value =
                evaluate_scalar_expr_with_like_mode(schema, row, when_expr, case_sensitive_like)?;
            if compare(&base_value, &when_value)? == Some(std::cmp::Ordering::Equal) {
                return evaluate_scalar_expr_with_like_mode(
                    schema,
                    row,
                    then_expr,
                    case_sensitive_like,
                );
            }
        }
    } else {
        for (when_expr, then_expr) in when_then_clauses {
            let condition = cast_value(
                evaluate_scalar_expr_with_like_mode(schema, row, when_expr, case_sensitive_like)?,
                ColumnType::Boolean,
            )?;
            if matches!(condition, Value::Boolean(true)) {
                return evaluate_scalar_expr_with_like_mode(
                    schema,
                    row,
                    then_expr,
                    case_sensitive_like,
                );
            }
        }
    }

    if let Some(else_expr) = else_expr {
        evaluate_scalar_expr_with_like_mode(schema, row, else_expr, case_sensitive_like)
    } else {
        Ok(Value::Null)
    }
}

fn evaluate_scalar_function(
    func: ScalarFunc,
    args: Vec<Value>,
    case_sensitive_like: bool,
) -> Result<Value> {
    match func {
        ScalarFunc::Length => {
            expect_arity("LENGTH", &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(value) => Ok(Value::Integer(
                    sqlite_text_prefix_before_nul(value).chars().count() as i64,
                )),
                Value::Blob(value) => Ok(Value::Integer(value.len() as i64)),
                value => Ok(Value::Integer(
                    sqlite_text_prefix_before_nul(&coerce_text_like_value(value))
                        .chars()
                        .count() as i64,
                )),
            }
        }
        ScalarFunc::OctetLength => {
            expect_arity("OCTET_LENGTH", &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Blob(value) => Ok(Value::Integer(value.len() as i64)),
                Value::Text(value) => Ok(Value::Integer(value.len() as i64)),
                Value::Integer(value) => Ok(Value::Integer(value.to_string().len() as i64)),
                Value::Real(value) => Ok(Value::Integer(sqlite_real_to_text(*value).len() as i64)),
                Value::Boolean(_) => Ok(Value::Integer(1)),
            }
        }
        ScalarFunc::Date => Ok(parse_date_time_args("DATE", &args)?
            .map(|parts| {
                Value::Text(format!(
                    "{:04}-{:02}-{:02}",
                    parts.year, parts.month, parts.day
                ))
            })
            .unwrap_or(Value::Null)),
        ScalarFunc::Time => Ok(parse_date_time_args("TIME", &args)?
            .map(|parts| {
                if date_time_args_have_subsecond(&args) {
                    Value::Text(format!(
                        "{:02}:{:02}:{:02}.{:03}",
                        parts.hour, parts.minute, parts.second, parts.millisecond
                    ))
                } else {
                    Value::Text(format!(
                        "{:02}:{:02}:{:02}",
                        parts.hour, parts.minute, parts.second
                    ))
                }
            })
            .unwrap_or(Value::Null)),
        ScalarFunc::DateTime => Ok(parse_date_time_args("DATETIME", &args)?
            .map(|parts| {
                if date_time_args_have_subsecond(&args) {
                    Value::Text(format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
                        parts.year,
                        parts.month,
                        parts.day,
                        parts.hour,
                        parts.minute,
                        parts.second,
                        parts.millisecond
                    ))
                } else {
                    Value::Text(format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        parts.year, parts.month, parts.day, parts.hour, parts.minute, parts.second
                    ))
                }
            })
            .unwrap_or(Value::Null)),
        ScalarFunc::TimeDiff => {
            expect_arity("TIMEDIFF", &args, 2)?;
            let left = parse_date_time_args("TIMEDIFF", &args[..1])?;
            let right = parse_date_time_args("TIMEDIFF", &args[1..2])?;
            let (Some(left), Some(right)) = (left, right) else {
                return Ok(Value::Null);
            };
            Ok(Value::Text(sqlite_timediff_between(right, left)))
        }
        ScalarFunc::Strftime => {
            if args.is_empty() {
                return Ok(Value::Null);
            }

            let format = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };

            let args = if args.len() == 1 {
                vec![Value::from("now")]
            } else {
                args[1..].to_vec()
            };
            let subsecond = date_time_args_have_subsecond(&args);
            Ok(parse_date_time_args("STRFTIME", &args)?
                .and_then(|parts| sqlite_strftime_minimal(&format, parts, subsecond))
                .map(Value::Text)
                .unwrap_or(Value::Null))
        }
        ScalarFunc::JulianDay => Ok(parse_date_time_args("JULIANDAY", &args)?
            .map(sqlite_julianday)
            .map(Value::Real)
            .unwrap_or(Value::Null)),
        ScalarFunc::UnixEpoch => {
            let subsecond = date_time_args_have_subsecond(&args);
            Ok(parse_date_time_args("UNIXEPOCH", &args)?
                .map(|parts| {
                    if subsecond {
                        Value::Real(sqlite_unixepoch_subsecond(parts))
                    } else {
                        Value::Integer(sqlite_unixepoch(parts))
                    }
                })
                .unwrap_or(Value::Null))
        }
        ScalarFunc::Lower => {
            expect_arity("LOWER", &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(value) => Ok(Value::Text(sqlite_ascii_lower(value))),
                value => Ok(Value::Text(sqlite_ascii_lower(&coerce_text_like_value(
                    value,
                )))),
            }
        }
        ScalarFunc::Upper => {
            expect_arity("UPPER", &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(value) => Ok(Value::Text(sqlite_ascii_upper(value))),
                value => Ok(Value::Text(sqlite_ascii_upper(&coerce_text_like_value(
                    value,
                )))),
            }
        }
        ScalarFunc::Abs => {
            expect_arity("ABS", &args, 1)?;
            match args[0] {
                Value::Null => Ok(Value::Null),
                Value::Integer(value) => value
                    .checked_abs()
                    .map(Value::Integer)
                    .ok_or_else(|| DbError::storage("ABS overflowed i64")),
                Value::Real(value) => Ok(Value::Real(value.abs())),
                Value::Boolean(value) => Ok(Value::Real(if value { 1.0_f64 } else { 0.0_f64 })),
                Value::Text(ref value) => Ok(Value::Real(
                    real_from_numeric_value(&sqlite_text_arithmetic_prefix(value))?.abs(),
                )),
                Value::Blob(ref value) => Ok(Value::Real(
                    real_from_numeric_value(&sqlite_text_arithmetic_prefix(
                        &String::from_utf8_lossy(value),
                    ))?
                    .abs(),
                )),
            }
        }
        ScalarFunc::TypeOf => {
            expect_arity("TYPEOF", &args, 1)?;
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
            expect_arity("SUBTYPE", &args, 1)?;
            Ok(Value::Integer(0))
        }
        ScalarFunc::Hex => {
            expect_arity("HEX", &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Text(String::new())),
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
                    sqlite_real_to_text(*value)
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
        ScalarFunc::Sign => {
            expect_arity("SIGN", &args, 1)?;
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
                    let value = value.trim();
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
        ScalarFunc::Round => {
            if !matches!(args.len(), 1 | 2) {
                return Err(DbError::storage(format!(
                    "ROUND expects 1 or 2 arguments but got {}",
                    args.len()
                )));
            }
            let value = match args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Integer(value) => value as f64,
                Value::Real(value) => value,
                Value::Boolean(value) => {
                    if value {
                        1.0
                    } else {
                        0.0
                    }
                }
                Value::Text(ref value) => {
                    real_from_numeric_value(&sqlite_text_arithmetic_prefix(value))?
                }
                Value::Blob(ref value) => real_from_numeric_value(&sqlite_text_arithmetic_prefix(
                    &String::from_utf8_lossy(value),
                ))?,
            };
            let precision = if args.len() == 2 {
                match args[1] {
                    Value::Null => return Ok(Value::Null),
                    Value::Boolean(value) => {
                        if value {
                            1
                        } else {
                            0
                        }
                    }
                    Value::Integer(value) => i32::try_from(value)
                        .map_err(|_| DbError::storage("ROUND precision does not fit in i32"))?,
                    Value::Real(value) => i32::try_from(value as i64)
                        .map_err(|_| DbError::storage("ROUND precision does not fit in i32"))?,
                    Value::Text(ref value) => {
                        i32::try_from(value.trim().parse::<i64>().unwrap_or(0))
                            .map_err(|_| DbError::storage("ROUND precision does not fit in i32"))?
                    }
                    Value::Blob(ref value) => i32::try_from(
                        String::from_utf8_lossy(value)
                            .trim()
                            .parse::<i64>()
                            .unwrap_or(0),
                    )
                    .map_err(|_| DbError::storage("ROUND precision does not fit in i32"))?,
                }
            } else {
                0
            };
            Ok(Value::Real(sqlite_round_f64(value, precision)))
        }
        ScalarFunc::Char => {
            let mut result = String::new();
            for arg in args {
                let code_point = match cast_value(arg, ColumnType::Integer)? {
                    Value::Null => continue,
                    Value::Integer(value) => value,
                    _ => unreachable!("integer cast must yield INTEGER or NULL"),
                };

                let ch = u32::try_from(code_point)
                    .ok()
                    .and_then(char::from_u32)
                    .unwrap_or(char::REPLACEMENT_CHARACTER);
                result.push(ch);
            }
            Ok(Value::Text(result))
        }
        ScalarFunc::ZeroBlob => {
            expect_arity("ZEROBLOB", &args, 1)?;
            let length = match cast_value(args[0].clone(), ColumnType::Integer)? {
                Value::Null => 0,
                Value::Integer(value) => value,
                _ => unreachable!("integer cast must yield INTEGER or NULL"),
            };
            let length = length.max(0);
            let length = usize::try_from(length)
                .map_err(|_| DbError::storage("ZEROBLOB length is too large"))?;
            Ok(Value::Blob(vec![0; length]))
        }
        ScalarFunc::Likely | ScalarFunc::Unlikely | ScalarFunc::Likelihood => {
            expect_arity(
                match func {
                    ScalarFunc::Likely => "LIKELY",
                    ScalarFunc::Unlikely => "UNLIKELY",
                    ScalarFunc::Likelihood => "LIKELIHOOD",
                    _ => unreachable!(),
                },
                &args,
                match func {
                    ScalarFunc::Likelihood => 2,
                    _ => 1,
                },
            )?;
            Ok(args.into_iter().next().unwrap_or(Value::Null))
        }
        ScalarFunc::Mod => {
            expect_arity("MOD", &args, 2)?;
            let mut args = args.into_iter();
            let left = args.next().unwrap_or(Value::Null);
            let right = args.next().unwrap_or(Value::Null);
            sqlite_mod_function(left, right)
        }
        ScalarFunc::Ceil | ScalarFunc::Ceiling | ScalarFunc::Floor | ScalarFunc::Trunc => {
            expect_arity(
                match func {
                    ScalarFunc::Ceil => "CEIL",
                    ScalarFunc::Ceiling => "CEILING",
                    ScalarFunc::Floor => "FLOOR",
                    ScalarFunc::Trunc => "TRUNC",
                    _ => unreachable!(),
                },
                &args,
                1,
            )?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Integer(value) => Ok(Value::Integer(*value)),
                Value::Real(value) => {
                    let result = match func {
                        ScalarFunc::Ceil | ScalarFunc::Ceiling => value.ceil(),
                        ScalarFunc::Floor => value.floor(),
                        ScalarFunc::Trunc => value.trunc(),
                        _ => unreachable!(),
                    };
                    Ok(Value::Real(result))
                }
                Value::Boolean(value) => Ok(Value::Integer(if *value { 1 } else { 0 })),
                Value::Text(value) => match value.trim().parse::<f64>() {
                    Ok(number) => {
                        let result = match func {
                            ScalarFunc::Ceil | ScalarFunc::Ceiling => number.ceil(),
                            ScalarFunc::Floor => number.floor(),
                            ScalarFunc::Trunc => number.trunc(),
                            _ => unreachable!(),
                        };
                        Ok(Value::Real(result))
                    }
                    Err(_) => Ok(Value::Null),
                },
                Value::Blob(_) => Ok(Value::Null),
            }
        }
        ScalarFunc::Pi => {
            expect_arity("PI", &args, 0)?;
            Ok(Value::Real(std::f64::consts::PI))
        }
        ScalarFunc::Sqrt => {
            expect_arity("SQRT", &args, 1)?;
            sqlite_unary_math_function(&args[0], "SQRT", |value| {
                if value < 0.0 {
                    None
                } else {
                    Some(value.sqrt())
                }
            })
        }
        ScalarFunc::Power => {
            expect_arity("POWER", &args, 2)?;
            sqlite_binary_math_function(&args[0], &args[1], "POWER", |left, right| {
                Some(left.powf(right))
            })
        }
        ScalarFunc::Exp => {
            expect_arity("EXP", &args, 1)?;
            sqlite_unary_math_function(&args[0], "EXP", |value| Some(value.exp()))
        }
        ScalarFunc::Sin => {
            expect_arity("SIN", &args, 1)?;
            sqlite_unary_math_function(&args[0], "SIN", |value| Some(value.sin()))
        }
        ScalarFunc::Cos => {
            expect_arity("COS", &args, 1)?;
            sqlite_unary_math_function(&args[0], "COS", |value| Some(value.cos()))
        }
        ScalarFunc::Tan => {
            expect_arity("TAN", &args, 1)?;
            sqlite_unary_math_function(&args[0], "TAN", |value| Some(value.tan()))
        }
        ScalarFunc::Sinh => {
            expect_arity("SINH", &args, 1)?;
            sqlite_unary_math_function(&args[0], "SINH", |value| Some(value.sinh()))
        }
        ScalarFunc::Cosh => {
            expect_arity("COSH", &args, 1)?;
            sqlite_unary_math_function(&args[0], "COSH", |value| Some(value.cosh()))
        }
        ScalarFunc::Tanh => {
            expect_arity("TANH", &args, 1)?;
            sqlite_unary_math_function(&args[0], "TANH", |value| Some(value.tanh()))
        }
        ScalarFunc::Acos => {
            expect_arity("ACOS", &args, 1)?;
            sqlite_unary_math_function(&args[0], "ACOS", |value| {
                if !(-1.0..=1.0).contains(&value) {
                    None
                } else {
                    Some(value.acos())
                }
            })
        }
        ScalarFunc::Asin => {
            expect_arity("ASIN", &args, 1)?;
            sqlite_unary_math_function(&args[0], "ASIN", |value| {
                if !(-1.0..=1.0).contains(&value) {
                    None
                } else {
                    Some(value.asin())
                }
            })
        }
        ScalarFunc::Atan => {
            expect_arity("ATAN", &args, 1)?;
            sqlite_unary_math_function(&args[0], "ATAN", |value| Some(value.atan()))
        }
        ScalarFunc::Atan2 => {
            expect_arity("ATAN2", &args, 2)?;
            sqlite_binary_math_function(&args[0], &args[1], "ATAN2", |left, right| {
                Some(left.atan2(right))
            })
        }
        ScalarFunc::Acosh => {
            expect_arity("ACOSH", &args, 1)?;
            sqlite_unary_math_function(&args[0], "ACOSH", |value| {
                if value < 1.0 {
                    None
                } else {
                    Some(value.acosh())
                }
            })
        }
        ScalarFunc::Asinh => {
            expect_arity("ASINH", &args, 1)?;
            sqlite_unary_math_function(&args[0], "ASINH", |value| Some(value.asinh()))
        }
        ScalarFunc::Atanh => {
            expect_arity("ATANH", &args, 1)?;
            sqlite_unary_math_function(&args[0], "ATANH", |value| {
                if value <= -1.0 || value >= 1.0 {
                    None
                } else {
                    Some(value.atanh())
                }
            })
        }
        ScalarFunc::Ln => {
            expect_arity("LN", &args, 1)?;
            sqlite_unary_math_function(&args[0], "LN", |value| {
                if value <= 0.0 { None } else { Some(value.ln()) }
            })
        }
        ScalarFunc::Log10 => {
            expect_arity("LOG10", &args, 1)?;
            sqlite_unary_math_function(&args[0], "LOG10", |value| {
                if value <= 0.0 {
                    None
                } else {
                    Some(value.log10())
                }
            })
        }
        ScalarFunc::Log2 => {
            expect_arity("LOG2", &args, 1)?;
            sqlite_unary_math_function(&args[0], "LOG2", |value| {
                if value <= 0.0 {
                    None
                } else {
                    Some(value.log2())
                }
            })
        }
        ScalarFunc::Log => {
            if !matches!(args.len(), 1 | 2) {
                return Err(DbError::storage(format!(
                    "LOG expects 1 or 2 arguments but got {}",
                    args.len()
                )));
            }
            if args.len() == 1 {
                sqlite_unary_math_function(&args[0], "LOG", |value| {
                    if value <= 0.0 {
                        None
                    } else {
                        Some(value.log10())
                    }
                })
            } else {
                sqlite_binary_math_function(&args[0], &args[1], "LOG", |base, value| {
                    if base <= 0.0 || value <= 0.0 || base == 1.0 {
                        None
                    } else {
                        Some(value.log(base))
                    }
                })
            }
        }
        ScalarFunc::Degrees => {
            expect_arity("DEGREES", &args, 1)?;
            sqlite_unary_math_function(&args[0], "DEGREES", |value| Some(value.to_degrees()))
        }
        ScalarFunc::Radians => {
            expect_arity("RADIANS", &args, 1)?;
            sqlite_unary_math_function(&args[0], "RADIANS", |value| Some(value.to_radians()))
        }
        ScalarFunc::Concat => {
            if args.is_empty() {
                return Err(DbError::storage("CONCAT expects at least 1 argument"));
            }
            let mut result = String::new();
            for arg in args {
                match arg {
                    Value::Null => {}
                    value => result.push_str(&coerce_text_like_value(&value)),
                }
            }
            Ok(Value::Text(result))
        }
        ScalarFunc::ConcatWs => {
            if args.len() < 2 {
                return Err(DbError::storage("CONCAT_WS expects at least 2 arguments"));
            }

            let separator = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };

            let mut parts = Vec::new();
            for arg in args.into_iter().skip(1) {
                match arg {
                    Value::Null => {}
                    value => parts.push(coerce_text_like_value(&value)),
                }
            }

            Ok(Value::Text(parts.join(&separator)))
        }
        ScalarFunc::Printf => {
            if args.is_empty() {
                return Err(DbError::storage("PRINTF expects at least 1 argument"));
            }

            let format = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };

            Ok(sqlite_printf(&format, &args[1..])?
                .map(Value::Text)
                .unwrap_or(Value::Null))
        }
        ScalarFunc::Unhex => {
            if !matches!(args.len(), 1 | 2) {
                return Err(DbError::storage(format!(
                    "UNHEX expects 1 or 2 arguments but got {}",
                    args.len()
                )));
            }

            let value = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };

            let ignore = if args.len() == 2 {
                match &args[1] {
                    Value::Null => return Ok(Value::Null),
                    value => Some(coerce_text_like_value(value)),
                }
            } else {
                None
            };

            let mut filtered = String::with_capacity(value.len());
            for ch in value.chars() {
                if ignore
                    .as_ref()
                    .is_some_and(|ignore| ignore.contains(ch) && !ch.is_ascii_hexdigit())
                {
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
            expect_arity("UNISTR", &args, 1)?;
            let value = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };
            Ok(Value::Text(sqlite_unistr(&value)?))
        }
        ScalarFunc::UnistrQuote => {
            expect_arity("UNISTR_QUOTE", &args, 1)?;
            Ok(Value::Text(sqlite_unistr_quote(&args[0])))
        }
        ScalarFunc::Substr => {
            if !matches!(args.len(), 2 | 3) {
                return Err(DbError::storage(format!(
                    "SUBSTR expects 2 or 3 arguments but got {}",
                    args.len()
                )));
            }

            let value = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => value,
            };

            let start = match args[1] {
                Value::Null => return Ok(Value::Null),
                Value::Boolean(value) => {
                    if value {
                        1
                    } else {
                        0
                    }
                }
                Value::Integer(value) => value,
                Value::Real(value) => value as i64,
                Value::Text(ref value) => value.trim().parse::<i64>().unwrap_or(0),
                Value::Blob(ref value) => String::from_utf8_lossy(value)
                    .trim()
                    .parse::<i64>()
                    .unwrap_or(0),
            };

            let length = if args.len() == 3 {
                match args[2] {
                    Value::Null => return Ok(Value::Null),
                    Value::Boolean(value) => Some(if value { 1 } else { 0 }),
                    Value::Integer(value) => Some(value),
                    Value::Real(value) => Some(value as i64),
                    Value::Text(ref value) => Some(value.trim().parse::<i64>().unwrap_or(0)),
                    Value::Blob(ref value) => Some(
                        String::from_utf8_lossy(value)
                            .trim()
                            .parse::<i64>()
                            .unwrap_or(0),
                    ),
                }
            } else {
                None
            };

            match value {
                Value::Blob(value) => Ok(Value::Blob(sqlite_substr_blob(value, start, length))),
                value => Ok(Value::Text(sqlite_substr_text(
                    sqlite_text_prefix_before_nul(&coerce_text_like_value(value)),
                    start,
                    length,
                ))),
            }
        }
        ScalarFunc::Trim => evaluate_trim_family_function("TRIM", &args, |value, characters| {
            value.trim_matches(|ch| characters.contains(ch)).to_string()
        }),
        ScalarFunc::LTrim => evaluate_trim_family_function("LTRIM", &args, |value, characters| {
            value
                .trim_start_matches(|ch| characters.contains(ch))
                .to_string()
        }),
        ScalarFunc::RTrim => evaluate_trim_family_function("RTRIM", &args, |value, characters| {
            value
                .trim_end_matches(|ch| characters.contains(ch))
                .to_string()
        }),
        ScalarFunc::Instr => {
            expect_arity("INSTR", &args, 2)?;
            sqlite_instr_value(&args[0], &args[1])
        }
        ScalarFunc::Replace => {
            expect_arity("REPLACE", &args, 3)?;

            let value = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => sqlite_text_prefix_before_nul(&coerce_text_like_value(value)).to_string(),
            };

            let pattern = match &args[1] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };

            let replacement = match &args[2] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };

            if pattern.is_empty() {
                return Ok(Value::Text(value));
            }

            Ok(Value::Text(value.replace(&pattern, &replacement)))
        }
        ScalarFunc::LikeFunc => {
            if !(2..=3).contains(&args.len()) {
                return Err(DbError::storage(format!(
                    "LIKE expects 2 or 3 arguments but got {}",
                    args.len()
                )));
            }
            let escape = if let Some(escape) = args.get(2) {
                match escape {
                    Value::Null => return Ok(Value::Null),
                    value => {
                        let text = coerce_text_like_value(value);
                        let _ = like_escape_char(Some(text.as_str()))?;
                        Some(text)
                    }
                }
            } else {
                None
            };
            let pattern = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };
            let value = match &args[1] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };
            Ok(Value::Boolean(matches_like_pattern(
                &value,
                &pattern,
                escape.as_deref(),
                case_sensitive_like,
            )?))
        }
        ScalarFunc::GlobFunc => {
            expect_arity("GLOB", &args, 2)?;
            let pattern = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };
            let value = match &args[1] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };
            Ok(Value::Boolean(matches_glob_pattern(&value, &pattern)))
        }
        ScalarFunc::RegexpFunc => {
            expect_arity("REGEXP", &args, 2)?;
            let pattern = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };
            let value = match &args[1] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };
            Ok(Value::Boolean(sqlite_regexp_matches(&pattern, &value)?))
        }
        ScalarFunc::MatchFunc => {
            expect_arity("MATCH", &args, 2)?;
            Err(DbError::storage(
                "unable to use function MATCH in the requested context",
            ))
        }
        ScalarFunc::NullIf => {
            expect_arity("NULLIF", &args, 2)?;
            if args[0] == args[1] {
                Ok(Value::Null)
            } else {
                Ok(args[0].clone())
            }
        }
        ScalarFunc::IfNull => {
            expect_arity("IFNULL", &args, 2)?;
            if matches!(args[0], Value::Null) {
                Ok(args[1].clone())
            } else {
                Ok(args[0].clone())
            }
        }
        ScalarFunc::Coalesce => {
            if args.len() < 2 {
                return Err(DbError::storage("COALESCE expects at least 2 arguments"));
            }
            Ok(args
                .into_iter()
                .find(|value| !matches!(value, Value::Null))
                .unwrap_or(Value::Null))
        }
        ScalarFunc::Unicode => {
            expect_arity("UNICODE", &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                value => Ok(
                    sqlite_text_prefix_before_nul(&coerce_text_like_value(value))
                        .chars()
                        .next()
                        .map(|ch| Value::Integer(i64::from(u32::from(ch))))
                        .unwrap_or(Value::Null),
                ),
            }
        }
        ScalarFunc::Quote => {
            expect_arity("QUOTE", &args, 1)?;
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
                Value::Real(value) => sqlite_real_to_text_for_quote(*value),
                Value::Blob(value) => format!(
                    "X'{}'",
                    value
                        .iter()
                        .map(|byte| format!("{byte:02X}"))
                        .collect::<String>()
                ),
                Value::Text(value) => {
                    format!(
                        "'{}'",
                        sqlite_text_prefix_before_nul(value).replace('\'', "''")
                    )
                }
            };
            Ok(Value::Text(quoted))
        }
        ScalarFunc::Json => {
            expect_arity("JSON", &args, 1)?;
            json_normalize_value(&args[0])
        }
        ScalarFunc::JsonValid => {
            if !matches!(args.len(), 1 | 2) {
                return Err(DbError::storage(format!(
                    "JSON_VALID expects 1 or 2 arguments but got {}",
                    args.len()
                )));
            }
            let json = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };
            let valid = if let Some(flags) = args.get(1) {
                let flags = json_valid_flags(flags)?;
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
            expect_arity("JSON_ERROR_POSITION", &args, 1)?;
            json_error_position_value(&args[0])
        }
        ScalarFunc::JsonPretty => json_pretty_value(&args),
        ScalarFunc::JsonQuote => {
            expect_arity("JSON_QUOTE", &args, 1)?;
            Ok(Value::Text(json_quote_value(&args[0])?))
        }
        ScalarFunc::JsonExtract => {
            if args.len() < 2 {
                return Err(DbError::storage(format!(
                    "JSON_EXTRACT expects at least 2 arguments but got {}",
                    args.len()
                )));
            }
            let json = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };
            if args.len() == 2 {
                let path = match &args[1] {
                    Value::Null => return Ok(Value::Null),
                    value => coerce_text_like_value(value),
                };
                json_extract_value(&json, &path)
            } else {
                let paths = args[1..]
                    .iter()
                    .map(|value| match value {
                        Value::Null => None,
                        value => Some(coerce_text_like_value(value)),
                    })
                    .collect::<Vec<_>>();
                if paths.iter().any(Option::is_none) {
                    return Ok(Value::Null);
                }
                json_extract_multi_value(&json, &paths)
            }
        }
        ScalarFunc::JsonType => {
            if !matches!(args.len(), 1 | 2) {
                return Err(DbError::storage(format!(
                    "JSON_TYPE expects 1 or 2 arguments but got {}",
                    args.len()
                )));
            }
            let json = match &args[0] {
                Value::Null => return Ok(Value::Null),
                value => coerce_text_like_value(value),
            };
            let parsed = parse_sqlite_json_value(&json)
                .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
            let value = if let Some(path) = args.get(1) {
                let path = match path {
                    Value::Null => return Ok(Value::Null),
                    value => coerce_text_like_value(value),
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
                .map(sql_value_to_json)
                .collect::<Result<Vec<_>>>()?;
            serde_json::to_string(&values)
                .map(Value::Text)
                .map_err(|error| DbError::storage(format!("failed to render JSON array: {error}")))
        }
        ScalarFunc::JsonObject => json_object_value(&args),
        ScalarFunc::JsonArrayLength => json_array_length_value(&args),
        ScalarFunc::JsonRemove => json_remove_value(&args),
        ScalarFunc::JsonSet => json_set_value(&args),
        ScalarFunc::JsonInsert => json_write_value("json_insert", &args, JsonWriteMode::Insert),
        ScalarFunc::JsonReplace => json_write_value("json_replace", &args, JsonWriteMode::Replace),
        ScalarFunc::JsonPatch => json_patch_value(&args),
        ScalarFunc::SqliteLog => {
            expect_arity("SQLITE_LOG", &args, 2)?;
            Ok(Value::Null)
        }
        ScalarFunc::MinScalar => {
            if args.is_empty() {
                return Err(DbError::storage("MIN expects at least 1 argument"));
            }
            evaluate_min_max_scalar_function("MIN", &args, true)
        }
        ScalarFunc::MaxScalar => {
            if args.is_empty() {
                return Err(DbError::storage("MAX expects at least 1 argument"));
            }
            evaluate_min_max_scalar_function("MAX", &args, false)
        }
        ScalarFunc::IIf | ScalarFunc::If => {
            unreachable!("short-circuit scalar functions are evaluated before eager dispatch")
        }
        _ => Err(DbError::storage(format!(
            "expression index term uses unsupported scalar function {:?}",
            func
        ))),
    }
}

fn evaluate_trim_family_function<F>(function_name: &str, args: &[Value], trim: F) -> Result<Value>
where
    F: FnOnce(&str, &str) -> String,
{
    if !matches!(args.len(), 1 | 2) {
        return Err(DbError::storage(format!(
            "{function_name} expects 1 or 2 arguments but got {}",
            args.len()
        )));
    }

    let value = match &args[0] {
        Value::Null => return Ok(Value::Null),
        value => coerce_text_like_value(value),
    };

    let characters = if args.len() == 2 {
        match &args[1] {
            Value::Null => return Ok(Value::Null),
            value => coerce_text_like_value(value),
        }
    } else {
        " ".to_string()
    };

    Ok(Value::Text(trim(&value, &characters)))
}

fn sqlite_printf(format: &str, args: &[Value]) -> Result<Option<String>> {
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
                '!' => flags.alternate_form_2 = true,
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
            dynamic_width = Some(sqlite_printf_integer_arg(&width_arg));
        }
        let precision = if chars.peek() == Some(&'.') {
            chars.next();
            if chars.peek() == Some(&'*') {
                chars.next();
                let precision_arg = args.get(arg_index).cloned().unwrap_or(Value::Null);
                arg_index += 1;
                let precision = sqlite_printf_integer_arg(&precision_arg);
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

        let Some(spec) = chars.next() else {
            rendered.push('%');
            break;
        };
        let arg = if spec == 'n' {
            Value::Null
        } else {
            let arg = args.get(arg_index).cloned().unwrap_or(Value::Null);
            arg_index += 1;
            arg
        };

        match spec {
            'd' | 'i' => {
                let value = match arg {
                    Value::Null => 0,
                    Value::Integer(value) => value,
                    Value::Real(value) => value as i64,
                    Value::Boolean(value) => {
                        if value {
                            1
                        } else {
                            0
                        }
                    }
                    Value::Text(value) => value.trim().parse::<i64>().unwrap_or(0),
                    Value::Blob(value) => String::from_utf8_lossy(&value)
                        .trim()
                        .parse::<i64>()
                        .unwrap_or(0),
                };

                let rendered_value = format_sqlite_signed_integer(value, flags, precision);
                push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
            }
            'u' => {
                let value = match arg {
                    Value::Null => 0,
                    Value::Integer(value) => value,
                    Value::Real(value) => value as i64,
                    Value::Boolean(value) => {
                        if value {
                            1
                        } else {
                            0
                        }
                    }
                    Value::Text(value) => value.trim().parse::<i64>().unwrap_or(0),
                    Value::Blob(value) => String::from_utf8_lossy(&value)
                        .trim()
                        .parse::<i64>()
                        .unwrap_or(0),
                } as u64;

                let rendered_value = format_sqlite_unsigned_integer(value, flags, precision);
                push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
            }
            'r' => {
                let value = sqlite_printf_integer_arg(&arg);
                let mut rendered_value = format_sqlite_signed_integer(value, flags, precision);
                rendered_value.push_str(sqlite_ordinal_suffix(value));
                push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
            }
            'f' => {
                let value = sqlite_printf_real_arg(&arg);
                let mut rendered_value = if let Some(infinity) = sqlite_printf_infinity_text(value)
                {
                    infinity
                } else if let Some(precision) = precision {
                    format!("{value:.precision$}")
                } else {
                    format!("{value:.6}")
                };
                if flags.alternate && !rendered_value.contains('.') {
                    rendered_value.push('.');
                }
                rendered_value = apply_sqlite_numeric_flags(rendered_value, flags);
                push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
            }
            'e' | 'E' => {
                let value = sqlite_printf_real_arg(&arg);
                let precision = precision.unwrap_or(6);
                let mut rendered_value = if let Some(infinity) = sqlite_printf_infinity_text(value)
                {
                    infinity
                } else {
                    normalize_sqlite_exponent(format!("{value:.precision$e}"), spec)
                };
                if flags.alternate {
                    rendered_value = ensure_sqlite_exponent_decimal_point(rendered_value);
                }
                rendered_value = apply_sqlite_numeric_flags(rendered_value, flags);
                push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
            }
            'g' | 'G' => {
                let value = sqlite_printf_real_arg(&arg);
                let precision = precision.unwrap_or(6);
                let mut rendered_value = sqlite_printf_infinity_text(value)
                    .unwrap_or_else(|| sqlite_printf_general_float(value, precision));
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
                rendered_value = apply_sqlite_numeric_flags(rendered_value, flags);
                push_sqlite_printf_numeric(&mut rendered, &rendered_value, width, flags);
            }
            'x' | 'X' | 'o' => {
                let value = match arg {
                    Value::Null => 0,
                    Value::Integer(value) => value,
                    Value::Real(value) => value as i64,
                    Value::Boolean(value) => {
                        if value {
                            1
                        } else {
                            0
                        }
                    }
                    Value::Text(value) => value.trim().parse::<i64>().unwrap_or(0),
                    Value::Blob(value) => String::from_utf8_lossy(&value)
                        .trim()
                        .parse::<i64>()
                        .unwrap_or(0),
                } as u64;
                let raw = match spec {
                    'x' => format!("{value:x}"),
                    'X' => format!("{value:X}"),
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
                        'X' => format!("0X{raw}"),
                        'o' => format!("0{raw}"),
                        _ => raw,
                    }
                } else {
                    raw
                };
                if flags.zero_pad && width > 0 {
                    push_sqlite_printf_prefixed_numeric(&mut rendered, &raw, width, flags);
                } else if width > 0 {
                    push_sqlite_printf_text(&mut rendered, &raw, width, flags.left_align);
                } else {
                    rendered.push_str(&raw);
                }
            }
            'p' => {
                let value = match arg {
                    Value::Null => 0,
                    Value::Integer(value) => value,
                    Value::Real(value) => value as i64,
                    Value::Boolean(value) => {
                        if value {
                            1
                        } else {
                            0
                        }
                    }
                    Value::Text(value) => value.trim().parse::<i64>().unwrap_or(0),
                    Value::Blob(value) => String::from_utf8_lossy(&value)
                        .trim()
                        .parse::<i64>()
                        .unwrap_or(0),
                } as u64;
                let raw = format!("{value:X}");
                if flags.zero_pad && width > 0 {
                    rendered.push_str(&format!("{raw:0>width$}", width = width));
                } else if width > 0 {
                    push_sqlite_printf_text(&mut rendered, &raw, width, flags.left_align);
                } else {
                    rendered.push_str(&raw);
                }
            }
            'n' => {}
            'c' => {
                let rendered_value = match arg {
                    Value::Null => String::new(),
                    value => {
                        let repeat = precision.unwrap_or(1).max(1);
                        coerce_text_like_value(&value)
                            .chars()
                            .next()
                            .map(|ch| ch.to_string().repeat(repeat))
                            .unwrap_or_default()
                    }
                };
                push_sqlite_printf_text(
                    &mut rendered,
                    &rendered_value,
                    width,
                    flags.left_align && !flags.zero_pad,
                );
            }
            's' => {
                let mut value = match arg {
                    Value::Null => String::new(),
                    value => {
                        sqlite_text_prefix_before_nul(&coerce_text_like_value(&value)).to_string()
                    }
                };
                if let Some(precision) = precision {
                    value = truncate_sqlite_printf_text(&value, precision, flags);
                }
                push_sqlite_printf_text(
                    &mut rendered,
                    &value,
                    width,
                    flags.left_align && !flags.zero_pad,
                );
            }
            'z' => {
                let mut value = match arg {
                    Value::Null => String::new(),
                    value => {
                        sqlite_text_prefix_before_nul(&coerce_text_like_value(&value)).to_string()
                    }
                };
                if let Some(precision) = precision {
                    value = truncate_sqlite_printf_text(&value, precision, flags);
                }
                push_sqlite_printf_text(
                    &mut rendered,
                    &value,
                    width,
                    flags.left_align && !flags.zero_pad,
                );
            }
            'q' => {
                let value = match arg {
                    Value::Null => "(NULL)".to_string(),
                    value if flags.alternate => sqlite_unistr_quote_unquoted(&value),
                    value => sqlite_text_prefix_before_nul(&coerce_text_like_value(&value))
                        .replace('\'', "''"),
                };
                push_sqlite_printf_text(
                    &mut rendered,
                    &value,
                    width,
                    flags.left_align && !flags.zero_pad,
                );
            }
            'Q' => {
                let value = match arg {
                    Value::Null => "NULL".to_string(),
                    value if flags.alternate => sqlite_unistr_quote(&value),
                    value => format!(
                        "'{}'",
                        sqlite_text_prefix_before_nul(&coerce_text_like_value(&value))
                            .replace('\'', "''")
                    ),
                };
                push_sqlite_printf_text(
                    &mut rendered,
                    &value,
                    width,
                    flags.left_align && !flags.zero_pad,
                );
            }
            'w' => {
                let value = match arg {
                    Value::Null => "(NULL)".to_string(),
                    value => sqlite_text_prefix_before_nul(&coerce_text_like_value(&value))
                        .replace('"', "\"\""),
                };
                push_sqlite_printf_text(
                    &mut rendered,
                    &value,
                    width,
                    flags.left_align && !flags.zero_pad,
                );
            }
            other => {
                let _ = other;
                return Ok((!rendered.is_empty()).then_some(rendered));
            }
        }
    }

    Ok(Some(rendered))
}

#[derive(Debug, Clone, Copy, Default)]
struct SqlitePrintfFlags {
    left_align: bool,
    sign_plus: bool,
    sign_space: bool,
    zero_pad: bool,
    grouping: bool,
    alternate: bool,
    alternate_form_2: bool,
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
        sqlite_group_digits(digits)
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
        sqlite_group_digits(digits)
    } else {
        digits
    }
}

fn truncate_sqlite_printf_text(value: &str, precision: usize, flags: SqlitePrintfFlags) -> String {
    if flags.alternate_form_2 {
        return value.chars().take(precision).collect();
    }
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(value.len()))
        .take_while(|index| *index <= precision)
        .last()
        .unwrap_or(0);
    value[..end].to_string()
}

fn sqlite_ordinal_suffix(value: i64) -> &'static str {
    let abs = value.unsigned_abs();
    let last_two = abs % 100;
    if (11..=13).contains(&last_two) {
        return "th";
    }
    match abs % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
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
        push_sqlite_printf_numeric(rendered, value, width, flags);
    }
}

fn apply_sqlite_numeric_flags(rendered: String, flags: SqlitePrintfFlags) -> String {
    let with_grouping = if flags.grouping {
        sqlite_group_decimal(rendered)
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
        return sqlite_group_signed_digits(rendered);
    };
    let integer = sqlite_group_signed_digits(rendered[..split_index].to_string());
    format!("{integer}{}", &rendered[split_index..])
}

fn sqlite_group_signed_digits(rendered: String) -> String {
    if let Some(digits) = rendered.strip_prefix('-') {
        format!("-{}", sqlite_group_digits(digits.to_string()))
    } else if let Some(digits) = rendered.strip_prefix('+') {
        format!("+{}", sqlite_group_digits(digits.to_string()))
    } else if let Some(digits) = rendered.strip_prefix(' ') {
        format!(" {}", sqlite_group_digits(digits.to_string()))
    } else {
        sqlite_group_digits(rendered)
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
        normalize_sqlite_exponent(raw, 'e')
    } else {
        let decimals = (significant as i32 - exponent - 1).max(0) as usize;
        format!("{value:.decimals$}")
    };

    trim_printf_general_float(rendered)
}

fn sqlite_printf_infinity_text(value: f64) -> Option<String> {
    if value == f64::INFINITY {
        Some("Inf".to_string())
    } else if value == f64::NEG_INFINITY {
        Some("-Inf".to_string())
    } else {
        None
    }
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
            Some(_) => output.push(parse_unistr_escape(&mut chars, 4)?),
            None => return Err(DbError::storage("invalid Unicode escape")),
        }
    }
    Ok(output)
}

fn sqlite_unistr_quote(value: &Value) -> String {
    let Value::Text(value) = value else {
        return sqlite_quote_value(value);
    };
    let value = sqlite_text_prefix_before_nul(value);
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

fn sqlite_unistr_quote_unquoted(value: &Value) -> String {
    let quoted = sqlite_unistr_quote(value);
    quoted
        .strip_prefix("unistr('")
        .and_then(|value| value.strip_suffix("')"))
        .or_else(|| {
            quoted
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .map(str::to_string)
        .unwrap_or(quoted)
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
    if value == f64::INFINITY {
        return "9.0e+999".to_string();
    }
    if value == f64::NEG_INFINITY {
        return "-9.0e+999".to_string();
    }

    sqlite_real_to_text(value)
}

fn json_quote_value(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok("null".to_string()),
        Value::Boolean(value) => Ok(if *value { "1" } else { "0" }.to_string()),
        Value::Integer(value) => Ok(value.to_string()),
        Value::Real(value) => Ok(sqlite_real_to_text(*value)),
        Value::Text(value) => serde_json::to_string(value)
            .map_err(|error| DbError::storage(format!("failed to quote JSON string: {error}"))),
        Value::Blob(_) => Err(DbError::storage("JSON cannot hold BLOB values")),
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
        Value::Blob(_) => return Err(DbError::storage("JSON cannot hold BLOB values")),
    })
}

fn sqlite_instr_value(haystack: &Value, needle: &Value) -> Result<Value> {
    match (haystack, needle) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Blob(haystack), Value::Blob(needle)) => {
            Ok(Value::Integer(sqlite_instr_blob(haystack, needle)))
        }
        (haystack, needle) => {
            let haystack = coerce_text_like_value(haystack);
            let needle = coerce_text_like_value(needle);

            if needle.is_empty() {
                return Ok(Value::Integer(1));
            }

            let position = haystack
                .find(&needle)
                .map(|byte_index| haystack[..byte_index].chars().count() as i64 + 1)
                .unwrap_or(0);
            Ok(Value::Integer(position))
        }
    }
}

fn json_normalize_value(value: &Value) -> Result<Value> {
    let json = match value {
        Value::Null => return Ok(Value::Null),
        value => coerce_text_like_value(value),
    };
    let parsed = parse_sqlite_json_value(&json)
        .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
    serde_json::to_string(&parsed)
        .map(Value::Text)
        .map_err(|error| DbError::storage(format!("failed to render JSON value: {error}")))
}

fn json_valid_flags(value: &Value) -> Result<i64> {
    let Value::Integer(flags) = cast_value(value.clone(), ColumnType::Integer)? else {
        return Err(DbError::storage(
            "FLAGS parameter to json_valid() must be between 1 and 15",
        ));
    };
    if !(1..=15).contains(&flags) {
        return Err(DbError::storage(
            "FLAGS parameter to json_valid() must be between 1 and 15",
        ));
    }
    Ok(flags)
}

fn json_error_position_value(value: &Value) -> Result<Value> {
    let json = match value {
        Value::Null => return Ok(Value::Null),
        value => coerce_text_like_value(value),
    };
    match parse_sqlite_json_value(&json) {
        Ok(_) => Ok(Value::Integer(0)),
        Err(error) => Ok(Value::Integer(json_error_position(&json, &error))),
    }
}

fn json_pretty_value(args: &[Value]) -> Result<Value> {
    if !matches!(args.len(), 1 | 2) {
        return Err(DbError::storage(format!(
            "JSON_PRETTY expects 1 or 2 arguments but got {}",
            args.len()
        )));
    }
    let json = match &args[0] {
        Value::Null => return Ok(Value::Null),
        value => coerce_text_like_value(value),
    };
    let indent = if let Some(indent) = args.get(1) {
        match indent {
            Value::Null => "    ".to_string(),
            value => coerce_text_like_value(value),
        }
    } else {
        "    ".to_string()
    };
    let parsed = parse_sqlite_json_value(&json)
        .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
    Ok(Value::Text(json_pretty_render(&parsed, &indent)))
}

fn json_object_value(args: &[Value]) -> Result<Value> {
    if args.len() % 2 != 0 {
        return Err(DbError::storage(
            "json_object() requires an even number of arguments",
        ));
    }

    let mut fields = Vec::with_capacity(args.len() / 2);
    for pair in args.chunks_exact(2) {
        let Value::Text(label) = &pair[0] else {
            return Err(DbError::storage("json_object() labels must be TEXT"));
        };
        let label = serde_json::to_string(label)
            .map_err(|error| DbError::storage(format!("failed to quote JSON label: {error}")))?;
        let value = sql_value_to_json(&pair[1])?;
        let value = serde_json::to_string(&value).map_err(|error| {
            DbError::storage(format!("failed to render JSON object value: {error}"))
        })?;
        fields.push(format!("{label}:{value}"));
    }

    Ok(Value::Text(format!("{{{}}}", fields.join(","))))
}

fn json_array_length_value(args: &[Value]) -> Result<Value> {
    if !matches!(args.len(), 1 | 2) {
        return Err(DbError::storage(format!(
            "JSON_ARRAY_LENGTH expects 1 or 2 arguments but got {}",
            args.len()
        )));
    }

    let json = match &args[0] {
        Value::Null => return Ok(Value::Null),
        value => coerce_text_like_value(value),
    };
    let parsed = parse_sqlite_json_value(&json)
        .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
    let value = if let Some(path) = args.get(1) {
        let path = match path {
            Value::Null => return Ok(Value::Null),
            value => coerce_text_like_value(value),
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
        return Err(DbError::storage("JSON_REMOVE expects at least 1 argument"));
    }
    let json = match &args[0] {
        Value::Null => return Ok(Value::Null),
        value => coerce_text_like_value(value),
    };
    let mut parsed = parse_sqlite_json_value(&json)
        .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
    for path in &args[1..] {
        let path = match path {
            Value::Null => return Ok(Value::Null),
            value => coerce_text_like_value(value),
        };
        if path == "$" {
            return Ok(Value::Null);
        }
        json_remove_path(&mut parsed, &path)
            .map_err(|_| DbError::storage(format!("bad JSON path: '{path}'")))?;
    }
    serde_json::to_string(&parsed)
        .map(Value::Text)
        .map_err(|error| DbError::storage(format!("failed to render JSON value: {error}")))
}

fn json_set_value(args: &[Value]) -> Result<Value> {
    json_write_value("json_set", args, JsonWriteMode::Set)
}

fn json_write_value(function_name: &str, args: &[Value], mode: JsonWriteMode) -> Result<Value> {
    if args.len() % 2 == 0 {
        return Err(DbError::storage(format!(
            "{function_name}() needs an odd number of arguments"
        )));
    }
    let json = match &args[0] {
        Value::Null => return Ok(Value::Null),
        value => coerce_text_like_value(value),
    };
    let mut parsed = parse_sqlite_json_value(&json)
        .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
    for pair in args[1..].chunks_exact(2) {
        let path = match &pair[0] {
            Value::Null => continue,
            value => coerce_text_like_value(value),
        };
        let replacement = sql_value_to_json(&pair[1])?;
        if path == "$" {
            match mode {
                JsonWriteMode::Set | JsonWriteMode::Replace => parsed = replacement,
                JsonWriteMode::Insert => {}
            }
            continue;
        }
        json_write_path(&mut parsed, &path, replacement, mode)
            .map_err(|_| DbError::storage(format!("bad JSON path: '{path}'")))?;
    }
    serde_json::to_string(&parsed)
        .map(Value::Text)
        .map_err(|error| DbError::storage(format!("failed to render JSON value: {error}")))
}

fn json_patch_value(args: &[Value]) -> Result<Value> {
    expect_arity("JSON_PATCH", args, 2)?;
    let target = match &args[0] {
        Value::Null => return Ok(Value::Null),
        value => coerce_text_like_value(value),
    };
    let patch = match &args[1] {
        Value::Null => return Ok(Value::Null),
        value => coerce_text_like_value(value),
    };
    let mut target = parse_sqlite_json_value(&target)
        .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
    let patch = parse_sqlite_json_value(&patch)
        .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
    json_merge_patch(&mut target, patch);
    serde_json::to_string(&target)
        .map(Value::Text)
        .map_err(|error| DbError::storage(format!("failed to render JSON value: {error}")))
}

fn json_extract_value(json: &str, path: &str) -> Result<Value> {
    let parsed = parse_sqlite_json_value(json)
        .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
    let Some(value) = json_path_lookup(&parsed, path)? else {
        return Ok(Value::Null);
    };
    json_value_to_sql(value)
}

fn json_extract_multi_value(json: &str, paths: &[Option<String>]) -> Result<Value> {
    let parsed = parse_sqlite_json_value(json)
        .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
    let mut values = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(path) = path else {
            values.push(serde_json::Value::Null);
            continue;
        };
        let Some(value) = json_path_lookup(&parsed, path)? else {
            values.push(serde_json::Value::Null);
            continue;
        };
        values.push(value.clone());
    }
    serde_json::to_string(&values)
        .map(Value::Text)
        .map_err(|error| DbError::storage(format!("failed to render JSON value: {error}")))
}

fn json_path_lookup<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Result<Option<&'a serde_json::Value>> {
    let mut current = value;
    let mut remaining = path
        .strip_prefix('$')
        .ok_or_else(|| DbError::storage("JSON path must start with '$'"))?;
    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix('.') {
            let (key, tail) = json_path_object_key(rest)?;
            let Some(next) = current.get(key.as_ref()) else {
                return Ok(None);
            };
            current = next;
            remaining = tail;
            continue;
        }
        if let Some(rest) = remaining.strip_prefix('[') {
            let Some(index_end) = rest.find(']') else {
                return Err(DbError::storage("invalid JSON path"));
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
        return Err(DbError::storage("invalid JSON path"));
    }
    Ok(Some(current))
}

fn json_path_object_key(rest: &str) -> Result<(std::borrow::Cow<'_, str>, &str)> {
    if rest.starts_with('"') {
        let mut escaped = false;
        for (offset, ch) in rest[1..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => {
                    let end = offset + 2;
                    let key = serde_json::from_str::<String>(&rest[..end])
                        .map_err(|_| DbError::storage("invalid JSON path"))?;
                    return Ok((std::borrow::Cow::Owned(key), &rest[end..]));
                }
                _ => {}
            }
        }
        return Err(DbError::storage("invalid JSON path"));
    }

    let key_end = rest.find(['.', '[']).unwrap_or(rest.len());
    let key = &rest[..key_end];
    if key.is_empty() {
        return Err(DbError::storage("invalid JSON path"));
    }
    Ok((std::borrow::Cow::Borrowed(key), &rest[key_end..]))
}

fn json_container_for_path_tail(tail: &str) -> serde_json::Value {
    if tail.starts_with('[') {
        serde_json::Value::Array(Vec::new())
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    }
}

fn json_path_array_index(value: &serde_json::Value, index: &str) -> Result<Option<usize>> {
    if index == "#" {
        return Ok(None);
    }
    if index.starts_with('"') && serde_json::from_str::<String>(index).is_ok() {
        return Ok(None);
    }
    if let Some(tail) = index.strip_prefix("#-") {
        let offset = tail
            .parse::<usize>()
            .map_err(|_| DbError::storage("invalid JSON array index"))?;
        let Some(length) = value.as_array().map(Vec::len) else {
            return Ok(None);
        };
        return Ok(length.checked_sub(offset));
    }
    index
        .parse::<usize>()
        .map(Some)
        .map_err(|_| DbError::storage("invalid JSON array index"))
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
        .ok_or_else(|| DbError::storage("JSON path must start with '$'"))?;
    if remaining.is_empty() {
        return Err(DbError::storage("root path is handled by caller"));
    }
    json_remove_path_tail(value, remaining)
}

fn json_remove_path_tail(value: &mut serde_json::Value, remaining: &str) -> Result<()> {
    if let Some(rest) = remaining.strip_prefix('.') {
        let (key, tail) = json_path_object_key(rest)?;
        if tail.is_empty() {
            if let serde_json::Value::Object(object) = value {
                object.remove(key.as_ref());
            }
            return Ok(());
        }
        let Some(next) = value.get_mut(key.as_ref()) else {
            return Ok(());
        };
        return json_remove_path_tail(next, tail);
    }

    if let Some(rest) = remaining.strip_prefix('[') {
        let Some(index_end) = rest.find(']') else {
            return Err(DbError::storage("invalid JSON path"));
        };
        let Some(index) = json_path_array_index(value, &rest[..index_end])? else {
            return Ok(());
        };
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

    Err(DbError::storage("invalid JSON path"))
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
        .ok_or_else(|| DbError::storage("JSON path must start with '$'"))?;
    if remaining.is_empty() {
        return Err(DbError::storage("root path is handled by caller"));
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
        let (key, tail) = json_path_object_key(rest)?;
        if tail.is_empty() {
            if !value.is_object() {
                if matches!(mode, JsonWriteMode::Replace) {
                    return Ok(());
                }
                *value = serde_json::Value::Object(serde_json::Map::new());
            }
            if let serde_json::Value::Object(object) = value {
                let exists = object.contains_key(key.as_ref());
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
        let next = match object.get_mut(key.as_ref()) {
            Some(next) => next,
            None if matches!(mode, JsonWriteMode::Set | JsonWriteMode::Insert) => object
                .entry(key.to_string())
                .or_insert_with(|| json_container_for_path_tail(tail)),
            None => return Ok(()),
        };
        return json_write_path_tail(next, tail, replacement, mode);
    }

    if let Some(rest) = remaining.strip_prefix('[') {
        let Some(index_end) = rest.find(']') else {
            return Err(DbError::storage("invalid JSON path"));
        };
        let index_token = &rest[..index_end];
        let tail = &rest[index_end + 1..];
        if index_token == "#" {
            let serde_json::Value::Array(values) = value else {
                return Ok(());
            };
            if tail.is_empty() {
                if matches!(mode, JsonWriteMode::Set | JsonWriteMode::Insert) {
                    values.push(replacement);
                }
                return Ok(());
            }
            if matches!(mode, JsonWriteMode::Replace) {
                return Ok(());
            }
            values.push(json_container_for_path_tail(tail));
            let next = values
                .last_mut()
                .expect("pushed JSON value must be addressable");
            return json_write_path_tail(next, tail, replacement, mode);
        }
        let Some(index) = json_path_array_index(value, index_token)? else {
            return Ok(());
        };
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
        if index == values.len() && matches!(mode, JsonWriteMode::Set | JsonWriteMode::Insert) {
            values.push(json_container_for_path_tail(tail));
            let next = values
                .last_mut()
                .expect("pushed JSON value must be addressable");
            return json_write_path_tail(next, tail, replacement, mode);
        }
        let Some(next) = values.get_mut(index) else {
            return Ok(());
        };
        return json_write_path_tail(next, tail, replacement, mode);
    }

    Err(DbError::storage("invalid JSON path"))
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
            .map_err(|error| DbError::storage(format!("failed to render JSON value: {error}"))),
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
            return Err(DbError::storage("invalid Unicode escape"));
        };
        let Some(digit) = ch.to_digit(16) else {
            return Err(DbError::storage("invalid Unicode escape"));
        };
        value = (value << 4) | digit;
    }
    char::from_u32(value).ok_or_else(|| DbError::storage("invalid Unicode escape"))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

fn sqlite_text_prefix_before_nul(value: &str) -> &str {
    value.split('\0').next().unwrap_or(value)
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

fn sqlite_instr_blob(haystack: &[u8], needle: &[u8]) -> i64 {
    if needle.is_empty() {
        return 1;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| index as i64 + 1)
        .unwrap_or(0)
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
        candidate.parse::<i64>().unwrap_or_else(|_| {
            if candidate.starts_with('-') {
                i64::MIN
            } else {
                i64::MAX
            }
        })
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

fn sqlite_not_value(value: &Value) -> Value {
    match value {
        Value::Null => Value::Null,
        value => Value::Boolean(!sqlite_is_true_value(value)),
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
        Value::Blob(value) => match sqlite_text_numeric_prefix(&String::from_utf8_lossy(value)) {
            Value::Integer(value) => value != 0,
            Value::Real(value) => value != 0.0,
            _ => unreachable!("sqlite numeric prefix only returns numeric values"),
        },
    }
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

#[derive(Clone, Copy)]
enum DateShiftRounding {
    Ceiling,
    Floor,
}

impl Default for DateShiftRounding {
    fn default() -> Self {
        Self::Ceiling
    }
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
    let uses_unixepoch =
        matches!(modifiers.first(), Some(modifier) if modifier.eq_ignore_ascii_case("unixepoch"));
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

fn date_time_args_have_subsecond(args: &[Value]) -> bool {
    let modifier_args = if args.is_empty() { &[][..] } else { &args[1..] };
    modifier_args.iter().any(|value| match value {
        Value::Null => false,
        value => {
            let modifier = coerce_text_like_value(value);
            modifier.eq_ignore_ascii_case("subsec") || modifier.eq_ignore_ascii_case("subsecond")
        }
    })
}

fn collect_date_time_modifiers(
    _function_name: &str,
    args: &[Value],
) -> Result<Option<Vec<String>>> {
    let mut modifiers = Vec::with_capacity(args.len());
    for value in args {
        match value {
            Value::Null => return Ok(None),
            value => modifiers.push(coerce_text_like_value(value)),
        }
    }
    Ok(Some(modifiers))
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
    let suffix = modifier.strip_prefix("weekday ")?;
    let weekday = suffix.trim().parse::<i64>().ok()?;
    if !(0..=6).contains(&weekday) {
        return None;
    }
    Some(weekday)
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

fn evaluate_min_max_scalar_function(
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
        .ok_or_else(|| DbError::storage(format!("{function_name} expects at least 1 argument")))?;
    for candidate in args.iter().skip(1) {
        let ordering = compare_min_max_scalar_values(candidate, &best)?.ok_or_else(|| {
            DbError::storage(format!(
                "{function_name} cannot compare {} and {}",
                candidate.type_name(),
                best.type_name()
            ))
        })?;
        let replace = if want_min {
            matches!(
                ordering,
                std::cmp::Ordering::Less | std::cmp::Ordering::Equal
            )
        } else {
            ordering == std::cmp::Ordering::Greater
        };
        if replace {
            best = candidate.clone();
        }
    }

    Ok(canonicalize_scalar_min_max_result(best))
}

fn canonicalize_scalar_min_max_result(value: Value) -> Value {
    match value {
        Value::Boolean(value) => Value::Integer(if value { 1 } else { 0 }),
        value => value,
    }
}

fn compare_min_max_scalar_values(
    left: &Value,
    right: &Value,
) -> Result<Option<std::cmp::Ordering>> {
    Ok(match (left, right) {
        (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
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
        _ => Some(
            sqlite_min_max_storage_class_rank(left).cmp(&sqlite_min_max_storage_class_rank(right)),
        ),
    })
}

fn sqlite_min_max_storage_class_rank(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Boolean(_) | Value::Integer(_) | Value::Real(_) => 1,
        Value::Text(_) => 2,
        Value::Blob(_) => 3,
    }
}

fn evaluate_binary_scalar(op: ScalarBinaryOp, left: Value, right: Value) -> Result<Value> {
    match op {
        ScalarBinaryOp::Add => {
            numeric_binary_op(left, right, i64::checked_add, |left, right| left + right)
        }
        ScalarBinaryOp::Subtract => {
            numeric_binary_op(left, right, i64::checked_sub, |left, right| left - right)
        }
        ScalarBinaryOp::Multiply => {
            numeric_binary_op(left, right, i64::checked_mul, |left, right| left * right)
        }
        ScalarBinaryOp::Divide => {
            let left = coerce_arithmetic_value(&left)?;
            let right = coerce_arithmetic_value(&right)?;
            match (left, right) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Integer(_), Value::Integer(0))
                | (Value::Real(_), Value::Real(0.0))
                | (Value::Integer(_), Value::Real(0.0))
                | (Value::Real(_), Value::Integer(0)) => Ok(Value::Null),
                (Value::Integer(i64::MIN), Value::Integer(-1)) => {
                    Ok(Value::Real(i64::MIN as f64 / -1.0))
                }
                (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(left / right)),
                (left, right) => Ok(Value::Real(
                    real_from_numeric_value(&left)? / real_from_numeric_value(&right)?,
                )),
            }
        }
        ScalarBinaryOp::Modulo => {
            let left = coerce_arithmetic_value(&left)?;
            let right = coerce_arithmetic_value(&right)?;
            match (left, right) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Integer(_), Value::Integer(0))
                | (Value::Integer(_), Value::Real(0.0))
                | (Value::Real(_), Value::Integer(0))
                | (Value::Real(_), Value::Real(0.0)) => Ok(Value::Null),
                (Value::Integer(i64::MIN), Value::Integer(-1)) => Ok(Value::Integer(0)),
                (Value::Integer(left), Value::Integer(right)) => Ok(Value::Integer(left % right)),
                (left, right) => {
                    let result_is_real =
                        matches!(left, Value::Real(_)) || matches!(right, Value::Real(_));
                    let left = real_from_numeric_value(&left)? as i64;
                    let right = real_from_numeric_value(&right)? as i64;
                    if right == 0 {
                        return Ok(Value::Null);
                    }
                    let result = if left == i64::MIN && right == -1 {
                        0
                    } else {
                        left % right
                    };
                    Ok(if result_is_real {
                        Value::Real(result as f64)
                    } else {
                        Value::Integer(result)
                    })
                }
            }
        }
        ScalarBinaryOp::BitAnd => bitwise_binary_op(left, right, |left, right| left & right),
        ScalarBinaryOp::BitOr => bitwise_binary_op(left, right, |left, right| left | right),
        ScalarBinaryOp::ShiftLeft => shift_op(left, right, true),
        ScalarBinaryOp::ShiftRight => shift_op(left, right, false),
        ScalarBinaryOp::Concat => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (left, right) => Ok(Value::Text(format!(
                "{}{}",
                coerce_text_like_value(&left),
                coerce_text_like_value(&right)
            ))),
        },
        ScalarBinaryOp::JsonExtract => evaluate_json_arrow_operator(&left, &right, false),
        ScalarBinaryOp::JsonExtractText => evaluate_json_arrow_operator(&left, &right, true),
    }
}

fn evaluate_json_arrow_operator(json: &Value, path: &Value, text_result: bool) -> Result<Value> {
    if matches!(json, Value::Null) || matches!(path, Value::Null) {
        return Ok(Value::Null);
    }
    let json = coerce_text_like_value(json);
    let path = sqlite_json_arrow_path(path);
    let parsed = parse_sqlite_json_value(&json)
        .map_err(|error| DbError::storage(format!("malformed JSON: {error}")))?;
    let Some(value) = json_path_lookup(&parsed, &path)? else {
        return Ok(Value::Null);
    };
    if text_result {
        json_value_to_sql(value)
    } else {
        serde_json::to_string(value)
            .map(Value::Text)
            .map_err(|error| DbError::storage(format!("failed to render JSON value: {error}")))
    }
}

fn sqlite_json_arrow_path(path: &Value) -> String {
    match path {
        Value::Integer(index) if *index >= 0 => format!("$[{index}]"),
        Value::Integer(index) => format!("$[#-{}]", index.unsigned_abs()),
        Value::Text(path) if path.starts_with('$') => path.clone(),
        value => format!("$.{}", coerce_text_like_value(value)),
    }
}

fn bitwise_binary_op(left: Value, right: Value, op: impl FnOnce(i64, i64) -> i64) -> Result<Value> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (left, right) => Ok(Value::Integer(op(
            sqlite_bitwise_integer_arg(&left)?,
            sqlite_bitwise_integer_arg(&right)?,
        ))),
    }
}

fn shift_op(left: Value, right: Value, left_shift: bool) -> Result<Value> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (left, right) => {
            let left = sqlite_bitwise_integer_arg(&left)?;
            let right = sqlite_bitwise_integer_arg(&right)?;
            let shift_left = if right < 0 { !left_shift } else { left_shift };
            let amount = right.unsigned_abs();

            if amount >= 64 {
                return Ok(Value::Integer(if shift_left || left >= 0 { 0 } else { -1 }));
            }

            let amount = u32::try_from(amount).expect("shift amount < 64 fits in u32");
            Ok(Value::Integer(if shift_left {
                left.wrapping_shl(amount)
            } else {
                left.wrapping_shr(amount)
            }))
        }
    }
}

fn sqlite_bitwise_integer_arg(value: &Value) -> Result<i64> {
    match cast_value(value.clone(), ColumnType::Integer)? {
        Value::Integer(value) => Ok(value),
        Value::Null => Ok(0),
        _ => unreachable!("integer cast must yield INTEGER or NULL"),
    }
}

fn numeric_binary_op(
    left: Value,
    right: Value,
    int_op: impl FnOnce(i64, i64) -> Option<i64>,
    real_op: impl FnOnce(f64, f64) -> f64,
) -> Result<Value> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Integer(left), Value::Integer(right)) => match int_op(left, right) {
            Some(result) => Ok(Value::Integer(result)),
            None => Ok(Value::Real(real_op(left as f64, right as f64))),
        },
        (left, right) => {
            let left = coerce_arithmetic_value(&left)?;
            let right = coerce_arithmetic_value(&right)?;
            match (left, right) {
                (Value::Integer(left), Value::Integer(right)) => match int_op(left, right) {
                    Some(result) => Ok(Value::Integer(result)),
                    None => Ok(Value::Real(real_op(left as f64, right as f64))),
                },
                (left, right) => Ok(Value::Real(real_op(
                    real_from_numeric_value(&left)?,
                    real_from_numeric_value(&right)?,
                ))),
            }
        }
    }
}

fn coerce_arithmetic_value(value: &Value) -> Result<Value> {
    Ok(match value {
        Value::Integer(value) => Value::Integer(*value),
        Value::Real(value) => Value::Real(*value),
        Value::Boolean(value) => Value::Integer(if *value { 1 } else { 0 }),
        Value::Text(value) => sqlite_text_arithmetic_prefix(value),
        Value::Null => Value::Null,
        Value::Blob(value) => sqlite_text_arithmetic_prefix(&String::from_utf8_lossy(value)),
    })
}

fn real_from_numeric_value(value: &Value) -> Result<f64> {
    match value {
        Value::Integer(value) => Ok(*value as f64),
        Value::Real(value) => Ok(*value),
        Value::Null => Ok(0.0),
        value => Err(DbError::storage(format!(
            "cannot coerce {} to numeric",
            value.type_name()
        ))),
    }
}

fn sqlite_text_arithmetic_prefix(value: &str) -> Value {
    let Some((candidate, has_real_syntax)) = sqlite_numeric_text_prefix(value) else {
        return Value::Integer(0);
    };
    if !has_real_syntax {
        return match candidate.parse::<i64>() {
            Ok(value) => Value::Integer(value),
            Err(_) => Value::Real(candidate.parse::<f64>().unwrap_or(0.0)),
        };
    }

    Value::Real(candidate.parse::<f64>().unwrap_or(0.0))
}

fn cast_value(value: Value, ty: ColumnType) -> Result<Value> {
    match ty {
        ColumnType::Boolean => match value {
            Value::Null => Ok(Value::Null),
            Value::Boolean(value) => Ok(Value::Boolean(value)),
            Value::Integer(value) => Ok(Value::Boolean(value != 0)),
            Value::Real(value) => Ok(Value::Boolean(value != 0.0)),
            Value::Text(value) => Ok(Value::Boolean(!value.is_empty() && value != "0")),
            Value::Blob(value) => Ok(Value::Boolean(!value.is_empty())),
        },
        ColumnType::Integer => match value {
            Value::Null => Ok(Value::Null),
            Value::Boolean(value) => Ok(Value::Integer(if value { 1 } else { 0 })),
            Value::Integer(value) => Ok(Value::Integer(value)),
            Value::Real(value) => Ok(Value::Integer(value as i64)),
            Value::Text(value) => Ok(Value::Integer(sqlite_text_integer_prefix(&value))),
            Value::Blob(value) => Ok(Value::Integer(sqlite_text_integer_prefix(
                &String::from_utf8_lossy(&value),
            ))),
        },
        ColumnType::Numeric => match value {
            Value::Null => Ok(Value::Null),
            Value::Boolean(value) => Ok(Value::Integer(if value { 1 } else { 0 })),
            Value::Integer(value) => Ok(Value::Integer(value)),
            Value::Real(value) => Ok(Value::Real(value)),
            Value::Text(value) => Ok(sqlite_text_numeric_prefix(&value)),
            Value::Blob(value) => Ok(sqlite_text_numeric_prefix(&String::from_utf8_lossy(&value))),
        },
        ColumnType::Real => match value {
            Value::Null => Ok(Value::Null),
            Value::Boolean(value) => Ok(Value::Real(if value { 1.0 } else { 0.0 })),
            Value::Integer(value) => Ok(Value::Real(value as f64)),
            Value::Real(value) => Ok(Value::Real(value)),
            Value::Text(value) => Ok(Value::Real(sqlite_text_real_prefix(&value))),
            Value::Blob(value) => Ok(Value::Real(sqlite_text_real_prefix(
                &String::from_utf8_lossy(&value),
            ))),
        },
        ColumnType::Text | ColumnType::Any => match value {
            Value::Null => Ok(Value::Null),
            value => Ok(Value::Text(coerce_text_like_value(&value))),
        },
        ColumnType::Blob => match value {
            Value::Null => Ok(Value::Null),
            Value::Blob(value) => Ok(Value::Blob(value)),
            value => Ok(Value::Blob(coerce_text_like_value(&value).into_bytes())),
        },
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
        Value::Real(value) => sqlite_real_to_text(*value),
        Value::Blob(value) => String::from_utf8_lossy(value).into_owned(),
        Value::Text(value) => value.clone(),
    }
}

fn sqlite_real_to_text(value: f64) -> String {
    if value == f64::INFINITY {
        return "Inf".to_string();
    }
    if value == f64::NEG_INFINITY {
        return "-Inf".to_string();
    }

    let rendered = value.to_string();
    if rendered.contains(['.', 'e', 'E']) {
        rendered
    } else {
        format!("{rendered}.0")
    }
}

fn sqlite_mod_function(left: Value, right: Value) -> Result<Value> {
    let left = match sqlite_math_arg(&left, "MOD")? {
        Some(value) => value,
        None => return Ok(Value::Null),
    };
    let right = match sqlite_math_arg(&right, "MOD")? {
        Some(value) => value,
        None => return Ok(Value::Null),
    };
    if right == 0.0 {
        return Ok(Value::Null);
    }
    Ok(Value::Real(left % right))
}

fn sqlite_unary_math_function(
    value: &Value,
    function_name: &str,
    op: impl FnOnce(f64) -> Option<f64>,
) -> Result<Value> {
    match sqlite_math_arg(value, function_name)? {
        Some(value) => Ok(op(value).map(Value::Real).unwrap_or(Value::Null)),
        None => Ok(Value::Null),
    }
}

fn sqlite_binary_math_function(
    left: &Value,
    right: &Value,
    function_name: &str,
    op: impl FnOnce(f64, f64) -> Option<f64>,
) -> Result<Value> {
    let left = match sqlite_math_arg(left, function_name)? {
        Some(value) => value,
        None => return Ok(Value::Null),
    };
    let right = match sqlite_math_arg(right, function_name)? {
        Some(value) => value,
        None => return Ok(Value::Null),
    };
    Ok(op(left, right).map(Value::Real).unwrap_or(Value::Null))
}

fn sqlite_math_arg(value: &Value, _function_name: &str) -> Result<Option<f64>> {
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

fn expect_arity(name: &str, args: &[Value], expected: usize) -> Result<()> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(DbError::storage(format!(
            "{name} expects {expected} argument{} but got {}",
            if expected == 1 { "" } else { "s" },
            args.len()
        )))
    }
}

fn compare(left: &Value, right: &Value) -> Result<Option<std::cmp::Ordering>> {
    Ok(match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
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
        (left, right) => Some(
            sqlite_min_max_storage_class_rank(left).cmp(&sqlite_min_max_storage_class_rank(right)),
        ),
    })
}

fn compare_with_operator(left: &Value, op: &CompareOp, right: &Value) -> Result<bool> {
    let Some(ordering) = compare(left, right)? else {
        return Ok(false);
    };
    Ok(match op {
        CompareOp::Eq => ordering == std::cmp::Ordering::Equal,
        CompareOp::Ne => ordering != std::cmp::Ordering::Equal,
        CompareOp::Gt => ordering == std::cmp::Ordering::Greater,
        CompareOp::Gte => matches!(
            ordering,
            std::cmp::Ordering::Greater | std::cmp::Ordering::Equal
        ),
        CompareOp::Lt => ordering == std::cmp::Ordering::Less,
        CompareOp::Lte => matches!(
            ordering,
            std::cmp::Ordering::Less | std::cmp::Ordering::Equal
        ),
    })
}

fn is_with_negation(left: &Value, right: &Value, negated: bool) -> bool {
    (left == right) ^ negated
}

fn in_result_value(left: &Value, values: &[Value], negated: bool) -> Result<Value> {
    if matches!(left, Value::Null) {
        return Ok(Value::Null);
    }
    let mut saw_null = false;
    for value in values {
        if matches!(value, Value::Null) {
            saw_null = true;
            continue;
        }
        if compare_with_operator(left, &CompareOp::Eq, value)? {
            return Ok(Value::Boolean(!negated));
        }
    }
    if saw_null {
        Ok(Value::Null)
    } else {
        Ok(Value::Boolean(negated))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LikeToken {
    Any,
    One,
    Literal(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LikeEscapeValue {
    Missing,
    Null,
    Text(String),
}

impl LikeEscapeValue {
    fn as_option_string(&self) -> Option<&str> {
        match self {
            Self::Missing | Self::Null => None,
            Self::Text(value) => Some(value.as_str()),
        }
    }
}

fn evaluate_like_escape(
    schema: &Schema,
    row: &Row,
    escape: &Option<Box<ScalarExpr>>,
    case_sensitive_like: bool,
) -> Result<LikeEscapeValue> {
    let Some(escape) = escape else {
        return Ok(LikeEscapeValue::Missing);
    };
    match evaluate_scalar_expr_with_like_mode(schema, row, escape, case_sensitive_like)? {
        Value::Null => Ok(LikeEscapeValue::Null),
        value => {
            let text = coerce_text_like_value(&value);
            let _ = like_escape_char(Some(text.as_str()))?;
            Ok(LikeEscapeValue::Text(text))
        }
    }
}

fn evaluate_like_pattern(
    schema: &Schema,
    row: &Row,
    pattern: &ScalarExpr,
    case_sensitive_like: bool,
) -> Result<LikeEscapeValue> {
    match evaluate_scalar_expr_with_like_mode(schema, row, pattern, case_sensitive_like)? {
        Value::Null => Ok(LikeEscapeValue::Null),
        value => Ok(LikeEscapeValue::Text(coerce_text_like_value(&value))),
    }
}

fn matches_like_pattern(
    value: &str,
    pattern: &str,
    escape: Option<&str>,
    case_sensitive: bool,
) -> Result<bool> {
    let escape = like_escape_char(escape)?;

    fn inner(value: &[char], pattern: &[LikeToken], case_sensitive: bool) -> bool {
        if pattern.is_empty() {
            return value.is_empty();
        }
        match pattern[0] {
            LikeToken::Any => {
                (0..=value.len()).any(|index| inner(&value[index..], &pattern[1..], case_sensitive))
            }
            LikeToken::One => {
                !value.is_empty() && inner(&value[1..], &pattern[1..], case_sensitive)
            }
            LikeToken::Literal(ch) => {
                !value.is_empty()
                    && sqlite_like_chars_equal(value[0], ch, case_sensitive)
                    && inner(&value[1..], &pattern[1..], case_sensitive)
            }
        }
    }

    let value_chars = sqlite_text_prefix_before_nul(value)
        .chars()
        .collect::<Vec<_>>();
    let pattern_tokens = like_tokens(pattern, escape);
    Ok(inner(&value_chars, &pattern_tokens, case_sensitive))
}

fn like_escape_char(escape: Option<&str>) -> Result<Option<char>> {
    Ok(match escape {
        Some(escape) => {
            let mut chars = escape.chars();
            let Some(ch) = chars.next() else {
                return Err(DbError::storage(
                    "ESCAPE expression must be a single character",
                ));
            };
            if chars.next().is_some() {
                return Err(DbError::storage(
                    "ESCAPE expression must be a single character",
                ));
            }
            Some(ch)
        }
        None => None,
    })
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

fn matches_glob_pattern(value: &str, pattern: &str) -> bool {
    fn matches_char_class(pattern: &[char], start: usize, ch: char) -> Option<(bool, usize)> {
        let mut index = start + 1;
        let negated = matches!(pattern.get(index), Some('^'));
        if negated {
            index += 1;
        }
        let mut matched = false;
        let mut saw_member = false;

        while index < pattern.len() {
            if pattern[index] == ']' && saw_member {
                return Some((matched ^ negated, index + 1));
            }

            if index + 2 < pattern.len() && pattern[index + 1] == '-' && pattern[index + 2] != ']' {
                let range_start = pattern[index];
                let range_end = pattern[index + 2];
                if range_start <= ch && ch <= range_end {
                    matched = true;
                }
                saw_member = true;
                index += 3;
            } else {
                if pattern[index] == ch {
                    matched = true;
                }
                saw_member = true;
                index += 1;
            }
        }

        None
    }

    fn inner(value: &[char], pattern: &[char]) -> bool {
        match pattern.first() {
            None => value.is_empty(),
            Some('*') => {
                inner(value, &pattern[1..]) || (!value.is_empty() && inner(&value[1..], pattern))
            }
            Some('?') => !value.is_empty() && inner(&value[1..], &pattern[1..]),
            Some('[') => {
                if value.is_empty() {
                    return false;
                }
                let Some((matched, next_index)) = matches_char_class(pattern, 0, value[0]) else {
                    return false;
                };
                matched && inner(&value[1..], &pattern[next_index..])
            }
            Some(ch) => !value.is_empty() && value[0] == *ch && inner(&value[1..], &pattern[1..]),
        }
    }

    let value_chars = sqlite_text_prefix_before_nul(value)
        .chars()
        .collect::<Vec<_>>();
    let pattern_chars = pattern.chars().collect::<Vec<_>>();
    inner(&value_chars, &pattern_chars)
}

fn sqlite_regexp_matches(pattern: &str, value: &str) -> Result<bool> {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = sqlite_text_prefix_before_nul(value)
        .chars()
        .collect::<Vec<_>>();
    if matches!(pattern.first(), Some('^')) {
        return regexp_match_here(&pattern, 1, &value, 0);
    }
    for index in 0..=value.len() {
        if regexp_match_here(&pattern, 0, &value, index)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn regexp_match_here(
    pattern: &[char],
    p_index: usize,
    value: &[char],
    v_index: usize,
) -> Result<bool> {
    if p_index == pattern.len() {
        return Ok(true);
    }
    if pattern[p_index] == '$' && p_index + 1 == pattern.len() {
        return Ok(v_index == value.len());
    }
    let (_, next_index) = regexp_atom_matches(pattern, p_index, '\0')?;
    if next_index < pattern.len() && pattern[next_index] == '*' {
        return regexp_match_star(pattern, p_index, next_index + 1, value, v_index);
    }
    if v_index >= value.len() {
        return Ok(false);
    }
    let (matches, next_index) = regexp_atom_matches(pattern, p_index, value[v_index])?;
    Ok(matches && regexp_match_here(pattern, next_index, value, v_index + 1)?)
}

fn regexp_match_star(
    pattern: &[char],
    atom_index: usize,
    rest_index: usize,
    value: &[char],
    mut v_index: usize,
) -> Result<bool> {
    if regexp_match_here(pattern, rest_index, value, v_index)? {
        return Ok(true);
    }
    while v_index < value.len() {
        let (matches, _) = regexp_atom_matches(pattern, atom_index, value[v_index])?;
        if !matches {
            break;
        }
        v_index += 1;
        if regexp_match_here(pattern, rest_index, value, v_index)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn regexp_atom_matches(pattern: &[char], index: usize, value: char) -> Result<(bool, usize)> {
    if pattern[index] == '.' {
        return Ok((true, index + 1));
    }
    if pattern[index] != '[' {
        return Ok((pattern[index] == value, index + 1));
    }

    let mut cursor = index + 1;
    let negated = matches!(pattern.get(cursor), Some('^'));
    if negated {
        cursor += 1;
    }
    let mut matched = false;
    let mut saw_member = false;
    while cursor < pattern.len() {
        if pattern[cursor] == ']' && saw_member {
            return Ok((matched ^ negated, cursor + 1));
        }
        if cursor + 2 < pattern.len() && pattern[cursor + 1] == '-' && pattern[cursor + 2] != ']' {
            let start = pattern[cursor];
            let end = pattern[cursor + 2];
            if start <= value && value <= end {
                matched = true;
            }
            cursor += 3;
            saw_member = true;
            continue;
        }
        if pattern[cursor] == value {
            matched = true;
        }
        cursor += 1;
        saw_member = true;
    }
    Err(DbError::storage("unclosed '['"))
}
