//! Shared database error types.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug)]
pub enum DbError {
    Sql(String),
    Plan(String),
    Storage(String),
    Txn(String),
    Io(std::io::Error),
    Serde(String),
}

impl DbError {
    #[must_use]
    pub fn sql(message: impl Into<String>) -> Self {
        Self::Sql(message.into())
    }

    #[must_use]
    pub fn plan(message: impl Into<String>) -> Self {
        Self::Plan(message.into())
    }

    #[must_use]
    pub fn storage(message: impl Into<String>) -> Self {
        Self::Storage(message.into())
    }

    #[must_use]
    pub fn txn(message: impl Into<String>) -> Self {
        Self::Txn(message.into())
    }

    #[must_use]
    pub fn serde(message: impl Into<String>) -> Self {
        Self::Serde(message.into())
    }
}

impl Display for DbError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(message) => write!(f, "sql error: {message}"),
            Self::Plan(message) => write!(f, "plan error: {message}"),
            Self::Storage(message) => write!(f, "storage error: {message}"),
            Self::Txn(message) => write!(f, "transaction error: {message}"),
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::Serde(message) => write!(f, "serde error: {message}"),
        }
    }
}

impl Error for DbError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sql(_) | Self::Plan(_) | Self::Storage(_) | Self::Txn(_) | Self::Serde(_) => None,
        }
    }
}

impl From<std::io::Error> for DbError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for DbError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value.to_string())
    }
}
