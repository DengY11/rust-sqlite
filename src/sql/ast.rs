use crate::common::types::{ColumnDef, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    CreateIndex {
        name: String,
        table: String,
        column: String,
    },
    Insert {
        table: String,
        values: Vec<Value>,
    },
    Select {
        columns: Vec<SelectItem>,
        table: String,
        filter: Option<Expr>,
    },
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectItem {
    Wildcard,
    Column(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Eq(String, Value),
    Gt(String, Value),
    Lt(String, Value),
}
