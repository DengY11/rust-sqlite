use std::collections::HashMap;

use rustsql::common::types::{
    CheckConstraint, CheckOp, ColumnDef, ColumnType, IndexMeta, Schema, Value,
};
use rustsql::sql::ast::{
    AggregateArg, AggregateFunc, AlterTableAction, CompareOp, Expr, FromItem, JoinClause, JoinKind,
    OrderBy, OrderByExpr, ScalarExpr, SelectItem, SelectStatement, Statement, TableConstraint,
};
use rustsql::sql::optimizer::Optimizer;
use rustsql::sql::parser::parse_sql;
use rustsql::sql::plan::{IndexBound, IndexRange, IndexScanMode, IndexScanSpec, JoinPlan, Plan};
use rustsql::sql::planner::{Planner, PlanningContext};

fn select_statement(columns: Vec<SelectItem>, table: &str, filter: Option<Expr>) -> Statement {
    Statement::Select(SelectStatement {
        with: None,
        distinct: false,
        columns,
        from: FromItem::Table {
            name: table.to_string(),
            alias: None,
        },
        joins: vec![],
        filter,
        group_by: vec![],
        having: None,
        compounds: vec![],
        order_by: vec![],
        limit: None,
    })
}

fn user_schema() -> Schema {
    Schema::new(
        "users",
        vec![
            ColumnDef::primary_key("id", ColumnType::Integer),
            ColumnDef::new("name", ColumnType::Text),
        ],
    )
}

fn build_users_context() -> PlanningContext {
    context_with_indexes(vec![])
}

fn context_with_indexes(indexes: Vec<IndexMeta>) -> PlanningContext {
    PlanningContext::new(
        HashMap::from([("users".to_string(), user_schema())]),
        HashMap::from([("users".to_string(), indexes)]),
    )
}

fn optimized_plan(statement: &Statement, context: &PlanningContext) -> Plan {
    let logical = Planner::new().plan_statement(statement, context).unwrap();
    Optimizer::new()
        .optimize_with_context(logical, context)
        .unwrap()
}

#[test]
fn plans_select_without_index_as_seq_scan() {
    let planner = Planner::new();
    let statement = select_statement(
        vec![SelectItem::Wildcard],
        "users",
        Some(Expr::Compare {
            column: "id".to_string(),
            op: CompareOp::Eq,
            value: Value::Integer(7),
        }),
    );

    let plan = planner
        .plan_statement(&statement, &context_with_indexes(vec![]))
        .unwrap();

    assert_eq!(
        plan,
        Plan::SeqScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
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
fn plans_select_from_derived_source_with_alias_exposed_columns() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql(
        "SELECT bucket FROM (SELECT age + 1 AS bucket FROM users) t ORDER BY bucket ASC;",
    )
    .unwrap()
    .remove(0);

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::DerivedSource {
            source: Box::new(Plan::SeqScan {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Expr {
                    expr: ScalarExpr::Binary {
                        left: Box::new(ScalarExpr::Column("age".to_string())),
                        op: rustsql::sql::ast::ScalarBinaryOp::Add,
                        right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                    },
                    alias: Some("bucket".to_string()),
                }],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            alias: "t".to_string(),
            output_columns: vec!["bucket".to_string()],
            columns: vec![SelectItem::Column("bucket".to_string())],
            filter: None,
            order_by: vec![OrderBy {
                expr: OrderByExpr::Column("bucket".to_string()),
                descending: false,
                nulls: None,
            }],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn plans_derived_source_with_unqualified_output_for_qualified_inner_column() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql("SELECT age FROM (SELECT u.age FROM users u) t ORDER BY age ASC;")
        .unwrap()
        .remove(0);

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::DerivedSource {
            source: Box::new(Plan::SeqScan {
                table: "users".to_string(),
                table_alias: Some("u".to_string()),
                columns: vec![SelectItem::Column("age".to_string())],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            alias: "t".to_string(),
            output_columns: vec!["age".to_string()],
            columns: vec![SelectItem::Column("age".to_string())],
            filter: None,
            order_by: vec![OrderBy {
                expr: OrderByExpr::Column("age".to_string()),
                descending: false,
                nulls: None,
            }],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn plans_derived_source_with_wildcard_expanded_output_columns() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql("SELECT age FROM (SELECT * FROM users) t ORDER BY age ASC;")
        .unwrap()
        .remove(0);

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::DerivedSource {
            source: Box::new(Plan::SeqScan {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            alias: "t".to_string(),
            output_columns: vec!["id".to_string(), "age".to_string()],
            columns: vec![SelectItem::Column("age".to_string())],
            filter: None,
            order_by: vec![OrderBy {
                expr: OrderByExpr::Column("age".to_string()),
                descending: false,
                nulls: None,
            }],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn planner_rejects_reference_to_inner_source_column_from_derived_source() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql("SELECT age FROM (SELECT age + 1 AS bucket FROM users) t;")
        .unwrap()
        .remove(0);

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert_eq!(error.to_string(), "plan error: unknown column age");
}

#[test]
fn planner_rejects_unqualified_duplicate_output_from_joined_wildcard_derived_source() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([
            (
                "users".to_string(),
                Schema::new(
                    "users",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("age", ColumnType::Integer),
                    ],
                ),
            ),
            (
                "orders".to_string(),
                Schema::new(
                    "orders",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("user_id", ColumnType::Integer),
                    ],
                ),
            ),
        ]),
        HashMap::new(),
    );
    let statement =
        parse_sql("SELECT id FROM (SELECT * FROM users u JOIN orders o ON u.id = o.user_id) t;")
            .unwrap()
            .remove(0);

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert_eq!(
        error.to_string(),
        "plan error: ambiguous column reference: id"
    );
}

#[test]
fn planner_rejects_qualified_duplicate_output_from_joined_wildcard_derived_source() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([
            (
                "users".to_string(),
                Schema::new(
                    "users",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("age", ColumnType::Integer),
                    ],
                ),
            ),
            (
                "orders".to_string(),
                Schema::new(
                    "orders",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("user_id", ColumnType::Integer),
                    ],
                ),
            ),
        ]),
        HashMap::new(),
    );
    let statement =
        parse_sql("SELECT t.id FROM (SELECT * FROM users u JOIN orders o ON u.id = o.user_id) t;")
            .unwrap()
            .remove(0);

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert_eq!(
        error.to_string(),
        "plan error: ambiguous column reference: t.id"
    );
}

#[test]
fn plans_union_and_union_all_as_distinct_compound_plans() {
    let planner = Planner::new();
    let context = build_users_context();

    let union = parse_sql("SELECT id FROM users UNION SELECT id FROM users;")
        .unwrap()
        .remove(0);
    let union_all = parse_sql("SELECT id FROM users UNION ALL SELECT id FROM users;")
        .unwrap()
        .remove(0);

    let union_plan = planner.plan_statement(&union, &context).unwrap();
    let union_all_plan = planner.plan_statement(&union_all, &context).unwrap();

    let union_debug = format!("{union_plan:?}");
    let union_all_debug = format!("{union_all_plan:?}");

    assert!(
        union_debug.contains("Union"),
        "expected UNION plan debug output, got {union_debug}"
    );
    assert!(
        union_all_debug.contains("Union"),
        "expected UNION ALL plan debug output, got {union_all_debug}"
    );
    assert!(
        union_debug.contains("all: false"),
        "expected UNION plan to record duplicate elimination, got {union_debug}"
    );
    assert!(
        union_all_debug.contains("all: true"),
        "expected UNION ALL plan to preserve duplicates, got {union_all_debug}"
    );
    assert_ne!(
        union_debug, union_all_debug,
        "UNION and UNION ALL should not lower to identical plans"
    );
}

#[test]
fn planner_rejects_union_with_mismatched_column_counts() {
    let planner = Planner::new();
    let context = build_users_context();
    let statement = parse_sql("SELECT id FROM users UNION SELECT id, name FROM users;")
        .unwrap()
        .remove(0);

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert_eq!(
        error.to_string(),
        "plan error: UNION branches must return the same number of columns"
    );
}

#[test]
fn plans_derived_source_wrapping_union_with_left_branch_output_name() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql(
        "SELECT id FROM (SELECT id FROM users UNION SELECT age FROM users) t ORDER BY id ASC;",
    )
    .unwrap()
    .remove(0);

    let plan = planner.plan_statement(&statement, &context).unwrap();
    let debug = format!("{plan:?}");

    assert!(
        debug.contains("DerivedSource"),
        "expected derived source wrapper, got {debug}"
    );
    assert!(
        debug.contains("Union"),
        "expected compound child plan inside derived source, got {debug}"
    );
    assert!(
        debug.contains("output_columns: [\"id\"]"),
        "expected derived source to expose left branch column name, got {debug}"
    );
    assert!(
        debug.contains("columns: [Column(\"id\")]") || debug.contains("columns: [Column(\"id\")],"),
        "expected outer query to consume exposed compound column name, got {debug}"
    );
}

