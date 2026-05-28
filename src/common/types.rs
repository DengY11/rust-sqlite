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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub column_type: ColumnType,
    pub nullable: bool,
    pub primary_key: bool,
}

impl ColumnDef {
    #[must_use]
    pub fn new(name: impl Into<String>, column_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            column_type,
            nullable: true,
            primary_key: false,
        }
    }

    #[must_use]
    pub fn primary_key(name: impl Into<String>, column_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            column_type,
            nullable: false,
            primary_key: true,
        }
    }

    #[must_use]
    pub fn nullable(mut self, nullable: bool) -> Self {
        self.nullable = nullable;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMeta {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
}

impl Schema {
    #[must_use]
    pub fn new(name: impl Into<String>, columns: Vec<ColumnDef>) -> Self {
        Self {
            name: name.into(),
            columns,
        }
    }

    pub fn validate_row_values(&self, row: &Row) -> Result<()> {
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
