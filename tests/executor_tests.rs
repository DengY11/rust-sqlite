use std::cell::Cell;

use rustsql::common::types::{ColumnDef, ColumnType, Value};
use rustsql::sql::ast::{CompareOp, Expr, SelectItem, SelectStatement, Statement};
use rustsql::sql::executor::Executor;
use rustsql::sql::plan::{IndexBound, IndexRange, IndexScanSpec, Plan};
use rustsql::sql::planner::Planner;
use rustsql::storage::memory::MemoryStorage;

fn select_statement(columns: Vec<SelectItem>, table: &str, filter: Option<Expr>) -> Statement {
    Statement::Select(SelectStatement {
        columns,
        table: table.to_string(),
        table_alias: None,
        joins: vec![],
        filter,
        group_by: vec![],
        order_by: vec![],
        limit: None,
        distinct: false,
        having: None,
    })
}

fn users_columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef::primary_key("id", ColumnType::Integer),
        ColumnDef::new("name", ColumnType::Text).nullable(false),
        ColumnDef::new("active", ColumnType::Boolean).nullable(false),
    ]
}

#[test]
fn executor_runs_seq_scan_with_wildcard_projection_and_filter() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let executor = Executor::new(&storage, &current_txn);

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: users_columns(),
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![
                Value::Integer(1),
                Value::from("alice"),
                Value::Boolean(true),
            ],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(2), Value::from("bob"), Value::Boolean(false)],
        })
        .unwrap();

    let rows = executor
        .execute(Plan::SeqScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            filter: Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(1),
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        })
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(2),
            Value::from("bob"),
            Value::Boolean(false)
        ]]
    );
}

#[test]
fn executor_runs_index_scan_selected_by_planner() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let executor = Executor::new(&storage, &current_txn);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_name".to_string(),
            table: "users".to_string(),
            columns: vec!["name".to_string()],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(1), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(2), Value::from("bob")],
        })
        .unwrap();

    let statement = select_statement(
        vec![SelectItem::Column("id".to_string())],
        "users",
        Some(Expr::Compare {
            column: "name".to_string(),
            op: CompareOp::Eq,
            value: Value::from("alice"),
        }),
    );
    let context = storage.planning_context(current_txn.get()).unwrap();
    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("id".to_string())],
            index: "idx_users_name".to_string(),
            key_prefix: vec![Value::from("alice")],
            range: None,
            filter: Some(Expr::Compare {
                column: "name".to_string(),
                op: CompareOp::Eq,
                value: Value::from("alice"),
            }),
            order_by: vec![],
            limit: None,
            distinct: false,
        }
    );

    let rows = executor.execute(plan).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn executor_rechecks_full_filter_after_prefix_index_scan() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let executor = Executor::new(&storage, &current_txn);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: users_columns(),
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_active_name".to_string(),
            table: "users".to_string(),
            columns: vec!["active".to_string(), "name".to_string()],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![
                Value::Integer(1),
                Value::from("alice"),
                Value::Boolean(true),
            ],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![
                Value::Integer(2),
                Value::from("alice"),
                Value::Boolean(false),
            ],
        })
        .unwrap();

    let statement = select_statement(
        vec![SelectItem::Column("id".to_string())],
        "users",
        Some(Expr::And(
            Box::new(Expr::Compare {
                column: "active".to_string(),
                op: CompareOp::Eq,
                value: Value::Boolean(true),
            }),
            Box::new(Expr::Compare {
                column: "name".to_string(),
                op: CompareOp::Eq,
                value: Value::from("alice"),
            }),
        )),
    );
    let context = storage.planning_context(current_txn.get()).unwrap();
    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("id".to_string())],
            index: "idx_users_active_name".to_string(),
            key_prefix: vec![Value::Boolean(true), Value::from("alice")],
            range: None,
            filter: Some(Expr::And(
                Box::new(Expr::Compare {
                    column: "active".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Boolean(true),
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

    let rows = executor.execute(plan).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn executor_uses_eq_prefix_index_scan_even_with_range_term_in_filter() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let executor = Executor::new(&storage, &current_txn);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("pk", ColumnType::Integer),
                ColumnDef::new("id", ColumnType::Integer).nullable(false),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_id_name".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(1), Value::Integer(7), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(2), Value::Integer(7), Value::from("bob")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(3), Value::Integer(8), Value::from("carol")],
        })
        .unwrap();

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
                op: CompareOp::Gt,
                value: Value::from("alice"),
            }),
        )),
    );
    let context = storage.planning_context(current_txn.get()).unwrap();
    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("name".to_string())],
            index: "idx_users_id_name".to_string(),
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

    let rows = executor.execute(plan).unwrap();
    assert_eq!(rows, vec![vec![Value::from("bob")]]);
}

