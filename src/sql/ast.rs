use crate::common::types::{CheckConstraint, ColumnDef, ForeignKey, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromItem {
    Table {
        name: String,
        alias: Option<String>,
    },
    Subquery {
        query: Box<SelectStatement>,
        alias: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStatement {
    pub with: Option<WithClause>,
    pub distinct: bool,
    pub columns: Vec<SelectItem>,
    pub from: FromItem,
    pub joins: Vec<JoinClause>,
    pub filter: Option<Expr>,
    pub group_by: Vec<ScalarExpr>,
    pub having: Option<Expr>,
    pub compounds: Vec<CompoundSelect>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundSelect {
    pub operator: CompoundOperator,
    pub select: Box<SelectStatement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompoundOperator {
    Union,
    UnionAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithClause {
    pub recursive: bool,
    pub ctes: Vec<CommonTableExpr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonTableExpr {
    pub name: String,
    pub query: Box<SelectStatement>,
}

impl SelectStatement {
    #[must_use]
    pub fn base_table(&self) -> Option<(&str, Option<&str>)> {
        match &self.from {
            FromItem::Table { name, alias } => Some((name.as_str(), alias.as_deref())),
            FromItem::Subquery { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinClause {
    pub kind: JoinKind,
    pub source: FromItem,
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
    pub nulls: Option<NullOrder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullOrder {
    First,
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderByExpr {
    Column(String),
    Position(usize),
    Expr(ScalarExpr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterTableAction {
    AddColumn(ColumnDef),
    RenameTable { new_name: String },
    RenameColumn { old_name: String, new_name: String },
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
    Expr { expr: ScalarExpr, distinct: bool },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
        constraints: Vec<TableConstraint>,
    },
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        unique: bool,
    },
    DropTable {
        name: String,
    },
    DropIndex {
        name: String,
    },
    AlterTable {
        table: String,
        action: AlterTableAction,
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
    ExplainQueryPlan(Box<Statement>),
    Begin {
        isolation_level: Option<IsolationLevel>,
    },
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableConstraint {
    Check(CheckConstraint),
    ForeignKey(ForeignKey),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectItem {
    Wildcard,
    Column(String),
    AliasedColumn {
        name: String,
        alias: String,
    },
    Expr {
        expr: ScalarExpr,
        alias: Option<String>,
    },
    Aggregate {
        func: AggregateFunc,
        arg: AggregateArg,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScalarExpr {
    Literal(Value),
    Column(String),
    UnaryMinus(Box<ScalarExpr>),
    Binary {
        left: Box<ScalarExpr>,
        op: ScalarBinaryOp,
        right: Box<ScalarExpr>,
    },
    Function {
        func: ScalarFunc,
        args: Vec<ScalarExpr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarBinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Concat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    Length,
    Lower,
    Upper,
    Abs,
    Coalesce,
    IfNull,
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
    CompareScalar {
        left: ScalarExpr,
        op: CompareOp,
        right: ScalarExpr,
    },
    IsNull {
        column: String,
        negated: bool,
    },
    IsNullScalar {
        expr: ScalarExpr,
        negated: bool,
    },
    InSubquery {
        column: String,
        query: Box<SelectStatement>,
        negated: bool,
    },
    InSubqueryScalar {
        expr: ScalarExpr,
        query: Box<SelectStatement>,
        negated: bool,
    },
    CompareSubquery {
        column: String,
        op: CompareOp,
        query: Box<SelectStatement>,
    },
    CompareSubqueryScalar {
        left: ScalarExpr,
        op: CompareOp,
        query: Box<SelectStatement>,
    },
    ExistsSubquery {
        query: Box<SelectStatement>,
        negated: bool,
    },
    Like {
        column: String,
        pattern: String,
        negated: bool,
    },
    LikeScalar {
        expr: ScalarExpr,
        pattern: String,
        negated: bool,
    },
    Between {
        column: String,
        low: Value,
        high: Value,
        negated: bool,
    },
    BetweenScalar {
        expr: ScalarExpr,
        low: ScalarExpr,
        high: ScalarExpr,
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
        AggregateArg, AggregateFunc, CompareOp, Expr, FromItem, SelectItem, SelectStatement,
        Statement,
    };

    #[test]
    fn statement_variants_preserve_payloads() {
        let statement = Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
            constraints: vec![],
        };
        assert_eq!(
            statement,
            Statement::CreateTable {
                name: "users".to_string(),
                columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
                constraints: vec![],
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
                with: None,
                distinct: false,
                columns: vec![SelectItem::Column("id".to_string())],
                from: FromItem::Table {
                    name: "users".to_string(),
                    alias: None,
                },
                joins: vec![],
                filter: None,
                group_by: vec![],
                having: None,
                compounds: vec![],
                order_by: vec![],
                limit: None,
            }),
            Statement::Select(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![SelectItem::Column("id".to_string())],
                from: FromItem::Table {
                    name: "users".to_string(),
                    alias: None,
                },
                joins: vec![],
                filter: None,
                group_by: vec![],
                having: None,
                compounds: vec![],
                order_by: vec![],
                limit: None,
            })
        );
    }
}
