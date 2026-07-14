use std::cell::Cell;

use rustsql::common::types::{ColumnDef, ColumnType, SortOrder, Value};
use rustsql::engine::traits::{CatalogStore, TransactionManager};
use rustsql::sql::ast::{CompareOp, Expr, FromItem, SelectItem, SelectStatement, Statement};
use rustsql::sql::executor::Executor;
use rustsql::sql::optimizer::Optimizer;
use rustsql::sql::plan::{IndexBound, IndexRange, IndexScanMode, IndexScanSpec, JoinPlan, Plan};
use rustsql::sql::planner::Planner;
use rustsql::storage::memory::MemoryStorage;

fn select_statement(columns: Vec<SelectItem>, table: &str, filter: Option<Expr>) -> Statement {
    Statement::Select(SelectStatement {
        with: None,
        columns,
        from: FromItem::Table {
            name: table.to_string(),
            schema: None,
            alias: None,
        },
        joins: vec![],
        compounds: vec![],
        filter,
        group_by: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
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

fn optimize_plan(plan: Plan, context: &rustsql::sql::planner::PlanningContext) -> Plan {
    Optimizer::new()
        .optimize_with_context(plan, context)
        .unwrap()
}

fn test_executor<'a>(
    storage: &'a MemoryStorage,
    current_txn: &'a Cell<Option<rustsql::engine::TransactionId>>,
    last_insert_rowid: &'a Cell<i64>,
) -> Executor<'a, MemoryStorage> {
    let savepoint_transaction = Box::leak(Box::new(Cell::new(false)));
    let savepoint_stack = Box::leak(Box::new(std::cell::RefCell::new(Vec::new())));
    let changes = Box::leak(Box::new(Cell::new(0)));
    let total_changes = Box::leak(Box::new(Cell::new(0)));
    let temp_database_used = Box::leak(Box::new(Cell::new(false)));
    let deferred_foreign_keys_pending = Box::leak(Box::new(Cell::new(false)));
    let defer_foreign_keys = Box::leak(Box::new(Cell::new(false)));
    let foreign_keys = Box::leak(Box::new(Cell::new(false)));
    let read_uncommitted = Box::leak(Box::new(Cell::new(false)));
    let query_only = Box::leak(Box::new(Cell::new(false)));
    let count_changes = Box::leak(Box::new(Cell::new(false)));
    let recursive_triggers = Box::leak(Box::new(Cell::new(false)));
    let trusted_schema = Box::leak(Box::new(Cell::new(false)));
    let threads = Box::leak(Box::new(Cell::new(0_u32)));
    let synchronous = Box::leak(Box::new(Cell::new(2_i64)));
    let temp_synchronous = Box::leak(Box::new(Cell::new(0_i64)));
    let temp_store = Box::leak(Box::new(Cell::new(0_i64)));
    let journal_mode = Box::leak(Box::new(std::cell::RefCell::new("memory".to_string())));
    let temp_journal_mode = Box::leak(Box::new(std::cell::RefCell::new("delete".to_string())));
    let locking_mode = Box::leak(Box::new(std::cell::RefCell::new("normal".to_string())));
    let temp_locking_mode = Box::leak(Box::new(std::cell::RefCell::new("exclusive".to_string())));
    let cache_size = Box::leak(Box::new(Cell::new(2000_i64)));
    let temp_cache_size = Box::leak(Box::new(Cell::new(0_i64)));
    let cache_spill = Box::leak(Box::new(Cell::new(2000_i64)));
    let busy_timeout = Box::leak(Box::new(Cell::new(0_i64)));
    let secure_delete = Box::leak(Box::new(Cell::new(2_i64)));
    let temp_secure_delete = Box::leak(Box::new(Cell::new(2_i64)));
    let wal_autocheckpoint = Box::leak(Box::new(Cell::new(1000_i64)));
    let auto_vacuum = Box::leak(Box::new(Cell::new(0_i64)));
    let max_page_count = Box::leak(Box::new(Cell::new(1_073_741_823_i64)));
    let temp_user_version = Box::leak(Box::new(Cell::new(0_u32)));
    let temp_application_id = Box::leak(Box::new(Cell::new(0_u32)));
    let temp_schema_version = Box::leak(Box::new(Cell::new(0_u32)));
    let mmap_size = Box::leak(Box::new(Cell::new(0_i64)));
    let analysis_limit = Box::leak(Box::new(Cell::new(0_u32)));
    let journal_size_limit = Box::leak(Box::new(Cell::new(-1_i64)));
    let soft_heap_limit = Box::leak(Box::new(Cell::new(0_i64)));
    let automatic_index = Box::leak(Box::new(Cell::new(true)));
    let cell_size_check = Box::leak(Box::new(Cell::new(false)));
    let full_column_names = Box::leak(Box::new(Cell::new(false)));
    let short_column_names = Box::leak(Box::new(Cell::new(true)));
    let fullfsync = Box::leak(Box::new(Cell::new(false)));
    let checkpoint_fullfsync = Box::leak(Box::new(Cell::new(true)));
    let empty_result_callbacks = Box::leak(Box::new(Cell::new(false)));
    let reverse_unordered_selects = Box::leak(Box::new(Cell::new(false)));
    let temp_page_size = Box::leak(Box::new(Cell::new(4096_u32)));
    Executor::new(
        storage,
        current_txn,
        savepoint_transaction,
        savepoint_stack,
        last_insert_rowid,
        changes,
        total_changes,
        temp_database_used,
        deferred_foreign_keys_pending,
        defer_foreign_keys,
        foreign_keys,
        read_uncommitted,
        query_only,
        count_changes,
        recursive_triggers,
        trusted_schema,
        threads,
        synchronous,
        temp_synchronous,
        temp_store,
        journal_mode,
        temp_journal_mode,
        locking_mode,
        temp_locking_mode,
        cache_size,
        temp_cache_size,
        cache_spill,
        busy_timeout,
        secure_delete,
        temp_secure_delete,
        wal_autocheckpoint,
        auto_vacuum,
        max_page_count,
        temp_user_version,
        temp_application_id,
        temp_schema_version,
        mmap_size,
        analysis_limit,
        journal_size_limit,
        soft_heap_limit,
        automatic_index,
        cell_size_check,
        full_column_names,
        short_column_names,
        fullfsync,
        checkpoint_fullfsync,
        empty_result_callbacks,
        reverse_unordered_selects,
        temp_page_size,
    )
}

#[test]
fn executor_runs_seq_scan_with_wildcard_projection_and_filter() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: users_columns(),
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
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
            or_conflict: None,
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
            offset: None,
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
fn executor_supports_generated_columns_on_create_insert_and_update() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);

    executor
        .execute(Plan::CreateTable {
            name: "metrics".to_string(),
            columns: vec![
                ColumnDef::new("base", ColumnType::Integer),
                ColumnDef::new("plus_one", ColumnType::Integer).generated_stored("base + 1"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();

    assert_eq!(
        executor
            .execute(Plan::Insert {
                table: "metrics".to_string(),
                or_conflict: None,
                values: vec![Value::Integer(3)],
            })
            .unwrap(),
        Vec::<Vec<Value>>::new()
    );

    let rows = executor
        .execute(Plan::SeqScan {
            table: "metrics".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            filter: None,
            order_by: vec![],
            limit: None,
            offset: None,
            distinct: false,
        })
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(3), Value::Integer(4)]]);

    executor
        .execute(Plan::Update {
            table: "metrics".to_string(),
            or_conflict: None,
            assignments: vec![rustsql::sql::ast::Assignment {
                column: "base".to_string(),
                value: rustsql::sql::ast::ScalarExpr::Literal(Value::Integer(5)),
            }],
            filter: None,
        })
        .unwrap();

    let updated_rows = executor
        .execute(Plan::SeqScan {
            table: "metrics".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            filter: None,
            order_by: vec![],
            limit: None,
            offset: None,
            distinct: false,
        })
        .unwrap();
    assert_eq!(
        updated_rows,
        vec![vec![Value::Integer(5), Value::Integer(6)]]
    );
}

