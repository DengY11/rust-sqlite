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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrimSide {
    Both,
    Start,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoundingFunc {
    Ceil,
    Ceiling,
    Floor,
    Trunc,
}

impl RoundingFunc {
    #[must_use]
    pub(crate) fn sql_name(self) -> &'static str {
        match self {
            Self::Ceil => "ceil",
            Self::Ceiling => "ceiling",
            Self::Floor => "floor",
            Self::Trunc => "trunc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnaryMathFunc {
    Sqrt,
    Ln,
    Log10,
    Log2,
    Exp,
    Sin,
    Cos,
    Tan,
    Sinh,
    Cosh,
    Tanh,
    Atan,
    Acos,
    Asin,
    Acosh,
    Asinh,
    Atanh,
    Degrees,
    Radians,
}

impl UnaryMathFunc {
    #[must_use]
    pub(crate) fn sql_name(self) -> &'static str {
        match self {
            Self::Sqrt => "sqrt",
            Self::Ln => "ln",
            Self::Log10 => "log10",
            Self::Log2 => "log2",
            Self::Exp => "exp",
            Self::Sin => "sin",
            Self::Cos => "cos",
            Self::Tan => "tan",
            Self::Sinh => "sinh",
            Self::Cosh => "cosh",
            Self::Tanh => "tanh",
            Self::Atan => "atan",
            Self::Acos => "acos",
            Self::Asin => "asin",
            Self::Acosh => "acosh",
            Self::Asinh => "asinh",
            Self::Atanh => "atanh",
            Self::Degrees => "degrees",
            Self::Radians => "radians",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryMathFunc {
    Power,
    Atan2,
    Log,
}

impl BinaryMathFunc {
    #[must_use]
    pub(crate) fn sql_name(self) -> &'static str {
        match self {
            Self::Power => "power",
            Self::Atan2 => "atan2",
            Self::Log => "log",
        }
    }
}

fn default_check_collation() -> String {
    "NOCASE".to_string()
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
    Regexp {
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
    LengthCompare {
        column: String,
        op: CheckOp,
        value: Value,
    },
    OctetLengthCompare {
        column: String,
        op: CheckOp,
        value: Value,
    },
    UnicodeCompare {
        column: String,
        op: CheckOp,
        value: Value,
    },
    UnicodeIsNull {
        column: String,
        negated: bool,
    },
    SignCompare {
        column: String,
        op: CheckOp,
        value: Value,
    },
    HexCompare {
        column: String,
        op: CheckOp,
        value: Value,
    },
    QuoteCompare {
        column: String,
        op: CheckOp,
        value: Value,
    },
    NullIfIsNull {
        column: String,
        value: Value,
        negated: bool,
    },
    ReplaceCompare {
        column: String,
        pattern: String,
        replacement: String,
        op: CheckOp,
        value: Value,
    },
    ReplaceColumnCompare {
        column: String,
        pattern: String,
        replacement: String,
        op: CheckOp,
    },
    RoundCompare {
        column: String,
        precision: Option<i32>,
        op: CheckOp,
        value: Value,
    },
    RoundingCompare {
        column: String,
        func: RoundingFunc,
        op: CheckOp,
        value: Value,
    },
    CastCompare {
        column: String,
        target_type: ColumnType,
        op: CheckOp,
        value: Value,
    },
    MinMaxColumnCompare {
        column: String,
        limit: Value,
        min: bool,
        op: CheckOp,
    },
    ConcatCompare {
        column: String,
        suffix: Vec<Value>,
        op: CheckOp,
        value: Value,
    },
    ConcatWsCompare {
        column: String,
        separator: Option<String>,
        suffix: Vec<Value>,
        op: CheckOp,
        value: Value,
    },
    JsonValidCompare {
        column: String,
        flags: Option<i64>,
        #[serde(default)]
        compare: Option<(CheckOp, Value)>,
    },
    AbsCompare {
        column: String,
        op: CheckOp,
        value: Value,
    },
    UnaryMathCompare {
        column: String,
        func: UnaryMathFunc,
        op: CheckOp,
        value: Value,
    },
    BinaryMathCompare {
        column: String,
        func: BinaryMathFunc,
        argument: Value,
        #[serde(default)]
        column_is_second: bool,
        op: CheckOp,
        value: Value,
    },
    ArithmeticCompare {
        column: String,
        addend: Value,
        op: CheckOp,
        value: Value,
    },
    MultiplyCompare {
        column: String,
        factor: Value,
        op: CheckOp,
        value: Value,
    },
    DivideCompare {
        column: String,
        divisor: Value,
        op: CheckOp,
        value: Value,
    },
    ModuloCompare {
        column: String,
        divisor: Value,
        op: CheckOp,
        value: Value,
        #[serde(default)]
        function_form: bool,
    },
    TypeOfCompare {
        column: String,
        op: CheckOp,
        value: Value,
    },
    NoCaseCompare {
        column: String,
        #[serde(default = "default_check_collation")]
        collation: String,
        op: CheckOp,
        value: Value,
    },
    CaseFoldCompare {
        column: String,
        upper: bool,
        op: CheckOp,
        value: Value,
    },
    TrimCompare {
        column: String,
        side: TrimSide,
        characters: Option<String>,
        op: CheckOp,
        value: Value,
    },
    CoalesceCompare {
        column: String,
        fallbacks: Vec<Value>,
        op: CheckOp,
        value: Value,
    },
    InstrCompare {
        column: String,
        needle: Value,
        op: CheckOp,
        value: Value,
    },
    SubstrCompare {
        column: String,
        start: i64,
        length: Option<i64>,
        op: CheckOp,
        value: Value,
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
    pub declared_type: Option<String>,
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

fn default_declared_type(column_type: ColumnType) -> Option<String> {
    (!matches!(column_type, ColumnType::Any)).then(|| column_type.name().to_string())
}

impl ColumnDef {
    #[must_use]
    pub fn new(name: impl Into<String>, column_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            column_type,
            declared_type: default_declared_type(column_type),
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
            declared_type: default_declared_type(column_type),
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
    pub fn declared_type(mut self, declared_type: impl Into<String>) -> Self {
        self.declared_type = Some(declared_type.into());
        self
    }

    #[must_use]
    pub fn pragma_declared_type(&self) -> &str {
        self.declared_type.as_deref().unwrap_or("")
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaObjectType {
    Table,
    View,
}

fn default_schema_object_type() -> SchemaObjectType {
    SchemaObjectType::Table
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    #[serde(default = "default_schema_object_type")]
    pub object_type: SchemaObjectType,
    #[serde(default)]
    pub create_sql: Option<String>,
    #[serde(skip)]
    pub view_select: Option<crate::sql::ast::SelectStatement>,
    #[serde(default)]
    pub view_columns: Option<Vec<String>>,
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
            object_type: SchemaObjectType::Table,
            create_sql: None,
            view_select: None,
            view_columns: None,
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
    pub fn view(
        name: impl Into<String>,
        columns: Vec<ColumnDef>,
        view_columns: Option<Vec<String>>,
        select: crate::sql::ast::SelectStatement,
        create_sql: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            columns,
            object_type: SchemaObjectType::View,
            create_sql: Some(create_sql.into()),
            view_select: Some(select),
            view_columns,
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
    pub fn is_view(&self) -> bool {
        self.object_type == SchemaObjectType::View
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
                let null_allowed_primary_key = !self.strict
                    && column.primary_key
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

    pub fn normalize_strict_row_values(&self, row: Row) -> Result<Row> {
        if !self.strict {
            return Ok(row);
        }

        self.columns
            .iter()
            .zip(row)
            .map(|(column, value)| self.normalize_strict_value(column, value))
            .collect()
    }

    fn normalize_strict_value(&self, column: &ColumnDef, value: Value) -> Result<Value> {
        if matches!(value, Value::Null) || matches!(column.column_type, ColumnType::Any) {
            return Ok(value);
        }

        let original_type = value.type_name();
        let normalized = match column.column_type {
            ColumnType::Any => Some(value),
            ColumnType::Integer => strict_integer_value(value),
            ColumnType::Real => strict_real_value(value),
            ColumnType::Text => strict_text_value(value),
            ColumnType::Blob => match value {
                Value::Blob(_) => Some(value),
                _ => None,
            },
            ColumnType::Numeric | ColumnType::Boolean => {
                if column.column_type.matches_value(&value) {
                    Some(value)
                } else {
                    None
                }
            }
        };

        normalized.ok_or_else(|| {
            DbError::storage(format!(
                "cannot store {} value in {} column {}.{}",
                original_type.to_ascii_uppercase(),
                column.column_type.name(),
                self.name,
                column.name
            ))
        })
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
            | CheckExpr::Regexp { column, .. }
            | CheckExpr::Like { column, .. }
            | CheckExpr::InList { column, .. }
            | CheckExpr::Between { column, .. }
            | CheckExpr::IsBool { column, .. }
            | CheckExpr::Truthy { column }
            | CheckExpr::IsDistinct { column, .. }
            | CheckExpr::LengthCompare { column, .. }
            | CheckExpr::OctetLengthCompare { column, .. }
            | CheckExpr::UnicodeCompare { column, .. }
            | CheckExpr::UnicodeIsNull { column, .. }
            | CheckExpr::SignCompare { column, .. }
            | CheckExpr::HexCompare { column, .. }
            | CheckExpr::QuoteCompare { column, .. }
            | CheckExpr::NullIfIsNull { column, .. }
            | CheckExpr::ReplaceCompare { column, .. }
            | CheckExpr::ReplaceColumnCompare { column, .. }
            | CheckExpr::RoundCompare { column, .. }
            | CheckExpr::RoundingCompare { column, .. }
            | CheckExpr::CastCompare { column, .. }
            | CheckExpr::MinMaxColumnCompare { column, .. }
            | CheckExpr::ConcatCompare { column, .. }
            | CheckExpr::ConcatWsCompare { column, .. }
            | CheckExpr::JsonValidCompare { column, .. }
            | CheckExpr::AbsCompare { column, .. }
            | CheckExpr::UnaryMathCompare { column, .. }
            | CheckExpr::BinaryMathCompare { column, .. }
            | CheckExpr::ArithmeticCompare { column, .. }
            | CheckExpr::MultiplyCompare { column, .. }
            | CheckExpr::DivideCompare { column, .. }
            | CheckExpr::ModuloCompare { column, .. }
            | CheckExpr::TypeOfCompare { column, .. }
            | CheckExpr::NoCaseCompare { column, .. }
            | CheckExpr::CaseFoldCompare { column, .. }
            | CheckExpr::TrimCompare { column, .. }
            | CheckExpr::CoalesceCompare { column, .. }
            | CheckExpr::InstrCompare { column, .. }
            | CheckExpr::SubstrCompare { column, .. } => {
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
                    value => Ok(Some(
                        matches_glob_pattern(&sqlite_text_like_value(value), pattern) ^ *negated,
                    )),
                }
            }
            CheckExpr::Regexp {
                column,
                pattern,
                negated,
            } => {
                let value = self.value_for_column(row, column)?;
                match value {
                    Value::Null => Ok(None),
                    value => Ok(Some(
                        sqlite_check_regexp(pattern, &sqlite_text_like_value(value))? ^ *negated,
                    )),
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
                    value => Ok(Some(
                        matches_like_pattern(
                            &sqlite_text_like_value(value),
                            pattern,
                            escape,
                            case_sensitive_like,
                        )? ^ *negated,
                    )),
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
            CheckExpr::LengthCompare { column, op, value } => {
                let left = self.value_for_column(row, column)?;
                let Some(length) = sqlite_check_length(left) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Integer(length),
                    *op,
                    value,
                ))
            }
            CheckExpr::OctetLengthCompare { column, op, value } => {
                let left = self.value_for_column(row, column)?;
                let Some(length) = sqlite_check_octet_length(left) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Integer(length),
                    *op,
                    value,
                ))
            }
            CheckExpr::UnicodeCompare { column, op, value } => {
                let left = self.value_for_column(row, column)?;
                let Some(codepoint) = sqlite_check_unicode(left) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Integer(codepoint),
                    *op,
                    value,
                ))
            }
            CheckExpr::UnicodeIsNull { column, negated } => {
                let left = self.value_for_column(row, column)?;
                Ok(Some(sqlite_check_unicode(left).is_none() ^ *negated))
            }
            CheckExpr::SignCompare { column, op, value } => {
                let left = self.value_for_column(row, column)?;
                let Some(sign) = sqlite_check_sign(left) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Integer(sign),
                    *op,
                    value,
                ))
            }
            CheckExpr::HexCompare { column, op, value } => {
                let left = self.value_for_column(row, column)?;
                let hex = sqlite_check_hex(left);
                Ok(Self::evaluate_check_compare(&Value::Text(hex), *op, value))
            }
            CheckExpr::QuoteCompare { column, op, value } => {
                let left = self.value_for_column(row, column)?;
                let quoted = sqlite_check_quote(left);
                Ok(Self::evaluate_check_compare(
                    &Value::Text(quoted),
                    *op,
                    value,
                ))
            }
            CheckExpr::NullIfIsNull {
                column,
                value,
                negated,
            } => {
                let left = self.value_for_column(row, column)?;
                let is_null = sqlite_check_nullif(left, value);
                Ok(Some(is_null ^ *negated))
            }
            CheckExpr::ReplaceCompare {
                column,
                pattern,
                replacement,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(replaced) = sqlite_check_replace(left, pattern, replacement) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Text(replaced),
                    *op,
                    value,
                ))
            }
            CheckExpr::ReplaceColumnCompare {
                column,
                pattern,
                replacement,
                op,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(replaced) = sqlite_check_replace(left, pattern, replacement) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Text(replaced),
                    *op,
                    left,
                ))
            }
            CheckExpr::RoundCompare {
                column,
                precision,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(rounded) = sqlite_check_round(left, *precision) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Real(rounded),
                    *op,
                    value,
                ))
            }
            CheckExpr::RoundingCompare {
                column,
                func,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(rounded) = sqlite_check_rounding(left, *func) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(&rounded, *op, value))
            }
            CheckExpr::CastCompare {
                column,
                target_type,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let casted = sqlite_check_cast(left, *target_type);
                Ok(Self::evaluate_check_compare(&casted, *op, value))
            }
            CheckExpr::MinMaxColumnCompare {
                column,
                limit,
                min,
                op,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(candidate) = sqlite_check_min_max(left, limit, *min) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(candidate, *op, left))
            }
            CheckExpr::ConcatCompare {
                column,
                suffix,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let concatenated = sqlite_check_concat(left, suffix);
                Ok(Self::evaluate_check_compare(
                    &Value::Text(concatenated),
                    *op,
                    value,
                ))
            }
            CheckExpr::ConcatWsCompare {
                column,
                separator,
                suffix,
                op,
                value,
            } => {
                let Some(separator) = separator else {
                    return Ok(None);
                };
                let left = self.value_for_column(row, column)?;
                let concatenated = sqlite_check_concat_ws(left, separator, suffix);
                Ok(Self::evaluate_check_compare(
                    &Value::Text(concatenated),
                    *op,
                    value,
                ))
            }
            CheckExpr::JsonValidCompare {
                column,
                flags,
                compare,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(valid) = sqlite_check_json_valid(left, *flags) else {
                    return Ok(None);
                };
                if let Some((op, value)) = compare {
                    return Ok(Self::evaluate_check_compare(
                        &Value::Integer(i64::from(valid)),
                        *op,
                        value,
                    ));
                }
                Ok(Some(valid))
            }
            CheckExpr::AbsCompare { column, op, value } => {
                let left = self.value_for_column(row, column)?;
                let Some(abs_value) = sqlite_check_abs(left) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(&abs_value, *op, value))
            }
            CheckExpr::UnaryMathCompare {
                column,
                func,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(result) = sqlite_check_unary_math(left, *func) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Real(result),
                    *op,
                    value,
                ))
            }
            CheckExpr::BinaryMathCompare {
                column,
                func,
                argument,
                column_is_second,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(result) =
                    sqlite_check_binary_math(left, argument, *func, *column_is_second)
                else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Real(result),
                    *op,
                    value,
                ))
            }
            CheckExpr::ArithmeticCompare {
                column,
                addend,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(sum) = sqlite_check_add(left, addend) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(&sum, *op, value))
            }
            CheckExpr::MultiplyCompare {
                column,
                factor,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(product) = sqlite_check_multiply(left, factor) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(&product, *op, value))
            }
            CheckExpr::DivideCompare {
                column,
                divisor,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(quotient) = sqlite_check_divide(left, divisor) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(&quotient, *op, value))
            }
            CheckExpr::ModuloCompare {
                column,
                divisor,
                op,
                value,
                ..
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(remainder) = sqlite_check_modulo(left, divisor) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(&remainder, *op, value))
            }
            CheckExpr::TypeOfCompare { column, op, value } => {
                let left = self.value_for_column(row, column)?;
                Ok(Self::evaluate_check_compare(
                    &Value::Text(sqlite_typeof_name(left).to_string()),
                    *op,
                    value,
                ))
            }
            CheckExpr::NoCaseCompare {
                column,
                collation,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let left = sqlite_check_collated_value(left, collation);
                let right = sqlite_check_collated_value(value, collation);
                Ok(Self::evaluate_check_compare(&left, *op, &right))
            }
            CheckExpr::CaseFoldCompare {
                column,
                upper,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                if matches!(left, Value::Null) {
                    return Ok(None);
                }
                let text = sqlite_text_like_value(left);
                let folded = if *upper {
                    sqlite_ascii_upper(&text)
                } else {
                    sqlite_ascii_lower(&text)
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Text(folded),
                    *op,
                    value,
                ))
            }
            CheckExpr::TrimCompare {
                column,
                side,
                characters,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                if matches!(left, Value::Null) {
                    return Ok(None);
                }
                let text = sqlite_trim(&sqlite_text_like_value(left), *side, characters.as_deref());
                Ok(Self::evaluate_check_compare(&Value::Text(text), *op, value))
            }
            CheckExpr::CoalesceCompare {
                column,
                fallbacks,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let fallback = fallbacks
                    .iter()
                    .find(|value| !matches!(value, Value::Null))
                    .unwrap_or(&Value::Null);
                let candidate = if matches!(left, Value::Null) {
                    fallback
                } else {
                    left
                };
                Ok(Self::evaluate_check_compare(candidate, *op, value))
            }
            CheckExpr::InstrCompare {
                column,
                needle,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                let Some(position) = sqlite_check_instr(left, needle) else {
                    return Ok(None);
                };
                Ok(Self::evaluate_check_compare(
                    &Value::Integer(position),
                    *op,
                    value,
                ))
            }
            CheckExpr::SubstrCompare {
                column,
                start,
                length,
                op,
                value,
            } => {
                let left = self.value_for_column(row, column)?;
                if matches!(left, Value::Null) {
                    return Ok(None);
                }
                let text = sqlite_substr_text(&sqlite_text_like_value(left), *start, *length);
                Ok(Self::evaluate_check_compare(&Value::Text(text), *op, value))
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
            (Value::Integer(left), Value::Real(right)) => (*left as f64).total_cmp(right),
            (Value::Real(left), Value::Integer(right)) => left.total_cmp(&(*right as f64)),
            (Value::Real(left), Value::Real(right)) => left.total_cmp(right),
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
        | CheckExpr::Regexp { column, .. }
        | CheckExpr::Like { column, .. }
        | CheckExpr::InList { column, .. }
        | CheckExpr::Between { column, .. }
        | CheckExpr::IsBool { column, .. }
        | CheckExpr::Truthy { column }
        | CheckExpr::IsDistinct { column, .. }
        | CheckExpr::LengthCompare { column, .. }
        | CheckExpr::OctetLengthCompare { column, .. }
        | CheckExpr::UnicodeCompare { column, .. }
        | CheckExpr::UnicodeIsNull { column, .. }
        | CheckExpr::SignCompare { column, .. }
        | CheckExpr::HexCompare { column, .. }
        | CheckExpr::QuoteCompare { column, .. }
        | CheckExpr::NullIfIsNull { column, .. }
        | CheckExpr::ReplaceCompare { column, .. }
        | CheckExpr::ReplaceColumnCompare { column, .. }
        | CheckExpr::RoundCompare { column, .. }
        | CheckExpr::RoundingCompare { column, .. }
        | CheckExpr::CastCompare { column, .. }
        | CheckExpr::MinMaxColumnCompare { column, .. }
        | CheckExpr::ConcatCompare { column, .. }
        | CheckExpr::ConcatWsCompare { column, .. }
        | CheckExpr::JsonValidCompare { column, .. }
        | CheckExpr::AbsCompare { column, .. }
        | CheckExpr::UnaryMathCompare { column, .. }
        | CheckExpr::BinaryMathCompare { column, .. }
        | CheckExpr::ArithmeticCompare { column, .. }
        | CheckExpr::MultiplyCompare { column, .. }
        | CheckExpr::DivideCompare { column, .. }
        | CheckExpr::ModuloCompare { column, .. }
        | CheckExpr::TypeOfCompare { column, .. }
        | CheckExpr::NoCaseCompare { column, .. }
        | CheckExpr::CaseFoldCompare { column, .. }
        | CheckExpr::TrimCompare { column, .. }
        | CheckExpr::CoalesceCompare { column, .. }
        | CheckExpr::InstrCompare { column, .. }
        | CheckExpr::SubstrCompare { column, .. } => {
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
        | CheckExpr::Regexp {
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
        }
        | CheckExpr::LengthCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::OctetLengthCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::UnicodeCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::UnicodeIsNull {
            column: expr_column,
            ..
        }
        | CheckExpr::SignCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::HexCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::QuoteCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::NullIfIsNull {
            column: expr_column,
            ..
        }
        | CheckExpr::ReplaceCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::ReplaceColumnCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::RoundCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::RoundingCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::CastCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::MinMaxColumnCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::ConcatCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::ConcatWsCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::JsonValidCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::AbsCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::UnaryMathCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::BinaryMathCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::ArithmeticCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::MultiplyCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::DivideCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::ModuloCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::TypeOfCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::NoCaseCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::CaseFoldCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::TrimCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::CoalesceCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::InstrCompare {
            column: expr_column,
            ..
        }
        | CheckExpr::SubstrCompare {
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

fn sqlite_check_length(value: &Value) -> Option<i64> {
    match value {
        Value::Null => None,
        Value::Blob(value) => Some(value.len() as i64),
        Value::Text(value) => Some(sqlite_text_prefix_before_nul(value).chars().count() as i64),
        value => Some(sqlite_text_like_value(value).chars().count() as i64),
    }
}

fn sqlite_check_octet_length(value: &Value) -> Option<i64> {
    match value {
        Value::Null => None,
        Value::Blob(value) => Some(value.len() as i64),
        Value::Text(value) => Some(value.len() as i64),
        Value::Integer(value) => Some(value.to_string().len() as i64),
        Value::Real(value) => Some(sqlite_real_to_text(*value).len() as i64),
        Value::Boolean(_) => Some(1),
    }
}

fn sqlite_check_unicode(value: &Value) -> Option<i64> {
    if matches!(value, Value::Null) {
        return None;
    }
    sqlite_text_prefix_before_nul(&sqlite_text_like_value(value))
        .chars()
        .next()
        .map(|ch| i64::from(u32::from(ch)))
}

fn sqlite_check_sign(value: &Value) -> Option<i64> {
    match value {
        Value::Null => None,
        Value::Boolean(value) => Some(if *value { 1 } else { 0 }),
        Value::Integer(value) => Some(value.signum()),
        Value::Real(value) => Some(sqlite_sign_real(*value)),
        Value::Text(value) => {
            let value = value.trim();
            if let Ok(value) = value.parse::<i64>() {
                Some(value.signum())
            } else {
                value.parse::<f64>().ok().map(sqlite_sign_real)
            }
        }
        Value::Blob(_) => None,
    }
}

fn sqlite_sign_real(value: f64) -> i64 {
    if value > 0.0 {
        1
    } else if value < 0.0 {
        -1
    } else {
        0
    }
}

fn sqlite_check_hex(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Blob(value) => sqlite_hex_bytes(value),
        Value::Text(value) => sqlite_hex_bytes(value.as_bytes()),
        Value::Integer(value) => sqlite_hex_bytes(value.to_string().as_bytes()),
        Value::Real(value) => sqlite_hex_bytes(sqlite_real_to_text(*value).as_bytes()),
        Value::Boolean(value) => {
            if *value {
                "31".to_string()
            } else {
                "30".to_string()
            }
        }
    }
}

fn sqlite_check_quote(value: &Value) -> String {
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
        Value::Blob(value) => format!("X'{}'", sqlite_hex_bytes(value)),
        Value::Text(value) => {
            format!(
                "'{}'",
                sqlite_text_prefix_before_nul(value).replace('\'', "''")
            )
        }
    }
}

fn sqlite_check_nullif(left: &Value, right: &Value) -> bool {
    matches!(left, Value::Null) || Schema::check_values_are_not_distinct(left, right)
}

fn sqlite_check_replace(value: &Value, pattern: &str, replacement: &str) -> Option<String> {
    if matches!(value, Value::Null) {
        return None;
    }
    let text = sqlite_text_like_value(value);
    let value = sqlite_text_prefix_before_nul(&text);
    if pattern.is_empty() {
        Some(value.to_string())
    } else {
        Some(value.replace(pattern, replacement))
    }
}

fn sqlite_check_round(value: &Value, precision: Option<i32>) -> Option<f64> {
    if matches!(value, Value::Null) {
        return None;
    }
    let value = sqlite_check_numeric_real(value);
    Some(sqlite_round_f64(value, precision.unwrap_or(0)))
}

#[must_use]
pub fn sqlite_round_f64(value: f64, precision: i32) -> f64 {
    let precision = precision.clamp(0, 30);
    if !value.is_finite() {
        return value;
    }
    if precision > 18 {
        let factor = 10_f64.powi(precision);
        return (value * factor).round() / factor;
    }

    let sign = if value.is_sign_negative() { -1.0 } else { 1.0 };
    let value = value.abs();
    if value == 0.0 {
        return sign * 0.0;
    }

    let bits = value.to_bits();
    let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    let (mantissa, binary_shift) = if exponent_bits == 0 {
        (u128::from(fraction), -1074)
    } else {
        (
            u128::from((1_u64 << 52) | fraction),
            exponent_bits - 1023 - 52,
        )
    };
    let pow10 = 10_u128.pow(precision as u32);
    let Some(numerator) = mantissa.checked_mul(pow10) else {
        let factor = 10_f64.powi(precision);
        return sign * ((value * factor).round() / factor);
    };

    let rounded = if binary_shift >= 0 {
        let shift = binary_shift as u32;
        if shift >= 128 {
            let factor = 10_f64.powi(precision);
            return sign * ((value * factor).round() / factor);
        }
        let Some(scaled) = numerator.checked_shl(shift) else {
            let factor = 10_f64.powi(precision);
            return sign * ((value * factor).round() / factor);
        };
        scaled
    } else {
        let denominator_shift = (-binary_shift) as u32;
        if denominator_shift >= 127 {
            return sign * 0.0;
        }
        let denominator = 1_u128 << denominator_shift;
        let Some(doubled_numerator) = numerator.checked_mul(2) else {
            let factor = 10_f64.powi(precision);
            return sign * ((value * factor).round() / factor);
        };
        let Some(rounding_numerator) = doubled_numerator.checked_add(denominator) else {
            let factor = 10_f64.powi(precision);
            return sign * ((value * factor).round() / factor);
        };
        rounding_numerator / (denominator << 1)
    };

    sign * (rounded as f64) / (pow10 as f64)
}

fn sqlite_check_rounding(value: &Value, func: RoundingFunc) -> Option<Value> {
    match value {
        Value::Null | Value::Blob(_) => None,
        Value::Boolean(value) => Some(Value::Integer(if *value { 1 } else { 0 })),
        Value::Integer(value) => Some(Value::Integer(*value)),
        Value::Real(value) => Some(Value::Real(match func {
            RoundingFunc::Ceil | RoundingFunc::Ceiling => value.ceil(),
            RoundingFunc::Floor => value.floor(),
            RoundingFunc::Trunc => value.trunc(),
        })),
        Value::Text(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            let parsed = trimmed.parse::<f64>().ok()?;
            Some(Value::Real(match func {
                RoundingFunc::Ceil | RoundingFunc::Ceiling => parsed.ceil(),
                RoundingFunc::Floor => parsed.floor(),
                RoundingFunc::Trunc => parsed.trunc(),
            }))
        }
    }
}

fn sqlite_check_cast(value: &Value, target_type: ColumnType) -> Value {
    match target_type {
        ColumnType::Any => value.clone(),
        ColumnType::Text => match value {
            Value::Null => Value::Null,
            value => Value::Text(sqlite_text_like_value(value)),
        },
        ColumnType::Blob => match value {
            Value::Null => Value::Null,
            Value::Blob(value) => Value::Blob(value.clone()),
            value => Value::Blob(sqlite_text_like_value(value).into_bytes()),
        },
        ColumnType::Integer => match value {
            Value::Null => Value::Null,
            Value::Integer(value) => Value::Integer(*value),
            Value::Boolean(value) => Value::Integer(if *value { 1 } else { 0 }),
            Value::Real(value) => Value::Integer(*value as i64),
            Value::Text(value) => Value::Integer(sqlite_text_integer_prefix(value)),
            Value::Blob(value) => {
                Value::Integer(sqlite_text_integer_prefix(&String::from_utf8_lossy(value)))
            }
        },
        ColumnType::Numeric => match value {
            Value::Null => Value::Null,
            Value::Integer(value) => Value::Integer(*value),
            Value::Boolean(value) => Value::Integer(if *value { 1 } else { 0 }),
            Value::Real(value) => Value::Real(*value),
            Value::Text(value) => sqlite_text_numeric_value(value),
            Value::Blob(value) => sqlite_text_numeric_value(&String::from_utf8_lossy(value)),
        },
        ColumnType::Real => match value {
            Value::Null => Value::Null,
            Value::Integer(value) => Value::Real(*value as f64),
            Value::Boolean(value) => Value::Real(if *value { 1.0 } else { 0.0 }),
            Value::Real(value) => Value::Real(*value),
            Value::Text(value) => Value::Real(sqlite_text_real_prefix(value)),
            Value::Blob(value) => {
                Value::Real(sqlite_text_real_prefix(&String::from_utf8_lossy(value)))
            }
        },
        ColumnType::Boolean => match value {
            Value::Null => Value::Null,
            Value::Boolean(value) => Value::Boolean(*value),
            Value::Integer(value) => Value::Boolean(*value != 0),
            Value::Real(value) => Value::Boolean(*value != 0.0),
            Value::Text(value) => Value::Boolean(!value.is_empty() && value != "0"),
            Value::Blob(value) => Value::Boolean(!value.is_empty() && value != b"0"),
        },
    }
}

fn sqlite_check_min_max<'a>(left: &'a Value, right: &'a Value, min: bool) -> Option<&'a Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return None;
    }
    let ordering = left.partial_cmp(right)?;
    if (min && ordering.is_gt()) || (!min && ordering.is_lt()) {
        Some(right)
    } else {
        Some(left)
    }
}