#[test]
fn plans_select_with_matching_eq_index_as_index_scan() {
    let statement = select_statement(
        vec![SelectItem::Column("name".to_string())],
        "users",
        Some(Expr::Compare {
            column: "id".to_string(),
            op: CompareOp::Eq,
            value: Value::Integer(7),
        }),
    );
    let indexes = vec![IndexMeta {
        name: "idx_users_id".to_string(),
        columns: vec!["id".to_string()],
        unique: false,
    }];

    let plan = optimized_plan(&statement, &context_with_indexes(indexes));

    assert_eq!(
        plan,
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
fn plans_and_equality_prefix_with_composite_index_as_index_scan() {
    let statement = select_statement(
        vec![SelectItem::Column("name".to_string())],
        "users",
        Some(Expr::And(
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
    );
    let indexes = vec![IndexMeta {
        name: "idx_users_id_name".to_string(),
        columns: vec!["id".to_string(), "name".to_string()],
        unique: false,
    }];

    let plan = optimized_plan(&statement, &context_with_indexes(indexes));

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("name".to_string())],
            index: "idx_users_id_name".to_string(),
            mode: IndexScanMode::Lookup,
            key_prefix: vec![Value::Integer(7), Value::from("alice")],
            range: None,
            filter: Some(Expr::And(
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
fn plans_eq_prefix_plus_range_predicate_as_index_scan_with_full_filter() {
    let indexes = vec![IndexMeta {
        name: "idx_users_id_name".to_string(),
        columns: vec!["id".to_string(), "name".to_string()],
        unique: false,
    }];
    let statement = select_statement(
        vec![SelectItem::Wildcard],
        "users",
        Some(Expr::And(
            Box::new(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(7),
            }),
            Box::new(Expr::Compare {
                column: "name".to_string(),
                op: CompareOp::Gt,
                value: Value::from("alice"),
            }),
        )),
    );

    let plan = optimized_plan(&statement, &context_with_indexes(indexes));

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
            filter: Some(Expr::And(
                Box::new(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(7),
                }),
                Box::new(Expr::Compare {
                    column: "name".to_string(),
                    op: CompareOp::Gt,
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
fn plans_eq_prefix_plus_two_sided_range_as_index_scan() {
    let indexes = vec![IndexMeta {
        name: "idx_users_id_name".to_string(),
        columns: vec!["id".to_string(), "name".to_string()],
        unique: false,
    }];
    let statement = select_statement(
        vec![SelectItem::Wildcard],
        "users",
        Some(Expr::And(
            Box::new(Expr::And(
                Box::new(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(7),
                }),
                Box::new(Expr::Compare {
                    column: "name".to_string(),
                    op: CompareOp::Gt,
                    value: Value::from("alice"),
                }),
            )),
            Box::new(Expr::Compare {
                column: "name".to_string(),
                op: CompareOp::Lt,
                value: Value::from("david"),
            }),
        )),
    );

    let plan = optimized_plan(&statement, &context_with_indexes(indexes));

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
                upper: Some(IndexBound {
                    op: CompareOp::Lt,
                    value: Value::from("david"),
                }),
            }),
            filter: Some(Expr::And(
                Box::new(Expr::And(
                    Box::new(Expr::Compare {
                        column: "id".to_string(),
                        op: CompareOp::Eq,
                        value: Value::Integer(7),
                    }),
                    Box::new(Expr::Compare {
                        column: "name".to_string(),
                        op: CompareOp::Gt,
                        value: Value::from("alice"),
                    }),
                )),
                Box::new(Expr::Compare {
                    column: "name".to_string(),
                    op: CompareOp::Lt,
                    value: Value::from("david"),
                }),
            )),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn plans_tightens_redundant_range_bounds_on_same_column() {
    let indexes = vec![IndexMeta {
        name: "idx_users_id_name".to_string(),
        columns: vec!["id".to_string(), "name".to_string()],
        unique: false,
    }];
    let statement = select_statement(
        vec![SelectItem::Wildcard],
        "users",
        Some(Expr::And(
            Box::new(Expr::And(
                Box::new(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(7),
                }),
                Box::new(Expr::Compare {
                    column: "name".to_string(),
                    op: CompareOp::Gt,
                    value: Value::from("alice"),
                }),
            )),
            Box::new(Expr::And(
                Box::new(Expr::Compare {
                    column: "name".to_string(),
                    op: CompareOp::Gt,
                    value: Value::from("bob"),
                }),
                Box::new(Expr::Compare {
                    column: "name".to_string(),
                    op: CompareOp::Lt,
                    value: Value::from("david"),
                }),
            )),
        )),
    );

    let plan = optimized_plan(&statement, &context_with_indexes(indexes));

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
                    value: Value::from("bob"),
                }),
                upper: Some(IndexBound {
                    op: CompareOp::Lt,
                    value: Value::from("david"),
                }),
            }),
            filter: Some(Expr::And(
                Box::new(Expr::And(
                    Box::new(Expr::Compare {
                        column: "id".to_string(),
                        op: CompareOp::Eq,
                        value: Value::Integer(7),
                    }),
                    Box::new(Expr::Compare {
                        column: "name".to_string(),
                        op: CompareOp::Gt,
                        value: Value::from("alice"),
                    }),
                )),
                Box::new(Expr::And(
                    Box::new(Expr::Compare {
                        column: "name".to_string(),
                        op: CompareOp::Gt,
                        value: Value::from("bob"),
                    }),
                    Box::new(Expr::Compare {
                        column: "name".to_string(),
                        op: CompareOp::Lt,
                        value: Value::from("david"),
                    }),
                )),
            )),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn plans_indexable_or_predicates_as_index_union() {
    let indexes = vec![
        IndexMeta {
            name: "idx_users_id".to_string(),
            columns: vec!["id".to_string()],
            unique: false,
        },
        IndexMeta {
            name: "idx_users_name".to_string(),
            columns: vec!["name".to_string()],
            unique: false,
        },
    ];
    let statement = select_statement(
        vec![SelectItem::Wildcard],
        "users",
        Some(Expr::Or(
            Box::new(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(1),
            }),
            Box::new(Expr::Compare {
                column: "name".to_string(),
                op: CompareOp::Eq,
                value: Value::from("alice"),
            }),
        )),
    );

    let plan = optimized_plan(&statement, &context_with_indexes(indexes));

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
                    key_prefix: vec![Value::Integer(1)],
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
                    value: Value::Integer(1),
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
fn plans_or_predicates_as_seq_scan_when_any_branch_is_not_indexable() {
    let planner = Planner::new();
    let indexes = vec![IndexMeta {
        name: "idx_users_id".to_string(),
        columns: vec!["id".to_string()],
        unique: false,
    }];
    let statement = select_statement(
        vec![SelectItem::Wildcard],
        "users",
        Some(Expr::Or(
            Box::new(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(1),
            }),
            Box::new(Expr::Compare {
                column: "name".to_string(),
                op: CompareOp::Eq,
                value: Value::from("alice"),
            }),
        )),
    );

    let plan = planner
        .plan_statement(&statement, &context_with_indexes(indexes))
        .unwrap();

    assert_eq!(
        plan,
        Plan::SeqScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            filter: Some(Expr::Or(
                Box::new(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(1),
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
fn plans_range_predicates_as_index_scan_when_leading_index_column_matches() {
    let indexes = vec![IndexMeta {
        name: "idx_users_id".to_string(),
        columns: vec!["id".to_string()],
        unique: false,
    }];
    let context = context_with_indexes(indexes);

    let gt_plan = optimized_plan(
        &select_statement(
            vec![SelectItem::Wildcard],
            "users",
            Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(1),
            }),
        ),
        &context,
    );
    let lt_plan = optimized_plan(
        &select_statement(
            vec![SelectItem::Wildcard],
            "users",
            Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Lt,
                value: Value::Integer(9),
            }),
        ),
        &context,
    );

    assert_eq!(
        gt_plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            index: "idx_users_id".to_string(),
            mode: IndexScanMode::Range,
            key_prefix: vec![],
            range: Some(IndexRange {
                column: "id".to_string(),
                lower: Some(IndexBound {
                    op: CompareOp::Gt,
                    value: Value::Integer(1),
                }),
                upper: None,
            }),
            filter: Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(1),
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
    assert_eq!(
        lt_plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            index: "idx_users_id".to_string(),
            mode: IndexScanMode::Range,
            key_prefix: vec![],
            range: Some(IndexRange {
                column: "id".to_string(),
                lower: None,
                upper: Some(IndexBound {
                    op: CompareOp::Lt,
                    value: Value::Integer(9),
                }),
            }),
            filter: Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Lt,
                value: Value::Integer(9),
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn plans_inclusive_range_predicates_as_index_scan() {
    let indexes = vec![IndexMeta {
        name: "idx_users_id".to_string(),
        columns: vec!["id".to_string()],
        unique: false,
    }];
    let statement = select_statement(
        vec![SelectItem::Wildcard],
        "users",
        Some(Expr::And(
            Box::new(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Gte,
                value: Value::Integer(1),
            }),
            Box::new(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Lte,
                value: Value::Integer(9),
            }),
        )),
    );

    let plan = optimized_plan(&statement, &context_with_indexes(indexes));

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            index: "idx_users_id".to_string(),
            mode: IndexScanMode::Range,
            key_prefix: vec![],
            range: Some(IndexRange {
                column: "id".to_string(),
                lower: Some(IndexBound {
                    op: CompareOp::Gte,
                    value: Value::Integer(1),
                }),
                upper: Some(IndexBound {
                    op: CompareOp::Lte,
                    value: Value::Integer(9),
                }),
            }),
            filter: Some(Expr::And(
                Box::new(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Gte,
                    value: Value::Integer(1),
                }),
                Box::new(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Lte,
                    value: Value::Integer(9),
                }),
            )),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn plans_create_insert_and_txn_statements() {
    let planner = Planner::new();
    let context = context_with_indexes(vec![]);

    let create_table = planner
        .plan_statement(
            &Statement::CreateTable {
                name: "users".to_string(),
                columns: user_schema().columns,
                constraints: vec![],
            },
            &context,
        )
        .unwrap();
    let create_index = planner
        .plan_statement(
            &Statement::CreateIndex {
                name: "idx_users_id".to_string(),
                table: "users".to_string(),
                columns: vec!["id".to_string()],
                unique: false,
            },
            &context,
        )
        .unwrap();
    let insert = planner
        .plan_statement(
            &Statement::Insert {
                table: "users".to_string(),
                columns: None,
                values: vec![Value::Integer(1), Value::Text("alice".to_string())],
            },
            &context,
        )
        .unwrap();
    let begin = planner
        .plan_statement(
            &Statement::Begin {
                isolation_level: None,
            },
            &context,
        )
        .unwrap();
    let commit = planner
        .plan_statement(&Statement::Commit, &context)
        .unwrap();
    let rollback = planner
        .plan_statement(&Statement::Rollback, &context)
        .unwrap();

    assert_eq!(
        create_table,
        Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text),
            ],
            constraints: vec![],
        }
    );
    assert_eq!(
        create_index,
        Plan::CreateIndex {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string()],
            unique: false,
        }
    );
    assert_eq!(
        insert,
        Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }
    );
    assert_eq!(
        begin,
        Plan::BeginTxn {
            isolation_level: rustsql::sql::ast::IsolationLevel::ReadCommitted,
        }
    );
    assert_eq!(commit, Plan::CommitTxn);
    assert_eq!(rollback, Plan::RollbackTxn);
}

