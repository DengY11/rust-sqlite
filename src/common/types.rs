//! Shared logical types used across the crate.

use std::cmp::Ordering;
use std::fmt::{self, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::common::error::{DbError, Result};

pub type Row = Vec<Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RowId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Blob(Vec<u8>),
    Text(String),
}

impl Display for Value {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Boolean(value) => write!(f, "{value}"),
            Self::Integer(value) => write!(f, "{value}"),
            Self::Real(value) => write!(f, "{value}"),
            Self::Blob(value) => {
                f.write_str("X'")?;
                for byte in value {
                    write!(f, "{byte:02X}")?;
                }
                f.write_str("'")
            }
            Self::Text(value) => f.write_str(value),
        }
    }
}

impl Value {
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Boolean(_) => "BOOLEAN",
            Self::Integer(_) => "INTEGER",
            Self::Real(_) => "REAL",
            Self::Blob(_) => "BLOB",
            Self::Text(_) => "TEXT",
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Boolean(left), Self::Boolean(right)) => left == right,
            (Self::Integer(left), Self::Integer(right)) => left == right,
            (Self::Real(left), Self::Real(right)) => left.to_bits() == right.to_bits(),
            (Self::Blob(left), Self::Blob(right)) => left == right,
            (Self::Text(left), Self::Text(right)) => left == right,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        fn rank(value: &Value) -> u8 {
            match value {
                Value::Null => 0,
                Value::Boolean(_) => 1,
                Value::Integer(_) => 2,
                Value::Real(_) => 3,
                Value::Blob(_) => 4,
                Value::Text(_) => 5,
            }
        }

        match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::Boolean(left), Self::Boolean(right)) => left.cmp(right),
            (Self::Integer(left), Self::Integer(right)) => left.cmp(right),
            (Self::Real(left), Self::Real(right)) => left.total_cmp(right),
            (Self::Blob(left), Self::Blob(right)) => left.cmp(right),
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Null => 0u8.hash(state),
            Self::Boolean(value) => {
                1u8.hash(state);
                value.hash(state);
            }
            Self::Integer(value) => {
                2u8.hash(state);
                value.hash(state);
            }
            Self::Real(value) => {
                3u8.hash(state);
                value.to_bits().hash(state);
            }
            Self::Blob(value) => {
                4u8.hash(state);
                value.hash(state);
            }
            Self::Text(value) => {
                5u8.hash(state);
                value.hash(state);
            }
        }
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Real(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnDefault {
    Literal(Value),
    CurrentTimestamp,
    CurrentDate,
    CurrentTime,
}

impl ColumnDefault {
    pub fn evaluate(&self) -> Result<Value> {
        match self {
            Self::Literal(value) => Ok(value.clone()),
            Self::CurrentTimestamp => Ok(Value::Text(current_timestamp_string()?)),
            Self::CurrentDate => Ok(Value::Text(current_date_string()?)),
            Self::CurrentTime => Ok(Value::Text(current_time_string()?)),
        }
    }
}

fn current_timestamp_string() -> Result<String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DbError::storage(format!("system clock is before unix epoch: {error}")))?;
    let seconds = i64::try_from(duration.as_secs())
        .map_err(|_| DbError::storage("system clock seconds do not fit in i64"))?;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
    ))
}

fn current_date_string() -> Result<String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DbError::storage(format!("system clock is before unix epoch: {error}")))?;
    let seconds = i64::try_from(duration.as_secs())
        .map_err(|_| DbError::storage("system clock seconds do not fit in i64"))?;
    let days = seconds.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

fn current_time_string() -> Result<String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DbError::storage(format!("system clock is before unix epoch: {error}")))?;
    let seconds = i64::try_from(duration.as_secs())
        .map_err(|_| DbError::storage("system clock seconds do not fit in i64"))?;
    let seconds_of_day = seconds.rem_euclid(86_400);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Ok(format!("{hour:02}:{minute:02}:{second:02}"))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnType {
    Any,
    Boolean,
    Integer,
    Numeric,
    Real,
    Blob,
    Text,
}