#[test]
fn executor_uses_leading_column_range_scan() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let executor = Executor::new(&storage, &current_txn);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string()],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(1), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(2), Value::from("bob")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(3), Value::from("carol")],
        })
        .unwrap();

    let statement = select_statement(
        vec![SelectItem::Column("name".to_string())],
        "users",
        Some(Expr::Compare {
            column: "id".to_string(),
            op: CompareOp::Gt,
            value: Value::Integer(1),
        }),
    );
    let context = storage.planning_context(current_txn.get()).unwrap();
    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("name".to_string())],
            index: "idx_users_id".to_string(),
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

    let rows = executor.execute(plan).unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("bob")], vec![Value::from("carol")]]
    );
}

#[test]
fn executor_uses_two_sided_range_scan_after_eq_prefix() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let executor = Executor::new(&storage, &current_txn);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("pk", ColumnType::Integer),
                ColumnDef::new("id", ColumnType::Integer).nullable(false),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_id_name".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(1), Value::Integer(7), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(2), Value::Integer(7), Value::from("bob")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(3), Value::Integer(7), Value::from("carol")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(4), Value::Integer(7), Value::from("david")],
        })
        .unwrap();

    let statement = select_statement(
        vec![SelectItem::Column("name".to_string())],
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
    let context = storage.planning_context(current_txn.get()).unwrap();
    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("name".to_string())],
            index: "idx_users_id_name".to_string(),
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

    let rows = executor.execute(plan).unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("bob")], vec![Value::from("carol")]]
    );
}

#[test]
fn executor_evaluates_not_is_null_and_inclusive_range_filters() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let executor = Executor::new(&storage, &current_txn);

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("email", ColumnType::Text),
                ColumnDef::new("active", ColumnType::Boolean).nullable(false),
            ],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(1), Value::Null, Value::Boolean(false)],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![
                Value::Integer(2),
                Value::from("alice@example.com"),
                Value::Boolean(true),
            ],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![
                Value::Integer(3),
                Value::from("bob@example.com"),
                Value::Boolean(false),
            ],
        })
        .unwrap();

    let rows = executor
        .execute(Plan::SeqScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("id".to_string())],
            filter: Some(Expr::And(
                Box::new(Expr::Not(Box::new(Expr::Compare {
                    column: "active".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Boolean(true),
                }))),
                Box::new(Expr::And(
                    Box::new(Expr::IsNull {
                        column: "email".to_string(),
                        negated: true,
                    }),
                    Box::new(Expr::And(
                        Box::new(Expr::Compare {
                            column: "id".to_string(),
                            op: CompareOp::Gte,
                            value: Value::Integer(2),
                        }),
                        Box::new(Expr::Compare {
                            column: "id".to_string(),
                            op: CompareOp::Lte,
                            value: Value::Integer(3),
                        }),
                    )),
                )),
            )),
            order_by: vec![],
            limit: None,
            distinct: false,
        })
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(3)]]);
}

#[test]
fn executor_merges_or_index_scans_and_deduplicates_rows() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let executor = Executor::new(&storage, &current_txn);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("pk", ColumnType::Integer),
                ColumnDef::new("id", ColumnType::Integer).nullable(false),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string()],
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_name".to_string(),
            table: "users".to_string(),
            columns: vec!["name".to_string()],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(1), Value::Integer(1), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(2), Value::Integer(2), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(3), Value::Integer(1), Value::from("bob")],
        })
        .unwrap();

    let statement = select_statement(
        vec![SelectItem::Column("pk".to_string())],
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
    let context = storage.planning_context(current_txn.get()).unwrap();
    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::IndexUnion {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("pk".to_string())],
            scans: vec![
                IndexScanSpec {
                    index: "idx_users_id".to_string(),
                    key_prefix: vec![Value::Integer(1)],
                    range: None,
                },
                IndexScanSpec {
                    index: "idx_users_name".to_string(),
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

    let rows = executor.execute(plan).unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );
}

#[test]
fn executor_projects_selected_columns_in_schema_order() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let executor = Executor::new(&storage, &current_txn);

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: users_columns(),
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            values: vec![
                Value::Integer(1),
                Value::from("alice"),
                Value::Boolean(true),
            ],
        })
        .unwrap();

    let rows = executor
        .execute(Plan::SeqScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![
                SelectItem::Column("name".to_string()),
                SelectItem::Column("id".to_string()),
            ],
            filter: None,
            order_by: vec![],
            limit: None,
            distinct: false,
        })
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(1)]]);
}