#[test]
fn plans_create_table_with_defaults_checks_and_foreign_keys() {
    let planner = Planner::new();
    let context = PlanningContext::new(HashMap::new(), HashMap::new());
    let statement = Statement::CreateTable {
        name: "users".to_string(),
        columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
        constraints: vec![TableConstraint::Check(CheckConstraint::compare(
            "users_id_positive",
            "id",
            CheckOp::Gt,
            Value::Integer(0),
        ))],
    };

    assert!(matches!(
        planner.plan_statement(&statement, &context).unwrap(),
        Plan::CreateTable { constraints, .. } if constraints.len() == 1
    ));
}

#[test]
fn planner_plans_alter_table_variants() {
    let planner = Planner::new();
    let mut schemas = HashMap::from([("users".to_string(), user_schema())]);
    schemas.insert(
        "orders".to_string(),
        Schema::new(
            "orders",
            vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("user_id", ColumnType::Integer),
            ],
        ),
    );
    let context = PlanningContext::new(schemas, HashMap::new());

    let add_column = planner
        .plan_statement(
            &Statement::AlterTable {
                table: "users".to_string(),
                action: AlterTableAction::AddColumn(ColumnDef::new("age", ColumnType::Integer)),
            },
            &context,
        )
        .unwrap();
    assert_eq!(
        add_column,
        Plan::AlterTable {
            table: "users".to_string(),
            action: AlterTableAction::AddColumn(ColumnDef::new("age", ColumnType::Integer)),
        }
    );

    let rename_table = planner
        .plan_statement(
            &Statement::AlterTable {
                table: "users".to_string(),
                action: AlterTableAction::RenameTable {
                    new_name: "customers".to_string(),
                },
            },
            &context,
        )
        .unwrap();
    assert_eq!(
        rename_table,
        Plan::AlterTable {
            table: "users".to_string(),
            action: AlterTableAction::RenameTable {
                new_name: "customers".to_string(),
            },
        }
    );

    let rename_column = planner
        .plan_statement(
            &Statement::AlterTable {
                table: "users".to_string(),
                action: AlterTableAction::RenameColumn {
                    old_name: "name".to_string(),
                    new_name: "full_name".to_string(),
                },
            },
            &context,
        )
        .unwrap();
    assert_eq!(
        rename_column,
        Plan::AlterTable {
            table: "users".to_string(),
            action: AlterTableAction::RenameColumn {
                old_name: "name".to_string(),
                new_name: "full_name".to_string(),
            },
        }
    );

    let duplicate_column = planner
        .plan_statement(
            &Statement::AlterTable {
                table: "users".to_string(),
                action: AlterTableAction::AddColumn(ColumnDef::new("name", ColumnType::Text)),
            },
            &context,
        )
        .unwrap_err();
    assert!(
        duplicate_column
            .to_string()
            .contains("column already exists")
    );

    let missing_old_column = planner
        .plan_statement(
            &Statement::AlterTable {
                table: "users".to_string(),
                action: AlterTableAction::RenameColumn {
                    old_name: "missing".to_string(),
                    new_name: "full_name".to_string(),
                },
            },
            &context,
        )
        .unwrap_err();
    assert!(missing_old_column.to_string().contains("unknown column"));

    let duplicate_new_column = planner
        .plan_statement(
            &Statement::AlterTable {
                table: "users".to_string(),
                action: AlterTableAction::RenameColumn {
                    old_name: "name".to_string(),
                    new_name: "id".to_string(),
                },
            },
            &context,
        )
        .unwrap_err();
    assert!(
        duplicate_new_column
            .to_string()
            .contains("column already exists")
    );

    let duplicate_table = planner
        .plan_statement(
            &Statement::AlterTable {
                table: "users".to_string(),
                action: AlterTableAction::RenameTable {
                    new_name: "orders".to_string(),
                },
            },
            &context,
        )
        .unwrap_err();
    assert!(duplicate_table.to_string().contains("table already exists"));
}