impl ColumnType {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Any => "ANY",
            Self::Boolean => "BOOLEAN",
            Self::Integer => "INTEGER",
            Self::Numeric => "NUMERIC",
            Self::Real => "REAL",
            Self::Blob => "BLOB",
            Self::Text => "TEXT",
        }
    }

    #[must_use]
    pub fn matches_value(&self, value: &Value) -> bool {
        matches!(
            (self, value),
            (Self::Any, _)
                | (Self::Boolean, Value::Boolean(_))
                | (Self::Integer, Value::Integer(_))
                | (Self::Numeric, Value::Integer(_) | Value::Real(_))
                | (Self::Real, Value::Real(_))
                | (Self::Blob, Value::Blob(_))
                | (Self::Text, Value::Text(_))
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CheckOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckExpr {
    Compare {
        column: String,
        op: CheckOp,
        value: Value,
    },
    IsNull {
        column: String,
        negated: bool,
    },
    Glob {
        column: String,
        pattern: String,
        negated: bool,
    },
    Like {
        column: String,
        pattern: String,
        escape: Option<String>,
        negated: bool,
    },
    InList {
        column: String,
        values: Vec<Value>,
        negated: bool,
    },
    Between {
        column: String,
        low: Value,
        high: Value,
        negated: bool,
    },
    IsBool {
        column: String,
        value: bool,
        negated: bool,
    },
    Truthy {
        column: String,
    },
    IsDistinct {
        column: String,
        value: Value,
        negated: bool,
    },
    And(Box<CheckExpr>, Box<CheckExpr>),
    Or(Box<CheckExpr>, Box<CheckExpr>),
    Not(Box<CheckExpr>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckConstraint {
    pub name: String,
    pub explicit_name: bool,
    pub expr: CheckExpr,
}

impl CheckConstraint {
    #[must_use]
    pub fn compare(
        name: impl Into<String>,
        column: impl Into<String>,
        op: CheckOp,
        value: Value,
    ) -> Self {
        Self {
            name: name.into(),
            explicit_name: false,
            expr: CheckExpr::Compare {
                column: column.into(),
                op,
                value,
            },
        }
    }

    #[must_use]
    pub fn named_compare(
        name: impl Into<String>,
        column: impl Into<String>,
        op: CheckOp,
        value: Value,
    ) -> Self {
        Self {
            name: name.into(),
            explicit_name: true,
            expr: CheckExpr::Compare {
                column: column.into(),
                op,
                value,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub constraint_name: Option<String>,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Option<Vec<String>>,
    pub match_clause: Option<String>,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
    pub deferrable: Option<bool>,
    pub initially_deferred: Option<bool>,
}

impl ForeignKey {
    #[must_use]
    pub fn single_column(
        column: impl Into<String>,
        ref_table: impl Into<String>,
        ref_column: impl Into<String>,
    ) -> Self {
        Self {
            constraint_name: None,
            columns: vec![column.into()],
            ref_table: ref_table.into(),
            ref_columns: Some(vec![ref_column.into()]),
            match_clause: None,
            on_delete: None,
            on_update: None,
            deferrable: None,
            initially_deferred: None,
        }
    }

    #[must_use]
    pub fn to_parent_primary_key(column: impl Into<String>, ref_table: impl Into<String>) -> Self {
        Self {
            constraint_name: None,
            columns: vec![column.into()],
            ref_table: ref_table.into(),
            ref_columns: None,
            match_clause: None,
            on_delete: None,
            on_update: None,
            deferrable: None,
            initially_deferred: None,
        }
    }

    #[must_use]
    pub fn multi_column(
        columns: Vec<String>,
        ref_table: impl Into<String>,
        ref_columns: Vec<String>,
    ) -> Self {
        Self {
            constraint_name: None,
            columns,
            ref_table: ref_table.into(),
            ref_columns: Some(ref_columns),
            match_clause: None,
            on_delete: None,
            on_update: None,
            deferrable: None,
            initially_deferred: None,
        }
    }

    #[must_use]
    pub fn multi_column_to_parent_primary_key(
        columns: Vec<String>,
        ref_table: impl Into<String>,
    ) -> Self {
        Self {
            constraint_name: None,
            columns,
            ref_table: ref_table.into(),
            ref_columns: None,
            match_clause: None,
            on_delete: None,
            on_update: None,
            deferrable: None,
            initially_deferred: None,
        }
    }

    #[must_use]
    pub fn named(mut self, constraint_name: impl Into<String>) -> Self {
        self.constraint_name = Some(constraint_name.into());
        self
    }

    #[must_use]
    pub fn with_match(mut self, match_clause: impl Into<String>) -> Self {
        self.match_clause = Some(match_clause.into());
        self
    }

    #[must_use]
    pub fn with_on_delete(mut self, action: impl Into<String>) -> Self {
        self.on_delete = Some(action.into());
        self
    }

    #[must_use]
    pub fn with_on_update(mut self, action: impl Into<String>) -> Self {
        self.on_update = Some(action.into());
        self
    }

    #[must_use]
    pub fn deferrable(mut self, deferrable: bool) -> Self {
        self.deferrable = Some(deferrable);
        self
    }

    #[must_use]
    pub fn initially_deferred(mut self, initially_deferred: bool) -> Self {
        self.initially_deferred = Some(initially_deferred);
        self
    }

    #[must_use]
    pub fn child_columns(&self) -> &[String] {
        &self.columns
    }

    #[must_use]
    pub fn referenced_columns(&self) -> Option<&[String]> {
        self.ref_columns.as_deref()
    }

    #[must_use]
    pub fn has_referenced_column(&self, column: &str) -> bool {
        self.ref_columns
            .as_ref()
            .is_some_and(|columns| columns.iter().any(|entry| entry == column))
    }

    #[must_use]
    pub fn rendered_child_columns(&self) -> String {
        self.columns.join(", ")
    }

    #[must_use]
    pub fn rendered_referenced_columns(&self) -> Option<String> {
        self.ref_columns.as_ref().map(|columns| columns.join(", "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniqueConstraint {
    pub constraint_name: Option<String>,
    pub conflict_clause: Option<String>,
    pub columns: Vec<String>,
    #[serde(default)]
    pub decorated_columns: Option<Vec<String>>,
}

impl UniqueConstraint {
    #[must_use]
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            constraint_name: None,
            conflict_clause: None,
            columns,
            decorated_columns: None,
        }
    }

    #[must_use]
    pub fn named(mut self, constraint_name: impl Into<String>) -> Self {
        self.constraint_name = Some(constraint_name.into());
        self
    }

    #[must_use]
    pub fn with_conflict_clause(mut self, conflict_clause: impl Into<String>) -> Self {
        self.conflict_clause = Some(conflict_clause.into());
        self
    }

    #[must_use]
    pub fn with_decorated_columns(mut self, decorated_columns: Vec<String>) -> Self {
        self.decorated_columns = Some(decorated_columns);
        self
    }

    #[must_use]
    pub fn rendered_columns(&self) -> &[String] {
        self.decorated_columns.as_deref().unwrap_or(&self.columns)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimaryKeyConstraint {
    pub constraint_name: Option<String>,
    pub columns: Vec<String>,
    #[serde(default)]
    pub decorated_columns: Option<Vec<String>>,
    #[serde(default)]
    pub conflict_clause: Option<String>,
}

impl PrimaryKeyConstraint {
    #[must_use]
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            constraint_name: None,
            columns,
            decorated_columns: None,
            conflict_clause: None,
        }
    }

    #[must_use]
    pub fn named(mut self, constraint_name: impl Into<String>) -> Self {
        self.constraint_name = Some(constraint_name.into());
        self
    }

    #[must_use]
    pub fn with_decorated_columns(mut self, decorated_columns: Vec<String>) -> Self {
        self.decorated_columns = Some(decorated_columns);
        self
    }

    #[must_use]
    pub fn with_conflict_clause(mut self, conflict_clause: impl Into<String>) -> Self {
        self.conflict_clause = Some(conflict_clause.into());
        self
    }

    #[must_use]
    pub fn rendered_columns(&self) -> &[String] {
        self.decorated_columns.as_deref().unwrap_or(&self.columns)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub column_type: ColumnType,
    pub collation: Option<String>,
    pub nullable: bool,
    pub not_null_constraint_name: Option<String>,
    pub not_null_conflict_clause: Option<String>,
    pub primary_key: bool,
    pub primary_key_constraint_name: Option<String>,
    pub primary_key_conflict_clause: Option<String>,
    pub primary_key_sort_order: Option<SortOrder>,
    pub unique: bool,
    pub unique_constraint_name: Option<String>,
    pub unique_conflict_clause: Option<String>,
    pub autoincrement: bool,
    pub default_value: Option<ColumnDefault>,
    pub generated_expr: Option<String>,
    pub generated_stored: bool,
    pub generated_storage_explicit: bool,
    pub generated_always_explicit: bool,
    pub checks: Vec<CheckConstraint>,
    pub foreign_key: Option<ForeignKey>,
}

impl ColumnDef {
    #[must_use]
    pub fn new(name: impl Into<String>, column_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            column_type,
            collation: None,
            nullable: true,
            not_null_constraint_name: None,
            not_null_conflict_clause: None,
            primary_key: false,
            primary_key_constraint_name: None,
            primary_key_conflict_clause: None,
            primary_key_sort_order: None,
            unique: false,
            unique_constraint_name: None,
            unique_conflict_clause: None,
            autoincrement: false,
            default_value: None,
            generated_expr: None,
            generated_stored: false,
            generated_storage_explicit: false,
            generated_always_explicit: false,
            checks: Vec::new(),
            foreign_key: None,
        }
    }

    #[must_use]
    pub fn primary_key(name: impl Into<String>, column_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            column_type,
            collation: None,
            nullable: false,
            not_null_constraint_name: None,
            not_null_conflict_clause: None,
            primary_key: true,
            primary_key_constraint_name: None,
            primary_key_conflict_clause: None,
            primary_key_sort_order: None,
            unique: false,
            unique_constraint_name: None,
            unique_conflict_clause: None,
            autoincrement: false,
            default_value: None,
            generated_expr: None,
            generated_stored: false,
            generated_storage_explicit: false,
            generated_always_explicit: false,
            checks: Vec::new(),
            foreign_key: None,
        }
    }

    #[must_use]
    pub fn nullable(mut self, nullable: bool) -> Self {
        self.nullable = nullable;
        self
    }

    #[must_use]
    pub fn with_not_null_name(mut self, constraint_name: impl Into<String>) -> Self {
        self.not_null_constraint_name = Some(constraint_name.into());
        self
    }

    #[must_use]
    pub fn with_not_null_conflict_clause(mut self, conflict_clause: impl Into<String>) -> Self {
        self.not_null_conflict_clause = Some(conflict_clause.into());
        self
    }

    #[must_use]
    pub fn primary_key_sort_order(mut self, sort_order: SortOrder) -> Self {
        self.primary_key_sort_order = Some(sort_order);
        self
    }

    #[must_use]
    pub fn with_primary_key_name(mut self, constraint_name: impl Into<String>) -> Self {
        self.primary_key_constraint_name = Some(constraint_name.into());
        self
    }

    #[must_use]
    pub fn with_primary_key_conflict_clause(mut self, conflict_clause: impl Into<String>) -> Self {
        self.primary_key_conflict_clause = Some(conflict_clause.into());
        self
    }

    #[must_use]
    pub fn collation(mut self, collation: impl Into<String>) -> Self {
        self.collation = Some(collation.into());
        self
    }

    #[must_use]
    pub fn default_value(mut self, value: ColumnDefault) -> Self {
        self.default_value = Some(value);
        self
    }

    #[must_use]
    pub fn autoincrement(mut self, autoincrement: bool) -> Self {
        self.autoincrement = autoincrement;
        self
    }

    #[must_use]
    pub fn unique(mut self, unique: bool) -> Self {
        self.unique = unique;
        self
    }

    #[must_use]
    pub fn with_unique_name(mut self, constraint_name: impl Into<String>) -> Self {
        self.unique_constraint_name = Some(constraint_name.into());
        self
    }

    #[must_use]
    pub fn with_unique_conflict_clause(mut self, conflict_clause: impl Into<String>) -> Self {
        self.unique_conflict_clause = Some(conflict_clause.into());
        self
    }

    #[must_use]
    pub fn generated_stored(mut self, expr: impl Into<String>) -> Self {
        self.generated_expr = Some(expr.into());
        self.generated_stored = true;
        self.generated_storage_explicit = true;
        self.generated_always_explicit = true;
        self
    }

    #[must_use]
    pub fn generated_virtual(mut self, expr: impl Into<String>) -> Self {
        self.generated_expr = Some(expr.into());
        self.generated_stored = false;
        self.generated_storage_explicit = true;
        self.generated_always_explicit = true;
        self
    }

    #[must_use]
    pub fn generated_virtual_implicit(mut self, expr: impl Into<String>) -> Self {
        self.generated_expr = Some(expr.into());
        self.generated_stored = false;
        self.generated_storage_explicit = false;
        self.generated_always_explicit = true;
        self
    }

    #[must_use]
    pub fn generated_as(mut self, expr: impl Into<String>) -> Self {
        self.generated_expr = Some(expr.into());
        self.generated_stored = false;
        self.generated_storage_explicit = false;
        self.generated_always_explicit = false;
        self
    }

    #[must_use]
    pub fn generated_as_virtual(mut self, expr: impl Into<String>) -> Self {
        self.generated_expr = Some(expr.into());
        self.generated_stored = false;
        self.generated_storage_explicit = true;
        self.generated_always_explicit = false;
        self
    }

    #[must_use]
    pub fn generated_as_stored(mut self, expr: impl Into<String>) -> Self {
        self.generated_expr = Some(expr.into());
        self.generated_stored = true;
        self.generated_storage_explicit = true;
        self.generated_always_explicit = false;
        self
    }

    #[must_use]
    pub fn check(mut self, check: CheckConstraint) -> Self {
        self.checks.push(check);
        self
    }

    #[must_use]
    pub fn references(
        mut self,
        ref_table: impl Into<String>,
        ref_column: impl Into<String>,
    ) -> Self {
        self.foreign_key = Some(ForeignKey::single_column(
            self.name.clone(),
            ref_table,
            ref_column,
        ));
        self
    }

    #[must_use]
    pub fn references_parent_primary_key(mut self, ref_table: impl Into<String>) -> Self {
        self.foreign_key = Some(ForeignKey::to_parent_primary_key(
            self.name.clone(),
            ref_table,
        ));
        self
    }

    #[must_use]
    pub fn with_foreign_key_name(mut self, constraint_name: impl Into<String>) -> Self {
        self.foreign_key = self
            .foreign_key
            .map(|foreign_key| foreign_key.named(constraint_name));
        self
    }

    #[must_use]
    pub fn with_foreign_key_match(mut self, match_clause: impl Into<String>) -> Self {
        self.foreign_key = self
            .foreign_key
            .map(|foreign_key| foreign_key.with_match(match_clause));
        self
    }

    #[must_use]
    pub fn with_foreign_key_action_on_delete(mut self, action: impl Into<String>) -> Self {
        self.foreign_key = self
            .foreign_key
            .map(|foreign_key| foreign_key.with_on_delete(action));
        self
    }

    #[must_use]
    pub fn with_foreign_key_action_on_update(mut self, action: impl Into<String>) -> Self {
        self.foreign_key = self
            .foreign_key
            .map(|foreign_key| foreign_key.with_on_update(action));
        self
    }

    #[must_use]
    pub fn with_foreign_key_deferrable(mut self, deferrable: bool) -> Self {
        self.foreign_key = self
            .foreign_key
            .map(|foreign_key| foreign_key.deferrable(deferrable));
        self
    }

    #[must_use]
    pub fn with_foreign_key_initially_deferred(mut self, initially_deferred: bool) -> Self {
        self.foreign_key = self
            .foreign_key
            .map(|foreign_key| foreign_key.initially_deferred(initially_deferred));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMeta {
    pub name: String,
    pub columns: Vec<String>,
    #[serde(default)]
    pub decorated_columns: Option<Vec<String>>,
    pub unique: bool,
    pub predicate: Option<String>,
}

impl IndexMeta {
    #[must_use]
    pub fn rendered_columns(&self) -> &[String] {
        self.decorated_columns.as_deref().unwrap_or(&self.columns)
    }

    #[must_use]
    pub fn enforces_unique_key(&self, key: &[Value]) -> bool {
        self.unique && !key.iter().any(|value| matches!(value, Value::Null))
    }

    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.predicate.is_none()
            && self
                .columns
                .iter()
                .all(|column| is_plain_index_column_name(column))
    }
}

fn is_plain_index_column_name(column: &str) -> bool {
    !column.is_empty()
        && column
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TableConstraintOrder {
    Check(usize),
    ForeignKey(usize),
    PrimaryKey,
    Unique(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub checks: Vec<CheckConstraint>,
    pub foreign_keys: Vec<ForeignKey>,
    pub primary_key_constraint: Option<PrimaryKeyConstraint>,
    pub unique_constraints: Vec<UniqueConstraint>,
    #[serde(default)]
    pub table_constraint_order: Vec<TableConstraintOrder>,
    pub strict: bool,
    pub without_rowid: bool,
}

impl Schema {
    #[must_use]
    pub fn new(name: impl Into<String>, columns: Vec<ColumnDef>) -> Self {
        Self {
            name: name.into(),
            columns,
            checks: Vec::new(),
            foreign_keys: Vec::new(),
            primary_key_constraint: None,
            unique_constraints: Vec::new(),
            table_constraint_order: Vec::new(),
            strict: false,
            without_rowid: false,
        }
    }

    #[must_use]
    pub fn with_check(mut self, check: CheckConstraint) -> Self {
        self.checks.push(check);
        self.table_constraint_order
            .push(TableConstraintOrder::Check(self.checks.len() - 1));
        self
    }

    #[must_use]
    pub fn with_foreign_key(mut self, foreign_key: ForeignKey) -> Self {
        self.foreign_keys.push(foreign_key);
        self.table_constraint_order
            .push(TableConstraintOrder::ForeignKey(
                self.foreign_keys.len() - 1,
            ));
        self
    }

    #[must_use]
    pub fn with_unique_constraint(mut self, unique_constraint: UniqueConstraint) -> Self {
        self.unique_constraints.push(unique_constraint);
        self.table_constraint_order
            .push(TableConstraintOrder::Unique(
                self.unique_constraints.len() - 1,
            ));
        self
    }

    pub fn with_primary_key_constraint(
        mut self,
        primary_key_constraint: PrimaryKeyConstraint,
    ) -> Result<Self> {
        self.mark_primary_key_columns(&primary_key_constraint)?;
        Ok(self)
    }

    pub fn mark_primary_key_columns(
        &mut self,
        primary_key_constraint: &PrimaryKeyConstraint,
    ) -> Result<()> {
        if primary_key_constraint.columns.is_empty() {
            return Err(DbError::storage(
                "PRIMARY KEY constraint must reference at least one column",
            ));
        }

        for column_name in &primary_key_constraint.columns {
            let Some(column) = self
                .columns
                .iter_mut()
                .find(|column| column.name == *column_name)
            else {
                return Err(DbError::storage(format!(
                    "unknown column in PRIMARY KEY constraint: {column_name}"
                )));
            };
            column.primary_key = true;
            if !matches!(
                (column.column_type, column.primary_key_sort_order),
                (ColumnType::Integer, Some(SortOrder::Desc))
            ) {
                column.nullable = false;
            }
        }

        self.primary_key_constraint = Some(primary_key_constraint.clone());
        if !self
            .table_constraint_order
            .iter()
            .any(|entry| matches!(entry, TableConstraintOrder::PrimaryKey))
        {
            self.table_constraint_order
                .push(TableConstraintOrder::PrimaryKey);
        }
        Ok(())
    }

    #[must_use]
    pub fn all_foreign_keys(&self) -> Vec<ForeignKey> {
        let mut foreign_keys = self.foreign_keys.clone();
        foreign_keys.extend(
            self.columns
                .iter()
                .filter_map(|column| column.foreign_key.clone()),
        );
        foreign_keys
    }

    pub fn validate_constraints_metadata(&self) -> Result<()> {
        for column in &self.columns {
            for check in &column.checks {
                self.validate_check_constraint_metadata(check)?;
            }
        }

        for check in &self.checks {
            self.validate_check_constraint_metadata(check)?;
        }

        for foreign_key in self.all_foreign_keys() {
            if foreign_key.columns.is_empty() {
                return Err(DbError::storage(
                    "FOREIGN KEY constraint must reference at least one column",
                ));
            }
            if let Some(ref_columns) = foreign_key.ref_columns.as_ref()
                && ref_columns.len() != foreign_key.columns.len()
            {
                return Err(DbError::storage(format!(
                    "FOREIGN KEY column count mismatch for parent table {}",
                    foreign_key.ref_table
                )));
            }
            for column in &foreign_key.columns {
                if !self.has_column(column) {
                    return Err(DbError::storage(format!(
                        "unknown column in FOREIGN KEY: {column}"
                    )));
                }
            }
        }

        for unique_constraint in &self.unique_constraints {
            if unique_constraint.columns.is_empty() {
                return Err(DbError::storage(
                    "UNIQUE constraint must reference at least one column",
                ));
            }
            for column in &unique_constraint.columns {
                if !self.has_column(column) {
                    return Err(DbError::storage(format!(
                        "unknown column in UNIQUE constraint: {column}"
                    )));
                }
            }
        }

        Ok(())
    }

    pub(crate) fn rename_column_references(&mut self, old_name: &str, new_name: &str) {
        for column in &mut self.columns {
            if column.name == old_name {
                column.name = new_name.to_string();
            }
            for check in &mut column.checks {
                rename_check_expr_column(&mut check.expr, old_name, new_name);
            }
            if let Some(foreign_key) = &mut column.foreign_key {
                for foreign_key_column in &mut foreign_key.columns {
                    if foreign_key_column == old_name {
                        *foreign_key_column = new_name.to_string();
                    }
                }
            }
        }

        for check in &mut self.checks {
            rename_check_expr_column(&mut check.expr, old_name, new_name);
        }
        for foreign_key in &mut self.foreign_keys {
            for foreign_key_column in &mut foreign_key.columns {
                if foreign_key_column == old_name {
                    *foreign_key_column = new_name.to_string();
                }
            }
            if foreign_key.ref_table == self.name
                && let Some(ref_columns) = &mut foreign_key.ref_columns
            {
                for ref_column in ref_columns {
                    if ref_column == old_name {
                        *ref_column = new_name.to_string();
                    }
                }
            }
        }
        for unique_constraint in &mut self.unique_constraints {
            for column in &mut unique_constraint.columns {
                if column == old_name {
                    *column = new_name.to_string();
                }
            }
        }
    }

    pub(crate) fn rename_foreign_key_ref_table(&mut self, old_name: &str, new_name: &str) {
        for column in &mut self.columns {
            if let Some(foreign_key) = &mut column.foreign_key
                && foreign_key.ref_table == old_name
            {
                foreign_key.ref_table = new_name.to_string();
            }
        }

        for foreign_key in &mut self.foreign_keys {
            if foreign_key.ref_table == old_name {
                foreign_key.ref_table = new_name.to_string();
            }
        }
    }

    pub(crate) fn rename_foreign_key_ref_column(
        &mut self,
        ref_table: &str,
        old_name: &str,
        new_name: &str,
    ) {
        for column in &mut self.columns {
            if let Some(foreign_key) = &mut column.foreign_key
                && foreign_key.ref_table == ref_table
            {
                if let Some(ref_columns) = &mut foreign_key.ref_columns {
                    for ref_column in ref_columns {
                        if ref_column == old_name {
                            *ref_column = new_name.to_string();
                        }
                    }
                }
            }
        }

        for foreign_key in &mut self.foreign_keys {
            if foreign_key.ref_table == ref_table {
                if let Some(ref_columns) = &mut foreign_key.ref_columns {
                    for ref_column in ref_columns {
                        if ref_column == old_name {
                            *ref_column = new_name.to_string();
                        }
                    }
                }
            }
        }
    }

    pub fn drop_column(&self, old_name: &str) -> Result<(Schema, usize)> {
        let removed_index = self
            .columns
            .iter()
            .position(|column| column.name == old_name)
            .ok_or_else(|| DbError::storage(format!("unknown column: {old_name}")))?;

        if self.columns.len() == 1 {
            return Err(DbError::storage(format!(
                "cannot drop the only column on table {}",
                self.name
            )));
        }
        if self.columns[removed_index].primary_key {
            return Err(DbError::storage(format!(
                "cannot drop primary key column '{old_name}'",
            )));
        }
        if self.columns.iter().any(|column| {
            column.name != old_name
                && column
                    .generated_expr
                    .as_deref()
                    .is_some_and(|expr| sql_expr_mentions_identifier(expr, old_name))
        }) {
            return Err(DbError::storage(format!(
                "cannot drop column '{old_name}' because generated columns depend on it",
            )));
        }
        if self
            .checks
            .iter()
            .any(|check| check_references_column(&check.expr, old_name))
            || self.columns.iter().any(|column| {
                column
                    .checks
                    .iter()
                    .any(|check| check_references_column(&check.expr, old_name))
            })
        {
            return Err(DbError::storage(format!(
                "cannot drop column '{old_name}' because a CHECK constraint depends on it",
            )));
        }
        if self
            .unique_constraints
            .iter()
            .any(|constraint| constraint.columns.iter().any(|column| column == old_name))
        {
            return Err(DbError::storage(format!(
                "cannot drop column '{old_name}' because a UNIQUE constraint depends on it",
            )));
        }
        if self
            .foreign_keys
            .iter()
            .any(|foreign_key| foreign_key.columns.iter().any(|column| column == old_name))
            || self.columns.iter().any(|column| {
                column.foreign_key.as_ref().is_some_and(|foreign_key| {
                    foreign_key.columns.iter().any(|entry| entry == old_name)
                })
            })
        {
            return Err(DbError::storage(format!(
                "cannot drop column '{old_name}' because a FOREIGN KEY depends on it",
            )));
        }
        if self.name != ""
            && self.foreign_keys.iter().any(|foreign_key| {
                foreign_key.ref_table == self.name && foreign_key.has_referenced_column(old_name)
            })
            || self.columns.iter().any(|column| {
                column.foreign_key.as_ref().is_some_and(|foreign_key| {
                    foreign_key.ref_table == self.name
                        && foreign_key.has_referenced_column(old_name)
                })
            })
        {
            return Err(DbError::storage(format!(
                "cannot drop column '{old_name}' because a FOREIGN KEY depends on it",
            )));
        }

        let mut updated_schema = self.clone();
        updated_schema.columns.remove(removed_index);
        updated_schema.validate_constraints_metadata()?;
        Ok((updated_schema, removed_index))
    }

    pub fn validate_row_values(&self, row: &Row) -> Result<()> {
        if row.len() != self.columns.len() {
            return Err(DbError::storage(format!(
                "insert into {} expected {} values but got {}",
                self.name,
                self.columns.len(),
                row.len()
            )));
        }

        for (column, value) in self.columns.iter().zip(row.iter()) {
            if matches!(value, Value::Null) {
                let null_allowed_primary_key = column.primary_key
                    && matches!(column.column_type, ColumnType::Integer)
                    && matches!(column.primary_key_sort_order, Some(SortOrder::Desc));
                if null_allowed_primary_key {
                    continue;
                }
                if column.primary_key {
                    return Err(DbError::storage(format!(
                        "primary key column '{}' cannot be NULL",
                        column.name
                    )));
                }

                if !column.nullable {
                    return Err(DbError::storage(format!(
                        "column '{}' cannot be NULL",
                        column.name
                    )));
                }

                continue;
            }

            if !column.column_type.matches_value(value) {
                return Err(DbError::storage(format!(
                    "column '{}' expected {} but got {}",
                    column.name,
                    column.column_type.name(),
                    value.type_name()
                )));
            }
        }

        Ok(())
    }

    pub fn validate_check_constraints(&self, row: &Row) -> Result<()> {
        self.validate_check_constraints_with_like_mode(row, false)
    }

    pub fn validate_check_constraints_with_like_mode(
        &self,
        row: &Row,
        case_sensitive_like: bool,
    ) -> Result<()> {
        for column in &self.columns {
            for check in &column.checks {
                self.validate_check_constraint(check, row, case_sensitive_like)?;
            }
        }
        for check in &self.checks {
            self.validate_check_constraint(check, row, case_sensitive_like)?;
        }
        Ok(())
    }

    fn validate_check_constraint(
        &self,
        check: &CheckConstraint,
        row: &Row,
        case_sensitive_like: bool,
    ) -> Result<()> {
        if self.evaluate_check_expr(&check.expr, row, case_sensitive_like)? == Some(false) {
            return Err(DbError::storage(format!(
                "check constraint '{}' failed",
                check.name
            )));
        }
        Ok(())
    }

    fn validate_check_constraint_metadata(&self, check: &CheckConstraint) -> Result<()> {
        self.validate_check_expr_metadata(&check.expr)
    }

    pub fn validate_check_expr_metadata(&self, expr: &CheckExpr) -> Result<()> {
        match expr {
            CheckExpr::Compare { column, .. }
            | CheckExpr::IsNull { column, .. }
            | CheckExpr::Glob { column, .. }
            | CheckExpr::Like { column, .. }
            | CheckExpr::InList { column, .. }
            | CheckExpr::Between { column, .. }
            | CheckExpr::IsBool { column, .. }
            | CheckExpr::Truthy { column }
            | CheckExpr::IsDistinct { column, .. } => {
                if self.has_column(column) {
                    Ok(())
                } else {
                    Err(DbError::storage(format!(
                        "unknown column in CHECK: {column}"
                    )))
                }
            }
            CheckExpr::And(left, right) | CheckExpr::Or(left, right) => {
                self.validate_check_expr_metadata(left)?;
                self.validate_check_expr_metadata(right)
            }
            CheckExpr::Not(expr) => self.validate_check_expr_metadata(expr),
        }
    }

    fn has_column(&self, column: &str) -> bool {
        self.columns.iter().any(|entry| entry.name == column)
    }

    fn evaluate_check_expr(
        &self,
        expr: &CheckExpr,
        row: &Row,
        case_sensitive_like: bool,
    ) -> Result<Option<bool>> {
        match expr {
            CheckExpr::Compare { column, op, value } => {
                let left = self.value_for_column(row, column)?;
                Ok(Self::evaluate_check_compare(left, *op, value))
            }
            CheckExpr::IsNull { column, negated } => {
                let value = self.value_for_column(row, column)?;
                Ok(Some(matches!(value, Value::Null) ^ *negated))
            }
            CheckExpr::Glob {
                column,
                pattern,
                negated,
            } => {
                let value = self.value_for_column(row, column)?;
                match value {
                    Value::Null => Ok(None),
                    Value::Text(value) => Ok(Some(matches_glob_pattern(value, pattern) ^ *negated)),
                    _ => Ok(Some(false ^ *negated)),
                }
            }
            CheckExpr::Like {
                column,
                pattern,
                escape,
                negated,
            } => {
                let value = self.value_for_column(row, column)?;
                match value {
                    Value::Null => Ok(None),
                    Value::Text(value) => Ok(Some(
                        matches_like_pattern(value, pattern, escape, case_sensitive_like)?
                            ^ *negated,
                    )),
                    _ => Ok(Some(false ^ *negated)),
                }
            }
            CheckExpr::InList {
                column,
                values,
                negated,
            } => {
                let value = self.value_for_column(row, column)?;
                if matches!(value, Value::Null) {
                    return Ok(None);
                }
                if values.iter().any(|candidate| candidate == value) {
                    return Ok(Some(!*negated));
                }
                if values
                    .iter()
                    .any(|candidate| matches!(candidate, Value::Null))
                {
                    return Ok(None);
                }
                Ok(Some(*negated))
            }
            CheckExpr::Between {
                column,
                low,
                high,
                negated,
            } => {
                let value = self.value_for_column(row, column)?;
                if matches!(value, Value::Null)
                    || matches!(low, Value::Null)
                    || matches!(high, Value::Null)
                {
                    return Ok(None);
                }
                let Some(low_cmp) = value.partial_cmp(low) else {
                    return Ok(None);
                };
                let Some(high_cmp) = value.partial_cmp(high) else {
                    return Ok(None);
                };
                let matches = matches!(low_cmp, Ordering::Greater | Ordering::Equal)
                    && matches!(high_cmp, Ordering::Less | Ordering::Equal);
                Ok(Some(matches ^ *negated))
            }
            CheckExpr::IsBool {
                column,
                value: expected,
                negated,
            } => {
                let value = self.value_for_column(row, column)?;
                let matches =
                    !matches!(value, Value::Null) && sqlite_check_truthy(value) == *expected;
                Ok(Some(matches ^ *negated))
            }
            CheckExpr::Truthy { column } => {
                let value = self.value_for_column(row, column)?;
                if matches!(value, Value::Null) {
                    Ok(None)
                } else {
                    Ok(Some(sqlite_check_truthy(value)))
                }
            }
            CheckExpr::IsDistinct {
                column,
                value,
                negated,
            } => {
                let left = self.value_for_column(row, column)?;
                let matches = Self::check_values_are_not_distinct(left, value);
                Ok(Some(matches ^ *negated))
            }
            CheckExpr::And(left, right) => {
                let left = self.evaluate_check_expr(left, row, case_sensitive_like)?;
                let right = self.evaluate_check_expr(right, row, case_sensitive_like)?;
                Ok(match (left, right) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                })
            }
            CheckExpr::Or(left, right) => {
                let left = self.evaluate_check_expr(left, row, case_sensitive_like)?;
                let right = self.evaluate_check_expr(right, row, case_sensitive_like)?;
                Ok(match (left, right) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                })
            }
            CheckExpr::Not(expr) => Ok(self
                .evaluate_check_expr(expr, row, case_sensitive_like)?
                .map(|value| !value)),
        }
    }

    pub fn matches_check_expr(&self, expr: &CheckExpr, row: &Row) -> Result<bool> {
        self.matches_check_expr_with_like_mode(expr, row, false)
    }

    pub fn matches_check_expr_with_like_mode(
        &self,
        expr: &CheckExpr,
        row: &Row,
        case_sensitive_like: bool,
    ) -> Result<bool> {
        Ok(self.evaluate_check_expr(expr, row, case_sensitive_like)? == Some(true))
    }

    pub(crate) fn column_index(&self, column: &str) -> Result<usize> {
        self.columns
            .iter()
            .position(|entry| entry.name == column)
            .ok_or_else(|| DbError::storage(format!("unknown column: {column}")))
    }

    pub(crate) fn value_for_column<'a>(&self, row: &'a Row, column: &str) -> Result<&'a Value> {
        let index = self.column_index(column)?;
        row.get(index)
            .ok_or_else(|| DbError::storage(format!("row is missing column {column}")))
    }

    fn evaluate_check_compare(left: &Value, op: CheckOp, right: &Value) -> Option<bool> {
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return None;
        }

        let ordering = match (left, right) {
            (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
            (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
            (Value::Blob(left), Value::Blob(right)) => left.cmp(right),
            (Value::Text(left), Value::Text(right)) => left.cmp(right),
            _ => return Some(false),
        };

        Some(match op {
            CheckOp::Eq => ordering.is_eq(),
            CheckOp::Ne => !ordering.is_eq(),
            CheckOp::Gt => ordering.is_gt(),
            CheckOp::Gte => ordering.is_gt() || ordering.is_eq(),
            CheckOp::Lt => ordering.is_lt(),
            CheckOp::Lte => ordering.is_lt() || ordering.is_eq(),
        })
    }

    fn check_values_are_not_distinct(left: &Value, right: &Value) -> bool {
        matches!((left, right), (Value::Null, Value::Null))
            || (!matches!(left, Value::Null)
                && !matches!(right, Value::Null)
                && left.partial_cmp(right) == Some(Ordering::Equal))
    }

    pub fn validate_primary_key_uniqueness(&self, row: &Row, existing_rows: &[&Row]) -> Result<()> {
        let primary_key_columns = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.primary_key)
            .collect::<Vec<_>>();
        if primary_key_columns.is_empty() {
            return Ok(());
        }

        let key = primary_key_columns
            .iter()
            .map(|(index, _)| row[*index].clone())
            .collect::<Vec<_>>();
        if existing_rows.iter().any(|existing_row| {
            primary_key_columns
                .iter()
                .all(|(index, _)| existing_row.get(*index) == Some(&row[*index]))
        }) {
            if primary_key_columns.len() == 1 {
                let (index, column) = primary_key_columns[0];
                return Err(DbError::storage(format!(
                    "duplicate primary key value for column '{}': {}",
                    column.name, row[index]
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

        Ok(())
    }
}

fn rename_check_expr_column(expr: &mut CheckExpr, old_name: &str, new_name: &str) {
    match expr {
        CheckExpr::Compare { column, .. }
        | CheckExpr::IsNull { column, .. }
        | CheckExpr::Glob { column, .. }
        | CheckExpr::Like { column, .. }
        | CheckExpr::InList { column, .. }
        | CheckExpr::Between { column, .. }
        | CheckExpr::IsBool { column, .. }
        | CheckExpr::Truthy { column }
        | CheckExpr::IsDistinct { column, .. } => {
            if column == old_name {
                *column = new_name.to_string();
            }
        }
        CheckExpr::And(left, right) | CheckExpr::Or(left, right) => {
            rename_check_expr_column(left, old_name, new_name);
            rename_check_expr_column(right, old_name, new_name);
        }
        CheckExpr::Not(expr) => rename_check_expr_column(expr, old_name, new_name),
    }
}

fn check_references_column(expr: &CheckExpr, column: &str) -> bool {
    match expr {
        CheckExpr::Compare {
            column: expr_column,
            ..
        }
        | CheckExpr::IsNull {
            column: expr_column,
            ..
        }
        | CheckExpr::Glob {
            column: expr_column,
            ..
        }
        | CheckExpr::Like {
            column: expr_column,
            ..
        }
        | CheckExpr::InList {
            column: expr_column,
            ..
        }
        | CheckExpr::Between {
            column: expr_column,
            ..
        }
        | CheckExpr::IsBool {
            column: expr_column,
            ..
        }
        | CheckExpr::Truthy {
            column: expr_column,
        }
        | CheckExpr::IsDistinct {
            column: expr_column,
            ..
        } => expr_column == column,
        CheckExpr::And(left, right) | CheckExpr::Or(left, right) => {
            check_references_column(left, column) || check_references_column(right, column)
        }
        CheckExpr::Not(expr) => check_references_column(expr, column),
    }
}

fn sqlite_check_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Boolean(value) => *value,
        Value::Integer(value) => *value != 0,
        Value::Real(value) => *value != 0.0,
        Value::Text(value) => sqlite_text_numeric_prefix(value) != 0.0,
        Value::Blob(value) => sqlite_text_numeric_prefix(&String::from_utf8_lossy(value)) != 0.0,
    }
}

fn sqlite_text_numeric_prefix(value: &str) -> f64 {
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
        return 0.0;
    }

    if let Some((exp_index, exp_ch)) = chars.peek().copied()
        && matches!(exp_ch, 'e' | 'E')
    {
        let mut lookahead = chars.clone();
        lookahead.next();
        if let Some((_, sign)) = lookahead.peek().copied()
            && matches!(sign, '+' | '-')
        {
            lookahead.next();
        }
        let mut exp_end = exp_index + exp_ch.len_utf8();
        let mut saw_exp_digit = false;
        while let Some((index, digit)) = lookahead.peek().copied() {
            if digit.is_ascii_digit() {
                saw_exp = true;
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

    let prefix = &trimmed[..end];
    if saw_dot || saw_exp {
        prefix.parse::<f64>().unwrap_or(0.0)
    } else {
        prefix
            .parse::<i64>()
            .map(|value| value as f64)
            .unwrap_or(0.0)
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

            if index + 2 < pattern.len() && pattern[index + 1] == '-' && pattern[index + 2] != ']' {
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

    let value = value.chars().collect::<Vec<_>>();
    let pattern = pattern.chars().collect::<Vec<_>>();
    inner(&value, &pattern)
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
    };

    fn char_eq(left: char, right: char, case_sensitive: bool) -> bool {
        if !case_sensitive && left.is_ascii() && right.is_ascii() {
            left.eq_ignore_ascii_case(&right)
        } else {
            left == right
        }
    }

    fn inner(value: &[char], pattern: &[char], escape: Option<char>, case_sensitive: bool) -> bool {
        if pattern.is_empty() {
            return value.is_empty();
        }

        if escape.is_some_and(|escape| pattern[0] == escape) {
            return pattern.get(1).is_some_and(|literal| {
                !value.is_empty() && char_eq(value[0], *literal, case_sensitive)
            }) && inner(&value[1..], &pattern[2..], escape, case_sensitive);
        }

        match pattern[0] {
            '%' => (0..=value.len())
                .any(|index| inner(&value[index..], &pattern[1..], escape, case_sensitive)),
            '_' => !value.is_empty() && inner(&value[1..], &pattern[1..], escape, case_sensitive),
            ch => {
                !value.is_empty()
                    && char_eq(value[0], ch, case_sensitive)
                    && inner(&value[1..], &pattern[1..], escape, case_sensitive)
            }
        }
    }

    let value = value.chars().collect::<Vec<_>>();
    let pattern = pattern.chars().collect::<Vec<_>>();
    Ok(inner(&value, &pattern, escape, case_sensitive))
}

fn sql_expr_mentions_identifier(expr: &str, identifier: &str) -> bool {
    let identifier = identifier.to_ascii_lowercase();
    let bytes = expr.as_bytes();
    let mut start = 0_usize;

    while start < bytes.len() {
        while start < bytes.len() && !(bytes[start].is_ascii_alphabetic() || bytes[start] == b'_') {
            start += 1;
        }
        if start >= bytes.len() {
            break;
        }

        let mut end = start + 1;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }

        if expr[start..end].eq_ignore_ascii_case(&identifier) {
            return true;
        }
        start = end;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::{
        CheckConstraint, CheckOp, ColumnDef, ColumnDefault, ColumnType, ForeignKey, Schema,
        UniqueConstraint, Value,
    };

    fn user_schema() -> Schema {
        Schema::new(
            "users",
            vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
                ColumnDef::new("active", ColumnType::Boolean),
            ],
        )
    }

    #[test]
    fn column_defs_preserve_default_and_inline_constraints() {
        let column = ColumnDef::new("age", ColumnType::Integer)
            .default_value(ColumnDefault::Literal(Value::Integer(0)))
            .check(CheckConstraint::compare(
                "age_non_negative",
                "age",
                CheckOp::Gte,
                Value::Integer(0),
            ))
            .references("users", "id");

        assert_eq!(
            column.default_value,
            Some(ColumnDefault::Literal(Value::Integer(0)))
        );
        assert_eq!(column.checks.len(), 1);
        assert_eq!(column.foreign_key.as_ref().unwrap().ref_table, "users");
    }

    #[test]
    fn schema_preserves_table_constraints() {
        let schema = Schema::new(
            "orders",
            vec![ColumnDef::primary_key("id", ColumnType::Integer)],
        )
        .with_check(CheckConstraint::compare(
            "id_positive",
            "id",
            CheckOp::Gt,
            Value::Integer(0),
        ))
        .with_unique_constraint(UniqueConstraint::new(vec!["id".to_string()]))
        .with_foreign_key(ForeignKey::single_column("user_id", "users", "id"));

        assert_eq!(schema.checks.len(), 1);
        assert_eq!(schema.foreign_keys.len(), 1);
        assert_eq!(
            schema.unique_constraints,
            vec![UniqueConstraint::new(vec!["id".to_string()])]
        );
    }

    #[test]
    fn validate_row_values_accepts_matching_types_and_nullable_columns() {
        let schema = user_schema();
        let row = vec![Value::Integer(1), Value::from("alice"), Value::Null];
        assert!(schema.validate_row_values(&row).is_ok());
    }

    #[test]
    fn validate_row_values_rejects_too_few_or_too_many_values() {
        let schema = user_schema();

        let too_few = schema
            .validate_row_values(&vec![Value::Integer(1), Value::from("alice")])
            .unwrap_err();
        assert!(too_few.to_string().contains("expected 3 values but got 2"));

        let too_many = schema
            .validate_row_values(&vec![
                Value::Integer(1),
                Value::from("alice"),
                Value::Boolean(true),
                Value::Null,
            ])
            .unwrap_err();
        assert!(too_many.to_string().contains("expected 3 values but got 4"));
    }

    #[test]
    fn validate_row_values_rejects_null_primary_key_and_type_mismatch() {
        let schema = user_schema();

        let null_primary_key = schema
            .validate_row_values(&vec![
                Value::Null,
                Value::from("alice"),
                Value::Boolean(true),
            ])
            .unwrap_err();
        assert!(
            null_primary_key
                .to_string()
                .contains("primary key column 'id' cannot be NULL")
        );

        let type_mismatch = schema
            .validate_row_values(&vec![
                Value::Integer(1),
                Value::Boolean(true),
                Value::Boolean(true),
            ])
            .unwrap_err();
        assert!(
            type_mismatch
                .to_string()
                .contains("column 'name' expected TEXT but got BOOLEAN")
        );
    }

    #[test]
    fn validate_primary_key_uniqueness_accepts_new_key_and_rejects_duplicate() {
        let schema = user_schema();
        let existing = [
            vec![
                Value::Integer(1),
                Value::from("alice"),
                Value::Boolean(true),
            ],
            vec![Value::Integer(2), Value::from("bob"), Value::Boolean(false)],
        ];
        let refs = existing.iter().collect::<Vec<_>>();

        assert!(
            schema
                .validate_primary_key_uniqueness(
                    &vec![
                        Value::Integer(3),
                        Value::from("carol"),
                        Value::Boolean(true)
                    ],
                    &refs,
                )
                .is_ok()
        );

        let duplicate = schema
            .validate_primary_key_uniqueness(
                &vec![
                    Value::Integer(2),
                    Value::from("carol"),
                    Value::Boolean(true),
                ],
                &refs,
            )
            .unwrap_err();
        assert!(
            duplicate
                .to_string()
                .contains("duplicate primary key value for column 'id': 2")
        );
    }
}