fn sqlite_check_concat(value: &Value, suffix: &[Value]) -> String {
    let mut result = String::new();
    if !matches!(value, Value::Null) {
        result.push_str(&sqlite_text_like_value(value));
    }
    for value in suffix {
        if !matches!(value, Value::Null) {
            result.push_str(&sqlite_text_like_value(value));
        }
    }
    result
}

fn sqlite_check_concat_ws(value: &Value, separator: &str, suffix: &[Value]) -> String {
    let mut parts = Vec::new();
    if !matches!(value, Value::Null) {
        parts.push(sqlite_text_like_value(value));
    }
    for value in suffix {
        if !matches!(value, Value::Null) {
            parts.push(sqlite_text_like_value(value));
        }
    }
    parts.join(separator)
}

fn sqlite_check_json_valid(value: &Value, flags: Option<i64>) -> Option<bool> {
    if matches!(value, Value::Null) {
        return None;
    }
    let json = sqlite_text_like_value(value);
    Some(if flags.is_some_and(|flags| flags & 0x02 != 0) {
        serde_json::from_str::<serde_json::Value>(&json).is_ok()
            || sqlite_json5_to_json(&json)
                .as_deref()
                .is_some_and(|json| serde_json::from_str::<serde_json::Value>(json).is_ok())
    } else {
        serde_json::from_str::<serde_json::Value>(&json).is_ok()
    })
}

