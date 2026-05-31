use std::collections::HashMap;

use rustsql::common::types::{
    CheckConstraint, CheckOp, ColumnDef, ColumnType, IndexMeta, Schema, Value,
};
use rustsql::sql::ast::{
    AggregateArg, AggregateFunc, AlterTableAction, CompareOp, Expr, JoinClause, JoinKind, OrderBy,
    OrderByExpr, SelectItem, SelectStatement, Statement, TableConstraint,
};
use rustsql::sql::optimizer::Optimizer;
use rustsql::sql::plan::{IndexBound, IndexRange, IndexScanMode, IndexScanSpec, JoinPlan, Plan};
use rustsql::sql::planner::{Planner, PlanningContext};

fn select_statement(columns: Vec<SelectItem>, table: &str, filter: Option<Expr>) -> Statement {
    Statement::Select(SelectStatement {
        distinct: false,
        columns,
        table: table.to_string(),
        table_alias: None,
        joins: vec![],
        filter,
        group_by: vec![],
        having: None,
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
    let begin = planner.plan_statement(&Statement::Begin, &context).unwrap();
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
    assert_eq!(begin, Plan::BeginTxn);
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
        columns: vec![
            SelectItem::Column("active".to_string()),
            SelectItem::Aggregate {
                func: AggregateFunc::Count,
                arg: AggregateArg::Wildcard,
                alias: Some("total".to_string()),
            },
        ],
        table: "users".to_string(),
        table_alias: None,
        joins: vec![],
        filter: None,
        group_by: vec!["active".to_string()],
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
            group_by: vec!["active".to_string()],
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
        columns: vec![
            SelectItem::Column("u.name".to_string()),
            SelectItem::Column("o.amount".to_string()),
        ],
        table: "users".to_string(),
        table_alias: Some("u".to_string()),
        joins: vec![JoinClause {
            table: "orders".to_string(),
            table_alias: Some("o".to_string()),
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
            table: "users".to_string(),
            table_alias: Some("u".to_string()),
            joins: vec![JoinPlan {
                table: "orders".to_string(),
                table_alias: Some("o".to_string()),
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
        columns: vec![SelectItem::Column("u.name".to_string())],
        table: "users".to_string(),
        table_alias: Some("u".to_string()),
        joins: vec![],
        filter: Some(Expr::ExistsSubquery {
            query: Box::new(SelectStatement {
                columns: vec![SelectItem::Column("id".to_string())],
                table: "orders".to_string(),
                table_alias: Some("o".to_string()),
                joins: vec![],
                filter: Some(Expr::CompareColumns {
                    left: "o.user_id".to_string(),
                    op: CompareOp::Eq,
                    right: "x.id".to_string(),
                }),
                group_by: vec![],
                order_by: vec![],
                limit: None,
                distinct: false,
                having: None,
            }),
            negated: false,
        }),
        group_by: vec![],
        order_by: vec![],
        limit: None,
        distinct: false,
        having: None,
    });

    let error = planner.plan_statement(&statement, &context).unwrap_err();

    assert_eq!(error.to_string(), "plan error: unknown column x.id");
}
