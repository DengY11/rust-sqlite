use std::collections::HashMap;

use rustsql::common::types::{ColumnDef, ColumnType, Value};
use rustsql::sql::ast::{Assignment, CompareOp, Expr, ScalarBinaryOp, ScalarExpr, SelectItem};
use rustsql::sql::optimizer::Optimizer;
use rustsql::sql::plan::{IndexBound, IndexRange, IndexScanMode, Plan};
use rustsql::sql::planner::PlanningContext;

#[test]
fn optimizer_exposes_named_default_pass_pipeline() {
    let optimizer = Optimizer::new();

    assert_eq!(optimizer.pass_names(), vec!["index_selection"]);
}

#[test]
fn optimizer_accepts_all_top_level_statement_plans() {
    let optimizer = Optimizer::new();
    let plans = vec![
        Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
            constraints: vec![],
        },
        Plan::CreateIndex {
            name: "idx_users_name".to_string(),
            table: "users".to_string(),
            columns: vec!["name".to_string()],
            unique: false,
        },
        Plan::DropTable {
            name: "users".to_string(),
        },
        Plan::DropIndex {
            table: "users".to_string(),
            name: "idx_users_name".to_string(),
        },
        Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(1)],
        },
        Plan::Delete {
            table: "users".to_string(),
            filter: Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(1),
            }),
        },
        Plan::Update {
            table: "users".to_string(),
            assignments: vec![Assignment {
                column: "name".to_string(),
                value: Value::from("alice"),
            }],
            filter: None,
        },
        Plan::SeqScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            filter: None,
            order_by: vec![],
            limit: None,
            distinct: false,
        },
        Plan::BeginTxn,
        Plan::CommitTxn,
        Plan::RollbackTxn,
    ];

    for plan in plans {
        let optimized = optimizer.optimize(plan.clone()).unwrap();
        assert_eq!(optimized, plan);
    }
}

#[test]
fn optimizer_rewrites_indexable_seq_scan_to_index_scan() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![rustsql::common::types::IndexMeta {
                name: "idx_users_id".to_string(),
                columns: vec!["id".to_string()],
                unique: false,
            }],
        )]),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("name".to_string())],
        filter: Some(Expr::Compare {
            column: "id".to_string(),
            op: CompareOp::Eq,
            value: Value::Integer(7),
        }),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer.optimize_with_context(plan, &context).unwrap();

    assert_eq!(
        optimized,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("name".to_string())],
            index: "idx_users_id".to_string(),
            mode: IndexScanMode::Lookup,
            key_prefix: vec![Value::Integer(7)],
            range: None,
            filter: Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(7),
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn optimizer_uses_indexable_and_term_with_scalar_residual_filter() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![rustsql::common::types::IndexMeta {
                name: "idx_users_id".to_string(),
                columns: vec!["id".to_string()],
                unique: false,
            }],
        )]),
    );
    let filter = Expr::And(
        Box::new(Expr::Compare {
            column: "id".to_string(),
            op: CompareOp::Eq,
            value: Value::Integer(7),
        }),
        Box::new(Expr::CompareScalar {
            left: ScalarExpr::Binary {
                left: Box::new(ScalarExpr::Column("age".to_string())),
                op: ScalarBinaryOp::Add,
                right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
            },
            op: CompareOp::Gte,
            right: ScalarExpr::Literal(Value::Integer(18)),
        }),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("age".to_string())],
        filter: Some(filter.clone()),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer.optimize_with_context(plan, &context).unwrap();

    assert_eq!(
        optimized,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("age".to_string())],
            index: "idx_users_id".to_string(),
            mode: IndexScanMode::Lookup,
            key_prefix: vec![Value::Integer(7)],
            range: None,
            filter: Some(filter),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn optimizer_does_not_rewrite_scalar_is_null_filter_to_index_scan() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("nickname", ColumnType::Text),
                    ColumnDef::new("name", ColumnType::Text),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![rustsql::common::types::IndexMeta {
                name: "idx_users_name".to_string(),
                columns: vec!["name".to_string()],
                unique: false,
            }],
        )]),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("id".to_string())],
        filter: Some(Expr::IsNullScalar {
            expr: ScalarExpr::Binary {
                left: Box::new(ScalarExpr::Column("nickname".to_string())),
                op: ScalarBinaryOp::Concat,
                right: Box::new(ScalarExpr::Column("name".to_string())),
            },
            negated: false,
        }),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer
        .optimize_with_context(plan.clone(), &context)
        .unwrap();

    assert_eq!(optimized, plan);
}