fn sqlite_json5_to_json(json: &str) -> Option<String> {
    let mut out = String::with_capacity(json.len());
    let mut chars = json.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch == '/' {
            if let Some((_, '/')) = chars.peek().copied() {
                chars.next();
                for (_, ch) in chars.by_ref() {
                    if ch == '\n' {
                        out.push('\n');
                        break;
                    }
                }
                continue;
            }
            if let Some((_, '*')) = chars.peek().copied() {
                chars.next();
                let mut prev = '\0';
                for (_, ch) in chars.by_ref() {
                    if prev == '*' && ch == '/' {
                        break;
                    }
                    prev = ch;
                }
                continue;
            }
        }

        if ch == '\'' {
            out.push('"');
            let mut escaped = false;
            for (_, value_ch) in chars.by_ref() {
                if escaped {
                    out.push(value_ch);
                    escaped = false;
                    continue;
                }
                if value_ch == '\\' {
                    out.push(value_ch);
                    escaped = true;
                    continue;
                }
                if value_ch == '\'' {
                    out.push('"');
                    break;
                }
                if value_ch == '"' {
                    out.push('\\');
                }
                out.push(value_ch);
            }
            continue;
        }

        if is_json5_identifier_start(ch) {
            let start = index;
            let mut end = index + ch.len_utf8();
            while let Some((next_index, next_ch)) = chars.peek().copied() {
                if !is_json5_identifier_continue(next_ch) {
                    break;
                }
                chars.next();
                end = next_index + next_ch.len_utf8();
            }
            let ident = &json[start..end];
            if json[end..].trim_start().starts_with(':') {
                out.push('"');
                out.push_str(ident);
                out.push('"');
            } else {
                out.push_str(ident);
            }
            continue;
        }

        out.push(ch);
    }

    let normalized = strip_json5_trailing_commas(&out);
    (normalized != json).then_some(normalized)
}

