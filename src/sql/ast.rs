use crate::common::types::{ColumnDef, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStatement {
    pub distinct: bool,
    pub columns: Vec<SelectItem>,
    pub table: String,
    pub table_alias: Option<String>,
    pub joins: Vec<JoinClause>,
    pub filter: Option<Expr>,
    pub group_by: Vec<String>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinClause {
    pub kind: JoinKind,
    pub table: String,
    pub table_alias: Option<String>,
    pub on: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub column: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub expr: OrderByExpr,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderByExpr {
    Column(String),
    Position(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateArg {
    Wildcard,
    Column { name: String, distinct: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
    },
    DropTable {
        name: String,
    },
    DropIndex {
        name: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        values: Vec<Value>,
    },
    Delete {
        table: String,
        table_alias: Option<String>,
        filter: Option<Expr>,
    },
    Update {
        table: String,
        table_alias: Option<String>,
        assignments: Vec<Assignment>,
        filter: Option<Expr>,
    },
    Select(SelectStatement),
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectItem {
    Wildcard,
    Column(String),
    AliasedColumn {
        name: String,
        alias: String,
    },
    Aggregate {
        func: AggregateFunc,
        arg: AggregateArg,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Compare {
        column: String,
        op: CompareOp,
        value: Value,
    },
    CompareColumns {
        left: String,
        op: CompareOp,
        right: String,
    },
    IsNull {
        column: String,
        negated: bool,
    },
    InSubquery {
        column: String,
        query: Box<SelectStatement>,
        negated: bool,
    },
    CompareSubquery {
        column: String,
        op: CompareOp,
        query: Box<SelectStatement>,
    },
    Like {
        column: String,
        pattern: String,
        negated: bool,
    },
    Between {
        column: String,
        low: Value,
        high: Value,
        negated: bool,
    },
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

#[cfg(test)]
mod tests {
    use crate::common::types::{ColumnDef, ColumnType, Value};

    use super::{
        AggregateArg, AggregateFunc, CompareOp, Expr, SelectItem, SelectStatement, Statement,
    };

    #[test]
    fn statement_variants_preserve_payloads() {
        let statement = Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
        };
        assert_eq!(
            statement,
            Statement::CreateTable {
                name: "users".to_string(),
                columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
            }
        );
    }

    #[test]
    fn select_items_and_exprs_are_comparable() {
        assert_eq!(
            SelectItem::Column("name".to_string()),
            SelectItem::Column("name".to_string())
        );
        assert_eq!(
            Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(1),
            },
            Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(1),
            }
        );
        assert_ne!(
            Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(1),
            },
            Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Lt,
                value: Value::Integer(1),
            }
        );
        assert_eq!(
            Expr::Not(Box::new(Expr::IsNull {
                column: "name".to_string(),
                negated: false,
            })),
            Expr::Not(Box::new(Expr::IsNull {
                column: "name".to_string(),
                negated: false,
            }))
        );
        assert_eq!(
            SelectItem::Aggregate {
                func: AggregateFunc::Count,
                arg: AggregateArg::Wildcard,
                alias: Some("total".to_string()),
            },
            SelectItem::Aggregate {
                func: AggregateFunc::Count,
                arg: AggregateArg::Wildcard,
                alias: Some("total".to_string()),
            }
        );
        assert_eq!(
            Statement::Select(SelectStatement {
                distinct: false,
                columns: vec![SelectItem::Column("id".to_string())],
                table: "users".to_string(),
                table_alias: None,
                joins: vec![],
                filter: None,
                group_by: vec![],
                having: None,
                order_by: vec![],
                limit: None,
            }),
            Statement::Select(SelectStatement {
                distinct: false,
                columns: vec![SelectItem::Column("id".to_string())],
                table: "users".to_string(),
                table_alias: None,
                joins: vec![],
                filter: None,
                group_by: vec![],
                having: None,
                order_by: vec![],
                limit: None,
            })
        );
    }
}
