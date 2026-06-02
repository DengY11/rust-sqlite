use crate::common::types::{ColumnDef, Value};
use crate::sql::ast::{
    AlterTableAction, Assignment, CompareOp, Expr, JoinKind, OrderBy, ScalarExpr, SelectItem,
    TableConstraint,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinPlan {
    pub kind: JoinKind,
    pub table: String,
    pub table_alias: Option<String>,
    pub on: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexScanSpec {
    pub index: String,
    pub mode: IndexScanMode,
    pub key_prefix: Vec<Value>,
    pub range: Option<IndexRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexScanMode {
    Lookup,
    Prefix,
    Range,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBound {
    pub op: CompareOp,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRange {
    pub column: String,
    pub lower: Option<IndexBound>,
    pub upper: Option<IndexBound>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
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
        table: String,
        name: String,
    },
    AlterTable {
        table: String,
        action: AlterTableAction,
    },
    Insert {
        table: String,
        values: Vec<Value>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
    Update {
        table: String,
        assignments: Vec<Assignment>,
        filter: Option<Expr>,
    },
    SeqScan {
        table: String,
        table_alias: Option<String>,
        columns: Vec<SelectItem>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        distinct: bool,
    },
    IndexScan {
        table: String,
        table_alias: Option<String>,
        columns: Vec<SelectItem>,
        index: String,
        mode: IndexScanMode,
        key_prefix: Vec<Value>,
        range: Option<IndexRange>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        distinct: bool,
    },
    IndexUnion {
        table: String,
        table_alias: Option<String>,
        columns: Vec<SelectItem>,
        scans: Vec<IndexScanSpec>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        distinct: bool,
    },
    NestedLoopJoin {
        table: String,
        table_alias: Option<String>,
        joins: Vec<JoinPlan>,
        columns: Vec<SelectItem>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        distinct: bool,
    },
    Aggregate {
        source: Box<Plan>,
        columns: Vec<SelectItem>,
        group_by: Vec<ScalarExpr>,
        having: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
    },
    ExplainQueryPlan {
        plan: Box<Plan>,
    },
    BeginTxn,
    CommitTxn,
    RollbackTxn,
}

#[cfg(test)]
mod tests {
    use crate::common::types::{ColumnDef, ColumnType, Value};
    use crate::sql::ast::{CompareOp, Expr, JoinKind, SelectItem};

    use super::{IndexBound, IndexRange, IndexScanMode, IndexScanSpec, JoinPlan, Plan};

    #[test]
    fn plan_variants_preserve_statement_payloads() {
        let plan = Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
            constraints: vec![],
        };
        assert_eq!(
            plan,
            Plan::CreateTable {
                name: "users".to_string(),
                columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
                constraints: vec![],
            }
        );
    }

    #[test]
    fn scan_plans_are_comparable_with_filters() {
        let plan = Plan::SeqScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            filter: Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(9),
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        };
        assert_eq!(
            plan,
            Plan::SeqScan {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Wildcard],
                filter: Some(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(9),
                }),
                order_by: vec![],
                limit: None,
                distinct: false,
            }
        );
    }

    #[test]
    fn index_scan_plans_are_comparable_with_range_bounds() {
        let plan = Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            index: "idx_users_id_name".to_string(),
            mode: IndexScanMode::Range,
            key_prefix: vec![Value::Integer(7)],
            range: Some(IndexRange {
                column: "name".to_string(),
                lower: Some(IndexBound {
                    op: CompareOp::Gt,
                    value: Value::from("alice"),
                }),
                upper: None,
            }),
            filter: None,
            order_by: vec![],
            limit: None,
            distinct: false,
        };
        assert_eq!(
            plan,
            Plan::IndexScan {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Wildcard],
                index: "idx_users_id_name".to_string(),
                mode: IndexScanMode::Range,
                key_prefix: vec![Value::Integer(7)],
                range: Some(IndexRange {
                    column: "name".to_string(),
                    lower: Some(IndexBound {
                        op: CompareOp::Gt,
                        value: Value::from("alice"),
                    }),
                    upper: None,
                }),
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }
        );
    }

    #[test]
    fn index_union_plans_are_comparable_with_scan_specs() {
        let plan = Plan::IndexUnion {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            scans: vec![
                IndexScanSpec {
                    index: "idx_users_id".to_string(),
                    mode: IndexScanMode::Lookup,
                    key_prefix: vec![Value::Integer(7)],
                    range: None,
                },
                IndexScanSpec {
                    index: "idx_users_name".to_string(),
                    mode: IndexScanMode::Lookup,
                    key_prefix: vec![Value::from("alice")],
                    range: None,
                },
            ],
            filter: Some(Expr::Or(
                Box::new(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(7),
                }),
                Box::new(Expr::Compare {
                    column: "name".to_string(),
                    op: CompareOp::Eq,
                    value: Value::from("alice"),
                }),
            )),
            order_by: vec![],
            limit: None,
            distinct: false,
        };

        assert_eq!(
            plan,
            Plan::IndexUnion {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Wildcard],
                scans: vec![
                    IndexScanSpec {
                        index: "idx_users_id".to_string(),
                        mode: IndexScanMode::Lookup,
                        key_prefix: vec![Value::Integer(7)],
                        range: None,
                    },
                    IndexScanSpec {
                        index: "idx_users_name".to_string(),
                        mode: IndexScanMode::Lookup,
                        key_prefix: vec![Value::from("alice")],
                        range: None,
                    },
                ],
                filter: Some(Expr::Or(
                    Box::new(Expr::Compare {
                        column: "id".to_string(),
                        op: CompareOp::Eq,
                        value: Value::Integer(7),
                    }),
                    Box::new(Expr::Compare {
                        column: "name".to_string(),
                        op: CompareOp::Eq,
                        value: Value::from("alice"),
                    }),
                )),
                order_by: vec![],
                limit: None,
                distinct: false,
            }
        );
    }

    #[test]
    fn nested_loop_join_plans_are_comparable() {
        let plan = Plan::NestedLoopJoin {
            table: "users".to_string(),
            table_alias: Some("u".to_string()),
            joins: vec![JoinPlan {
                kind: JoinKind::Inner,
                table: "orders".to_string(),
                table_alias: Some("o".to_string()),
                on: Expr::CompareColumns {
                    left: "u.id".to_string(),
                    op: CompareOp::Eq,
                    right: "o.user_id".to_string(),
                },
            }],
            columns: vec![SelectItem::Column("u.id".to_string())],
            filter: None,
            order_by: vec![],
            limit: None,
            distinct: false,
        };

        assert_eq!(
            plan,
            Plan::NestedLoopJoin {
                table: "users".to_string(),
                table_alias: Some("u".to_string()),
                joins: vec![JoinPlan {
                    kind: JoinKind::Inner,
                    table: "orders".to_string(),
                    table_alias: Some("o".to_string()),
                    on: Expr::CompareColumns {
                        left: "u.id".to_string(),
                        op: CompareOp::Eq,
                        right: "o.user_id".to_string(),
                    },
                }],
                columns: vec![SelectItem::Column("u.id".to_string())],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }
        );
    }
}