fn is_json5_identifier_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

fn is_json5_identifier_continue(ch: char) -> bool {
    is_json5_identifier_start(ch) || ch.is_ascii_digit()
}

fn strip_json5_trailing_commas(json: &str) -> String {
    let chars = json.chars().collect::<Vec<_>>();
    let mut out = String::with_capacity(json.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            index += 1;
            continue;
        }
        if ch == ',' {
            let mut next = index + 1;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            if next < chars.len() && matches!(chars[next], '}' | ']') {
                index += 1;
                continue;
            }
        }
        out.push(ch);
        index += 1;
    }
    out
}

fn sqlite_hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

fn sqlite_check_abs(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::Integer(value) => value
            .checked_abs()
            .map(Value::Integer)
            .or_else(|| Some(Value::Real((*value as f64).abs()))),
        Value::Real(value) => Some(Value::Real(value.abs())),
        Value::Boolean(value) => Some(Value::Integer(if *value { 1 } else { 0 })),
        Value::Text(value) => Some(Value::Real(sqlite_text_numeric_prefix(value).abs())),
        Value::Blob(value) => Some(Value::Real(
            sqlite_text_numeric_prefix(&String::from_utf8_lossy(value)).abs(),
        )),
    }
}

fn sqlite_check_unary_math(value: &Value, func: UnaryMathFunc) -> Option<f64> {
    let numeric = sqlite_check_math_arg(value)?;

    match func {
        UnaryMathFunc::Sqrt if numeric < 0.0 => None,
        UnaryMathFunc::Sqrt => Some(numeric.sqrt()),
        UnaryMathFunc::Ln if numeric <= 0.0 => None,
        UnaryMathFunc::Ln => Some(numeric.ln()),
        UnaryMathFunc::Log10 if numeric <= 0.0 => None,
        UnaryMathFunc::Log10 => Some(numeric.log10()),
        UnaryMathFunc::Log2 if numeric <= 0.0 => None,
        UnaryMathFunc::Log2 => Some(numeric.log2()),
        UnaryMathFunc::Exp => Some(numeric.exp()),
        UnaryMathFunc::Sin => Some(numeric.sin()),
        UnaryMathFunc::Cos => Some(numeric.cos()),
        UnaryMathFunc::Tan => Some(numeric.tan()),
        UnaryMathFunc::Sinh => Some(numeric.sinh()),
        UnaryMathFunc::Cosh => Some(numeric.cosh()),
        UnaryMathFunc::Tanh => Some(numeric.tanh()),
        UnaryMathFunc::Atan => Some(numeric.atan()),
        UnaryMathFunc::Acos if !(-1.0..=1.0).contains(&numeric) => None,
        UnaryMathFunc::Acos => Some(numeric.acos()),
        UnaryMathFunc::Asin if !(-1.0..=1.0).contains(&numeric) => None,
        UnaryMathFunc::Asin => Some(numeric.asin()),
        UnaryMathFunc::Acosh if numeric < 1.0 => None,
        UnaryMathFunc::Acosh => Some(numeric.acosh()),
        UnaryMathFunc::Asinh => Some(numeric.asinh()),
        UnaryMathFunc::Atanh if numeric <= -1.0 || numeric >= 1.0 => None,
        UnaryMathFunc::Atanh => Some(numeric.atanh()),
        UnaryMathFunc::Degrees => Some(numeric.to_degrees()),
        UnaryMathFunc::Radians => Some(numeric.to_radians()),
    }
}