#[test]
fn optimizer_does_not_rewrite_scalar_like_filter_to_index_scan() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![rustsql::common::types::IndexMeta {
                name: "idx_users_name".to_string(),
                columns: vec!["name".to_string()],
                unique: false,
            }],
        )]),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("id".to_string())],
        filter: Some(Expr::LikeScalar {
            expr: ScalarExpr::Function {
                func: rustsql::sql::ast::ScalarFunc::Lower,
                args: vec![ScalarExpr::Column("name".to_string())],
            },
            pattern: "a%".to_string(),
            negated: false,
        }),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer
        .optimize_with_context(plan.clone(), &context)
        .unwrap();

    assert_eq!(optimized, plan);
}

#[test]
fn optimizer_does_not_rewrite_scalar_between_filter_to_index_scan() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![rustsql::common::types::IndexMeta {
                name: "idx_users_age".to_string(),
                columns: vec!["age".to_string()],
                unique: false,
            }],
        )]),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("id".to_string())],
        filter: Some(Expr::BetweenScalar {
            expr: ScalarExpr::Binary {
                left: Box::new(ScalarExpr::Column("age".to_string())),
                op: ScalarBinaryOp::Add,
                right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
            },
            low: ScalarExpr::Literal(Value::Integer(18)),
            high: ScalarExpr::Literal(Value::Integer(30)),
            negated: false,
        }),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer
        .optimize_with_context(plan.clone(), &context)
        .unwrap();

    assert_eq!(optimized, plan);
}

#[test]
fn optimizer_rewrites_between_filter_to_index_range_scan() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![rustsql::common::types::IndexMeta {
                name: "idx_users_age".to_string(),
                columns: vec!["age".to_string()],
                unique: false,
            }],
        )]),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("id".to_string())],
        filter: Some(Expr::Between {
            column: "age".to_string(),
            low: Value::Integer(18),
            high: Value::Integer(30),
            negated: false,
        }),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer.optimize_with_context(plan, &context).unwrap();

    assert_eq!(
        optimized,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("id".to_string())],
            index: "idx_users_age".to_string(),
            mode: IndexScanMode::Range,
            key_prefix: vec![],
            range: Some(IndexRange {
                column: "age".to_string(),
                lower: Some(IndexBound {
                    op: CompareOp::Gte,
                    value: Value::Integer(18),
                }),
                upper: Some(IndexBound {
                    op: CompareOp::Lte,
                    value: Value::Integer(30),
                }),
            }),
            filter: Some(Expr::Between {
                column: "age".to_string(),
                low: Value::Integer(18),
                high: Value::Integer(30),
                negated: false,
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn optimizer_rewrites_prefix_like_filter_to_index_range_scan() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![rustsql::common::types::IndexMeta {
                name: "idx_users_name".to_string(),
                columns: vec!["name".to_string()],
                unique: false,
            }],
        )]),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("id".to_string())],
        filter: Some(Expr::Like {
            column: "name".to_string(),
            pattern: "ali%".to_string(),
            negated: false,
        }),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer.optimize_with_context(plan, &context).unwrap();

    assert_eq!(
        optimized,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("id".to_string())],
            index: "idx_users_name".to_string(),
            mode: IndexScanMode::Range,
            key_prefix: vec![],
            range: Some(IndexRange {
                column: "name".to_string(),
                lower: Some(IndexBound {
                    op: CompareOp::Gte,
                    value: Value::from("ali"),
                }),
                upper: Some(IndexBound {
                    op: CompareOp::Lt,
                    value: Value::from("alj"),
                }),
            }),
            filter: Some(Expr::Like {
                column: "name".to_string(),
                pattern: "ali%".to_string(),
                negated: false,
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn optimizer_rewrites_is_null_filter_to_null_prefix_index_scan() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("email", ColumnType::Text),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![rustsql::common::types::IndexMeta {
                name: "idx_users_email".to_string(),
                columns: vec!["email".to_string()],
                unique: false,
            }],
        )]),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("id".to_string())],
        filter: Some(Expr::IsNull {
            column: "email".to_string(),
            negated: false,
        }),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer.optimize_with_context(plan, &context).unwrap();

    assert_eq!(
        optimized,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("id".to_string())],
            index: "idx_users_email".to_string(),
            mode: IndexScanMode::Lookup,
            key_prefix: vec![Value::Null],
            range: None,
            filter: Some(Expr::IsNull {
                column: "email".to_string(),
                negated: false,
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn optimizer_does_not_rewrite_is_not_null_filter_to_null_prefix_index_scan() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("email", ColumnType::Text),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![rustsql::common::types::IndexMeta {
                name: "idx_users_email".to_string(),
                columns: vec!["email".to_string()],
                unique: false,
            }],
        )]),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("id".to_string())],
        filter: Some(Expr::IsNull {
            column: "email".to_string(),
            negated: true,
        }),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer
        .optimize_with_context(plan.clone(), &context)
        .unwrap();

    assert_eq!(optimized, plan);
}