#[test]
fn executor_rejects_explicit_values_for_generated_columns() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);

    executor
        .execute(Plan::CreateTable {
            name: "metrics".to_string(),
            columns: vec![
                ColumnDef::new("base", ColumnType::Integer),
                ColumnDef::new("plus_one", ColumnType::Integer).generated_stored("base + 1"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();

    let error = executor
        .execute(Plan::Insert {
            table: "metrics".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(3), Value::Integer(4)],
        })
        .unwrap_err();

    assert!(
        error.to_string().contains("generated column"),
        "unexpected error: {error}"
    );
}

#[test]
fn executor_allows_without_rowid_in_create_table() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);

    executor
        .execute(Plan::CreateTable {
            name: "memberships".to_string(),
            columns: vec![
                ColumnDef::new("user_id", ColumnType::Integer),
                ColumnDef::new("group_id", ColumnType::Integer),
            ],
            constraints: vec![rustsql::sql::ast::TableConstraint::PrimaryKey(
                rustsql::common::types::PrimaryKeyConstraint::new(vec![
                    "user_id".to_string(),
                    "group_id".to_string(),
                ]),
            )],
            strict: false,
            without_rowid: true,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();

    let txn = storage.begin().unwrap();
    let schema = storage
        .get_schema(txn, "memberships")
        .unwrap()
        .expect("memberships schema should exist");
    storage.rollback(txn).unwrap();
    assert!(schema.without_rowid);
    assert_eq!(
        schema
            .primary_key_constraint
            .as_ref()
            .map(|constraint| constraint.columns.clone()),
        Some(vec!["user_id".to_string(), "group_id".to_string()])
    );
}

#[test]
fn executor_allows_desc_integer_primary_key_in_create_table() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer)
                    .primary_key_sort_order(SortOrder::Desc),
                ColumnDef::new("name", ColumnType::Text),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();

    let txn = storage.begin().unwrap();
    let schema = storage.get_schema(txn, "users").unwrap().unwrap();
    storage.rollback(txn).unwrap();

    assert!(schema.columns[0].primary_key);
    assert_eq!(
        schema.columns[0].primary_key_sort_order,
        Some(SortOrder::Desc)
    );
}