fn sqlite_check_binary_math(
    column_value: &Value,
    argument: &Value,
    func: BinaryMathFunc,
    column_is_second: bool,
) -> Option<f64> {
    let column_value = sqlite_check_math_arg(column_value)?;
    let argument = sqlite_check_math_arg(argument)?;
    let (left, right) = if column_is_second {
        (argument, column_value)
    } else {
        (column_value, argument)
    };
    let result = match func {
        BinaryMathFunc::Power => left.powf(right),
        BinaryMathFunc::Atan2 => left.atan2(right),
        BinaryMathFunc::Log if left <= 0.0 || right <= 0.0 || left == 1.0 => return None,
        BinaryMathFunc::Log => right.log(left),
    };
    result.is_finite().then_some(result)
}

fn sqlite_check_math_arg(value: &Value) -> Option<f64> {
    match value {
        Value::Null | Value::Blob(_) => None,
        Value::Boolean(value) => {
            if *value {
                Some(1.0)
            } else {
                Some(0.0)
            }
        }
        Value::Integer(value) => Some(*value as f64),
        Value::Real(value) => Some(*value),
        Value::Text(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            trimmed.parse::<f64>().ok()
        }
    }
}

fn sqlite_check_add(left: &Value, right: &Value) -> Option<Value> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Integer(left), Value::Integer(right)) => left
            .checked_add(*right)
            .map(Value::Integer)
            .or_else(|| Some(Value::Real(*left as f64 + *right as f64))),
        _ => Some(Value::Real(
            sqlite_check_numeric_real(left) + sqlite_check_numeric_real(right),
        )),
    }
}