#[test]
fn optimizer_prefers_unique_index_when_match_quality_ties() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("email", ColumnType::Text),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![
                rustsql::common::types::IndexMeta {
                    name: "idx_users_email_unique".to_string(),
                    columns: vec!["email".to_string()],
                    unique: true,
                },
                rustsql::common::types::IndexMeta {
                    name: "idx_users_email".to_string(),
                    columns: vec!["email".to_string()],
                    unique: false,
                },
            ],
        )]),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("id".to_string())],
        filter: Some(Expr::Compare {
            column: "email".to_string(),
            op: CompareOp::Eq,
            value: Value::from("alice@example.com"),
        }),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer.optimize_with_context(plan, &context).unwrap();

    assert_eq!(
        optimized,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("id".to_string())],
            index: "idx_users_email_unique".to_string(),
            mode: IndexScanMode::Lookup,
            key_prefix: vec![Value::from("alice@example.com")],
            range: None,
            filter: Some(Expr::Compare {
                column: "email".to_string(),
                op: CompareOp::Eq,
                value: Value::from("alice@example.com"),
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn optimizer_prefers_narrower_index_when_match_quality_and_uniqueness_tie() {
    let optimizer = Optimizer::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("email", ColumnType::Text),
                    ColumnDef::new("created_at", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::from([(
            "users".to_string(),
            vec![
                rustsql::common::types::IndexMeta {
                    name: "idx_users_email".to_string(),
                    columns: vec!["email".to_string()],
                    unique: false,
                },
                rustsql::common::types::IndexMeta {
                    name: "idx_users_email_created_at".to_string(),
                    columns: vec!["email".to_string(), "created_at".to_string()],
                    unique: false,
                },
            ],
        )]),
    );
    let plan = Plan::SeqScan {
        table: "users".to_string(),
        table_alias: None,
        columns: vec![SelectItem::Column("id".to_string())],
        filter: Some(Expr::Compare {
            column: "email".to_string(),
            op: CompareOp::Eq,
            value: Value::from("alice@example.com"),
        }),
        order_by: vec![],
        limit: None,
        distinct: false,
    };

    let optimized = optimizer.optimize_with_context(plan, &context).unwrap();

    assert_eq!(
        optimized,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("id".to_string())],
            index: "idx_users_email".to_string(),
            mode: IndexScanMode::Lookup,
            key_prefix: vec![Value::from("alice@example.com")],
            range: None,
            filter: Some(Expr::Compare {
                column: "email".to_string(),
                op: CompareOp::Eq,
                value: Value::from("alice@example.com"),
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}