#[test]
fn executor_runs_index_scan_selected_by_optimizer() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_name".to_string(),
            table: "users".to_string(),
            columns: vec!["name".to_string()],
            decorated_columns: None,
            unique: false,
            predicate: None,
            if_not_exists: false,
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(1), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
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
    let plan = optimize_plan(
        planner.plan_statement(&statement, &context).unwrap(),
        &context,
    );

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("id".to_string())],
            index: "idx_users_name".to_string(),
            mode: IndexScanMode::Lookup,
            key_prefix: vec![Value::from("alice")],
            range: None,
            filter: Some(Expr::Compare {
                column: "name".to_string(),
                op: CompareOp::Eq,
                value: Value::from("alice"),
            }),
            order_by: vec![],
            limit: None,
            offset: None,
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
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: users_columns(),
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_active_name".to_string(),
            table: "users".to_string(),
            columns: vec!["active".to_string(), "name".to_string()],
            decorated_columns: None,
            unique: false,
            predicate: None,
            if_not_exists: false,
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
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
            or_conflict: None,
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
    let plan = optimize_plan(
        planner.plan_statement(&statement, &context).unwrap(),
        &context,
    );

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("id".to_string())],
            index: "idx_users_active_name".to_string(),
            mode: IndexScanMode::Lookup,
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
            offset: None,
            distinct: false,
        }
    );

    let rows = executor.execute(plan).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn executor_rejects_qualified_duplicate_output_from_joined_wildcard_derived_source() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("age", ColumnType::Integer),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::CreateTable {
            name: "orders".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("user_id", ColumnType::Integer),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(1), Value::Integer(20)],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "orders".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(10), Value::Integer(1)],
        })
        .unwrap();

    let error = executor
        .execute(Plan::DerivedSource {
            source: Box::new(Plan::NestedLoopJoin {
                source: Box::new(Plan::SeqScan {
                    table: "users".to_string(),
                    table_alias: Some("u".to_string()),
                    columns: vec![SelectItem::Wildcard],
                    filter: None,
                    order_by: vec![],
                    limit: None,
                    offset: None,
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
                        offset: None,
                        distinct: false,
                    }),
                    on: Expr::CompareColumns {
                        left: "u.id".to_string(),
                        op: CompareOp::Eq,
                        right: "o.user_id".to_string(),
                    },
                    kind: rustsql::sql::ast::JoinKind::Inner,
                    using_columns: Vec::new(),
                }],
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                offset: None,
                distinct: false,
            }),
            alias: "t".to_string(),
            output_columns: vec![
                "id".to_string(),
                "age".to_string(),
                "id".to_string(),
                "user_id".to_string(),
            ],
            columns: vec![SelectItem::Column("t.id".to_string())],
            filter: None,
            order_by: vec![],
            limit: None,
            offset: None,
            distinct: false,
        })
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "plan error: ambiguous column reference: t.id"
    );
}