fn sqlite_check_multiply(left: &Value, right: &Value) -> Option<Value> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Integer(left), Value::Integer(right)) => left
            .checked_mul(*right)
            .map(Value::Integer)
            .or_else(|| Some(Value::Real(*left as f64 * *right as f64))),
        _ => Some(Value::Real(
            sqlite_check_numeric_real(left) * sqlite_check_numeric_real(right),
        )),
    }
}

fn sqlite_check_divide(left: &Value, right: &Value) -> Option<Value> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (_, Value::Integer(0)) => None,
        (_, Value::Real(value)) if *value == 0.0 => None,
        (Value::Integer(left), Value::Integer(right)) => Some(Value::Integer(left / right)),
        _ => Some(Value::Real(
            sqlite_check_numeric_real(left) / sqlite_check_numeric_real(right),
        )),
    }
}

fn sqlite_check_modulo(left: &Value, right: &Value) -> Option<Value> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        _ => {
            let left = sqlite_check_numeric_real(left) as i64;
            let right = sqlite_check_numeric_real(right) as i64;
            if right == 0 {
                None
            } else {
                Some(Value::Integer(left % right))
            }
        }
    }
}

fn sqlite_check_numeric_real(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Boolean(value) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        Value::Integer(value) => *value as f64,
        Value::Real(value) => *value,
        Value::Text(value) => sqlite_text_numeric_prefix(value),
        Value::Blob(value) => sqlite_text_numeric_prefix(&String::from_utf8_lossy(value)),
    }
}