#[test]
fn plans_multi_column_create_index_statement() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                    ColumnDef::new("email", ColumnType::Text),
                ],
            ),
        )]),
        HashMap::new(),
    );

    let plan = planner
        .plan_statement(
            &Statement::CreateIndex {
                name: "idx_users_name_email".to_string(),
                table: "users".to_string(),
                columns: vec!["name".to_string(), "email".to_string()],
                unique: false,
            },
            &context,
        )
        .unwrap();

    assert_eq!(
        plan,
        Plan::CreateIndex {
            name: "idx_users_name_email".to_string(),
            table: "users".to_string(),
            columns: vec!["name".to_string(), "email".to_string()],
            unique: false,
        }
    );
}

#[test]
fn plans_create_unique_index_statement() {
    let planner = Planner::new();
    let context = build_users_context();

    let plan = planner
        .plan_statement(
            &Statement::CreateIndex {
                name: "idx_users_name".to_string(),
                table: "users".to_string(),
                columns: vec!["name".to_string()],
                unique: true,
            },
            &context,
        )
        .unwrap();

    assert_eq!(
        plan,
        Plan::CreateIndex {
            name: "idx_users_name".to_string(),
            table: "users".to_string(),
            columns: vec!["name".to_string()],
            unique: true,
        }
    );
}

