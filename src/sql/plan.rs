use crate::common::types::{ColumnDef, Value};
use crate::sql::ast::{Expr, SelectItem};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
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
    SeqScan {
        table: String,
        columns: Vec<SelectItem>,
        filter: Option<Expr>,
    },
    IndexScan {
        table: String,
        columns: Vec<SelectItem>,
        index: String,
        column: String,
        value: Value,
    },
    BeginTxn,
    CommitTxn,
    RollbackTxn,
}
