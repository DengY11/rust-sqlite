use std::collections::HashMap;

use rustsql::common::types::{ColumnDef, ColumnType, IndexMeta, Schema, Value};
use rustsql::sql::ast::{Expr, SelectItem, Statement};
use rustsql::sql::plan::Plan;
use rustsql::sql::planner::{Planner, PlanningContext};

fn user_schema() -> Schema {
    Schema::new(
        "users",
        vec![
            ColumnDef::primary_key("id", ColumnType::Integer),
            ColumnDef::new("name", ColumnType::Text),
        ],
    )
}

fn context_with_indexes(indexes: Vec<IndexMeta>) -> PlanningContext {
    PlanningContext::new(
        HashMap::from([("users".to_string(), user_schema())]),
        HashMap::from([("users".to_string(), indexes)]),
    )
}

#[test]
fn plans_select_without_index_as_seq_scan() {
    let planner = Planner::new();
    let statement = Statement::Select {
        columns: vec![SelectItem::Wildcard],
        table: "users".to_string(),
        filter: Some(Expr::Eq("id".to_string(), Value::Integer(7))),
    };

    let plan = planner
        .plan_statement(&statement, &context_with_indexes(vec![]))
        .unwrap();

    assert_eq!(
        plan,
        Plan::SeqScan {
            table: "users".to_string(),
            columns: vec![SelectItem::Wildcard],
            filter: Some(Expr::Eq("id".to_string(), Value::Integer(7))),
        }
    );
}

#[test]
fn plans_select_with_matching_eq_index_as_index_scan() {
    let planner = Planner::new();
    let statement = Statement::Select {
        columns: vec![SelectItem::Column("name".to_string())],
        table: "users".to_string(),
        filter: Some(Expr::Eq("id".to_string(), Value::Integer(7))),
    };
    let indexes = vec![IndexMeta {
        name: "idx_users_id".to_string(),
        columns: vec!["id".to_string()],
        unique: false,
    }];

    let plan = planner
        .plan_statement(&statement, &context_with_indexes(indexes))
        .unwrap();

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            columns: vec![SelectItem::Column("name".to_string())],
            index: "idx_users_id".to_string(),
            column: "id".to_string(),
            value: Value::Integer(7),
        }
    );
}

#[test]
fn plans_range_predicates_as_seq_scan_even_when_index_exists() {
    let planner = Planner::new();
    let indexes = vec![IndexMeta {
        name: "idx_users_id".to_string(),
        columns: vec!["id".to_string()],
        unique: false,
    }];
    let context = context_with_indexes(indexes);

    let gt_plan = planner
        .plan_statement(
            &Statement::Select {
                columns: vec![SelectItem::Wildcard],
                table: "users".to_string(),
                filter: Some(Expr::Gt("id".to_string(), Value::Integer(1))),
            },
            &context,
        )
        .unwrap();
    let lt_plan = planner
        .plan_statement(
            &Statement::Select {
                columns: vec![SelectItem::Wildcard],
                table: "users".to_string(),
                filter: Some(Expr::Lt("id".to_string(), Value::Integer(9))),
            },
            &context,
        )
        .unwrap();

    assert_eq!(
        gt_plan,
        Plan::SeqScan {
            table: "users".to_string(),
            columns: vec![SelectItem::Wildcard],
            filter: Some(Expr::Gt("id".to_string(), Value::Integer(1))),
        }
    );
    assert_eq!(
        lt_plan,
        Plan::SeqScan {
            table: "users".to_string(),
            columns: vec![SelectItem::Wildcard],
            filter: Some(Expr::Lt("id".to_string(), Value::Integer(9))),
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
            },
            &context,
        )
        .unwrap();
    let create_index = planner
        .plan_statement(
            &Statement::CreateIndex {
                name: "idx_users_id".to_string(),
                table: "users".to_string(),
                column: "id".to_string(),
            },
            &context,
        )
        .unwrap();
    let insert = planner
        .plan_statement(
            &Statement::Insert {
                table: "users".to_string(),
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
        }
    );
    assert_eq!(
        create_index,
        Plan::CreateIndex {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            column: "id".to_string(),
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