#[test]
fn planner_rejects_duplicate_columns_in_create_index() {
    let planner = Planner::new();
    let context = build_users_context();
    let error = planner
        .plan_statement(
            &Statement::CreateIndex {
                name: "idx_users_bad".to_string(),
                table: "users".to_string(),
                columns: vec!["name".to_string(), "name".to_string()],
                unique: false,
            },
            &context,
        )
        .unwrap_err();

    assert!(error.to_string().contains("duplicate index column name"));
}

#[test]
fn plans_group_by_aggregate_as_aggregate_plan() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                    ColumnDef::new("active", ColumnType::Boolean),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = Statement::Select(SelectStatement {
        with: None,
        columns: vec![
            SelectItem::Column("active".to_string()),
            SelectItem::Aggregate {
                func: AggregateFunc::Count,
                arg: AggregateArg::Wildcard,
                alias: Some("total".to_string()),
            },
        ],
        from: FromItem::Table {
            name: "users".to_string(),
            alias: None,
        },
        joins: vec![],
        filter: None,
        group_by: vec![ScalarExpr::Column("active".to_string())],
        compounds: vec![],
        order_by: vec![OrderBy {
            expr: OrderByExpr::Column("total".to_string()),
            descending: true,
            nulls: None,
        }],
        limit: Some(2),
        distinct: false,
        having: None,
    });

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::Aggregate {
            source: Box::new(Plan::SeqScan {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            columns: vec![
                SelectItem::Column("active".to_string()),
                SelectItem::Aggregate {
                    func: AggregateFunc::Count,
                    arg: AggregateArg::Wildcard,
                    alias: Some("total".to_string()),
                },
            ],
            group_by: vec![ScalarExpr::Column("active".to_string())],
            order_by: vec![OrderBy {
                expr: OrderByExpr::Column("total".to_string()),
                descending: true,
                nulls: None,
            }],
            limit: Some(2),
            having: None,
        }
    );
}

#[test]
fn plans_aggregate_scalar_expression_arguments() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = Statement::Select(SelectStatement {
        with: None,
        columns: vec![SelectItem::Aggregate {
            func: AggregateFunc::Sum,
            arg: AggregateArg::Expr {
                expr: ScalarExpr::Binary {
                    left: Box::new(ScalarExpr::Column("age".to_string())),
                    op: rustsql::sql::ast::ScalarBinaryOp::Add,
                    right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                },
                distinct: false,
            },
            alias: Some("total".to_string()),
        }],
        from: FromItem::Table {
            name: "users".to_string(),
            alias: None,
        },
        joins: vec![],
        filter: None,
        group_by: vec![],
        compounds: vec![],
        order_by: vec![],
        limit: None,
        distinct: false,
        having: None,
    });

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::Aggregate {
            source: Box::new(Plan::SeqScan {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            columns: vec![SelectItem::Aggregate {
                func: AggregateFunc::Sum,
                arg: AggregateArg::Expr {
                    expr: ScalarExpr::Binary {
                        left: Box::new(ScalarExpr::Column("age".to_string())),
                        op: rustsql::sql::ast::ScalarBinaryOp::Add,
                        right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                    },
                    distinct: false,
                },
                alias: Some("total".to_string()),
            }],
            group_by: vec![],
            order_by: vec![],
            limit: None,
            having: None,
        }
    );
}

#[test]
fn planner_rejects_having_reference_to_ungrouped_source_column() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = Statement::Select(SelectStatement {
        with: None,
        columns: vec![SelectItem::Aggregate {
            func: AggregateFunc::Count,
            arg: AggregateArg::Wildcard,
            alias: Some("total".to_string()),
        }],
        from: FromItem::Table {
            name: "users".to_string(),
            alias: None,
        },
        joins: vec![],
        filter: None,
        group_by: vec![],
        compounds: vec![],
        order_by: vec![],
        limit: None,
        distinct: false,
        having: Some(Expr::Compare {
            column: "age".to_string(),
            op: CompareOp::Gt,
            value: Value::Integer(20),
        }),
    });

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert!(
        error.to_string().contains("age"),
        "unexpected error: {error}"
    );
}

#[test]
fn planner_rejects_order_by_ungrouped_source_column_in_grouped_projection() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = Statement::Select(SelectStatement {
        with: None,
        columns: vec![SelectItem::Column("age".to_string())],
        from: FromItem::Table {
            name: "users".to_string(),
            alias: None,
        },
        joins: vec![],
        filter: None,
        group_by: vec![ScalarExpr::Column("age".to_string())],
        compounds: vec![],
        order_by: vec![OrderBy {
            expr: OrderByExpr::Column("id".to_string()),
            descending: true,
            nulls: None,
        }],
        limit: None,
        distinct: false,
        having: None,
    });

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert!(
        error.to_string().contains("id"),
        "unexpected error: {error}"
    );
}

#[test]
fn planner_rejects_sum_non_integer_scalar_expression_argument() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = Statement::Select(SelectStatement {
        with: None,
        columns: vec![SelectItem::Aggregate {
            func: AggregateFunc::Sum,
            arg: AggregateArg::Expr {
                expr: ScalarExpr::Binary {
                    left: Box::new(ScalarExpr::Column("name".to_string())),
                    op: rustsql::sql::ast::ScalarBinaryOp::Concat,
                    right: Box::new(ScalarExpr::Literal(Value::Text("x".to_string()))),
                },
                distinct: false,
            },
            alias: Some("total".to_string()),
        }],
        from: FromItem::Table {
            name: "users".to_string(),
            alias: None,
        },
        joins: vec![],
        filter: None,
        group_by: vec![],
        compounds: vec![],
        order_by: vec![],
        limit: None,
        distinct: false,
        having: None,
    });

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert_eq!(
        error.to_string(),
        "plan error: SUM only supports INTEGER columns"
    );
}