fn sqlite_typeof_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Boolean(_) | Value::Integer(_) => "integer",
        Value::Real(_) => "real",
        Value::Blob(_) => "blob",
        Value::Text(_) => "text",
    }
}

fn sqlite_check_collated_value(value: &Value, collation: &str) -> Value {
    match (value, collation.to_ascii_uppercase().as_str()) {
        (Value::Text(value), "NOCASE") => Value::Text(sqlite_ascii_lower(value)),
        (Value::Text(value), "RTRIM") => Value::Text(value.trim_end_matches(' ').to_string()),
        (value, _) => value.clone(),
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

fn sqlite_real_to_text_for_quote(value: f64) -> String {
    if value == f64::INFINITY {
        return "9.0e+999".to_string();
    }
    if value == f64::NEG_INFINITY {
        return "-9.0e+999".to_string();
    }

    let rendered = value.to_string();
    if rendered.contains(['.', 'e', 'E']) {
        rendered
    } else {
        format!("{rendered}.0")
    }
}

fn sqlite_ascii_lower(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_uppercase() {
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
            if ch.is_ascii_lowercase() {
                ch.to_ascii_uppercase()
            } else {
                ch
            }
        })
        .collect()
}

fn sqlite_trim(value: &str, side: TrimSide, characters: Option<&str>) -> String {
    let characters = characters.unwrap_or(" ");
    let matches_trim_char = |ch| characters.chars().any(|candidate| candidate == ch);
    match side {
        TrimSide::Both => value.trim_matches(matches_trim_char).to_string(),
        TrimSide::Start => value.trim_start_matches(matches_trim_char).to_string(),
        TrimSide::End => value.trim_end_matches(matches_trim_char).to_string(),
    }
}

fn sqlite_check_instr(value: &Value, needle: &Value) -> Option<i64> {
    if matches!(value, Value::Null) || matches!(needle, Value::Null) {
        return None;
    }
    if let (Value::Blob(haystack), Value::Blob(needle)) = (value, needle) {
        if needle.is_empty() {
            return Some(1);
        }
        return haystack
            .windows(needle.len())
            .position(|window| window == needle.as_slice())
            .map(|index| index as i64 + 1)
            .or(Some(0));
    }
    let haystack = sqlite_text_like_value(value);
    let needle = sqlite_text_like_value(needle);
    if needle.is_empty() {
        return Some(1);
    }
    haystack
        .find(&needle)
        .map(|byte_index| haystack[..byte_index].chars().count() as i64 + 1)
        .or(Some(0))
}

fn sqlite_substr_text(value: &str, start: i64, length: Option<i64>) -> String {
    if length.is_some_and(|length| length <= 0) {
        return String::new();
    }
    let chars = value.chars().collect::<Vec<_>>();
    let len = chars.len() as i64;
    let start_index = if start > 0 {
        start - 1
    } else if start < 0 {
        len + start
    } else {
        0
    };
    let start_index = start_index.clamp(0, len) as usize;
    chars
        .into_iter()
        .skip(start_index)
        .take(length.map_or(usize::MAX, |length| length as usize))
        .collect()
}

fn sqlite_text_prefix_before_nul(value: &str) -> &str {
    value.split_once('\0').map_or(value, |(prefix, _)| prefix)
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

fn sqlite_text_integer_prefix(value: &str) -> i64 {
    let numeric = sqlite_text_numeric_prefix(value);
    if numeric.is_nan() {
        0
    } else if numeric >= i64::MAX as f64 {
        i64::MAX
    } else if numeric <= i64::MIN as f64 {
        i64::MIN
    } else {
        numeric as i64
    }
}

fn sqlite_text_real_prefix(value: &str) -> f64 {
    sqlite_text_numeric_prefix(value)
}

fn sqlite_text_numeric_value(value: &str) -> Value {
    let numeric = sqlite_text_numeric_prefix(value);
    if numeric.is_finite() && numeric.fract() == 0.0 {
        Value::Integer(numeric as i64)
    } else {
        Value::Real(numeric)
    }
}

fn sqlite_text_like_value(value: &Value) -> String {
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
        Value::Real(value) => {
            if *value == f64::INFINITY {
                "Inf".to_string()
            } else if *value == f64::NEG_INFINITY {
                "-Inf".to_string()
            } else {
                value.to_string()
            }
        }
        Value::Blob(value) => String::from_utf8_lossy(value).into_owned(),
        Value::Text(value) => value.clone(),
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

fn sqlite_check_regexp(pattern: &str, value: &str) -> Result<bool> {
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

fn strict_integer_value(value: Value) -> Option<Value> {
    match value {
        Value::Integer(_) => Some(value),
        Value::Boolean(value) => Some(Value::Integer(i64::from(value))),
        Value::Real(value) if value.is_finite() && value.fract() == 0.0 => {
            if value >= i64::MIN as f64 && value <= i64::MAX as f64 {
                Some(Value::Integer(value as i64))
            } else {
                None
            }
        }
        Value::Text(value) => {
            let trimmed = value.trim();
            if let Ok(integer) = trimmed.parse::<i64>() {
                Some(Value::Integer(integer))
            } else if let Ok(real) = trimmed.parse::<f64>() {
                if real.is_finite()
                    && real.fract() == 0.0
                    && real >= i64::MIN as f64
                    && real <= i64::MAX as f64
                {
                    Some(Value::Integer(real as i64))
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn strict_real_value(value: Value) -> Option<Value> {
    match value {
        Value::Real(_) => Some(value),
        Value::Integer(value) => Some(Value::Real(value as f64)),
        Value::Boolean(value) => Some(Value::Real(if value { 1.0 } else { 0.0 })),
        Value::Text(value) => value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(Value::Real),
        _ => None,
    }
}

fn strict_text_value(value: Value) -> Option<Value> {
    match value {
        Value::Text(_) => Some(value),
        Value::Integer(value) => Some(Value::Text(value.to_string())),
        Value::Real(value) => Some(Value::Text(value.to_string())),
        Value::Boolean(value) => Some(Value::Text(if value { "1" } else { "0" }.to_string())),
        _ => None,
    }
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
