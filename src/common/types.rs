//! Shared logical types used across the crate.

use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::common::error::{DbError, Result};

pub type Row = Vec<Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RowId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    Text(String),
}

impl Display for Value {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Boolean(value) => write!(f, "{value}"),
            Self::Integer(value) => write!(f, "{value}"),
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
            Self::Text(_) => "TEXT",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnType {
    Boolean,
    Integer,
    Text,
}

impl ColumnType {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Boolean => "BOOLEAN",
            Self::Integer => "INTEGER",
            Self::Text => "TEXT",
        }
    }

    #[must_use]
    pub fn matches_value(&self, value: &Value) -> bool {
        matches!(
            (self, value),
            (Self::Boolean, Value::Boolean(_))
                | (Self::Integer, Value::Integer(_))
                | (Self::Text, Value::Text(_))
        )
    }
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
    And(Box<CheckExpr>, Box<CheckExpr>),
    Or(Box<CheckExpr>, Box<CheckExpr>),
    Not(Box<CheckExpr>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckConstraint {
    pub name: String,
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
    pub column: String,
    pub ref_table: String,
    pub ref_column: String,
}

impl ForeignKey {
    #[must_use]
    pub fn single_column(
        column: impl Into<String>,
        ref_table: impl Into<String>,
        ref_column: impl Into<String>,
    ) -> Self {
        Self {
            column: column.into(),
            ref_table: ref_table.into(),
            ref_column: ref_column.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub column_type: ColumnType,
    pub nullable: bool,
    pub primary_key: bool,
    pub default_value: Option<Value>,
    pub checks: Vec<CheckConstraint>,
    pub foreign_key: Option<ForeignKey>,
}

impl ColumnDef {
    #[must_use]
    pub fn new(name: impl Into<String>, column_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            column_type,
            nullable: true,
            primary_key: false,
            default_value: None,
            checks: Vec::new(),
            foreign_key: None,
        }
    }

    #[must_use]
    pub fn primary_key(name: impl Into<String>, column_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            column_type,
            nullable: false,
            primary_key: true,
            default_value: None,
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
    pub fn default_value(mut self, value: Value) -> Self {
        self.default_value = Some(value);
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMeta {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

impl IndexMeta {
    #[must_use]
    pub fn enforces_unique_key(&self, key: &[Value]) -> bool {
        self.unique && !key.iter().any(|value| matches!(value, Value::Null))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub checks: Vec<CheckConstraint>,
    pub foreign_keys: Vec<ForeignKey>,
}

impl Schema {
    #[must_use]
    pub fn new(name: impl Into<String>, columns: Vec<ColumnDef>) -> Self {
        Self {
            name: name.into(),
            columns,
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_check(mut self, check: CheckConstraint) -> Self {
        self.checks.push(check);
        self
    }

    #[must_use]
    pub fn with_foreign_key(mut self, foreign_key: ForeignKey) -> Self {
        self.foreign_keys.push(foreign_key);
        self
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
            if !self.has_column(&foreign_key.column) {
                return Err(DbError::storage(format!(
                    "unknown column in FOREIGN KEY: {}",
                    foreign_key.column
                )));
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
            if let Some(foreign_key) = &mut column.foreign_key
                && foreign_key.column == old_name
            {
                foreign_key.column = new_name.to_string();
            }
        }

        for check in &mut self.checks {
            rename_check_expr_column(&mut check.expr, old_name, new_name);
        }
        for foreign_key in &mut self.foreign_keys {
            if foreign_key.column == old_name {
                foreign_key.column = new_name.to_string();
            }
            if foreign_key.ref_table == self.name && foreign_key.ref_column == old_name {
                foreign_key.ref_column = new_name.to_string();
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
                && foreign_key.ref_column == old_name
            {
                foreign_key.ref_column = new_name.to_string();
            }
        }

        for foreign_key in &mut self.foreign_keys {
            if foreign_key.ref_table == ref_table && foreign_key.ref_column == old_name {
                foreign_key.ref_column = new_name.to_string();
            }
        }
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
        for column in &self.columns {
            for check in &column.checks {
                self.validate_check_constraint(check, row)?;
            }
        }
        for check in &self.checks {
            self.validate_check_constraint(check, row)?;
        }
        Ok(())
    }

    fn validate_check_constraint(&self, check: &CheckConstraint, row: &Row) -> Result<()> {
        if self.evaluate_check_expr(&check.expr, row)? == Some(false) {
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

    fn validate_check_expr_metadata(&self, expr: &CheckExpr) -> Result<()> {
        match expr {
            CheckExpr::Compare { column, .. } | CheckExpr::IsNull { column, .. } => {
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

    fn evaluate_check_expr(&self, expr: &CheckExpr, row: &Row) -> Result<Option<bool>> {
        match expr {
            CheckExpr::Compare { column, op, value } => {
                let left = self.value_for_column(row, column)?;
                Ok(Self::evaluate_check_compare(left, *op, value))
            }
            CheckExpr::IsNull { column, negated } => {
                let value = self.value_for_column(row, column)?;
                Ok(Some(matches!(value, Value::Null) ^ *negated))
            }
            CheckExpr::And(left, right) => {
                let left = self.evaluate_check_expr(left, row)?;
                let right = self.evaluate_check_expr(right, row)?;
                Ok(match (left, right) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                })
            }
            CheckExpr::Or(left, right) => {
                let left = self.evaluate_check_expr(left, row)?;
                let right = self.evaluate_check_expr(right, row)?;
                Ok(match (left, right) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                })
            }
            CheckExpr::Not(expr) => Ok(self.evaluate_check_expr(expr, row)?.map(|value| !value)),
        }
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

    pub fn validate_primary_key_uniqueness(&self, row: &Row, existing_rows: &[&Row]) -> Result<()> {
        for (index, column) in self.columns.iter().enumerate() {
            if !column.primary_key {
                continue;
            }

            let value = &row[index];
            if existing_rows
                .iter()
                .any(|existing_row| existing_row.get(index) == Some(value))
            {
                return Err(DbError::storage(format!(
                    "duplicate primary key value for column '{}': {}",
                    column.name, value
                )));
            }
        }

        Ok(())
    }
}

fn rename_check_expr_column(expr: &mut CheckExpr, old_name: &str, new_name: &str) {
    match expr {
        CheckExpr::Compare { column, .. } | CheckExpr::IsNull { column, .. } => {
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

#[cfg(test)]
mod tests {
    use super::{CheckConstraint, CheckOp, ColumnDef, ColumnType, ForeignKey, Schema, Value};

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
            .default_value(Value::Integer(0))
            .check(CheckConstraint::compare(
                "age_non_negative",
                "age",
                CheckOp::Gte,
                Value::Integer(0),
            ))
            .references("users", "id");

        assert_eq!(column.default_value, Some(Value::Integer(0)));
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
        .with_foreign_key(ForeignKey::single_column("user_id", "users", "id"));

        assert_eq!(schema.checks.len(), 1);
        assert_eq!(schema.foreign_keys.len(), 1);
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