#[test]
fn plans_join_query_as_nested_loop_join() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([
            ("users".to_string(), user_schema()),
            (
                "orders".to_string(),
                Schema::new(
                    "orders",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("user_id", ColumnType::Integer),
                        ColumnDef::new("amount", ColumnType::Integer),
                    ],
                ),
            ),
        ]),
        HashMap::new(),
    );
    let statement = Statement::Select(SelectStatement {
        with: None,
        columns: vec![
            SelectItem::Column("u.name".to_string()),
            SelectItem::Column("o.amount".to_string()),
        ],
        from: FromItem::Table {
            name: "users".to_string(),
            alias: Some("u".to_string()),
        },
        joins: vec![JoinClause {
            source: FromItem::Table {
                name: "orders".to_string(),
                alias: Some("o".to_string()),
            },
            on: Expr::CompareColumns {
                left: "u.id".to_string(),
                op: CompareOp::Eq,
                right: "o.user_id".to_string(),
            },
            kind: JoinKind::Inner,
        }],
        filter: Some(Expr::Compare {
            column: "o.amount".to_string(),
            op: CompareOp::Gt,
            value: Value::Integer(10),
        }),
        group_by: vec![],
        compounds: vec![],
        order_by: vec![OrderBy {
            expr: OrderByExpr::Column("u.name".to_string()),
            descending: false,
            nulls: None,
        }],
        limit: Some(5),
        distinct: false,
        having: None,
    });

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::NestedLoopJoin {
            source: Box::new(Plan::SeqScan {
                table: "users".to_string(),
                table_alias: Some("u".to_string()),
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            joins: vec![JoinPlan {
                source: Box::new(Plan::SeqScan {
                    table: "orders".to_string(),
                    table_alias: Some("o".to_string()),
                    columns: vec![SelectItem::Wildcard],
                    filter: None,
                    order_by: vec![],
                    limit: None,
                    distinct: false,
                }),
                on: Expr::CompareColumns {
                    left: "u.id".to_string(),
                    op: CompareOp::Eq,
                    right: "o.user_id".to_string(),
                },
                kind: JoinKind::Inner,
            }],
            columns: vec![
                SelectItem::Column("u.name".to_string()),
                SelectItem::Column("o.amount".to_string()),
            ],
            filter: Some(Expr::Compare {
                column: "o.amount".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(10),
            }),
            order_by: vec![OrderBy {
                expr: OrderByExpr::Column("u.name".to_string()),
                descending: false,
                nulls: None,
            }],
            limit: Some(5),
            distinct: false,
        }
    );
}

