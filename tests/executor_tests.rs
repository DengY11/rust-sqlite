use std::cell::Cell;

use rustsql::common::types::{ColumnDef, ColumnType, Value};
use rustsql::sql::ast::{Expr, SelectItem, Statement};
use rustsql::sql::executor::Executor;
use rustsql::sql::plan::Plan;
use rustsql::sql::planner::Planner;
use rustsql::storage::memory::MemoryStorage;

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
            columns: vec![SelectItem::Wildcard],
            filter: Some(Expr::Gt("id".to_string(), Value::Integer(1))),
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
            column: "name".to_string(),
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

    let statement = Statement::Select {
        columns: vec![SelectItem::Column("id".to_string())],
        table: "users".to_string(),
        filter: Some(Expr::Eq("name".to_string(), Value::from("alice"))),
    };
    let context = storage.planning_context(current_txn.get()).unwrap();
    let plan = planner.plan_statement(&statement, &context).unwrap();

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            columns: vec![SelectItem::Column("id".to_string())],
            index: "idx_users_name".to_string(),
            column: "name".to_string(),
            value: Value::from("alice"),
        }
    );

    let rows = executor.execute(plan).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
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
            columns: vec![
                SelectItem::Column("name".to_string()),
                SelectItem::Column("id".to_string()),
            ],
            filter: None,
        })
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(1)]]);
}