#[test]
fn executor_uses_eq_prefix_index_scan_even_with_range_term_in_filter() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("pk", ColumnType::Integer),
                ColumnDef::new("id", ColumnType::Integer).nullable(false),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_id_name".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            decorated_columns: None,
            unique: false,
            predicate: None,
            if_not_exists: false,
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(1), Value::Integer(7), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(2), Value::Integer(7), Value::from("bob")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
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
    let plan = optimize_plan(
        planner.plan_statement(&statement, &context).unwrap(),
        &context,
    );

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("name".to_string())],
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
            offset: None,
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
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string()],
            decorated_columns: None,
            unique: false,
            predicate: None,
            if_not_exists: false,
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(1), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(2), Value::from("bob")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
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
    let plan = optimize_plan(
        planner.plan_statement(&statement, &context).unwrap(),
        &context,
    );

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("name".to_string())],
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
            offset: None,
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
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("pk", ColumnType::Integer),
                ColumnDef::new("id", ColumnType::Integer).nullable(false),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_id_name".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            decorated_columns: None,
            unique: false,
            predicate: None,
            if_not_exists: false,
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(1), Value::Integer(7), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(2), Value::Integer(7), Value::from("bob")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(3), Value::Integer(7), Value::from("carol")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
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
    let plan = optimize_plan(
        planner.plan_statement(&statement, &context).unwrap(),
        &context,
    );

    assert_eq!(
        plan,
        Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("name".to_string())],
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
            offset: None,
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
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("email", ColumnType::Text),
                ColumnDef::new("active", ColumnType::Boolean).nullable(false),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(1), Value::Null, Value::Boolean(false)],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
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
            or_conflict: None,
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
            offset: None,
            distinct: false,
        })
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(3)]]);
}

#[test]
fn executor_merges_or_index_scans_and_deduplicates_rows() {
    let storage = MemoryStorage::new();
    let current_txn = Cell::new(None);
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);
    let planner = Planner::new();

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("pk", ColumnType::Integer),
                ColumnDef::new("id", ColumnType::Integer).nullable(false),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string()],
            decorated_columns: None,
            unique: false,
            predicate: None,
            if_not_exists: false,
        })
        .unwrap();
    executor
        .execute(Plan::CreateIndex {
            name: "idx_users_name".to_string(),
            table: "users".to_string(),
            columns: vec!["name".to_string()],
            decorated_columns: None,
            unique: false,
            predicate: None,
            if_not_exists: false,
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(1), Value::Integer(1), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
            values: vec![Value::Integer(2), Value::Integer(2), Value::from("alice")],
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
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
    let plan = optimize_plan(
        planner.plan_statement(&statement, &context).unwrap(),
        &context,
    );

    assert_eq!(
        plan,
        Plan::IndexUnion {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Column("pk".to_string())],
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
            offset: None,
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
    let last_insert_rowid = Cell::new(0);
    let executor = test_executor(&storage, &current_txn, &last_insert_rowid);

    executor
        .execute(Plan::CreateTable {
            name: "users".to_string(),
            columns: users_columns(),
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        })
        .unwrap();
    executor
        .execute(Plan::Insert {
            table: "users".to_string(),
            or_conflict: None,
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
            offset: None,
            distinct: false,
        })
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(1)]]);
}