#[test]
fn plans_join_with_derived_source_on_right_as_nested_loop_join() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([
            (
                "users".to_string(),
                Schema::new(
                    "users",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("name", ColumnType::Text),
                        ColumnDef::new("age", ColumnType::Integer),
                    ],
                ),
            ),
            (
                "orders".to_string(),
                Schema::new(
                    "orders",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("user_id", ColumnType::Integer),
                        ColumnDef::new("amount", ColumnType::Integer),
                    ],
                ),
            ),
        ]),
        HashMap::new(),
    );
    let statement = parse_sql(
        "SELECT u.name, t.bucket
         FROM users u
         JOIN (SELECT id, age + 1 AS bucket FROM users) t ON u.id = t.id
         ORDER BY u.name ASC;",
    )
    .unwrap()
    .remove(0);

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::NestedLoopJoin {
            source: Box::new(Plan::SeqScan {
                table: "users".to_string(),
                table_alias: Some("u".to_string()),
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            joins: vec![JoinPlan {
                source: Box::new(Plan::DerivedSource {
                    source: Box::new(Plan::SeqScan {
                        table: "users".to_string(),
                        table_alias: None,
                        columns: vec![
                            SelectItem::Column("id".to_string()),
                            SelectItem::Expr {
                                expr: ScalarExpr::Binary {
                                    left: Box::new(ScalarExpr::Column("age".to_string())),
                                    op: rustsql::sql::ast::ScalarBinaryOp::Add,
                                    right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                                },
                                alias: Some("bucket".to_string()),
                            },
                        ],
                        filter: None,
                        order_by: vec![],
                        limit: None,
                        distinct: false,
                    }),
                    alias: "t".to_string(),
                    output_columns: vec!["id".to_string(), "bucket".to_string()],
                    columns: vec![SelectItem::Wildcard],
                    filter: None,
                    order_by: vec![],
                    limit: None,
                    distinct: false,
                }),
                on: Expr::CompareScalar {
                    left: rustsql::sql::ast::ScalarExpr::Column("u.id".to_string()),
                    op: CompareOp::Eq,
                    right: rustsql::sql::ast::ScalarExpr::Column("t.id".to_string()),
                },
                kind: JoinKind::Inner,
            }],
            columns: vec![
                SelectItem::Column("u.name".to_string()),
                SelectItem::Column("t.bucket".to_string()),
            ],
            filter: None,
            order_by: vec![OrderBy {
                expr: OrderByExpr::Column("u.name".to_string()),
                descending: false,
                nulls: None,
            }],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn planner_rejects_reference_to_unexposed_inner_column_from_joined_derived_source() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql(
        "SELECT u.name
         FROM users u
         JOIN (SELECT id, age + 1 AS bucket FROM users) t ON u.id = t.id
         WHERE t.age > 20;",
    )
    .unwrap()
    .remove(0);

    let error = planner.plan_statement(&statement, &context).unwrap_err();
    assert_eq!(error.to_string(), "plan error: unknown column t.age");
}

#[test]
fn plans_aggregate_query_over_derived_source() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql(
        "SELECT bucket, COUNT(*) AS total
         FROM (SELECT age + 1 AS bucket FROM users) t
         GROUP BY bucket
         HAVING bucket > 20
         ORDER BY total DESC;",
    )
    .unwrap()
    .remove(0);

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::Aggregate {
            source: Box::new(Plan::DerivedSource {
                source: Box::new(Plan::SeqScan {
                    table: "users".to_string(),
                    table_alias: None,
                    columns: vec![SelectItem::Expr {
                        expr: ScalarExpr::Binary {
                            left: Box::new(ScalarExpr::Column("age".to_string())),
                            op: rustsql::sql::ast::ScalarBinaryOp::Add,
                            right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                        },
                        alias: Some("bucket".to_string()),
                    }],
                    filter: None,
                    order_by: vec![],
                    limit: None,
                    distinct: false,
                }),
                alias: "t".to_string(),
                output_columns: vec!["bucket".to_string()],
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            columns: vec![
                SelectItem::Column("bucket".to_string()),
                SelectItem::Aggregate {
                    func: AggregateFunc::Count,
                    arg: AggregateArg::Wildcard,
                    alias: Some("total".to_string()),
                },
            ],
            group_by: vec![ScalarExpr::Column("bucket".to_string())],
            having: Some(Expr::Compare {
                column: "bucket".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(20),
            }),
            order_by: vec![OrderBy {
                expr: OrderByExpr::Column("total".to_string()),
                descending: true,
                nulls: None,
            }],
            limit: None,
        }
    );
}

#[test]
fn planner_lowers_single_cte_source_to_aggregate_over_derived_source() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql(
        "WITH buckets AS (SELECT age + 1 AS bucket FROM users)
         SELECT bucket, COUNT(*) AS total
         FROM buckets
         GROUP BY bucket
         HAVING bucket > 20
         ORDER BY total DESC;",
    )
    .unwrap()
    .remove(0);

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::Aggregate {
            source: Box::new(Plan::DerivedSource {
                source: Box::new(Plan::SeqScan {
                    table: "users".to_string(),
                    table_alias: None,
                    columns: vec![SelectItem::Expr {
                        expr: ScalarExpr::Binary {
                            left: Box::new(ScalarExpr::Column("age".to_string())),
                            op: rustsql::sql::ast::ScalarBinaryOp::Add,
                            right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                        },
                        alias: Some("bucket".to_string()),
                    }],
                    filter: None,
                    order_by: vec![],
                    limit: None,
                    distinct: false,
                }),
                alias: "buckets".to_string(),
                output_columns: vec!["bucket".to_string()],
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            columns: vec![
                SelectItem::Column("bucket".to_string()),
                SelectItem::Aggregate {
                    func: AggregateFunc::Count,
                    arg: AggregateArg::Wildcard,
                    alias: Some("total".to_string()),
                },
            ],
            group_by: vec![ScalarExpr::Column("bucket".to_string())],
            having: Some(Expr::Compare {
                column: "bucket".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(20),
            }),
            order_by: vec![OrderBy {
                expr: OrderByExpr::Column("total".to_string()),
                descending: true,
                nulls: None,
            }],
            limit: None,
        }
    );
}

#[test]
fn planner_lowers_chained_cte_references_to_nested_derived_sources() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql(
        "WITH adults AS (SELECT id, name FROM users WHERE age >= 18),
              named AS (SELECT id FROM adults WHERE name IS NOT NULL)
         SELECT id FROM named ORDER BY id ASC;",
    )
    .unwrap()
    .remove(0);

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::DerivedSource {
            source: Box::new(Plan::DerivedSource {
                source: Box::new(Plan::SeqScan {
                    table: "users".to_string(),
                    table_alias: None,
                    columns: vec![
                        SelectItem::Column("id".to_string()),
                        SelectItem::Column("name".to_string()),
                    ],
                    filter: Some(Expr::Compare {
                        column: "age".to_string(),
                        op: CompareOp::Gte,
                        value: Value::Integer(18),
                    }),
                    order_by: vec![],
                    limit: None,
                    distinct: false,
                }),
                alias: "adults".to_string(),
                output_columns: vec!["id".to_string(), "name".to_string()],
                columns: vec![SelectItem::Column("id".to_string())],
                filter: Some(Expr::IsNull {
                    column: "name".to_string(),
                    negated: true,
                }),
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            alias: "named".to_string(),
            output_columns: vec!["id".to_string()],
            columns: vec![SelectItem::Column("id".to_string())],
            filter: None,
            order_by: vec![OrderBy {
                expr: OrderByExpr::Column("id".to_string()),
                descending: false,
                nulls: None,
            }],
            limit: None,
            distinct: false,
        }
    );
}

#[test]
fn planner_rejects_duplicate_cte_names() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql(
        "WITH dup AS (SELECT id FROM users),
              dup AS (SELECT age FROM users)
         SELECT id FROM dup;",
    )
    .unwrap()
    .remove(0);

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert_eq!(error.to_string(), "plan error: duplicate CTE name: dup");
}

#[test]
fn plans_aggregate_query_over_joined_derived_source() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = parse_sql(
        "SELECT t.bucket, COUNT(*) AS total
         FROM users u
         JOIN (SELECT id, age + 1 AS bucket FROM users) t ON u.id = t.id
         GROUP BY t.bucket
         HAVING t.bucket > 20
         ORDER BY t.bucket ASC;",
    )
    .unwrap()
    .remove(0);

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::Aggregate {
            source: Box::new(Plan::NestedLoopJoin {
                source: Box::new(Plan::SeqScan {
                    table: "users".to_string(),
                    table_alias: Some("u".to_string()),
                    columns: vec![SelectItem::Wildcard],
                    filter: None,
                    order_by: vec![],
                    limit: None,
                    distinct: false,
                }),
                joins: vec![JoinPlan {
                    source: Box::new(Plan::DerivedSource {
                        source: Box::new(Plan::SeqScan {
                            table: "users".to_string(),
                            table_alias: None,
                            columns: vec![
                                SelectItem::Column("id".to_string()),
                                SelectItem::Expr {
                                    expr: ScalarExpr::Binary {
                                        left: Box::new(ScalarExpr::Column("age".to_string())),
                                        op: rustsql::sql::ast::ScalarBinaryOp::Add,
                                        right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                                    },
                                    alias: Some("bucket".to_string()),
                                },
                            ],
                            filter: None,
                            order_by: vec![],
                            limit: None,
                            distinct: false,
                        }),
                        alias: "t".to_string(),
                        output_columns: vec!["id".to_string(), "bucket".to_string()],
                        columns: vec![SelectItem::Wildcard],
                        filter: None,
                        order_by: vec![],
                        limit: None,
                        distinct: false,
                    }),
                    on: Expr::CompareScalar {
                        left: ScalarExpr::Column("u.id".to_string()),
                        op: CompareOp::Eq,
                        right: ScalarExpr::Column("t.id".to_string()),
                    },
                    kind: JoinKind::Inner,
                }],
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            columns: vec![
                SelectItem::Column("t.bucket".to_string()),
                SelectItem::Aggregate {
                    func: AggregateFunc::Count,
                    arg: AggregateArg::Wildcard,
                    alias: Some("total".to_string()),
                },
            ],
            group_by: vec![ScalarExpr::Column("t.bucket".to_string())],
            having: Some(Expr::Compare {
                column: "t.bucket".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(20),
            }),
            order_by: vec![OrderBy {
                expr: OrderByExpr::Column("t.bucket".to_string()),
                descending: false,
                nulls: None,
            }],
            limit: None,
        }
    );
}

#[test]
fn planner_rejects_aggregate_reference_to_ambiguous_joined_derived_output() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([
            (
                "users".to_string(),
                Schema::new(
                    "users",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("age", ColumnType::Integer),
                    ],
                ),
            ),
            (
                "orders".to_string(),
                Schema::new(
                    "orders",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("user_id", ColumnType::Integer),
                    ],
                ),
            ),
        ]),
        HashMap::new(),
    );
    let statement = parse_sql(
        "SELECT id, COUNT(*) AS total
         FROM (SELECT * FROM users u JOIN orders o ON u.id = o.user_id) t
         GROUP BY id;",
    )
    .unwrap()
    .remove(0);

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert_eq!(
        error.to_string(),
        "plan error: ambiguous column reference: id"
    );
}

#[test]
fn plans_aggregate_query_with_scalar_group_by_and_order_by_expression() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([(
            "users".to_string(),
            Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("age", ColumnType::Integer),
                ],
            ),
        )]),
        HashMap::new(),
    );
    let statement = Statement::Select(SelectStatement {
        with: None,
        columns: vec![
            SelectItem::Expr {
                expr: ScalarExpr::Binary {
                    left: Box::new(ScalarExpr::Column("age".to_string())),
                    op: rustsql::sql::ast::ScalarBinaryOp::Add,
                    right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                },
                alias: Some("bucket".to_string()),
            },
            SelectItem::Aggregate {
                func: AggregateFunc::Count,
                arg: AggregateArg::Wildcard,
                alias: Some("total".to_string()),
            },
        ],
        from: FromItem::Table {
            name: "users".to_string(),
            alias: None,
        },
        joins: vec![],
        filter: None,
        group_by: vec![ScalarExpr::Binary {
            left: Box::new(ScalarExpr::Column("age".to_string())),
            op: rustsql::sql::ast::ScalarBinaryOp::Add,
            right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
        }],
        compounds: vec![],
        order_by: vec![OrderBy {
            expr: OrderByExpr::Expr(ScalarExpr::Binary {
                left: Box::new(ScalarExpr::Column("total".to_string())),
                op: rustsql::sql::ast::ScalarBinaryOp::Add,
                right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
            }),
            descending: true,
            nulls: None,
        }],
        limit: None,
        distinct: false,
        having: Some(Expr::Compare {
            column: "bucket".to_string(),
            op: CompareOp::Gt,
            value: Value::Integer(20),
        }),
    });

    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::Aggregate {
            source: Box::new(Plan::SeqScan {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                distinct: false,
            }),
            columns: vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Binary {
                        left: Box::new(ScalarExpr::Column("age".to_string())),
                        op: rustsql::sql::ast::ScalarBinaryOp::Add,
                        right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                    },
                    alias: Some("bucket".to_string()),
                },
                SelectItem::Aggregate {
                    func: AggregateFunc::Count,
                    arg: AggregateArg::Wildcard,
                    alias: Some("total".to_string()),
                },
            ],
            group_by: vec![ScalarExpr::Binary {
                left: Box::new(ScalarExpr::Column("age".to_string())),
                op: rustsql::sql::ast::ScalarBinaryOp::Add,
                right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
            }],
            having: Some(Expr::Compare {
                column: "bucket".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(20),
            }),
            order_by: vec![OrderBy {
                expr: OrderByExpr::Expr(ScalarExpr::Binary {
                    left: Box::new(ScalarExpr::Column("total".to_string())),
                    op: rustsql::sql::ast::ScalarBinaryOp::Add,
                    right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                }),
                descending: true,
                nulls: None,
            }],
            limit: None,
        }
    );
}

#[test]
fn planner_rejects_unknown_qualified_column_in_correlated_subquery() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([
            ("users".to_string(), user_schema()),
            (
                "orders".to_string(),
                Schema::new(
                    "orders",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("user_id", ColumnType::Integer),
                    ],
                ),
            ),
        ]),
        HashMap::new(),
    );
    let statement = Statement::Select(SelectStatement {
        with: None,
        columns: vec![SelectItem::Column("u.name".to_string())],
        from: FromItem::Table {
            name: "users".to_string(),
            alias: Some("u".to_string()),
        },
        joins: vec![],
        filter: Some(Expr::ExistsSubquery {
            query: Box::new(SelectStatement {
                with: None,
                columns: vec![SelectItem::Column("id".to_string())],
                from: FromItem::Table {
                    name: "orders".to_string(),
                    alias: Some("o".to_string()),
                },
                joins: vec![],
                filter: Some(Expr::CompareColumns {
                    left: "o.user_id".to_string(),
                    op: CompareOp::Eq,
                    right: "x.id".to_string(),
                }),
                group_by: vec![],
                compounds: vec![],
                order_by: vec![],
                limit: None,
                distinct: false,
                having: None,
            }),
            negated: false,
        }),
        group_by: vec![],
        compounds: vec![],
        order_by: vec![],
        limit: None,
        distinct: false,
        having: None,
    });

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert_eq!(error.to_string(), "plan error: unknown column x.id");
}

#[test]
fn planner_rejects_join_condition_reference_to_future_join_alias() {
    let planner = Planner::new();
    let context = PlanningContext::new(
        HashMap::from([
            ("users".to_string(), user_schema()),
            (
                "orders".to_string(),
                Schema::new(
                    "orders",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("user_id", ColumnType::Integer),
                    ],
                ),
            ),
            (
                "payments".to_string(),
                Schema::new(
                    "payments",
                    vec![
                        ColumnDef::primary_key("id", ColumnType::Integer),
                        ColumnDef::new("order_id", ColumnType::Integer),
                    ],
                ),
            ),
        ]),
        HashMap::new(),
    );
    let statement = Statement::Select(SelectStatement {
        with: None,
        columns: vec![SelectItem::Column("u.name".to_string())],
        from: FromItem::Table {
            name: "users".to_string(),
            alias: Some("u".to_string()),
        },
        joins: vec![
            JoinClause {
                source: FromItem::Table {
                    name: "orders".to_string(),
                    alias: Some("o".to_string()),
                },
                on: Expr::CompareScalar {
                    left: rustsql::sql::ast::ScalarExpr::Column("u.id".to_string()),
                    op: CompareOp::Eq,
                    right: rustsql::sql::ast::ScalarExpr::Column("p.order_id".to_string()),
                },
                kind: JoinKind::Inner,
            },
            JoinClause {
                source: FromItem::Table {
                    name: "payments".to_string(),
                    alias: Some("p".to_string()),
                },
                on: Expr::CompareScalar {
                    left: rustsql::sql::ast::ScalarExpr::Column("o.id".to_string()),
                    op: CompareOp::Eq,
                    right: rustsql::sql::ast::ScalarExpr::Column("p.order_id".to_string()),
                },
                kind: JoinKind::Inner,
            },
        ],
        filter: None,
        group_by: vec![],
        compounds: vec![],
        order_by: vec![],
        limit: None,
        distinct: false,
        having: None,
    });

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert_eq!(error.to_string(), "plan error: unknown column p.order_id");
}
