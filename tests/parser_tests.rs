use rustsql::common::types::{
    CheckConstraint, CheckOp, ColumnDef, ColumnDefault, ColumnType, ForeignKey, SortOrder, Value,
};
use rustsql::sql::ast::{
    AggregateArg, AggregateFunc, AlterTableAction, Assignment, CommonTableExpr, CompareOp, CteBody,
    Expr, FromItem, JoinClause, JoinKind, NullOrder, OrderBy, OrderByExpr, ScalarBinaryOp,
    ScalarExpr, ScalarFunc, SelectItem, SelectStatement, Statement, TableConstraint, WithClause,
};
use rustsql::sql::lexer::{TokenKind, lex};
use rustsql::sql::parser::parse_sql;

fn select_statement(
    columns: Vec<SelectItem>,
    table: &str,
    table_alias: Option<&str>,
    filter: Option<Expr>,
    order_by: Vec<OrderBy>,
    limit: Option<usize>,
) -> Statement {
    Statement::Select(SelectStatement {
        with: None,
        distinct: false,
        columns,
        from: FromItem::Table {
            name: table.to_string(),
            alias: table_alias.map(str::to_string),
        },
        joins: vec![],
        filter,
        group_by: vec![],
        having: None,
        compounds: vec![],
        order_by,
        limit,
        offset: None,
    })
}

fn token_kind_debugs(sql: &str) -> Vec<String> {
    lex(sql)
        .unwrap()
        .into_iter()
        .map(|token| format!("{:?}", token.kind))
        .collect()
}

fn single_statement_debug(sql: &str) -> String {
    let statements = parse_sql(sql).unwrap();
    assert_eq!(
        statements.len(),
        1,
        "unexpected statements: {statements:#?}"
    );
    format!("{:#?}", statements[0])
}

#[test]
fn parses_select_all_quantifier_like_sqlite() {
    let statements = parse_sql("SELECT ALL name FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_explain_query_plan_select_statement() {
    let statements = parse_sql("EXPLAIN QUERY PLAN SELECT name FROM users WHERE id = 1;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::ExplainQueryPlan(Box::new(select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(1),
            }),
            vec![],
            None,
        )))]
    );
}

#[test]
fn parses_create_table_statement() {
    let statements = parse_sql("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_temp_table_statements_like_sqlite() {
    let statements = parse_sql(
        "CREATE TEMP TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TEMPORARY TABLE IF NOT EXISTS logs (id INTEGER, message TEXT);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![
            Statement::CreateTable {
                name: "users".to_string(),
                columns: vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text),
                ],
                constraints: vec![],
                strict: false,
                without_rowid: false,
                if_not_exists: false,
            },
            Statement::CreateTable {
                name: "logs".to_string(),
                columns: vec![
                    ColumnDef::new("id", ColumnType::Integer),
                    ColumnDef::new("message", ColumnType::Text),
                ],
                constraints: vec![],
                strict: false,
                without_rowid: false,
                if_not_exists: true,
            },
        ]
    );
}

#[test]
fn parses_create_table_as_select_like_sqlite() {
    let statements =
        parse_sql("CREATE TABLE archive_users AS SELECT id, UPPER(name) AS name FROM users;")
            .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTableAs {
            name: "archive_users".to_string(),
            if_not_exists: false,
            select: SelectStatement {
                with: None,
                distinct: false,
                columns: vec![
                    SelectItem::Column("id".to_string()),
                    SelectItem::Expr {
                        expr: ScalarExpr::Function {
                            func: ScalarFunc::Upper,
                            args: vec![ScalarExpr::Column("name".to_string())],
                        },
                        alias: Some("name".to_string()),
                    },
                ],
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
                offset: None,
            },
        }]
    );
}

#[test]
fn parses_create_table_if_not_exists_as_select_like_sqlite() {
    let statements =
        parse_sql("CREATE TABLE IF NOT EXISTS archive_users AS SELECT id, name FROM users;")
            .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTableAs {
            name: "archive_users".to_string(),
            if_not_exists: true,
            select: SelectStatement {
                with: None,
                distinct: false,
                columns: vec![
                    SelectItem::Column("id".to_string()),
                    SelectItem::Column("name".to_string()),
                ],
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
                offset: None,
            },
        }]
    );
}

#[test]
fn parses_create_table_with_desc_primary_key_column() {
    let statements =
        parse_sql("CREATE TABLE users (id INTEGER PRIMARY KEY DESC, name TEXT);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
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
        }]
    );
}

#[test]
fn parses_create_table_if_not_exists_statement() {
    let statements =
        parse_sql("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: true,
        }]
    );
}

#[test]
fn parses_create_table_with_quoted_identifiers() {
    let statements =
        parse_sql("CREATE TABLE \"users\" (\"id\" INTEGER PRIMARY KEY, \"name\" TEXT);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_typeless_columns() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id PRIMARY KEY,
            name,
            created_at DEFAULT CURRENT_TIMESTAMP
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Any),
                ColumnDef::new("name", ColumnType::Any),
                ColumnDef::new("created_at", ColumnType::Any)
                    .default_value(ColumnDefault::CurrentTimestamp),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_backtick_identifiers() {
    let statements =
        parse_sql("CREATE TABLE `users` (`id` INTEGER PRIMARY KEY, `name` TEXT);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_bracket_identifiers() {
    let statements =
        parse_sql("CREATE TABLE [users] ([id] INTEGER PRIMARY KEY, [name] TEXT);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_blob_column() {
    let statements =
        parse_sql("CREATE TABLE files (id INTEGER PRIMARY KEY, payload BLOB);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "files".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("payload", ColumnType::Blob),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_autoincrement_primary_key() {
    let statements =
        parse_sql("CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer).autoincrement(true),
                ColumnDef::new("name", ColumnType::Text),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_sqlite_type_aliases_and_modifiers() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INT PRIMARY KEY,
            name VARCHAR(255),
            slug CHAR(16),
            bio CLOB,
            visits BIGINT
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text),
                ColumnDef::new("slug", ColumnType::Text),
                ColumnDef::new("bio", ColumnType::Text),
                ColumnDef::new("visits", ColumnType::Integer),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_current_timestamp_default() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("created_at", ColumnType::Text)
                    .default_value(ColumnDefault::CurrentTimestamp),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_current_date_and_time_defaults() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            created_date TEXT DEFAULT CURRENT_DATE,
            created_time TEXT DEFAULT CURRENT_TIME
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("created_date", ColumnType::Text)
                    .default_value(ColumnDefault::CurrentDate),
                ColumnDef::new("created_time", ColumnType::Text)
                    .default_value(ColumnDefault::CurrentTime),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_parenthesized_literal_defaults() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            visits INTEGER DEFAULT (0),
            nickname TEXT DEFAULT ('guest')
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("visits", ColumnType::Integer)
                    .default_value(ColumnDefault::Literal(Value::Integer(0))),
                ColumnDef::new("nickname", ColumnType::Text)
                    .default_value(ColumnDefault::Literal(Value::from("guest"))),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_signed_and_hex_literal_defaults() {
    let statements = parse_sql(
        "CREATE TABLE nums (
            id INTEGER PRIMARY KEY,
            visits INTEGER DEFAULT (+1),
            half REAL DEFAULT (+.5),
            mask INTEGER DEFAULT (0x10),
            delta INTEGER DEFAULT (-0x10)
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "nums".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("visits", ColumnType::Integer)
                    .default_value(ColumnDefault::Literal(Value::Integer(1))),
                ColumnDef::new("half", ColumnType::Real)
                    .default_value(ColumnDefault::Literal(Value::Real(0.5))),
                ColumnDef::new("mask", ColumnType::Integer)
                    .default_value(ColumnDefault::Literal(Value::Integer(16))),
                ColumnDef::new("delta", ColumnType::Integer)
                    .default_value(ColumnDefault::Literal(Value::Integer(-16))),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_numeric_type_precision_suffixes() {
    let statements = parse_sql(
        "CREATE TABLE metrics (
            id INTEGER PRIMARY KEY,
            code VARCHAR(32),
            amount NUMERIC(10,2),
            price DECIMAL(10,2)
        );",
    )
    .unwrap();

    let Statement::CreateTable { columns, .. } = &statements[0] else {
        panic!("expected create table");
    };

    assert_eq!(columns[1].column_type, ColumnType::Text);
    assert_eq!(columns[2].column_type, ColumnType::Real);
    assert_eq!(columns[3].column_type, ColumnType::Real);
}

#[test]
fn parses_create_table_with_unique_constraints() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            email TEXT UNIQUE,
            username TEXT,
            UNIQUE(username)
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("email", ColumnType::Text).unique(true),
                ColumnDef::new("username", ColumnType::Text),
            ],
            constraints: vec![rustsql::sql::ast::TableConstraint::Unique(
                rustsql::common::types::UniqueConstraint::new(vec!["username".to_string()])
                    .with_decorated_columns(vec!["username".to_string()]),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_composite_primary_key_constraint() {
    let statements = parse_sql(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            PRIMARY KEY(user_id, group_id)
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "memberships".to_string(),
            columns: vec![
                ColumnDef::new("user_id", ColumnType::Integer),
                ColumnDef::new("group_id", ColumnType::Integer),
            ],
            constraints: vec![rustsql::sql::ast::TableConstraint::PrimaryKey(
                rustsql::common::types::PrimaryKeyConstraint::new(vec![
                    "user_id".to_string(),
                    "group_id".to_string(),
                ])
                .with_decorated_columns(vec!["user_id".to_string(), "group_id".to_string()]),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_strict_mode() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name TEXT
        ) STRICT;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text),
            ],
            constraints: vec![],
            strict: true,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_without_rowid() {
    let statements = parse_sql(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            PRIMARY KEY(user_id, group_id)
        ) WITHOUT ROWID;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "memberships".to_string(),
            columns: vec![
                ColumnDef::new("user_id", ColumnType::Integer),
                ColumnDef::new("group_id", ColumnType::Integer),
            ],
            constraints: vec![rustsql::sql::ast::TableConstraint::PrimaryKey(
                rustsql::common::types::PrimaryKeyConstraint::new(vec![
                    "user_id".to_string(),
                    "group_id".to_string(),
                ])
                .with_decorated_columns(vec!["user_id".to_string(), "group_id".to_string()]),
            )],
            strict: false,
            without_rowid: true,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_stored_generated_column() {
    let statements = parse_sql(
        "CREATE TABLE metrics (
            base INTEGER,
            plus_one INTEGER GENERATED ALWAYS AS (base + 1) STORED
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "metrics".to_string(),
            columns: vec![
                ColumnDef::new("base", ColumnType::Integer),
                ColumnDef::new("plus_one", ColumnType::Integer).generated_stored("base + 1"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_virtual_generated_column() {
    let statements = parse_sql(
        "CREATE TABLE metrics (
            base INTEGER,
            plus_one INTEGER GENERATED ALWAYS AS (base + 1) VIRTUAL
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "metrics".to_string(),
            columns: vec![
                ColumnDef::new("base", ColumnType::Integer),
                ColumnDef::new("plus_one", ColumnType::Integer).generated_virtual("base + 1"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_implicit_virtual_generated_column() {
    let statements = parse_sql(
        "CREATE TABLE metrics (
            base INTEGER,
            plus_one INTEGER GENERATED ALWAYS AS (base + 1)
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "metrics".to_string(),
            columns: vec![
                ColumnDef::new("base", ColumnType::Integer),
                ColumnDef::new("plus_one", ColumnType::Integer)
                    .generated_virtual_implicit("base + 1"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_as_generated_column_syntax() {
    let statements = parse_sql(
        "CREATE TABLE metrics (
            base INTEGER,
            plus_one INTEGER AS (base + 1)
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "metrics".to_string(),
            columns: vec![
                ColumnDef::new("base", ColumnType::Integer),
                ColumnDef::new("plus_one", ColumnType::Integer).generated_as("base + 1"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_and_index_with_collate_clauses() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name TEXT COLLATE NOCASE
        );
        CREATE INDEX idx_users_name_nocase ON users(name COLLATE NOCASE DESC);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![
            Statement::CreateTable {
                name: "users".to_string(),
                columns: vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text).collation("NOCASE"),
                ],
                constraints: vec![],
                strict: false,
                without_rowid: false,
                if_not_exists: false,
            },
            Statement::CreateIndex {
                name: "idx_users_name_nocase".to_string(),
                table: "users".to_string(),
                columns: vec!["name".to_string()],
                decorated_columns: Some(vec!["name COLLATE NOCASE DESC".to_string()]),
                unique: false,
                predicate: None,
                if_not_exists: false,
            },
        ]
    );
}

#[test]
fn parses_create_table_with_default_then_collate_column() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            nickname TEXT DEFAULT ('guest') COLLATE NOCASE
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("nickname", ColumnType::Text)
                    .default_value(ColumnDefault::Literal(Value::from("guest")))
                    .collation("NOCASE"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_partial_index_statement() {
    let statements =
        parse_sql("CREATE INDEX idx_users_email_active ON users(email) WHERE active = 1;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateIndex {
            name: "idx_users_email_active".to_string(),
            table: "users".to_string(),
            columns: vec!["email".to_string()],
            decorated_columns: Some(vec!["email".to_string()]),
            unique: false,
            predicate: Some("active = 1".to_string()),
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_defaults_checks_and_foreign_keys() {
    let statements = parse_sql(
        "CREATE TABLE orders (
            id INTEGER PRIMARY KEY,
            user_id INTEGER REFERENCES users(id),
            amount INTEGER DEFAULT 0 CHECK (amount >= 0),
            CHECK (id > 0),
            FOREIGN KEY (user_id) REFERENCES users(id)
        );",
    )
    .unwrap();

    let Statement::CreateTable {
        name,
        columns,
        constraints,
        ..
    } = &statements[0]
    else {
        panic!("expected create table");
    };

    assert_eq!(name, "orders");
    assert_eq!(
        columns[2].default_value,
        Some(ColumnDefault::Literal(Value::Integer(0)))
    );
    assert_eq!(columns[1].foreign_key.as_ref().unwrap().ref_table, "users");
    assert_eq!(constraints.len(), 2);
}

#[test]
fn parses_create_table_with_references_parent_primary_key_shorthand() {
    let statements = parse_sql(
        "CREATE TABLE orders (
            id INTEGER PRIMARY KEY,
            user_id INTEGER REFERENCES users
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "orders".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("user_id", ColumnType::Integer)
                    .references_parent_primary_key("users"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_composite_foreign_key_references_parent_primary_key_shorthand() {
    let statements = parse_sql(
        "CREATE TABLE child (
            x INTEGER,
            y INTEGER,
            FOREIGN KEY (x, y) REFERENCES parents
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "child".to_string(),
            columns: vec![
                ColumnDef::new("x", ColumnType::Integer),
                ColumnDef::new("y", ColumnType::Integer),
            ],
            constraints: vec![TableConstraint::ForeignKey(
                ForeignKey::multi_column_to_parent_primary_key(
                    vec!["x".to_string(), "y".to_string()],
                    "parents",
                ),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_named_column_constraints() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER CONSTRAINT pk PRIMARY KEY,
            age INTEGER CONSTRAINT age_nonneg CHECK (age >= 0),
            email TEXT CONSTRAINT uq UNIQUE,
            user_id INTEGER CONSTRAINT fk REFERENCES accounts(id)
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer).with_primary_key_name("pk"),
                ColumnDef::new("age", ColumnType::Integer).check(CheckConstraint::named_compare(
                    "age_nonneg",
                    "age",
                    CheckOp::Gte,
                    Value::Integer(0),
                )),
                ColumnDef::new("email", ColumnType::Text)
                    .unique(true)
                    .with_unique_name("uq"),
                ColumnDef::new("user_id", ColumnType::Integer)
                    .references("accounts", "id")
                    .with_foreign_key_name("fk"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_preserving_named_column_primary_key_and_unique_constraints() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER CONSTRAINT pk PRIMARY KEY,
            email TEXT CONSTRAINT uq UNIQUE
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer).with_primary_key_name("pk"),
                ColumnDef::new("email", ColumnType::Text)
                    .unique(true)
                    .with_unique_name("uq"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_preserving_named_not_null_constraints() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name TEXT CONSTRAINT nn NOT NULL
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text)
                    .nullable(false)
                    .with_not_null_name("nn"),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_preserving_named_check_constraints() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            age INTEGER CONSTRAINT age_nonneg CHECK (age >= 0),
            score INTEGER,
            CONSTRAINT score_cap CHECK (score <= 100)
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("age", ColumnType::Integer).check(CheckConstraint::named_compare(
                    "age_nonneg",
                    "age",
                    CheckOp::Gte,
                    Value::Integer(0),
                )),
                ColumnDef::new("score", ColumnType::Integer),
            ],
            constraints: vec![TableConstraint::Check(CheckConstraint::named_compare(
                "score_cap",
                "score",
                CheckOp::Lte,
                Value::Integer(100),
            ))],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_on_conflict_column_and_table_constraints() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY ON CONFLICT REPLACE,
            email TEXT UNIQUE ON CONFLICT IGNORE,
            name TEXT NOT NULL ON CONFLICT FAIL,
            nickname TEXT,
            CONSTRAINT uq UNIQUE(name, nickname) ON CONFLICT ABORT
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer)
                    .with_primary_key_conflict_clause("REPLACE"),
                ColumnDef::new("email", ColumnType::Text)
                    .unique(true)
                    .with_unique_conflict_clause("IGNORE"),
                ColumnDef::new("name", ColumnType::Text)
                    .nullable(false)
                    .with_not_null_conflict_clause("FAIL"),
                ColumnDef::new("nickname", ColumnType::Text),
            ],
            constraints: vec![TableConstraint::Unique(
                rustsql::common::types::UniqueConstraint::new(vec![
                    "name".to_string(),
                    "nickname".to_string(),
                ])
                .named("uq")
                .with_decorated_columns(vec!["name".to_string(), "nickname".to_string(),])
                .with_conflict_clause("ABORT"),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_foreign_key_action_clauses() {
    let statements = parse_sql(
        "CREATE TABLE posts (
            id INTEGER PRIMARY KEY,
            user_id INTEGER REFERENCES users(id) ON DELETE CASCADE ON UPDATE RESTRICT,
            author_id INTEGER,
            FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE SET NULL ON UPDATE NO ACTION
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "posts".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("user_id", ColumnType::Integer)
                    .references("users", "id")
                    .with_foreign_key_action_on_delete("CASCADE")
                    .with_foreign_key_action_on_update("RESTRICT"),
                ColumnDef::new("author_id", ColumnType::Integer),
            ],
            constraints: vec![TableConstraint::ForeignKey(
                rustsql::common::types::ForeignKey::single_column("author_id", "users", "id")
                    .with_on_delete("SET NULL")
                    .with_on_update("NO ACTION"),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_deferrable_foreign_keys() {
    let statements = parse_sql(
        "CREATE TABLE posts (
            id INTEGER PRIMARY KEY,
            user_id INTEGER REFERENCES users(id) DEFERRABLE INITIALLY DEFERRED,
            author_id INTEGER,
            FOREIGN KEY (author_id) REFERENCES users(id) NOT DEFERRABLE INITIALLY IMMEDIATE
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "posts".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("user_id", ColumnType::Integer)
                    .references("users", "id")
                    .with_foreign_key_deferrable(true)
                    .with_foreign_key_initially_deferred(true),
                ColumnDef::new("author_id", ColumnType::Integer),
            ],
            constraints: vec![TableConstraint::ForeignKey(
                rustsql::common::types::ForeignKey::single_column("author_id", "users", "id")
                    .deferrable(false)
                    .initially_deferred(false),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_foreign_key_match_clauses() {
    let statements = parse_sql(
        "CREATE TABLE posts (
            id INTEGER PRIMARY KEY,
            user_id INTEGER REFERENCES users(id) MATCH FULL,
            author_id INTEGER,
            FOREIGN KEY (author_id) REFERENCES users(id) MATCH SIMPLE
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "posts".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("user_id", ColumnType::Integer)
                    .references("users", "id")
                    .with_foreign_key_match("FULL"),
                ColumnDef::new("author_id", ColumnType::Integer),
            ],
            constraints: vec![TableConstraint::ForeignKey(
                rustsql::common::types::ForeignKey::single_column("author_id", "users", "id")
                    .with_match("SIMPLE"),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_preserving_on_conflict_clauses() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY ON CONFLICT REPLACE,
            email TEXT UNIQUE ON CONFLICT IGNORE,
            name TEXT CONSTRAINT nn NOT NULL ON CONFLICT FAIL,
            nickname TEXT,
            CONSTRAINT uq UNIQUE(name, nickname) ON CONFLICT ABORT
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer)
                    .with_primary_key_conflict_clause("REPLACE"),
                ColumnDef::new("email", ColumnType::Text)
                    .unique(true)
                    .with_unique_conflict_clause("IGNORE"),
                ColumnDef::new("name", ColumnType::Text)
                    .nullable(false)
                    .with_not_null_name("nn")
                    .with_not_null_conflict_clause("FAIL"),
                ColumnDef::new("nickname", ColumnType::Text),
            ],
            constraints: vec![TableConstraint::Unique(
                rustsql::common::types::UniqueConstraint::new(vec![
                    "name".to_string(),
                    "nickname".to_string(),
                ])
                .named("uq")
                .with_decorated_columns(vec!["name".to_string(), "nickname".to_string(),])
                .with_conflict_clause("ABORT"),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_preserving_named_foreign_keys() {
    let statements = parse_sql(
        "CREATE TABLE posts (
            id INTEGER PRIMARY KEY,
            user_id INTEGER CONSTRAINT fk_user REFERENCES users(id),
            author_id INTEGER,
            CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id)
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "posts".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("user_id", ColumnType::Integer)
                    .references("users", "id")
                    .with_foreign_key_name("fk_user"),
                ColumnDef::new("author_id", ColumnType::Integer),
            ],
            constraints: vec![TableConstraint::ForeignKey(
                rustsql::common::types::ForeignKey::single_column("author_id", "users", "id")
                    .named("fk_author"),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_decorated_table_constraint_columns() {
    let statements = parse_sql(
        "CREATE TABLE users (
            name TEXT,
            email TEXT,
            CONSTRAINT uq UNIQUE(name COLLATE NOCASE DESC, email ASC),
            PRIMARY KEY(name COLLATE BINARY ASC)
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::new("name", ColumnType::Text),
                ColumnDef::new("email", ColumnType::Text),
            ],
            constraints: vec![
                TableConstraint::Unique(
                    rustsql::common::types::UniqueConstraint::new(vec![
                        "name".to_string(),
                        "email".to_string(),
                    ])
                    .named("uq")
                    .with_decorated_columns(vec![
                        "name COLLATE NOCASE DESC".to_string(),
                        "email ASC".to_string(),
                    ]),
                ),
                TableConstraint::PrimaryKey(
                    rustsql::common::types::PrimaryKeyConstraint::new(vec!["name".to_string()])
                        .with_decorated_columns(vec!["name COLLATE BINARY ASC".to_string()]),
                ),
            ],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_with_primary_key_on_conflict_clause() {
    let statements = parse_sql(
        "CREATE TABLE users (
            name TEXT,
            email TEXT,
            PRIMARY KEY(name, email) ON CONFLICT FAIL
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::new("name", ColumnType::Text),
                ColumnDef::new("email", ColumnType::Text),
            ],
            constraints: vec![TableConstraint::PrimaryKey(
                rustsql::common::types::PrimaryKeyConstraint::new(vec![
                    "name".to_string(),
                    "email".to_string(),
                ])
                .with_decorated_columns(vec!["name".to_string(), "email".to_string(),])
                .with_conflict_clause("FAIL"),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_table_preserving_named_unique_constraints() {
    let statements = parse_sql(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name TEXT,
            nickname TEXT,
            CONSTRAINT uq_user_names UNIQUE(name, nickname)
        );",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text),
                ColumnDef::new("nickname", ColumnType::Text),
            ],
            constraints: vec![TableConstraint::Unique(
                rustsql::common::types::UniqueConstraint::new(vec![
                    "name".to_string(),
                    "nickname".to_string(),
                ])
                .named("uq_user_names")
                .with_decorated_columns(vec!["name".to_string(), "nickname".to_string(),]),
            )],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_alter_table_variants() {
    let statements = parse_sql(
        "ALTER TABLE users ADD COLUMN age INTEGER DEFAULT 0;
         ALTER TABLE users RENAME TO customers;
         ALTER TABLE customers RENAME COLUMN name TO full_name;
         ALTER TABLE customers DROP COLUMN age;",
    )
    .unwrap();

    assert_eq!(statements.len(), 4);
    assert_eq!(
        statements[0],
        Statement::AlterTable {
            table: "users".to_string(),
            action: AlterTableAction::AddColumn(
                ColumnDef::new("age", ColumnType::Integer)
                    .default_value(ColumnDefault::Literal(Value::Integer(0)))
            ),
        }
    );
    assert_eq!(
        statements[1],
        Statement::AlterTable {
            table: "users".to_string(),
            action: AlterTableAction::RenameTable {
                new_name: "customers".to_string(),
            },
        }
    );
    assert_eq!(
        statements[2],
        Statement::AlterTable {
            table: "customers".to_string(),
            action: AlterTableAction::RenameColumn {
                old_name: "name".to_string(),
                new_name: "full_name".to_string(),
            },
        }
    );
    assert_eq!(
        statements[3],
        Statement::AlterTable {
            table: "customers".to_string(),
            action: AlterTableAction::DropColumn {
                old_name: "age".to_string(),
            },
        }
    );
}

#[test]
fn parses_not_null_column_constraint() {
    let statements =
        parse_sql("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text).nullable(false),
            ],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_index_statement() {
    let statements = parse_sql("CREATE INDEX idx_users_id ON users (id);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateIndex {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string()],
            decorated_columns: Some(vec!["id".to_string()]),
            unique: false,
            predicate: None,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_index_if_not_exists_statement() {
    let statements = parse_sql("CREATE INDEX IF NOT EXISTS idx_users_id ON users (id);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateIndex {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string()],
            decorated_columns: Some(vec!["id".to_string()]),
            unique: false,
            predicate: None,
            if_not_exists: true,
        }]
    );
}

#[test]
fn parses_create_unique_index_statement() {
    let statements = parse_sql("CREATE UNIQUE INDEX idx_users_email ON users (email);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateIndex {
            name: "idx_users_email".to_string(),
            table: "users".to_string(),
            columns: vec!["email".to_string()],
            decorated_columns: Some(vec!["email".to_string()]),
            unique: true,
            predicate: None,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_multi_column_create_index_statement() {
    let statements = parse_sql("CREATE INDEX idx_users_id_name ON users (id, name);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateIndex {
            name: "idx_users_id_name".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            decorated_columns: Some(vec!["id".to_string(), "name".to_string()]),
            unique: false,
            predicate: None,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_index_statement_with_column_sort_order() {
    let statements =
        parse_sql("CREATE INDEX idx_users_name_age ON users (name DESC, age ASC);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateIndex {
            name: "idx_users_name_age".to_string(),
            table: "users".to_string(),
            columns: vec!["name".to_string(), "age".to_string()],
            decorated_columns: Some(vec!["name DESC".to_string(), "age ASC".to_string(),]),
            unique: false,
            predicate: None,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_create_index_statement_with_expression_term() {
    let statements =
        parse_sql("CREATE INDEX idx_users_lower_name ON users (lower(name));").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateIndex {
            name: "idx_users_lower_name".to_string(),
            table: "users".to_string(),
            columns: vec!["lower(name)".to_string()],
            decorated_columns: Some(vec!["lower(name)".to_string()]),
            unique: false,
            predicate: None,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_three_column_create_index_statement() {
    let statements =
        parse_sql("CREATE INDEX idx_users_id_name_active ON users (id, name, active);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::CreateIndex {
            name: "idx_users_id_name_active".to_string(),
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string(), "active".to_string()],
            decorated_columns: Some(vec![
                "id".to_string(),
                "name".to_string(),
                "active".to_string(),
            ]),
            unique: false,
            predicate: None,
            if_not_exists: false,
        }]
    );
}

#[test]
fn parses_drop_table_and_drop_index_statements() {
    let statements = parse_sql("DROP INDEX idx_users_name; DROP TABLE users;").unwrap();

    assert_eq!(
        statements,
        vec![
            Statement::DropIndex {
                name: "idx_users_name".to_string(),
                if_exists: false,
            },
            Statement::DropTable {
                name: "users".to_string(),
                if_exists: false,
            },
        ]
    );
}

#[test]
fn parses_drop_if_exists_statements() {
    let statements =
        parse_sql("DROP INDEX IF EXISTS idx_users_name; DROP TABLE IF EXISTS users;").unwrap();

    assert_eq!(
        statements,
        vec![
            Statement::DropIndex {
                name: "idx_users_name".to_string(),
                if_exists: true,
            },
            Statement::DropTable {
                name: "users".to_string(),
                if_exists: true,
            },
        ]
    );
}

#[test]
fn rejects_empty_create_index_column_list() {
    let error = parse_sql("CREATE INDEX idx_users_empty ON users (); ").unwrap_err();

    assert!(error.to_string().contains("expected identifier"));
}

#[test]
fn parses_insert_statement() {
    let statements = parse_sql("INSERT INTO users VALUES (1, 'alice');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "users".to_string(),
            columns: None,
            or_conflict: None,
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_insert_statement_with_blob_hex_literal() {
    let statements = parse_sql("INSERT INTO files VALUES (1, X'0001FEFF');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "files".to_string(),
            columns: None,
            or_conflict: None,
            values: vec![Value::Integer(1), Value::Blob(vec![0x00, 0x01, 0xfe, 0xff]),],
        }]
    );
}

#[test]
fn parses_insert_statement_with_multiple_rows() {
    let statements = parse_sql("INSERT INTO users VALUES (1, 'alice'), (2, 'bob');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::InsertMany {
            table: "users".to_string(),
            columns: None,
            or_conflict: None,
            rows: vec![
                vec![Value::Integer(1), Value::Text("alice".to_string())],
                vec![Value::Integer(2), Value::Text("bob".to_string())],
            ],
        }]
    );
}

#[test]
fn parses_top_level_values_query_like_sqlite() {
    let statements = parse_sql("VALUES (1, 'alice'), (2, 'bob');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Values(vec![
            vec![
                ScalarExpr::Literal(Value::Integer(1)),
                ScalarExpr::Literal(Value::from("alice")),
            ],
            vec![
                ScalarExpr::Literal(Value::Integer(2)),
                ScalarExpr::Literal(Value::from("bob")),
            ],
        ])]
    );
}

#[test]
fn parses_values_derived_table_like_sqlite() {
    let statements =
        parse_sql("SELECT v.column1, column2 FROM (VALUES (1, 'alice'), (2, 'bob')) AS v;")
            .unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: None,
            distinct: false,
            columns: vec![
                SelectItem::Column("v.column1".to_string()),
                SelectItem::Column("column2".to_string()),
            ],
            from: FromItem::Values {
                rows: vec![
                    vec![
                        ScalarExpr::Literal(Value::Integer(1)),
                        ScalarExpr::Literal(Value::from("alice")),
                    ],
                    vec![
                        ScalarExpr::Literal(Value::Integer(2)),
                        ScalarExpr::Literal(Value::from("bob")),
                    ],
                ],
                alias: Some("v".to_string()),
                columns: None,
            },
            joins: vec![],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })],
    );
}

#[test]
fn parses_insert_statement_with_scalar_expressions() {
    let statements =
        parse_sql("INSERT INTO users VALUES (1 + 1, LOWER('ALICE')), (3, COALESCE(NULL, 'bob'));")
            .unwrap();

    assert_eq!(
        statements,
        vec![Statement::InsertManyExpr {
            table: "users".to_string(),
            columns: None,
            or_conflict: None,
            rows: vec![
                vec![
                    ScalarExpr::Binary {
                        left: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                        op: ScalarBinaryOp::Add,
                        right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                    },
                    ScalarExpr::Function {
                        func: ScalarFunc::Lower,
                        args: vec![ScalarExpr::Literal(Value::from("ALICE"))],
                    },
                ],
                vec![
                    ScalarExpr::Literal(Value::Integer(3)),
                    ScalarExpr::Function {
                        func: ScalarFunc::Coalesce,
                        args: vec![
                            ScalarExpr::Literal(Value::Null),
                            ScalarExpr::Literal(Value::from("bob")),
                        ],
                    },
                ],
            ],
        }]
    );
}

#[test]
fn parses_insert_statement_with_unary_plus_and_hex_integer_literals() {
    let statements = parse_sql("INSERT INTO nums VALUES (+1, +.5, 0x10, -0x10);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "nums".to_string(),
            columns: None,
            or_conflict: None,
            values: vec![
                Value::Integer(1),
                Value::Real(0.5),
                Value::Integer(16),
                Value::Integer(-16),
            ],
        }]
    );
}

#[test]
fn parses_insert_statement_with_underscored_numeric_literals() {
    let statements =
        parse_sql("INSERT INTO nums VALUES (1_000, 1_234.5_6, 1_7e+1, 1_704_067_200);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "nums".to_string(),
            columns: None,
            or_conflict: None,
            values: vec![
                Value::Integer(1000),
                Value::Real(1234.56),
                Value::Real(170.0),
                Value::Integer(1_704_067_200),
            ],
        }]
    );
}

#[test]
fn parses_insert_with_explicit_column_list() {
    let statements = parse_sql("INSERT INTO users (id, name) VALUES (1, 'alice');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "users".to_string(),
            columns: Some(vec!["id".to_string(), "name".to_string()]),
            or_conflict: None,
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_insert_or_ignore_statement() {
    let statements = parse_sql("INSERT OR IGNORE INTO users VALUES (1, 'alice');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "users".to_string(),
            columns: None,
            or_conflict: Some("IGNORE".to_string()),
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_insert_on_conflict_do_nothing_statement() {
    let statements =
        parse_sql("INSERT INTO users VALUES (1, 'alice') ON CONFLICT DO NOTHING;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::InsertDoNothing {
            table: "users".to_string(),
            columns: None,
            target: None,
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_insert_on_conflict_target_do_nothing_statement() {
    let statements =
        parse_sql("INSERT INTO users VALUES (1, 'alice') ON CONFLICT(id) DO NOTHING;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::InsertDoNothing {
            table: "users".to_string(),
            columns: None,
            target: Some(vec!["id".to_string()]),
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_insert_or_replace_statement() {
    let statements = parse_sql("INSERT OR REPLACE INTO users VALUES (1, 'alice');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "users".to_string(),
            columns: None,
            or_conflict: Some("REPLACE".to_string()),
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_replace_into_statement() {
    let statements = parse_sql("REPLACE INTO users VALUES (1, 'alice');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "users".to_string(),
            columns: None,
            or_conflict: Some("REPLACE".to_string()),
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_insert_or_rollback_statement() {
    let statements = parse_sql("INSERT OR ROLLBACK INTO users VALUES (1, 'alice');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "users".to_string(),
            columns: None,
            or_conflict: Some("ROLLBACK".to_string()),
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_insert_or_abort_statement() {
    let statements = parse_sql("INSERT OR ABORT INTO users VALUES (1, 'alice');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "users".to_string(),
            columns: None,
            or_conflict: Some("ABORT".to_string()),
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_insert_or_fail_statement() {
    let statements = parse_sql("INSERT OR FAIL INTO users VALUES (1, 'alice');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "users".to_string(),
            columns: None,
            or_conflict: Some("FAIL".to_string()),
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_insert_default_values_statement() {
    let statements = parse_sql("INSERT INTO users DEFAULT VALUES;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "users".to_string(),
            columns: None,
            or_conflict: None,
            values: vec![],
        }]
    );
}

#[test]
fn parses_insert_select_statement() {
    let statements =
        parse_sql("INSERT INTO archive_users SELECT id, name FROM users WHERE id > 10;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::InsertSelect {
            table: "archive_users".to_string(),
            columns: None,
            or_conflict: None,
            select: Box::new(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![
                    SelectItem::Column("id".to_string()),
                    SelectItem::Column("name".to_string()),
                ],
                from: FromItem::Table {
                    name: "users".to_string(),
                    alias: None,
                },
                joins: vec![],
                filter: Some(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Gt,
                    value: Value::Integer(10),
                }),
                group_by: vec![],
                having: None,
                compounds: vec![],
                order_by: vec![],
                limit: None,
                offset: None,
            }),
        }]
    );
}

#[test]
fn parses_insert_select_statement_with_explicit_column_list() {
    let statements =
        parse_sql("INSERT INTO archive_users (id, name) SELECT id, name FROM users WHERE id >= 2;")
            .unwrap();

    assert_eq!(
        statements,
        vec![Statement::InsertSelect {
            table: "archive_users".to_string(),
            columns: Some(vec!["id".to_string(), "name".to_string()]),
            or_conflict: None,
            select: Box::new(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![
                    SelectItem::Column("id".to_string()),
                    SelectItem::Column("name".to_string()),
                ],
                from: FromItem::Table {
                    name: "users".to_string(),
                    alias: None,
                },
                joins: vec![],
                filter: Some(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Gte,
                    value: Value::Integer(2),
                }),
                group_by: vec![],
                having: None,
                compounds: vec![],
                order_by: vec![],
                limit: None,
                offset: None,
            }),
        }]
    );
}

#[test]
fn parses_insert_select_on_conflict_do_nothing_statement() {
    let statements =
        parse_sql("INSERT INTO archive_users SELECT id, name FROM users ON CONFLICT DO NOTHING;")
            .unwrap();

    assert_eq!(
        statements,
        vec![Statement::InsertSelectDoNothing {
            table: "archive_users".to_string(),
            columns: None,
            target: None,
            select: Box::new(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![
                    SelectItem::Column("id".to_string()),
                    SelectItem::Column("name".to_string()),
                ],
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
                offset: None,
            }),
        }]
    );
}

#[test]
fn parses_insert_select_on_conflict_target_do_nothing_statement() {
    let statements = parse_sql(
        "INSERT INTO archive_users SELECT id, name FROM users ON CONFLICT(id) DO NOTHING;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::InsertSelectDoNothing {
            table: "archive_users".to_string(),
            columns: None,
            target: Some(vec!["id".to_string()]),
            select: Box::new(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![
                    SelectItem::Column("id".to_string()),
                    SelectItem::Column("name".to_string()),
                ],
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
                offset: None,
            }),
        }]
    );
}

#[test]
fn parses_replace_into_select_statement() {
    let statements =
        parse_sql("REPLACE INTO archive_users SELECT id, name FROM users WHERE id = 1;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::InsertSelect {
            table: "archive_users".to_string(),
            columns: None,
            or_conflict: Some("REPLACE".to_string()),
            select: Box::new(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![
                    SelectItem::Column("id".to_string()),
                    SelectItem::Column("name".to_string()),
                ],
                from: FromItem::Table {
                    name: "users".to_string(),
                    alias: None,
                },
                joins: vec![],
                filter: Some(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(1),
                }),
                group_by: vec![],
                having: None,
                compounds: vec![],
                order_by: vec![],
                limit: None,
                offset: None,
            }),
        }]
    );
}

#[test]
fn parses_select_statement_with_where_clause() {
    let statements = parse_sql("SELECT id, name FROM users WHERE id = 1;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Column("id".to_string()),
                SelectItem::Column("name".to_string()),
            ],
            "users",
            None,
            Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(1),
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_from_subquery_with_alias() {
    let statements =
        parse_sql("SELECT bucket FROM (SELECT age + 1 AS bucket FROM users) t;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: None,
            distinct: false,
            columns: vec![SelectItem::Column("bucket".to_string())],
            from: FromItem::Subquery {
                query: Box::new(SelectStatement {
                    with: None,
                    distinct: false,
                    columns: vec![SelectItem::Expr {
                        expr: ScalarExpr::Binary {
                            left: Box::new(ScalarExpr::Column("age".to_string())),
                            op: ScalarBinaryOp::Add,
                            right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                        },
                        alias: Some("bucket".to_string()),
                    }],
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
                    offset: None,
                }),
                alias: "t".to_string(),
                columns: None,
            },
            joins: vec![],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })],
    );
}

#[test]
fn parses_from_subquery_without_alias_like_sqlite() {
    let statements =
        parse_sql("SELECT bucket FROM (SELECT age + 1 AS bucket FROM users);").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: None,
            distinct: false,
            columns: vec![SelectItem::Column("bucket".to_string())],
            from: FromItem::Subquery {
                query: Box::new(SelectStatement {
                    with: None,
                    distinct: false,
                    columns: vec![SelectItem::Expr {
                        expr: ScalarExpr::Binary {
                            left: Box::new(ScalarExpr::Column("age".to_string())),
                            op: ScalarBinaryOp::Add,
                            right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                        },
                        alias: Some("bucket".to_string()),
                    }],
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
                    offset: None,
                }),
                alias: String::new(),
                columns: None,
            },
            joins: vec![],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })],
    );
}

#[test]
fn parses_delete_update_order_by_limit_and_aliases() {
    let statements = parse_sql(
        "DELETE FROM users AS u WHERE u.id = 1;
         UPDATE users u SET name = 'bob', active = FALSE WHERE u.id = 2;
         SELECT u.name AS username, u.id user_id FROM users AS u WHERE u.active = TRUE ORDER BY username DESC, u.id ASC LIMIT 5;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![
            Statement::Delete {
                table: "users".to_string(),
                table_alias: Some("u".to_string()),
                filter: Some(Expr::Compare {
                    column: "u.id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(1),
                }),
            },
            Statement::Update {
                table: "users".to_string(),
                table_alias: Some("u".to_string()),
                assignments: vec![
                    Assignment {
                        column: "name".to_string(),
                        value: ScalarExpr::Literal(Value::from("bob")),
                    },
                    Assignment {
                        column: "active".to_string(),
                        value: ScalarExpr::Literal(Value::Boolean(false)),
                    },
                ],
                filter: Some(Expr::Compare {
                    column: "u.id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(2),
                }),
            },
            Statement::Select(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![
                    SelectItem::AliasedColumn {
                        name: "u.name".to_string(),
                        alias: "username".to_string(),
                    },
                    SelectItem::AliasedColumn {
                        name: "u.id".to_string(),
                        alias: "user_id".to_string(),
                    },
                ],
                from: FromItem::Table {
                    name: "users".to_string(),
                    alias: Some("u".to_string()),
                },
                joins: vec![],
                filter: Some(Expr::Compare {
                    column: "u.active".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Boolean(true),
                }),
                group_by: vec![],
                having: None,
                compounds: vec![],
                order_by: vec![
                    OrderBy {
                        expr: OrderByExpr::Column("username".to_string()),
                        collation: None,
                        descending: true,
                        nulls: None,
                    },
                    OrderBy {
                        expr: OrderByExpr::Column("u.id".to_string()),
                        collation: None,
                        descending: false,
                        nulls: None,
                    },
                ],
                limit: Some(5),
                offset: None,
            }),
        ]
    );
}

#[test]
fn parses_string_literal_select_aliases_like_sqlite() {
    let statements = parse_sql("SELECT 1 AS 'one', 2 'two' FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Literal(Value::Integer(1)),
                    alias: Some("one".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Literal(Value::Integer(2)),
                    alias: Some("two".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_boolean_keyword_select_aliases_like_sqlite() {
    let statements = parse_sql("SELECT 1 AS TRUE, 2 FALSE FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Literal(Value::Integer(1)),
                    alias: Some("true".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Literal(Value::Integer(2)),
                    alias: Some("false".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_rollback_keyword_select_alias_like_sqlite() {
    let statements = parse_sql("SELECT 1 AS ROLLBACK FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Expr {
                expr: ScalarExpr::Literal(Value::Integer(1)),
                alias: Some("rollback".to_string()),
            }],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_begin_keyword_select_alias_like_sqlite() {
    let statements = parse_sql("SELECT 1 AS BEGIN FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Expr {
                expr: ScalarExpr::Literal(Value::Integer(1)),
                alias: Some("begin".to_string()),
            }],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_limit_offset_forms_like_sqlite() {
    let explicit_offset =
        single_statement_debug("SELECT id FROM users ORDER BY id ASC LIMIT 2 OFFSET 1;");
    assert!(
        explicit_offset.contains("limit: Some") && explicit_offset.contains("2"),
        "unexpected AST: {explicit_offset}"
    );
    assert!(
        explicit_offset.contains("offset: Some") && explicit_offset.contains("1"),
        "unexpected AST: {explicit_offset}"
    );

    let comma_offset = single_statement_debug("SELECT id FROM users ORDER BY id ASC LIMIT 1, 2;");
    assert!(
        comma_offset.contains("limit: Some") && comma_offset.contains("2"),
        "unexpected AST: {comma_offset}"
    );
    assert!(
        comma_offset.contains("offset: Some") && comma_offset.contains("1"),
        "unexpected AST: {comma_offset}"
    );
}

#[test]
fn parses_integral_real_limit_offset_like_sqlite() {
    let debug =
        single_statement_debug("SELECT id FROM users ORDER BY id ASC LIMIT 2.0 OFFSET 1.0;");

    assert!(
        debug.contains("limit: Some") && debug.contains("2"),
        "unexpected AST: {debug}"
    );
    assert!(
        debug.contains("offset: Some") && debug.contains("1"),
        "unexpected AST: {debug}"
    );
}

#[test]
fn parses_update_assignment_scalar_expression() {
    let statements = parse_sql("UPDATE metrics SET value = value + 1 WHERE id = 2;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Update {
            table: "metrics".to_string(),
            table_alias: None,
            assignments: vec![Assignment {
                column: "value".to_string(),
                value: ScalarExpr::Binary {
                    left: Box::new(ScalarExpr::Column("value".to_string())),
                    op: ScalarBinaryOp::Add,
                    right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                },
            }],
            filter: Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(2),
            }),
        }]
    );
}

#[test]
fn parses_order_by_nulls_first_and_last() {
    let statements =
        parse_sql("SELECT age, name FROM users ORDER BY age NULLS FIRST, name DESC NULLS LAST;")
            .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Column("age".to_string()),
                SelectItem::Column("name".to_string()),
            ],
            "users",
            None,
            None,
            vec![
                OrderBy {
                    expr: OrderByExpr::Column("age".to_string()),
                    collation: None,
                    descending: false,
                    nulls: Some(NullOrder::First),
                },
                OrderBy {
                    expr: OrderByExpr::Column("name".to_string()),
                    collation: None,
                    descending: true,
                    nulls: Some(NullOrder::Last),
                },
            ],
            None,
        )]
    );
}

#[test]
fn parses_order_by_collation_like_sqlite() {
    let statements = parse_sql("SELECT name FROM users ORDER BY name COLLATE NOCASE ASC;").unwrap();

    let debug = format!("{statements:#?}");
    assert!(
        debug.contains("collation: Some") && debug.contains("\"NOCASE\""),
        "unexpected AST: {debug}"
    );
}

#[test]
fn parses_null_ordering_words_as_identifiers() {
    let statements = parse_sql(
        "CREATE TABLE t (first INTEGER, last TEXT, nulls INTEGER);
         SELECT first, last, nulls FROM t ORDER BY nulls NULLS LAST;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![
            Statement::CreateTable {
                name: "t".to_string(),
                columns: vec![
                    ColumnDef::new("first", ColumnType::Integer),
                    ColumnDef::new("last", ColumnType::Text),
                    ColumnDef::new("nulls", ColumnType::Integer),
                ],
                constraints: vec![],
                strict: false,
                without_rowid: false,
                if_not_exists: false,
            },
            select_statement(
                vec![
                    SelectItem::Column("first".to_string()),
                    SelectItem::Column("last".to_string()),
                    SelectItem::Column("nulls".to_string()),
                ],
                "t",
                None,
                None,
                vec![OrderBy {
                    expr: OrderByExpr::Column("nulls".to_string()),
                    collation: None,
                    descending: false,
                    nulls: Some(NullOrder::Last),
                }],
                None,
            ),
        ]
    );
}

#[test]
fn parses_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT LENGTH(name) AS name_len,
                TYPEOF(name) AS name_type,
                CAST(delta AS TEXT) AS delta_text,
                UPPER(LOWER(name)) AS normalized,
                ABS(delta) + 1 AS shifted,
                COALESCE(nickname, name, 'anonymous') AS display_name,
                IFNULL(nickname, name) AS fallback,
                NULLIF(name, nickname) AS maybe_name
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Length,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    alias: Some("name_len".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::TypeOf,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    alias: Some("name_type".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Cast {
                        expr: Box::new(ScalarExpr::Column("delta".to_string())),
                        ty: ColumnType::Text,
                    },
                    alias: Some("delta_text".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Upper,
                        args: vec![ScalarExpr::Function {
                            func: ScalarFunc::Lower,
                            args: vec![ScalarExpr::Column("name".to_string())],
                        }],
                    },
                    alias: Some("normalized".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Binary {
                        left: Box::new(ScalarExpr::Function {
                            func: ScalarFunc::Abs,
                            args: vec![ScalarExpr::Column("delta".to_string())],
                        }),
                        op: ScalarBinaryOp::Add,
                        right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                    },
                    alias: Some("shifted".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Coalesce,
                        args: vec![
                            ScalarExpr::Column("nickname".to_string()),
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Literal(Value::from("anonymous")),
                        ],
                    },
                    alias: Some("display_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::IfNull,
                        args: vec![
                            ScalarExpr::Column("nickname".to_string()),
                            ScalarExpr::Column("name".to_string()),
                        ],
                    },
                    alias: Some("fallback".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::NullIf,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Column("nickname".to_string()),
                        ],
                    },
                    alias: Some("maybe_name".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_min_and_max_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT MAX(age, delta, 10) AS max_value,
                MIN(name, nickname, 'zzz') AS min_text
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::MaxScalar,
                        args: vec![
                            ScalarExpr::Column("age".to_string()),
                            ScalarExpr::Column("delta".to_string()),
                            ScalarExpr::Literal(Value::Integer(10)),
                        ],
                    },
                    alias: Some("max_value".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::MinScalar,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Column("nickname".to_string()),
                            ScalarExpr::Literal(Value::from("zzz")),
                        ],
                    },
                    alias: Some("min_text".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_and_datetime_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT DATE(created_at) AS created_date,
                TIME(created_at) AS created_time,
                DATETIME(created_at) AS created_ts
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![ScalarExpr::Column("created_at".to_string())],
                    },
                    alias: Some("created_date".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Time,
                        args: vec![ScalarExpr::Column("created_at".to_string())],
                    },
                    alias: Some("created_time".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![ScalarExpr::Column("created_at".to_string())],
                    },
                    alias: Some("created_ts".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_current_date_time_and_timestamp_as_sqlite_special_literals() {
    let statements = parse_sql(
        "SELECT CURRENT_DATE AS current_date,
                CURRENT_TIME AS current_time,
                CURRENT_TIMESTAMP AS current_timestamp
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![ScalarExpr::Literal(Value::from("now"))],
                    },
                    alias: Some("current_date".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Time,
                        args: vec![ScalarExpr::Literal(Value::from("now"))],
                    },
                    alias: Some("current_time".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![ScalarExpr::Literal(Value::from("now"))],
                    },
                    alias: Some("current_timestamp".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_strftime_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT STRFTIME('%Y-%m', created_at) AS created_month,
                STRFTIME('%F %T', created_at) AS created_ts
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%Y-%m")),
                            ScalarExpr::Column("created_at".to_string()),
                        ],
                    },
                    alias: Some("created_month".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                        ],
                    },
                    alias: Some("created_ts".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_julianday_scalar_function_expressions() {
    let statements =
        parse_sql("SELECT JULIANDAY(created_at) AS created_jd, JULIANDAY(updated_at) AS updated_jd FROM users;")
            .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![ScalarExpr::Column("created_at".to_string())],
                    },
                    alias: Some("created_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![ScalarExpr::Column("updated_at".to_string())],
                    },
                    alias: Some("updated_jd".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_unixepoch_scalar_function_expressions() {
    let statements =
        parse_sql("SELECT UNIXEPOCH(created_at) AS created_epoch, UNIXEPOCH(updated_at) AS updated_epoch FROM users;")
            .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![ScalarExpr::Column("created_at".to_string())],
                    },
                    alias: Some("created_epoch".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![ScalarExpr::Column("updated_at".to_string())],
                    },
                    alias: Some("updated_epoch".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_day_modifiers() {
    let statements = parse_sql(
        "SELECT DATE(created_at, '+1 day') AS next_day,
                DATETIME(created_at, '-2 day') AS two_days_earlier,
                STRFTIME('%F', created_at, '+1 day') AS shifted_fmt,
                JULIANDAY(created_at, '+1 day') AS shifted_jd,
                UNIXEPOCH(created_at, '+1 day') AS shifted_epoch
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 day")),
                        ],
                    },
                    alias: Some("next_day".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("-2 day")),
                        ],
                    },
                    alias: Some("two_days_earlier".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 day")),
                        ],
                    },
                    alias: Some("shifted_fmt".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 day")),
                        ],
                    },
                    alias: Some("shifted_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 day")),
                        ],
                    },
                    alias: Some("shifted_epoch".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_start_of_day_modifier() {
    let statements = parse_sql(
        "SELECT DATE(created_at, 'start of day') AS day_start,
                DATETIME(created_at, 'start of day') AS datetime_start,
                STRFTIME('%F %T', created_at, 'start of day') AS formatted_start,
                JULIANDAY(created_at, 'start of day') AS jd_start,
                UNIXEPOCH(created_at, 'start of day') AS epoch_start
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of day")),
                        ],
                    },
                    alias: Some("day_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of day")),
                        ],
                    },
                    alias: Some("datetime_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of day")),
                        ],
                    },
                    alias: Some("formatted_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of day")),
                        ],
                    },
                    alias: Some("jd_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of day")),
                        ],
                    },
                    alias: Some("epoch_start".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_hour_modifiers() {
    let statements = parse_sql(
        "SELECT DATE(created_at, '+1 hour') AS next_hour_date,
                DATETIME(created_at, '-2 hour') AS shifted_datetime,
                STRFTIME('%F %T', created_at, '+2 hour') AS shifted_fmt,
                JULIANDAY(created_at, '+2 hour') AS shifted_jd,
                UNIXEPOCH(created_at, '+2 hour') AS shifted_epoch
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 hour")),
                        ],
                    },
                    alias: Some("next_hour_date".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("-2 hour")),
                        ],
                    },
                    alias: Some("shifted_datetime".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+2 hour")),
                        ],
                    },
                    alias: Some("shifted_fmt".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+2 hour")),
                        ],
                    },
                    alias: Some("shifted_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+2 hour")),
                        ],
                    },
                    alias: Some("shifted_epoch".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_minute_modifiers() {
    let statements = parse_sql(
        "SELECT DATE(created_at, '+1 minute') AS next_minute_date,
                DATETIME(created_at, '-2 minute') AS shifted_datetime,
                STRFTIME('%F %T', created_at, '+2 minute') AS shifted_fmt,
                JULIANDAY(created_at, '+2 minute') AS shifted_jd,
                UNIXEPOCH(created_at, '+2 minute') AS shifted_epoch
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 minute")),
                        ],
                    },
                    alias: Some("next_minute_date".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("-2 minute")),
                        ],
                    },
                    alias: Some("shifted_datetime".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+2 minute")),
                        ],
                    },
                    alias: Some("shifted_fmt".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+2 minute")),
                        ],
                    },
                    alias: Some("shifted_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+2 minute")),
                        ],
                    },
                    alias: Some("shifted_epoch".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_second_modifiers() {
    let statements = parse_sql(
        "SELECT DATE(created_at, '+1 second') AS next_second_date,
                DATETIME(created_at, '-2 second') AS shifted_datetime,
                STRFTIME('%F %T', created_at, '+2 second') AS shifted_fmt,
                JULIANDAY(created_at, '+2 second') AS shifted_jd,
                UNIXEPOCH(created_at, '+2 second') AS shifted_epoch
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 second")),
                        ],
                    },
                    alias: Some("next_second_date".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("-2 second")),
                        ],
                    },
                    alias: Some("shifted_datetime".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+2 second")),
                        ],
                    },
                    alias: Some("shifted_fmt".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+2 second")),
                        ],
                    },
                    alias: Some("shifted_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+2 second")),
                        ],
                    },
                    alias: Some("shifted_epoch".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_start_of_month_modifier() {
    let statements = parse_sql(
        "SELECT DATE(created_at, 'start of month') AS month_start,
                DATETIME(created_at, 'start of month') AS datetime_start,
                STRFTIME('%F %T', created_at, 'start of month') AS formatted_start,
                JULIANDAY(created_at, 'start of month') AS jd_start,
                UNIXEPOCH(created_at, 'start of month') AS epoch_start
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of month")),
                        ],
                    },
                    alias: Some("month_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of month")),
                        ],
                    },
                    alias: Some("datetime_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of month")),
                        ],
                    },
                    alias: Some("formatted_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of month")),
                        ],
                    },
                    alias: Some("jd_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of month")),
                        ],
                    },
                    alias: Some("epoch_start".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_start_of_year_modifier() {
    let statements = parse_sql(
        "SELECT DATE(created_at, 'start of year') AS year_start,
                DATETIME(created_at, 'start of year') AS datetime_start,
                STRFTIME('%F %T', created_at, 'start of year') AS formatted_start,
                JULIANDAY(created_at, 'start of year') AS jd_start,
                UNIXEPOCH(created_at, 'start of year') AS epoch_start
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of year")),
                        ],
                    },
                    alias: Some("year_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of year")),
                        ],
                    },
                    alias: Some("datetime_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of year")),
                        ],
                    },
                    alias: Some("formatted_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of year")),
                        ],
                    },
                    alias: Some("jd_start".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of year")),
                        ],
                    },
                    alias: Some("epoch_start".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_month_modifiers() {
    let statements = parse_sql(
        "SELECT DATE(created_at, '+1 month') AS next_month,
                DATETIME(created_at, '-2 month') AS shifted_datetime,
                STRFTIME('%F %T', created_at, '+1 month') AS shifted_fmt,
                JULIANDAY(created_at, '+1 month') AS shifted_jd,
                UNIXEPOCH(created_at, '+1 month') AS shifted_epoch
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 month")),
                        ],
                    },
                    alias: Some("next_month".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("-2 month")),
                        ],
                    },
                    alias: Some("shifted_datetime".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 month")),
                        ],
                    },
                    alias: Some("shifted_fmt".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 month")),
                        ],
                    },
                    alias: Some("shifted_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 month")),
                        ],
                    },
                    alias: Some("shifted_epoch".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_year_modifiers() {
    let statements = parse_sql(
        "SELECT DATE(created_at, '+1 year') AS next_year,
                DATETIME(created_at, '-2 year') AS shifted_datetime,
                STRFTIME('%F %T', created_at, '+1 year') AS shifted_fmt,
                JULIANDAY(created_at, '+1 year') AS shifted_jd,
                UNIXEPOCH(created_at, '+1 year') AS shifted_epoch
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 year")),
                        ],
                    },
                    alias: Some("next_year".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("-2 year")),
                        ],
                    },
                    alias: Some("shifted_datetime".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 year")),
                        ],
                    },
                    alias: Some("shifted_fmt".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 year")),
                        ],
                    },
                    alias: Some("shifted_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("+1 year")),
                        ],
                    },
                    alias: Some("shifted_epoch".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_weekday_modifier() {
    let statements = parse_sql(
        "SELECT DATE(created_at, 'weekday 1') AS next_monday,
                DATETIME(created_at, 'weekday 0') AS next_sunday,
                STRFTIME('%F %T', created_at, 'weekday 1') AS shifted_fmt,
                JULIANDAY(created_at, 'weekday 1') AS shifted_jd,
                UNIXEPOCH(created_at, 'weekday 1') AS shifted_epoch
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("weekday 1")),
                        ],
                    },
                    alias: Some("next_monday".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("weekday 0")),
                        ],
                    },
                    alias: Some("next_sunday".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("weekday 1")),
                        ],
                    },
                    alias: Some("shifted_fmt".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("weekday 1")),
                        ],
                    },
                    alias: Some("shifted_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("weekday 1")),
                        ],
                    },
                    alias: Some("shifted_epoch".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_multiple_modifiers() {
    let statements = parse_sql(
        "SELECT DATE(created_at, 'start of month', '+1 month', '-1 day') AS month_end,
                DATETIME(created_at, 'weekday 1', '+1 day') AS weekday_then_day,
                STRFTIME('%F %T', created_at, 'start of month', '+1 month', '-1 second') AS fmt_shifted,
                JULIANDAY(created_at, 'start of month', '+1 month', '-1 day') AS shifted_jd,
                UNIXEPOCH(created_at, 'start of month', '+1 month', '-1 day') AS shifted_epoch
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of month")),
                            ScalarExpr::Literal(Value::from("+1 month")),
                            ScalarExpr::Literal(Value::from("-1 day")),
                        ],
                    },
                    alias: Some("month_end".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("weekday 1")),
                            ScalarExpr::Literal(Value::from("+1 day")),
                        ],
                    },
                    alias: Some("weekday_then_day".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of month")),
                            ScalarExpr::Literal(Value::from("+1 month")),
                            ScalarExpr::Literal(Value::from("-1 second")),
                        ],
                    },
                    alias: Some("fmt_shifted".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of month")),
                            ScalarExpr::Literal(Value::from("+1 month")),
                            ScalarExpr::Literal(Value::from("-1 day")),
                        ],
                    },
                    alias: Some("shifted_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("start of month")),
                            ScalarExpr::Literal(Value::from("+1 month")),
                            ScalarExpr::Literal(Value::from("-1 day")),
                        ],
                    },
                    alias: Some("shifted_epoch".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_unixepoch_modifier() {
    let statements = parse_sql(
        "SELECT DATE(created_at, 'unixepoch') AS from_epoch_date,
                DATETIME(created_at, 'unixepoch', '+1 day') AS shifted_epoch_dt,
                STRFTIME('%F %T', created_at, 'unixepoch') AS formatted_epoch,
                JULIANDAY(created_at, 'unixepoch') AS epoch_jd,
                UNIXEPOCH(created_at, 'unixepoch') AS epoch_value
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("unixepoch")),
                        ],
                    },
                    alias: Some("from_epoch_date".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("unixepoch")),
                            ScalarExpr::Literal(Value::from("+1 day")),
                        ],
                    },
                    alias: Some("shifted_epoch_dt".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("unixepoch")),
                        ],
                    },
                    alias: Some("formatted_epoch".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("unixepoch")),
                        ],
                    },
                    alias: Some("epoch_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("unixepoch")),
                        ],
                    },
                    alias: Some("epoch_value".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_date_time_functions_with_auto_modifier() {
    let statements = parse_sql(
        "SELECT DATE(created_at, 'auto') AS auto_date,
                DATETIME(created_at, 'auto', '+1 day') AS shifted_auto_dt,
                STRFTIME('%F %T', created_at, 'auto') AS formatted_auto,
                JULIANDAY(created_at, 'auto') AS auto_jd,
                UNIXEPOCH(created_at, 'auto') AS auto_epoch
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Date,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("auto")),
                        ],
                    },
                    alias: Some("auto_date".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::DateTime,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("auto")),
                            ScalarExpr::Literal(Value::from("+1 day")),
                        ],
                    },
                    alias: Some("shifted_auto_dt".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Strftime,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%F %T")),
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("auto")),
                        ],
                    },
                    alias: Some("formatted_auto".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::JulianDay,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("auto")),
                        ],
                    },
                    alias: Some("auto_jd".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::UnixEpoch,
                        args: vec![
                            ScalarExpr::Column("created_at".to_string()),
                            ScalarExpr::Literal(Value::from("auto")),
                        ],
                    },
                    alias: Some("auto_epoch".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_hex_scalar_function_expressions() {
    let statements = parse_sql("SELECT HEX(payload) AS payload_hex FROM files;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Expr {
                expr: ScalarExpr::Function {
                    func: ScalarFunc::Hex,
                    args: vec![ScalarExpr::Column("payload".to_string())],
                },
                alias: Some("payload_hex".to_string()),
            }],
            "files",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_trim_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT TRIM(name) AS trimmed_name,
                LTRIM(name) AS left_trimmed_name,
                RTRIM(name) AS right_trimmed_name,
                TRIM(name, ' x') AS custom_trimmed_name,
                LTRIM(name, ' x') AS custom_left_trimmed_name,
                RTRIM(name, ' x') AS custom_right_trimmed_name
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Trim,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    alias: Some("trimmed_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::LTrim,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    alias: Some("left_trimmed_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::RTrim,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    alias: Some("right_trimmed_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Trim,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Literal(Value::from(" x")),
                        ],
                    },
                    alias: Some("custom_trimmed_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::LTrim,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Literal(Value::from(" x")),
                        ],
                    },
                    alias: Some("custom_left_trimmed_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::RTrim,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Literal(Value::from(" x")),
                        ],
                    },
                    alias: Some("custom_right_trimmed_name".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_substr_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT SUBSTR(name, 2) AS tail_name,
                SUBSTR(name, 2, 3) AS middle_name
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Substr,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Literal(Value::Integer(2)),
                        ],
                    },
                    alias: Some("tail_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Substr,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Literal(Value::Integer(2)),
                            ScalarExpr::Literal(Value::Integer(3)),
                        ],
                    },
                    alias: Some("middle_name".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_instr_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT INSTR(name, 'li') AS li_pos,
                INSTR(name, 'z') AS z_pos
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Instr,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Literal(Value::from("li")),
                        ],
                    },
                    alias: Some("li_pos".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Instr,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Literal(Value::from("z")),
                        ],
                    },
                    alias: Some("z_pos".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_replace_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT REPLACE(name, 'ice', 'ICE') AS replaced_name,
                REPLACE(name, 'x', '') AS stripped_name
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Replace,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Literal(Value::from("ice")),
                            ScalarExpr::Literal(Value::from("ICE")),
                        ],
                    },
                    alias: Some("replaced_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Replace,
                        args: vec![
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Literal(Value::from("x")),
                            ScalarExpr::Literal(Value::from("")),
                        ],
                    },
                    alias: Some("stripped_name".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_quote_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT QUOTE(name) AS quoted_name,
                QUOTE(payload) AS quoted_payload
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Quote,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    alias: Some("quoted_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Quote,
                        args: vec![ScalarExpr::Column("payload".to_string())],
                    },
                    alias: Some("quoted_payload".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_unicode_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT UNICODE(name) AS first_codepoint,
                UNICODE(nickname) AS nickname_codepoint
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Unicode,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    alias: Some("first_codepoint".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Unicode,
                        args: vec![ScalarExpr::Column("nickname".to_string())],
                    },
                    alias: Some("nickname_codepoint".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_char_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT CHAR(65) AS ascii_a,
                CHAR(20320, 22909) AS nihao
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Char,
                        args: vec![ScalarExpr::Literal(Value::Integer(65))],
                    },
                    alias: Some("ascii_a".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Char,
                        args: vec![
                            ScalarExpr::Literal(Value::Integer(20320)),
                            ScalarExpr::Literal(Value::Integer(22909)),
                        ],
                    },
                    alias: Some("nihao".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_zeroblob_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT ZEROBLOB(4) AS blob4,
                ZEROBLOB(0) AS blob0
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::ZeroBlob,
                        args: vec![ScalarExpr::Literal(Value::Integer(4))],
                    },
                    alias: Some("blob4".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::ZeroBlob,
                        args: vec![ScalarExpr::Literal(Value::Integer(0))],
                    },
                    alias: Some("blob0".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_octet_length_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT OCTET_LENGTH(name) AS name_bytes,
                OCTET_LENGTH(payload) AS payload_bytes
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::OctetLength,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    alias: Some("name_bytes".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::OctetLength,
                        args: vec![ScalarExpr::Column("payload".to_string())],
                    },
                    alias: Some("payload_bytes".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_likely_and_unlikely_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT LIKELY(active) AS likely_active,
                UNLIKELY(score) AS unlikely_score
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Likely,
                        args: vec![ScalarExpr::Column("active".to_string())],
                    },
                    alias: Some("likely_active".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Unlikely,
                        args: vec![ScalarExpr::Column("score".to_string())],
                    },
                    alias: Some("unlikely_score".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_likelihood_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT LIKELIHOOD(active, 0.25) AS weighted_active,
                LIKELIHOOD(score, 0.75) AS weighted_score
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Likelihood,
                        args: vec![
                            ScalarExpr::Column("active".to_string()),
                            ScalarExpr::Literal(Value::Real(0.25)),
                        ],
                    },
                    alias: Some("weighted_active".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Likelihood,
                        args: vec![
                            ScalarExpr::Column("score".to_string()),
                            ScalarExpr::Literal(Value::Real(0.75)),
                        ],
                    },
                    alias: Some("weighted_score".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_modulo_scalar_expressions() {
    let statements = parse_sql(
        "SELECT value % 2 AS parity,
                (value + 5) % divisor AS shifted_mod
         FROM numbers;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Binary {
                        left: Box::new(ScalarExpr::Column("value".to_string())),
                        op: ScalarBinaryOp::Modulo,
                        right: Box::new(ScalarExpr::Literal(Value::Integer(2))),
                    },
                    alias: Some("parity".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Binary {
                        left: Box::new(ScalarExpr::Binary {
                            left: Box::new(ScalarExpr::Column("value".to_string())),
                            op: ScalarBinaryOp::Add,
                            right: Box::new(ScalarExpr::Literal(Value::Integer(5))),
                        }),
                        op: ScalarBinaryOp::Modulo,
                        right: Box::new(ScalarExpr::Column("divisor".to_string())),
                    },
                    alias: Some("shifted_mod".to_string()),
                },
            ],
            "numbers",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_mod_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT MOD(value, 2) AS parity,
                MOD(value + 5, divisor) AS shifted_mod
         FROM numbers;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Mod,
                        args: vec![
                            ScalarExpr::Column("value".to_string()),
                            ScalarExpr::Literal(Value::Integer(2)),
                        ],
                    },
                    alias: Some("parity".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Mod,
                        args: vec![
                            ScalarExpr::Binary {
                                left: Box::new(ScalarExpr::Column("value".to_string())),
                                op: ScalarBinaryOp::Add,
                                right: Box::new(ScalarExpr::Literal(Value::Integer(5))),
                            },
                            ScalarExpr::Column("divisor".to_string()),
                        ],
                    },
                    alias: Some("shifted_mod".to_string()),
                },
            ],
            "numbers",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_bitwise_scalar_expressions() {
    let statements = parse_sql(
        "SELECT value & mask AS bit_and,
                value | flag AS bit_or,
                value << shift AS shifted_left,
                value >> shift AS shifted_right,
                ~value AS inverted,
                1 + 2 << 1 AS precedence_check
         FROM numbers;",
    )
    .unwrap();

    let debug = format!("{statements:#?}");
    assert!(debug.contains("BitAnd"), "unexpected AST: {debug}");
    assert!(debug.contains("BitOr"), "unexpected AST: {debug}");
    assert!(debug.contains("ShiftLeft"), "unexpected AST: {debug}");
    assert!(debug.contains("ShiftRight"), "unexpected AST: {debug}");
    assert!(debug.contains("BitNot"), "unexpected AST: {debug}");
}

#[test]
fn parses_sqlite_version_scalar_function_expression() {
    let statements = parse_sql("SELECT SQLITE_VERSION() AS version FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Expr {
                expr: ScalarExpr::Function {
                    func: ScalarFunc::SqliteVersion,
                    args: vec![],
                },
                alias: Some("version".to_string()),
            }],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_sign_scalar_function_expression() {
    let statements = parse_sql("SELECT SIGN(delta) AS delta_sign FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Expr {
                expr: ScalarExpr::Function {
                    func: ScalarFunc::Sign,
                    args: vec![ScalarExpr::Column("delta".to_string())],
                },
                alias: Some("delta_sign".to_string()),
            }],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_rounding_and_pi_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT CEIL(delta) AS ceil_delta,
                CEILING(score) AS ceiling_score,
                FLOOR(amount) AS floor_amount,
                TRUNC(offset) AS trunc_offset,
                PI() AS circle_constant
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Ceil,
                        args: vec![ScalarExpr::Column("delta".to_string())],
                    },
                    alias: Some("ceil_delta".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Ceiling,
                        args: vec![ScalarExpr::Column("score".to_string())],
                    },
                    alias: Some("ceiling_score".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Floor,
                        args: vec![ScalarExpr::Column("amount".to_string())],
                    },
                    alias: Some("floor_amount".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Trunc,
                        args: vec![ScalarExpr::Column("offset".to_string())],
                    },
                    alias: Some("trunc_offset".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Pi,
                        args: vec![],
                    },
                    alias: Some("circle_constant".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_math_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT SQRT(delta) AS root_delta,
                POWER(base, exponent) AS powered,
                POW(base, exponent) AS powed,
                EXP(growth) AS exp_growth,
                LN(value) AS natural_log,
                LOG10(value) AS common_log,
                LOG2(value) AS binary_log,
                LOG(base, value) AS base_log,
                DEGREES(angle) AS degrees_angle,
                RADIANS(turns) AS radians_turns
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Sqrt,
                        args: vec![ScalarExpr::Column("delta".to_string())],
                    },
                    alias: Some("root_delta".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Power,
                        args: vec![
                            ScalarExpr::Column("base".to_string()),
                            ScalarExpr::Column("exponent".to_string()),
                        ],
                    },
                    alias: Some("powered".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Power,
                        args: vec![
                            ScalarExpr::Column("base".to_string()),
                            ScalarExpr::Column("exponent".to_string()),
                        ],
                    },
                    alias: Some("powed".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Exp,
                        args: vec![ScalarExpr::Column("growth".to_string())],
                    },
                    alias: Some("exp_growth".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Ln,
                        args: vec![ScalarExpr::Column("value".to_string())],
                    },
                    alias: Some("natural_log".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Log10,
                        args: vec![ScalarExpr::Column("value".to_string())],
                    },
                    alias: Some("common_log".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Log2,
                        args: vec![ScalarExpr::Column("value".to_string())],
                    },
                    alias: Some("binary_log".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Log,
                        args: vec![
                            ScalarExpr::Column("base".to_string()),
                            ScalarExpr::Column("value".to_string()),
                        ],
                    },
                    alias: Some("base_log".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Degrees,
                        args: vec![ScalarExpr::Column("angle".to_string())],
                    },
                    alias: Some("degrees_angle".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Radians,
                        args: vec![ScalarExpr::Column("turns".to_string())],
                    },
                    alias: Some("radians_turns".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_sqlite_source_id_scalar_function_expression() {
    let statements = parse_sql("SELECT SQLITE_SOURCE_ID() AS source_id FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Expr {
                expr: ScalarExpr::Function {
                    func: ScalarFunc::SqliteSourceId,
                    args: vec![],
                },
                alias: Some("source_id".to_string()),
            }],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_sqlite_compileoption_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT SQLITE_COMPILEOPTION_USED('OMIT_LOAD_EXTENSION') AS has_option,
                SQLITE_COMPILEOPTION_GET(0) AS option_name
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::SqliteCompileOptionUsed,
                        args: vec![ScalarExpr::Literal(Value::from("OMIT_LOAD_EXTENSION"))],
                    },
                    alias: Some("has_option".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::SqliteCompileOptionGet,
                        args: vec![ScalarExpr::Literal(Value::Integer(0))],
                    },
                    alias: Some("option_name".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_randomblob_scalar_function_expression() {
    let statements = parse_sql("SELECT RANDOMBLOB(4) AS nonce FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Expr {
                expr: ScalarExpr::Function {
                    func: ScalarFunc::RandomBlob,
                    args: vec![ScalarExpr::Literal(Value::Integer(4))],
                },
                alias: Some("nonce".to_string()),
            }],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_random_scalar_function_expression() {
    let statements = parse_sql("SELECT RANDOM() AS sample FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Expr {
                expr: ScalarExpr::Function {
                    func: ScalarFunc::Random,
                    args: vec![],
                },
                alias: Some("sample".to_string()),
            }],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_unhex_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT UNHEX(code) AS bytes,
                UNHEX(code, '-') AS dashed_bytes
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Unhex,
                        args: vec![ScalarExpr::Column("code".to_string())],
                    },
                    alias: Some("bytes".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Unhex,
                        args: vec![
                            ScalarExpr::Column("code".to_string()),
                            ScalarExpr::Literal(Value::from("-")),
                        ],
                    },
                    alias: Some("dashed_bytes".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_changes_and_total_changes_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT CHANGES() AS last_change_count,
                TOTAL_CHANGES() AS total_change_count
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Changes,
                        args: vec![],
                    },
                    alias: Some("last_change_count".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::TotalChanges,
                        args: vec![],
                    },
                    alias: Some("total_change_count".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_concat_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT CONCAT(first_name, ' ', last_name) AS full_name,
                CONCAT_WS('-', team, role, level) AS team_role
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Concat,
                        args: vec![
                            ScalarExpr::Column("first_name".to_string()),
                            ScalarExpr::Literal(Value::from(" ")),
                            ScalarExpr::Column("last_name".to_string()),
                        ],
                    },
                    alias: Some("full_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::ConcatWs,
                        args: vec![
                            ScalarExpr::Literal(Value::from("-")),
                            ScalarExpr::Column("team".to_string()),
                            ScalarExpr::Column("role".to_string()),
                            ScalarExpr::Column("level".to_string()),
                        ],
                    },
                    alias: Some("team_role".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_aggregate_scalar_expression_arguments() {
    let statements = parse_sql(
        "SELECT SUM(age + 1) AS total, COUNT(DISTINCT age + 1) AS distinct_total FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Aggregate {
                    func: AggregateFunc::Sum,
                    arg: AggregateArg::Expr {
                        expr: ScalarExpr::Binary {
                            left: Box::new(ScalarExpr::Column("age".to_string())),
                            op: ScalarBinaryOp::Add,
                            right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                        },
                        order_by: vec![],
                        distinct: false,
                    },
                    filter: None,
                    alias: Some("total".to_string()),
                },
                SelectItem::Aggregate {
                    func: AggregateFunc::Count,
                    arg: AggregateArg::Expr {
                        expr: ScalarExpr::Binary {
                            left: Box::new(ScalarExpr::Column("age".to_string())),
                            op: ScalarBinaryOp::Add,
                            right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                        },
                        order_by: vec![],
                        distinct: true,
                    },
                    filter: None,
                    alias: Some("distinct_total".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_aggregate_all_modifier_like_sqlite() {
    let statements = parse_sql("SELECT COUNT(ALL age) AS total FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Aggregate {
                func: AggregateFunc::Count,
                arg: AggregateArg::Expr {
                    expr: ScalarExpr::Column("age".to_string()),
                    distinct: false,
                    order_by: vec![],
                },
                filter: None,
                alias: Some("total".to_string()),
            }],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_order_by_scalar_expressions() {
    let statements =
        parse_sql("SELECT name FROM users ORDER BY LENGTH(name) DESC, LOWER(name) ASC NULLS LAST;")
            .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            None,
            vec![
                OrderBy {
                    expr: OrderByExpr::Expr(ScalarExpr::Function {
                        func: ScalarFunc::Length,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    }),
                    collation: None,
                    descending: true,
                    nulls: None,
                },
                OrderBy {
                    expr: OrderByExpr::Expr(ScalarExpr::Function {
                        func: ScalarFunc::Lower,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    }),
                    collation: None,
                    descending: false,
                    nulls: Some(NullOrder::Last),
                },
            ],
            None,
        )]
    );
}

#[test]
fn parses_median_aggregate_like_sqlite() {
    let debug = single_statement_debug("SELECT MEDIAN(score) AS mid_score FROM metrics;");

    assert!(
        debug.contains("Median") && debug.contains("mid_score"),
        "unexpected AST: {debug}"
    );
}

#[test]
fn parses_group_by_and_aggregate_order_by_scalar_expressions() {
    let statements = parse_sql(
        "SELECT age + 1 AS bucket, COUNT(*) AS total
         FROM users
         GROUP BY age + 1
         HAVING bucket > 20
         ORDER BY total + 1 DESC;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: None,
            distinct: false,
            columns: vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Binary {
                        left: Box::new(ScalarExpr::Column("age".to_string())),
                        op: ScalarBinaryOp::Add,
                        right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                    },
                    alias: Some("bucket".to_string()),
                },
                SelectItem::Aggregate {
                    func: AggregateFunc::Count,
                    arg: AggregateArg::Wildcard,
                    filter: None,
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
                op: ScalarBinaryOp::Add,
                right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
            }],
            having: Some(Expr::Compare {
                column: "bucket".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(20),
            }),
            compounds: vec![],
            order_by: vec![OrderBy {
                expr: OrderByExpr::Expr(ScalarExpr::Binary {
                    left: Box::new(ScalarExpr::Column("total".to_string())),
                    op: ScalarBinaryOp::Add,
                    right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                }),
                collation: None,
                descending: true,
                nulls: None,
            }],
            limit: None,
            offset: None,
        })]
    );
}

#[test]
fn parses_count_with_empty_arg_list_like_sqlite() {
    let statements = parse_sql("SELECT COUNT() AS total FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Aggregate {
                func: AggregateFunc::Count,
                arg: AggregateArg::Wildcard,
                filter: None,
                alias: Some("total".to_string()),
            }],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_join_on_scalar_expression() {
    let statements = parse_sql(
        "SELECT u.name, o.amount
         FROM users u
         JOIN orders o ON u.id + 1 = o.user_id;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: None,
            distinct: false,
            columns: vec![
                SelectItem::Column("u.name".to_string()),
                SelectItem::Column("o.amount".to_string()),
            ],
            from: FromItem::Table {
                name: "users".to_string(),
                alias: Some("u".to_string()),
            },
            joins: vec![JoinClause {
                kind: JoinKind::Inner,
                source: FromItem::Table {
                    name: "orders".to_string(),
                    alias: Some("o".to_string()),
                },
                on: Expr::CompareScalar {
                    left: ScalarExpr::Binary {
                        left: Box::new(ScalarExpr::Column("u.id".to_string())),
                        op: ScalarBinaryOp::Add,
                        right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                    },
                    op: CompareOp::Eq,
                    right: ScalarExpr::Column("o.user_id".to_string()),
                },
                using_columns: Vec::new(),
                natural: false,
            }],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })]
    );
}

#[test]
fn parses_cross_join_without_on_like_sqlite() {
    let debug = single_statement_debug(
        "SELECT u.name, o.amount
         FROM users u
         CROSS JOIN orders o
         WHERE u.id = o.user_id;",
    );

    assert!(
        debug.contains("joins: [") && debug.contains("kind: Inner"),
        "unexpected AST: {debug}"
    );
    assert!(
        debug.contains("Literal") && debug.contains("Boolean") && debug.contains("true"),
        "unexpected AST: {debug}"
    );
}

#[test]
fn parses_join_with_derived_source_on_right() {
    let statements = parse_sql(
        "SELECT u.id, t.bucket
         FROM users u
         JOIN (SELECT id, age + 1 AS bucket FROM users) t ON u.id = t.id;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: None,
            distinct: false,
            columns: vec![
                SelectItem::Column("u.id".to_string()),
                SelectItem::Column("t.bucket".to_string()),
            ],
            from: FromItem::Table {
                name: "users".to_string(),
                alias: Some("u".to_string()),
            },
            joins: vec![JoinClause {
                kind: JoinKind::Inner,
                source: FromItem::Subquery {
                    query: Box::new(SelectStatement {
                        with: None,
                        distinct: false,
                        columns: vec![
                            SelectItem::Column("id".to_string()),
                            SelectItem::Expr {
                                expr: ScalarExpr::Binary {
                                    left: Box::new(ScalarExpr::Column("age".to_string())),
                                    op: ScalarBinaryOp::Add,
                                    right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                                },
                                alias: Some("bucket".to_string()),
                            },
                        ],
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
                        offset: None,
                    }),
                    alias: "t".to_string(),
                    columns: None,
                },
                on: Expr::CompareScalar {
                    left: ScalarExpr::Column("u.id".to_string()),
                    op: CompareOp::Eq,
                    right: ScalarExpr::Column("t.id".to_string()),
                },
                using_columns: Vec::new(),
                natural: false,
            }],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })]
    );
}

#[test]
fn parses_join_with_derived_source_on_left() {
    let statements = parse_sql(
        "SELECT t.bucket, u.id
         FROM (SELECT id, age + 1 AS bucket FROM users) t
         JOIN users u ON t.id = u.id;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: None,
            distinct: false,
            columns: vec![
                SelectItem::Column("t.bucket".to_string()),
                SelectItem::Column("u.id".to_string()),
            ],
            from: FromItem::Subquery {
                query: Box::new(SelectStatement {
                    with: None,
                    distinct: false,
                    columns: vec![
                        SelectItem::Column("id".to_string()),
                        SelectItem::Expr {
                            expr: ScalarExpr::Binary {
                                left: Box::new(ScalarExpr::Column("age".to_string())),
                                op: ScalarBinaryOp::Add,
                                right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                            },
                            alias: Some("bucket".to_string()),
                        },
                    ],
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
                    offset: None,
                }),
                alias: "t".to_string(),
                columns: None,
            },
            joins: vec![JoinClause {
                kind: JoinKind::Inner,
                source: FromItem::Table {
                    name: "users".to_string(),
                    alias: Some("u".to_string()),
                },
                on: Expr::CompareScalar {
                    left: ScalarExpr::Column("t.id".to_string()),
                    op: CompareOp::Eq,
                    right: ScalarExpr::Column("u.id".to_string()),
                },
                using_columns: Vec::new(),
                natural: false,
            }],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })]
    );
}

#[test]
fn parses_where_scalar_expression_comparisons() {
    let statements = parse_sql(
        "SELECT name FROM users WHERE LENGTH(name) > 3 AND age + 1 >= 21 AND LOWER(name) = 'alice';",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::And(
                Box::new(Expr::And(
                    Box::new(Expr::CompareScalar {
                        left: ScalarExpr::Function {
                            func: ScalarFunc::Length,
                            args: vec![ScalarExpr::Column("name".to_string())],
                        },
                        op: CompareOp::Gt,
                        right: ScalarExpr::Literal(Value::Integer(3)),
                    }),
                    Box::new(Expr::CompareScalar {
                        left: ScalarExpr::Binary {
                            left: Box::new(ScalarExpr::Column("age".to_string())),
                            op: ScalarBinaryOp::Add,
                            right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                        },
                        op: CompareOp::Gte,
                        right: ScalarExpr::Literal(Value::Integer(21)),
                    }),
                )),
                Box::new(Expr::CompareScalar {
                    left: ScalarExpr::Function {
                        func: ScalarFunc::Lower,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    op: CompareOp::Eq,
                    right: ScalarExpr::Literal(Value::from("alice")),
                }),
            )),
            vec![],
            None,
        )]
    );

    let statements = parse_sql("SELECT name FROM users WHERE (age + 1) >= 21;").unwrap();
    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::CompareScalar {
                left: ScalarExpr::Binary {
                    left: Box::new(ScalarExpr::Column("age".to_string())),
                    op: ScalarBinaryOp::Add,
                    right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                },
                op: CompareOp::Gte,
                right: ScalarExpr::Literal(Value::Integer(21)),
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_row_value_values_in_select_list_as_scalar_in_list() {
    let statements = parse_sql("SELECT (1, 2) IN (VALUES (1, 2), (3, 4));").unwrap();

    let Statement::Select(select) = &statements[0] else {
        panic!("unexpected statement: {statements:#?}");
    };
    let SelectItem::Expr { expr, .. } = &select.columns[0] else {
        panic!("unexpected select item: {:#?}", select.columns[0]);
    };
    let ScalarExpr::InList {
        expr,
        values,
        negated,
    } = expr
    else {
        panic!("unexpected expression: {expr:#?}");
    };

    assert!(!negated);
    assert_eq!(
        expr.as_ref(),
        &ScalarExpr::Tuple(vec![
            ScalarExpr::Literal(Value::Integer(1)),
            ScalarExpr::Literal(Value::Integer(2)),
        ])
    );
    assert_eq!(
        values,
        &vec![
            ScalarExpr::Tuple(vec![
                ScalarExpr::Literal(Value::Integer(1)),
                ScalarExpr::Literal(Value::Integer(2)),
            ]),
            ScalarExpr::Tuple(vec![
                ScalarExpr::Literal(Value::Integer(3)),
                ScalarExpr::Literal(Value::Integer(4)),
            ]),
        ]
    );
}

#[test]
fn parses_scalar_expression_collation_comparisons() {
    let debug = single_statement_debug(
        "SELECT 'A' = 'a' COLLATE NOCASE AS nocase_equal,
                'A' = 'a' COLLATE BINARY AS binary_equal
         FROM users
         WHERE name = 'alice' COLLATE NOCASE;",
    );

    assert!(
        debug.contains("Collate") && debug.contains("\"NOCASE\"") && debug.contains("\"BINARY\""),
        "unexpected AST: {debug}"
    );
}

#[test]
fn parses_where_scalar_expression_is_null() {
    let statements = parse_sql(
        "SELECT name FROM users WHERE COALESCE(nickname, name) IS NOT NULL AND LENGTH(name) IS NOT NULL;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::And(
                Box::new(Expr::IsNullScalar {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Coalesce,
                        args: vec![
                            ScalarExpr::Column("nickname".to_string()),
                            ScalarExpr::Column("name".to_string()),
                        ],
                    },
                    negated: true,
                }),
                Box::new(Expr::IsNullScalar {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Length,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    negated: true,
                }),
            )),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_scalar_expression_isnull_and_notnull_suffixes() {
    let statements = parse_sql(
        "SELECT name
         FROM users
         WHERE COALESCE(nickname, name) ISNULL
            OR LENGTH(name) NOTNULL
            OR active NOT NULL;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::Or(
                Box::new(Expr::Or(
                    Box::new(Expr::IsNullScalar {
                        expr: ScalarExpr::Function {
                            func: ScalarFunc::Coalesce,
                            args: vec![
                                ScalarExpr::Column("nickname".to_string()),
                                ScalarExpr::Column("name".to_string()),
                            ],
                        },
                        negated: false,
                    }),
                    Box::new(Expr::IsNullScalar {
                        expr: ScalarExpr::Function {
                            func: ScalarFunc::Length,
                            args: vec![ScalarExpr::Column("name".to_string())],
                        },
                        negated: true,
                    }),
                )),
                Box::new(Expr::IsNull {
                    column: "active".to_string(),
                    negated: true,
                }),
            )),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_scalar_expression_is_true_and_false() {
    let statements = parse_sql(
        "SELECT name
         FROM users
         WHERE active IS TRUE
           AND COALESCE(nickname, name) IS NOT FALSE;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::And(
                Box::new(Expr::IsBool {
                    expr: ScalarExpr::Column("active".to_string()),
                    value: true,
                    negated: false,
                    explicit: true,
                }),
                Box::new(Expr::IsBool {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Coalesce,
                        args: vec![
                            ScalarExpr::Column("nickname".to_string()),
                            ScalarExpr::Column("name".to_string()),
                        ],
                    },
                    value: false,
                    negated: true,
                    explicit: true,
                }),
            )),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_scalar_not_expression_like_sqlite() {
    let statements = parse_sql(
        "SELECT NOT active AS inactive
         FROM users
         WHERE NOT COALESCE(active, false);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Expr {
                expr: ScalarExpr::Not(Box::new(ScalarExpr::Column("active".to_string()))),
                alias: Some("inactive".to_string()),
            }],
            "users",
            None,
            Some(Expr::Not(Box::new(Expr::IsBool {
                expr: ScalarExpr::Function {
                    func: ScalarFunc::Coalesce,
                    args: vec![
                        ScalarExpr::Column("active".to_string()),
                        ScalarExpr::Literal(Value::Boolean(false)),
                    ],
                },
                value: true,
                negated: false,
                explicit: false,
            }))),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_and_projection_scalar_expression_is_and_is_not() {
    let debug = single_statement_debug(
        "SELECT 1 IS 1 AS same,
                1 IS NOT 2 AS different
         FROM users
         WHERE COALESCE(nickname, name) IS name
           AND active IS NOT 0;",
    );

    assert_eq!(
        debug,
        r#"Select(
    SelectStatement {
        with: None,
        distinct: false,
        columns: [
            Expr {
                expr: Is {
                    left: Literal(
                        Integer(
                            1,
                        ),
                    ),
                    right: Literal(
                        Integer(
                            1,
                        ),
                    ),
                    negated: false,
                },
                alias: Some(
                    "same",
                ),
            },
            Expr {
                expr: Is {
                    left: Literal(
                        Integer(
                            1,
                        ),
                    ),
                    right: Literal(
                        Integer(
                            2,
                        ),
                    ),
                    negated: true,
                },
                alias: Some(
                    "different",
                ),
            },
        ],
        from: Table {
            name: "users",
            alias: None,
        },
        joins: [],
        filter: Some(
            And(
                Is {
                    left: Function {
                        func: Coalesce,
                        args: [
                            Column(
                                "nickname",
                            ),
                            Column(
                                "name",
                            ),
                        ],
                    },
                    right: Column(
                        "name",
                    ),
                    negated: false,
                },
                Is {
                    left: Column(
                        "active",
                    ),
                    right: Literal(
                        Integer(
                            0,
                        ),
                    ),
                    negated: true,
                },
            ),
        ),
        group_by: [],
        having: None,
        compounds: [],
        order_by: [],
        limit: None,
        offset: None,
    },
)"#
    );
}

#[test]
fn parses_is_distinct_from_like_sqlite() {
    let debug = single_statement_debug(
        "SELECT 1 IS DISTINCT FROM NULL AS one_distinct_null,
                NULL IS NOT DISTINCT FROM NULL AS null_not_distinct_null
         FROM users
         WHERE email IS DISTINCT FROM name
           AND active IS NOT DISTINCT FROM 1;",
    );

    assert!(
        debug.contains("one_distinct_null")
            && debug.contains("null_not_distinct_null")
            && debug.contains("negated: true")
            && debug.contains("negated: false"),
        "unexpected AST: {debug}"
    );
}

#[test]
fn parses_where_scalar_expression_like() {
    let statements = parse_sql(
        "SELECT name FROM users WHERE LOWER(name) LIKE 'a%' AND COALESCE(nickname, name) NOT LIKE 'x%';",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::And(
                Box::new(Expr::LikeScalar {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Lower,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    pattern: "a%".to_string(),
                    escape: None,
                    negated: false,
                }),
                Box::new(Expr::LikeScalar {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Coalesce,
                        args: vec![
                            ScalarExpr::Column("nickname".to_string()),
                            ScalarExpr::Column("name".to_string()),
                        ],
                    },
                    pattern: "x%".to_string(),
                    escape: None,
                    negated: true,
                }),
            )),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_scalar_expression_between() {
    let statements = parse_sql(
        "SELECT name FROM users WHERE age + 1 BETWEEN 18 AND 30 AND LENGTH(name) NOT BETWEEN 1 AND 3;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::And(
                Box::new(Expr::BetweenScalar {
                    expr: ScalarExpr::Binary {
                        left: Box::new(ScalarExpr::Column("age".to_string())),
                        op: ScalarBinaryOp::Add,
                        right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                    },
                    low: ScalarExpr::Literal(Value::Integer(18)),
                    high: ScalarExpr::Literal(Value::Integer(30)),
                    negated: false,
                }),
                Box::new(Expr::BetweenScalar {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Length,
                        args: vec![ScalarExpr::Column("name".to_string())],
                    },
                    low: ScalarExpr::Literal(Value::Integer(1)),
                    high: ScalarExpr::Literal(Value::Integer(3)),
                    negated: true,
                }),
            )),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_between_keeps_legacy_column_literal_form() {
    let statements = parse_sql(
        "SELECT name FROM users WHERE age BETWEEN 18 AND 30 AND age NOT BETWEEN 40 AND 50;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::And(
                Box::new(Expr::Between {
                    column: "age".to_string(),
                    low: Value::Integer(18),
                    high: Value::Integer(30),
                    negated: false,
                }),
                Box::new(Expr::Between {
                    column: "age".to_string(),
                    low: Value::Integer(40),
                    high: Value::Integer(50),
                    negated: true,
                }),
            )),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_between_with_scalar_bounds() {
    let statements =
        parse_sql("SELECT name FROM users WHERE age BETWEEN 17 + 1 AND 40 - 10;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::BetweenScalar {
                expr: ScalarExpr::Column("age".to_string()),
                low: ScalarExpr::Binary {
                    left: Box::new(ScalarExpr::Literal(Value::Integer(17))),
                    op: ScalarBinaryOp::Add,
                    right: Box::new(ScalarExpr::Literal(Value::Integer(1))),
                },
                high: ScalarExpr::Binary {
                    left: Box::new(ScalarExpr::Literal(Value::Integer(40))),
                    op: ScalarBinaryOp::Subtract,
                    right: Box::new(ScalarExpr::Literal(Value::Integer(10))),
                },
                negated: false,
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_scalar_expression_in_subquery() {
    let statements = parse_sql(
        "SELECT name FROM users u WHERE COALESCE(alias_id, id) IN (SELECT user_id FROM orders o WHERE o.user_id = u.id AND o.amount >= 100);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            Some("u"),
            Some(Expr::InSubqueryScalar {
                expr: ScalarExpr::Function {
                    func: ScalarFunc::Coalesce,
                    args: vec![
                        ScalarExpr::Column("alias_id".to_string()),
                        ScalarExpr::Column("id".to_string()),
                    ],
                },
                query: Box::new(SelectStatement {
                    with: None,
                    distinct: false,
                    columns: vec![SelectItem::Column("user_id".to_string())],
                    from: FromItem::Table {
                        name: "orders".to_string(),
                        alias: Some("o".to_string()),
                    },
                    joins: vec![],
                    filter: Some(Expr::And(
                        Box::new(Expr::CompareScalar {
                            left: ScalarExpr::Column("o.user_id".to_string()),
                            op: CompareOp::Eq,
                            right: ScalarExpr::Column("u.id".to_string()),
                        }),
                        Box::new(Expr::Compare {
                            column: "o.amount".to_string(),
                            op: CompareOp::Gte,
                            value: Value::Integer(100),
                        }),
                    )),
                    group_by: vec![],
                    having: None,
                    compounds: vec![],
                    order_by: vec![],
                    limit: None,
                    offset: None,
                }),
                negated: false,
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_printf_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT PRINTF('member-%03d-%s', id, name) AS label,
                PRINTF('%04d-', id) AS padded
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Printf,
                        args: vec![
                            ScalarExpr::Literal(Value::from("member-%03d-%s")),
                            ScalarExpr::Column("id".to_string()),
                            ScalarExpr::Column("name".to_string()),
                        ],
                    },
                    alias: Some("label".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Printf,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%04d-")),
                            ScalarExpr::Column("id".to_string()),
                        ],
                    },
                    alias: Some("padded".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_format_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT FORMAT('member-%03d-%s', id, name) AS label,
                FORMAT('%04d-', id) AS padded
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Printf,
                        args: vec![
                            ScalarExpr::Literal(Value::from("member-%03d-%s")),
                            ScalarExpr::Column("id".to_string()),
                            ScalarExpr::Column("name".to_string()),
                        ],
                    },
                    alias: Some("label".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Printf,
                        args: vec![
                            ScalarExpr::Literal(Value::from("%04d-")),
                            ScalarExpr::Column("id".to_string()),
                        ],
                    },
                    alias: Some("padded".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_iif_and_if_scalar_function_expressions() {
    let statements = parse_sql(
        "SELECT IIF(active, name, nickname) AS preferred_name,
                IF(active, role, 'guest') AS display_role
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::IIf,
                        args: vec![
                            ScalarExpr::Column("active".to_string()),
                            ScalarExpr::Column("name".to_string()),
                            ScalarExpr::Column("nickname".to_string()),
                        ],
                    },
                    alias: Some("preferred_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::If,
                        args: vec![
                            ScalarExpr::Column("active".to_string()),
                            ScalarExpr::Column("role".to_string()),
                            ScalarExpr::Literal(Value::from("guest")),
                        ],
                    },
                    alias: Some("display_role".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_case_scalar_expressions() {
    let statements = parse_sql(
        "SELECT CASE WHEN active THEN name ELSE nickname END AS preferred_name,
                CASE role WHEN 'admin' THEN 1 WHEN 'staff' THEN 2 ELSE 0 END AS role_rank
         FROM users;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![
                SelectItem::Expr {
                    expr: ScalarExpr::Case {
                        base: None,
                        when_then_clauses: vec![(
                            ScalarExpr::Column("active".to_string()),
                            ScalarExpr::Column("name".to_string()),
                        )],
                        else_expr: Some(Box::new(ScalarExpr::Column("nickname".to_string()))),
                    },
                    alias: Some("preferred_name".to_string()),
                },
                SelectItem::Expr {
                    expr: ScalarExpr::Case {
                        base: Some(Box::new(ScalarExpr::Column("role".to_string()))),
                        when_then_clauses: vec![
                            (
                                ScalarExpr::Literal(Value::from("admin")),
                                ScalarExpr::Literal(Value::Integer(1)),
                            ),
                            (
                                ScalarExpr::Literal(Value::from("staff")),
                                ScalarExpr::Literal(Value::Integer(2)),
                            ),
                        ],
                        else_expr: Some(Box::new(ScalarExpr::Literal(Value::Integer(0)))),
                    },
                    alias: Some("role_rank".to_string()),
                },
            ],
            "users",
            None,
            None,
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_scalar_expression_glob() {
    let statements = parse_sql(
        "SELECT name FROM users WHERE name GLOB 'a*' AND COALESCE(nickname, name) NOT GLOB 'x*';",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::And(
                Box::new(Expr::Glob {
                    column: "name".to_string(),
                    pattern: "a*".to_string(),
                    negated: false,
                }),
                Box::new(Expr::GlobScalar {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Coalesce,
                        args: vec![
                            ScalarExpr::Column("nickname".to_string()),
                            ScalarExpr::Column("name".to_string()),
                        ],
                    },
                    pattern: "x*".to_string(),
                    negated: true,
                }),
            )),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_in_literal_list() {
    let statements = parse_sql(
        "SELECT name FROM users WHERE id IN (1, 400) AND COALESCE(alias_id, id) NOT IN (2, 3);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::And(
                Box::new(Expr::InList {
                    column: "id".to_string(),
                    values: vec![Value::Integer(1), Value::Integer(400)],
                    negated: false,
                }),
                Box::new(Expr::InListScalar {
                    expr: ScalarExpr::Function {
                        func: ScalarFunc::Coalesce,
                        args: vec![
                            ScalarExpr::Column("alias_id".to_string()),
                            ScalarExpr::Column("id".to_string()),
                        ],
                    },
                    values: vec![
                        ScalarExpr::Literal(Value::Integer(2)),
                        ScalarExpr::Literal(Value::Integer(3)),
                    ],
                    negated: true,
                }),
            )),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_projection_scalar_expression_in_literal_lists() {
    let debug = single_statement_debug(
        "SELECT 1 IN () AS empty_in,
                3 NOT IN (NULL, 2) AS not_in_with_null
         FROM users;",
    );

    assert_eq!(
        debug,
        r#"Select(
    SelectStatement {
        with: None,
        distinct: false,
        columns: [
            Expr {
                expr: InList {
                    expr: Literal(
                        Integer(
                            1,
                        ),
                    ),
                    values: [],
                    negated: false,
                },
                alias: Some(
                    "empty_in",
                ),
            },
            Expr {
                expr: InList {
                    expr: Literal(
                        Integer(
                            3,
                        ),
                    ),
                    values: [
                        Literal(
                            Null,
                        ),
                        Literal(
                            Integer(
                                2,
                            ),
                        ),
                    ],
                    negated: true,
                },
                alias: Some(
                    "not_in_with_null",
                ),
            },
        ],
        from: Table {
            name: "users",
            alias: None,
        },
        joins: [],
        filter: None,
        group_by: [],
        having: None,
        compounds: [],
        order_by: [],
        limit: None,
        offset: None,
    },
)"#
    );
}

#[test]
fn parses_projection_scalar_expression_comparisons() {
    let debug = single_statement_debug(
        "SELECT 1 = 1 AS eq_true,
                2 < 3 AS lt_true,
                NULL <> NULL AS ne_null
         FROM users;",
    );

    assert_eq!(
        debug,
        r#"Select(
    SelectStatement {
        with: None,
        distinct: false,
        columns: [
            Expr {
                expr: Compare {
                    left: Literal(
                        Integer(
                            1,
                        ),
                    ),
                    op: Eq,
                    right: Literal(
                        Integer(
                            1,
                        ),
                    ),
                },
                alias: Some(
                    "eq_true",
                ),
            },
            Expr {
                expr: Compare {
                    left: Literal(
                        Integer(
                            2,
                        ),
                    ),
                    op: Lt,
                    right: Literal(
                        Integer(
                            3,
                        ),
                    ),
                },
                alias: Some(
                    "lt_true",
                ),
            },
            Expr {
                expr: Compare {
                    left: Literal(
                        Null,
                    ),
                    op: Ne,
                    right: Literal(
                        Null,
                    ),
                },
                alias: Some(
                    "ne_null",
                ),
            },
        ],
        from: Table {
            name: "users",
            alias: None,
        },
        joins: [],
        filter: None,
        group_by: [],
        having: None,
        compounds: [],
        order_by: [],
        limit: None,
        offset: None,
    },
)"#
    );
}

#[test]
fn parses_double_equals_comparison_like_sqlite() {
    let debug = single_statement_debug("SELECT 1 == 1 AS eq_true FROM users;");

    assert!(
        debug.contains("op: Eq") && debug.contains("eq_true"),
        "unexpected AST: {debug}"
    );
}

#[test]
fn parses_json_arrow_operators_like_sqlite() {
    let debug = single_statement_debug(
        "SELECT payload -> '$.a' AS json_value,
                payload ->> '$.b' AS sql_value
         FROM events;",
    );

    assert!(
        debug.contains("JsonExtract")
            && debug.contains("JsonExtractText")
            && debug.contains("json_value")
            && debug.contains("sql_value"),
        "unexpected AST: {debug}"
    );
}

#[test]
fn parses_projection_scalar_expression_like_glob_and_between() {
    let debug = single_statement_debug(
        "SELECT 'abc' LIKE 'a%' AS like_true,
                'abc' GLOB 'a*' AS glob_true,
                2 BETWEEN 1 AND 3 AS between_true,
                2 NOT BETWEEN 1 AND 3 AS not_between_false
         FROM users;",
    );

    assert_eq!(
        debug,
        r#"Select(
    SelectStatement {
        with: None,
        distinct: false,
        columns: [
            Expr {
                expr: Like {
                    expr: Literal(
                        Text(
                            "abc",
                        ),
                    ),
                    pattern: "a%",
                    escape: None,
                    negated: false,
                },
                alias: Some(
                    "like_true",
                ),
            },
            Expr {
                expr: Glob {
                    expr: Literal(
                        Text(
                            "abc",
                        ),
                    ),
                    pattern: "a*",
                    negated: false,
                },
                alias: Some(
                    "glob_true",
                ),
            },
            Expr {
                expr: Between {
                    expr: Literal(
                        Integer(
                            2,
                        ),
                    ),
                    low: Literal(
                        Integer(
                            1,
                        ),
                    ),
                    high: Literal(
                        Integer(
                            3,
                        ),
                    ),
                    negated: false,
                },
                alias: Some(
                    "between_true",
                ),
            },
            Expr {
                expr: Between {
                    expr: Literal(
                        Integer(
                            2,
                        ),
                    ),
                    low: Literal(
                        Integer(
                            1,
                        ),
                    ),
                    high: Literal(
                        Integer(
                            3,
                        ),
                    ),
                    negated: true,
                },
                alias: Some(
                    "not_between_false",
                ),
            },
        ],
        from: Table {
            name: "users",
            alias: None,
        },
        joins: [],
        filter: None,
        group_by: [],
        having: None,
        compounds: [],
        order_by: [],
        limit: None,
        offset: None,
    },
)"#
    );
}

#[test]
fn parses_projection_scalar_expression_like_escape() {
    let debug = single_statement_debug(
        "SELECT 'a_' LIKE 'a!_' ESCAPE '!' AS escaped_underscore,
                'a%' NOT LIKE 'a!%' ESCAPE '!' AS escaped_percent_not_like
         FROM users;",
    );

    assert!(
        debug.contains("escape: Some") && debug.contains("\"!\""),
        "unexpected AST: {debug}"
    );
    assert!(
        debug.contains("escaped_underscore") && debug.contains("escaped_percent_not_like"),
        "unexpected AST: {debug}"
    );
}

#[test]
fn parses_where_scalar_expression_not_in_subquery() {
    let statements = parse_sql(
        "SELECT name FROM users u WHERE COALESCE(alias_id, id) NOT IN (SELECT user_id FROM orders o WHERE o.user_id = u.id AND o.amount >= 100);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            Some("u"),
            Some(Expr::InSubqueryScalar {
                expr: ScalarExpr::Function {
                    func: ScalarFunc::Coalesce,
                    args: vec![
                        ScalarExpr::Column("alias_id".to_string()),
                        ScalarExpr::Column("id".to_string()),
                    ],
                },
                query: Box::new(SelectStatement {
                    with: None,
                    distinct: false,
                    columns: vec![SelectItem::Column("user_id".to_string())],
                    from: FromItem::Table {
                        name: "orders".to_string(),
                        alias: Some("o".to_string()),
                    },
                    joins: vec![],
                    filter: Some(Expr::And(
                        Box::new(Expr::CompareScalar {
                            left: ScalarExpr::Column("o.user_id".to_string()),
                            op: CompareOp::Eq,
                            right: ScalarExpr::Column("u.id".to_string()),
                        }),
                        Box::new(Expr::Compare {
                            column: "o.amount".to_string(),
                            op: CompareOp::Gte,
                            value: Value::Integer(100),
                        }),
                    )),
                    group_by: vec![],
                    having: None,
                    compounds: vec![],
                    order_by: vec![],
                    limit: None,
                    offset: None,
                }),
                negated: true,
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_scalar_expression_compare_subquery() {
    let statements = parse_sql(
        "SELECT name FROM users u WHERE COALESCE(alias_id, id) = (SELECT user_id FROM orders o WHERE o.user_id = u.id AND o.amount >= 100);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            Some("u"),
            Some(Expr::CompareSubqueryScalar {
                left: ScalarExpr::Function {
                    func: ScalarFunc::Coalesce,
                    args: vec![
                        ScalarExpr::Column("alias_id".to_string()),
                        ScalarExpr::Column("id".to_string()),
                    ],
                },
                op: CompareOp::Eq,
                query: Box::new(SelectStatement {
                    with: None,
                    distinct: false,
                    columns: vec![SelectItem::Column("user_id".to_string())],
                    from: FromItem::Table {
                        name: "orders".to_string(),
                        alias: Some("o".to_string()),
                    },
                    joins: vec![],
                    filter: Some(Expr::And(
                        Box::new(Expr::CompareScalar {
                            left: ScalarExpr::Column("o.user_id".to_string()),
                            op: CompareOp::Eq,
                            right: ScalarExpr::Column("u.id".to_string()),
                        }),
                        Box::new(Expr::Compare {
                            column: "o.amount".to_string(),
                            op: CompareOp::Gte,
                            value: Value::Integer(100),
                        }),
                    )),
                    group_by: vec![],
                    having: None,
                    compounds: vec![],
                    order_by: vec![],
                    limit: None,
                    offset: None,
                }),
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_column_compare_subquery_as_scalar_form() {
    let statements = parse_sql(
        "SELECT name FROM users u WHERE u.id = (SELECT user_id FROM orders o WHERE o.user_id = u.id AND o.amount >= 100);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            Some("u"),
            Some(Expr::CompareSubqueryScalar {
                left: ScalarExpr::Column("u.id".to_string()),
                op: CompareOp::Eq,
                query: Box::new(SelectStatement {
                    with: None,
                    distinct: false,
                    columns: vec![SelectItem::Column("user_id".to_string())],
                    from: FromItem::Table {
                        name: "orders".to_string(),
                        alias: Some("o".to_string()),
                    },
                    joins: vec![],
                    filter: Some(Expr::And(
                        Box::new(Expr::CompareScalar {
                            left: ScalarExpr::Column("o.user_id".to_string()),
                            op: CompareOp::Eq,
                            right: ScalarExpr::Column("u.id".to_string()),
                        }),
                        Box::new(Expr::Compare {
                            column: "o.amount".to_string(),
                            op: CompareOp::Gte,
                            value: Value::Integer(100),
                        }),
                    )),
                    group_by: vec![],
                    having: None,
                    compounds: vec![],
                    order_by: vec![],
                    limit: None,
                    offset: None,
                }),
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_where_column_column_comparison_as_scalar_form() {
    let statements = parse_sql("SELECT name FROM users WHERE id = alias_id;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::CompareScalar {
                left: ScalarExpr::Column("id".to_string()),
                op: CompareOp::Eq,
                right: ScalarExpr::Column("alias_id".to_string()),
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_boolean_where_expression_with_precedence_and_parentheses() {
    let statements = parse_sql(
        "SELECT id FROM users WHERE name = 'alice' OR active = TRUE AND (id = 1 OR id = 2);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("id".to_string())],
            "users",
            None,
            Some(Expr::Or(
                Box::new(Expr::Compare {
                    column: "name".to_string(),
                    op: CompareOp::Eq,
                    value: Value::from("alice"),
                }),
                Box::new(Expr::And(
                    Box::new(Expr::Compare {
                        column: "active".to_string(),
                        op: CompareOp::Eq,
                        value: Value::Boolean(true),
                    }),
                    Box::new(Expr::Or(
                        Box::new(Expr::Compare {
                            column: "id".to_string(),
                            op: CompareOp::Eq,
                            value: Value::Integer(1),
                        }),
                        Box::new(Expr::Compare {
                            column: "id".to_string(),
                            op: CompareOp::Eq,
                            value: Value::Integer(2),
                        }),
                    )),
                )),
            )),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_select_all_statement() {
    let statements = parse_sql("SELECT * FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Wildcard],
            "users",
            None,
            None,
            vec![],
            None
        )]
    );
}

#[test]
fn parses_transaction_statements() {
    assert_eq!(
        parse_sql("BEGIN;").unwrap(),
        vec![Statement::Begin {
            isolation_level: None,
        }]
    );
    assert_eq!(
        parse_sql("BEGIN ISOLATION LEVEL SERIALIZABLE;").unwrap(),
        vec![Statement::Begin {
            isolation_level: Some(rustsql::sql::ast::IsolationLevel::Serializable),
        }]
    );
    assert_eq!(
        parse_sql("START TRANSACTION ISOLATION LEVEL READ COMMITTED;").unwrap(),
        vec![Statement::Begin {
            isolation_level: Some(rustsql::sql::ast::IsolationLevel::ReadCommitted),
        }]
    );
    assert_eq!(parse_sql("COMMIT;").unwrap(), vec![Statement::Commit]);
    assert_eq!(parse_sql("ROLLBACK;").unwrap(), vec![Statement::Rollback]);
}

#[test]
fn parses_boolean_and_null_literals_in_where_clause() {
    let statements = parse_sql("SELECT active FROM users WHERE active = TRUE").unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("active".to_string())],
            "users",
            None,
            Some(Expr::Compare {
                column: "active".to_string(),
                op: CompareOp::Eq,
                value: Value::Boolean(true),
            }),
            vec![],
            None,
        )]
    );

    let statements = parse_sql("SELECT name FROM users WHERE name = NULL;").unwrap();
    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::Compare {
                column: "name".to_string(),
                op: CompareOp::Eq,
                value: Value::Null,
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn lexes_non_ascii_string_literal_without_utf8_slice_panics() {
    let tokens = lex("INSERT INTO users VALUES ('你好', 1);").unwrap();

    assert_eq!(
        tokens
            .into_iter()
            .map(|token| token.kind)
            .collect::<Vec<_>>(),
        vec![
            TokenKind::Insert,
            TokenKind::Into,
            TokenKind::Identifier("users".to_string()),
            TokenKind::Values,
            TokenKind::LParen,
            TokenKind::String("你好".to_string()),
            TokenKind::Comma,
            TokenKind::Integer(1),
            TokenKind::RParen,
            TokenKind::Semicolon,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn parses_multiple_statements_with_optional_trailing_semicolon() {
    let statements = parse_sql("BEGIN; COMMIT;;").unwrap();

    assert_eq!(
        statements,
        vec![
            Statement::Begin {
                isolation_level: None,
            },
            Statement::Commit,
        ]
    );
}

#[test]
fn parses_comparison_operators() {
    let statements = parse_sql("SELECT id FROM users WHERE id > 1;").unwrap();
    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("id".to_string())],
            "users",
            None,
            Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(1),
            }),
            vec![],
            None,
        )]
    );

    let statements = parse_sql("SELECT id FROM users WHERE id < 9;").unwrap();
    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("id".to_string())],
            "users",
            None,
            Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Lt,
                value: Value::Integer(9),
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn parses_extended_comparison_operators_and_negative_integers() {
    let statements =
        parse_sql("SELECT id FROM users WHERE id >= -1 AND id <= 9 AND id != 3;").unwrap();
    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("id".to_string())],
            "users",
            None,
            Some(Expr::And(
                Box::new(Expr::And(
                    Box::new(Expr::Compare {
                        column: "id".to_string(),
                        op: CompareOp::Gte,
                        value: Value::Integer(-1),
                    }),
                    Box::new(Expr::Compare {
                        column: "id".to_string(),
                        op: CompareOp::Lte,
                        value: Value::Integer(9),
                    }),
                )),
                Box::new(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Ne,
                    value: Value::Integer(3),
                }),
            )),
            vec![],
            None,
        )]
    );

    let statements = parse_sql("SELECT id FROM users WHERE id <> 4;").unwrap();
    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("id".to_string())],
            "users",
            None,
            Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Ne,
                value: Value::Integer(4),
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn lexes_with_and_recursive_keywords() {
    let tokens =
        lex("WITH RECURSIVE recent AS (SELECT id FROM users) SELECT id FROM recent;").unwrap();

    assert_eq!(
        tokens
            .into_iter()
            .map(|token| token.kind)
            .collect::<Vec<_>>(),
        vec![
            TokenKind::With,
            TokenKind::Recursive,
            TokenKind::Identifier("recent".to_string()),
            TokenKind::As,
            TokenKind::LParen,
            TokenKind::Select,
            TokenKind::Identifier("id".to_string()),
            TokenKind::From,
            TokenKind::Identifier("users".to_string()),
            TokenKind::RParen,
            TokenKind::Select,
            TokenKind::Identifier("id".to_string()),
            TokenKind::From,
            TokenKind::Identifier("recent".to_string()),
            TokenKind::Semicolon,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lexes_union_and_union_all_keywords() {
    assert_eq!(
        token_kind_debugs("SELECT id FROM left_src UNION ALL SELECT id FROM right_src;"),
        vec![
            "Select".to_string(),
            "Identifier(\"id\")".to_string(),
            "From".to_string(),
            "Identifier(\"left_src\")".to_string(),
            "Union".to_string(),
            "All".to_string(),
            "Select".to_string(),
            "Identifier(\"id\")".to_string(),
            "From".to_string(),
            "Identifier(\"right_src\")".to_string(),
            "Semicolon".to_string(),
            "Eof".to_string(),
        ]
    );
}

#[test]
fn parses_basic_union_statement() {
    let debug = single_statement_debug("SELECT id FROM left_src UNION SELECT id FROM right_src;");

    assert!(debug.contains("Union"), "unexpected AST: {debug}");
    assert!(debug.contains("left_src"), "unexpected AST: {debug}");
    assert!(debug.contains("right_src"), "unexpected AST: {debug}");
    assert!(
        debug.find("left_src") < debug.find("right_src"),
        "unexpected AST order: {debug}"
    );
}

#[test]
fn parses_union_all_statement() {
    let debug =
        single_statement_debug("SELECT id FROM left_src UNION ALL SELECT id FROM right_src;");

    assert!(debug.contains("Union"), "unexpected AST: {debug}");
    assert!(
        debug.contains("All") || debug.contains("all: true"),
        "unexpected AST: {debug}"
    );
    assert!(debug.contains("left_src"), "unexpected AST: {debug}");
    assert!(debug.contains("right_src"), "unexpected AST: {debug}");
}

#[test]
fn parses_chained_union_and_union_all_preserving_order() {
    let debug = single_statement_debug(
        "SELECT id FROM first_src UNION SELECT id FROM second_src UNION ALL SELECT id FROM third_src;",
    );

    assert!(debug.contains("Union"), "unexpected AST: {debug}");
    assert!(debug.contains("first_src"), "unexpected AST: {debug}");
    assert!(debug.contains("second_src"), "unexpected AST: {debug}");
    assert!(debug.contains("third_src"), "unexpected AST: {debug}");
    assert!(
        debug.find("first_src") < debug.find("second_src")
            && debug.find("second_src") < debug.find("third_src"),
        "unexpected AST order: {debug}"
    );
    assert!(
        debug.contains("All") || debug.contains("all: true"),
        "unexpected AST: {debug}"
    );
}

#[test]
fn parses_intersect_and_except_compound_selects() {
    let debug = single_statement_debug(
        "SELECT id FROM first_src INTERSECT SELECT id FROM second_src EXCEPT SELECT id FROM third_src;",
    );

    assert!(debug.contains("Intersect"), "unexpected AST: {debug}");
    assert!(debug.contains("Except"), "unexpected AST: {debug}");
    assert!(debug.contains("first_src"), "unexpected AST: {debug}");
    assert!(debug.contains("second_src"), "unexpected AST: {debug}");
    assert!(debug.contains("third_src"), "unexpected AST: {debug}");
}

#[test]
fn parses_union_with_outer_order_by_and_limit() {
    let debug = single_statement_debug(
        "SELECT id FROM left_src UNION SELECT id FROM right_src ORDER BY id DESC LIMIT 7;",
    );

    assert!(debug.contains("Union"), "unexpected AST: {debug}");
    assert!(debug.contains("left_src"), "unexpected AST: {debug}");
    assert!(debug.contains("right_src"), "unexpected AST: {debug}");
    assert!(debug.contains("id"), "unexpected AST: {debug}");
    assert!(
        debug.contains("descending: true"),
        "unexpected AST: {debug}"
    );
    assert!(debug.contains("limit: Some"), "unexpected AST: {debug}");
    assert!(debug.contains("7"), "unexpected AST: {debug}");
}

#[test]
fn rejects_dangling_union_without_rhs() {
    let error = parse_sql("SELECT id FROM left_src UNION;").unwrap_err();

    assert!(
        error.to_string().contains("expected"),
        "unexpected error: {error}"
    );
}

#[test]
fn parses_top_level_with_multiple_ctes() {
    let statements = parse_sql(
        "WITH adults AS (SELECT id, name FROM users WHERE age >= 18),
              named AS (SELECT id FROM adults WHERE name IS NOT NULL)
         SELECT id FROM named;",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: Some(WithClause {
                recursive: false,
                ctes: vec![
                    CommonTableExpr {
                        name: "adults".to_string(),
                        columns: None,
                        query: CteBody::Select(Box::new(SelectStatement {
                            with: None,
                            distinct: false,
                            columns: vec![
                                SelectItem::Column("id".to_string()),
                                SelectItem::Column("name".to_string()),
                            ],
                            from: FromItem::Table {
                                name: "users".to_string(),
                                alias: None,
                            },
                            joins: vec![],
                            filter: Some(Expr::Compare {
                                column: "age".to_string(),
                                op: CompareOp::Gte,
                                value: Value::Integer(18),
                            }),
                            group_by: vec![],
                            having: None,
                            compounds: vec![],
                            order_by: vec![],
                            limit: None,
                            offset: None,
                        })),
                    },
                    CommonTableExpr {
                        name: "named".to_string(),
                        columns: None,
                        query: CteBody::Select(Box::new(SelectStatement {
                            with: None,
                            distinct: false,
                            columns: vec![SelectItem::Column("id".to_string())],
                            from: FromItem::Table {
                                name: "adults".to_string(),
                                alias: None,
                            },
                            joins: vec![],
                            filter: Some(Expr::IsNull {
                                column: "name".to_string(),
                                negated: true,
                            }),
                            group_by: vec![],
                            having: None,
                            compounds: vec![],
                            order_by: vec![],
                            limit: None,
                            offset: None,
                        })),
                    },
                ],
            }),
            distinct: false,
            columns: vec![SelectItem::Column("id".to_string())],
            from: FromItem::Table {
                name: "named".to_string(),
                alias: None,
            },
            joins: vec![],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })]
    );
}

#[test]
fn parses_values_cte_like_sqlite() {
    let statements =
        parse_sql("WITH vals AS (VALUES (2, 'bob'), (1, 'alice')) SELECT column1 FROM vals;")
            .unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: Some(WithClause {
                recursive: false,
                ctes: vec![CommonTableExpr {
                    name: "vals".to_string(),
                    columns: None,
                    query: CteBody::Values(vec![
                        vec![
                            ScalarExpr::Literal(Value::Integer(2)),
                            ScalarExpr::Literal(Value::from("bob")),
                        ],
                        vec![
                            ScalarExpr::Literal(Value::Integer(1)),
                            ScalarExpr::Literal(Value::from("alice")),
                        ],
                    ]),
                }],
            }),
            distinct: false,
            columns: vec![SelectItem::Column("column1".to_string())],
            from: FromItem::Table {
                name: "vals".to_string(),
                alias: None,
            },
            joins: vec![],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })],
    );
}

#[test]
fn parses_values_cte_with_column_names_like_sqlite() {
    let statements =
        parse_sql("WITH vals(c1, c2) AS (VALUES (2, 'bob'), (1, 'alice')) SELECT c1 FROM vals;")
            .unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: Some(WithClause {
                recursive: false,
                ctes: vec![CommonTableExpr {
                    name: "vals".to_string(),
                    columns: Some(vec!["c1".to_string(), "c2".to_string()]),
                    query: CteBody::Values(vec![
                        vec![
                            ScalarExpr::Literal(Value::Integer(2)),
                            ScalarExpr::Literal(Value::from("bob")),
                        ],
                        vec![
                            ScalarExpr::Literal(Value::Integer(1)),
                            ScalarExpr::Literal(Value::from("alice")),
                        ],
                    ]),
                }],
            }),
            distinct: false,
            columns: vec![SelectItem::Column("c1".to_string())],
            from: FromItem::Table {
                name: "vals".to_string(),
                alias: None,
            },
            joins: vec![],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })],
    );
}

#[test]
fn parses_with_recursive_non_recursive_cte_like_sqlite() {
    let statements =
        parse_sql("WITH RECURSIVE nums AS (SELECT id FROM users) SELECT id FROM nums;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select(SelectStatement {
            with: Some(WithClause {
                recursive: true,
                ctes: vec![CommonTableExpr {
                    name: "nums".to_string(),
                    columns: None,
                    query: CteBody::Select(Box::new(SelectStatement {
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
                        offset: None,
                    })),
                }],
            }),
            distinct: false,
            columns: vec![SelectItem::Column("id".to_string())],
            from: FromItem::Table {
                name: "nums".to_string(),
                alias: None,
            },
            joins: vec![],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })]
    );
}

#[test]
fn parses_not_and_is_null_expressions_with_precedence() {
    let statements = parse_sql(
        "SELECT id FROM users WHERE NOT active = TRUE OR name IS NULL AND NOT (id <= 3);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("id".to_string())],
            "users",
            None,
            Some(Expr::Or(
                Box::new(Expr::Not(Box::new(Expr::Compare {
                    column: "active".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Boolean(true),
                }))),
                Box::new(Expr::And(
                    Box::new(Expr::IsNull {
                        column: "name".to_string(),
                        negated: false,
                    }),
                    Box::new(Expr::Not(Box::new(Expr::Compare {
                        column: "id".to_string(),
                        op: CompareOp::Lte,
                        value: Value::Integer(3),
                    }))),
                )),
            )),
            vec![],
            None,
        )]
    );

    let statements = parse_sql("SELECT name FROM users WHERE email IS NOT NULL;").unwrap();
    assert_eq!(
        statements,
        vec![select_statement(
            vec![SelectItem::Column("name".to_string())],
            "users",
            None,
            Some(Expr::IsNull {
                column: "email".to_string(),
                negated: true,
            }),
            vec![],
            None,
        )]
    );
}

#[test]
fn lexes_case_insensitive_keywords_and_escaped_quotes() {
    let tokens = lex("select 'it''s ok' FROM users;").unwrap();

    assert_eq!(
        tokens
            .into_iter()
            .map(|token| token.kind)
            .collect::<Vec<_>>(),
        vec![
            TokenKind::Select,
            TokenKind::String("it's ok".to_string()),
            TokenKind::From,
            TokenKind::Identifier("users".to_string()),
            TokenKind::Semicolon,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lexes_extended_operators_and_negative_integer_tokens() {
    let tokens = lex(
        "SELECT * FROM users WHERE id >= -10 AND id <> 3 AND name != 'x' AND email IS NOT NULL;",
    )
    .unwrap();

    assert_eq!(
        tokens
            .into_iter()
            .map(|token| token.kind)
            .collect::<Vec<_>>(),
        vec![
            TokenKind::Select,
            TokenKind::Star,
            TokenKind::From,
            TokenKind::Identifier("users".to_string()),
            TokenKind::Where,
            TokenKind::Identifier("id".to_string()),
            TokenKind::Gte,
            TokenKind::Minus,
            TokenKind::Integer(10),
            TokenKind::And,
            TokenKind::Identifier("id".to_string()),
            TokenKind::Ne,
            TokenKind::Integer(3),
            TokenKind::And,
            TokenKind::Identifier("name".to_string()),
            TokenKind::Ne,
            TokenKind::String("x".to_string()),
            TokenKind::And,
            TokenKind::Identifier("email".to_string()),
            TokenKind::Is,
            TokenKind::Not,
            TokenKind::Null,
            TokenKind::Semicolon,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lexes_scientific_notation_real_literals() {
    let tokens = lex("SELECT 1e3, 1.5e2, -2.5e-1;").unwrap();

    assert_eq!(
        tokens
            .into_iter()
            .map(|token| token.kind)
            .collect::<Vec<_>>(),
        vec![
            TokenKind::Select,
            TokenKind::Real(1000.0),
            TokenKind::Comma,
            TokenKind::Real(150.0),
            TokenKind::Comma,
            TokenKind::Minus,
            TokenKind::Real(0.25),
            TokenKind::Semicolon,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lexes_leading_and_trailing_dot_real_literals() {
    let tokens = lex("SELECT .5, 1., -.25, +.5;").unwrap();

    assert_eq!(
        tokens
            .into_iter()
            .map(|token| token.kind)
            .collect::<Vec<_>>(),
        vec![
            TokenKind::Select,
            TokenKind::Real(0.5),
            TokenKind::Comma,
            TokenKind::Real(1.0),
            TokenKind::Comma,
            TokenKind::Minus,
            TokenKind::Real(0.25),
            TokenKind::Comma,
            TokenKind::Plus,
            TokenKind::Real(0.5),
            TokenKind::Semicolon,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lexes_underscored_numeric_literals() {
    let tokens = lex("SELECT 1_000, 1_234.5_6, 1_7e+1, 1_704_067_200;").unwrap();

    assert_eq!(
        tokens
            .into_iter()
            .map(|token| token.kind)
            .collect::<Vec<_>>(),
        vec![
            TokenKind::Select,
            TokenKind::Integer(1000),
            TokenKind::Comma,
            TokenKind::Real(1234.56),
            TokenKind::Comma,
            TokenKind::Real(170.0),
            TokenKind::Comma,
            TokenKind::Integer(1_704_067_200),
            TokenKind::Semicolon,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lex_skips_sql_comments() {
    let tokens = lex("SELECT 1 -- line comment
         ; SELECT /* block comment */ 2; SELECT 1/*inline*/+/*x*/2;")
    .unwrap();

    assert_eq!(
        tokens
            .into_iter()
            .map(|token| token.kind)
            .collect::<Vec<_>>(),
        vec![
            TokenKind::Select,
            TokenKind::Integer(1),
            TokenKind::Semicolon,
            TokenKind::Select,
            TokenKind::Integer(2),
            TokenKind::Semicolon,
            TokenKind::Select,
            TokenKind::Integer(1),
            TokenKind::Plus,
            TokenKind::Integer(2),
            TokenKind::Semicolon,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lex_reports_unexpected_characters_and_unterminated_strings() {
    let bad_char = lex("SELECT @ FROM users;").unwrap_err();
    assert!(
        bad_char
            .to_string()
            .contains("unexpected character '@' at position 7")
    );

    let unterminated = lex("SELECT 'oops").unwrap_err();
    assert!(
        unterminated
            .to_string()
            .contains("unterminated string literal")
    );

    let unterminated_comment = lex("SELECT /* oops").unwrap_err();
    assert!(
        unterminated_comment
            .to_string()
            .contains("unterminated block comment")
    );
}

#[test]
fn parse_rejects_empty_input_and_missing_column_values() {
    let empty = parse_sql("   ").unwrap_err();
    assert_eq!(empty.to_string(), "sql error: empty SQL input");

    let missing_values = parse_sql("INSERT INTO users VALUES (); ").unwrap_err();
    assert!(
        missing_values
            .to_string()
            .contains("expected literal or scalar expression")
    );
}

#[test]
fn parse_rejects_missing_from_keyword_and_invalid_where_operator() {
    let missing_from = parse_sql("SELECT id users WHERE id = 1;").unwrap_err();
    assert!(missing_from.to_string().contains("expected FROM"));

    let invalid_where = parse_sql("SELECT id FROM users WHERE id ! 1;").unwrap_err();
    assert!(
        invalid_where
            .to_string()
            .contains("unexpected character '!'")
    );
}

#[test]
fn parse_rejects_incomplete_not_is_null_and_bare_minus_literals() {
    let dangling_not = parse_sql("SELECT id FROM users WHERE NOT;").unwrap_err();
    assert!(dangling_not.to_string().contains("expected"));

    let dangling_is = parse_sql("SELECT id FROM users WHERE name IS;").unwrap_err();
    assert!(
        dangling_is
            .to_string()
            .contains("expected scalar expression")
    );

    let bare_minus = parse_sql("SELECT id FROM users WHERE id = -;").unwrap_err();
    assert!(bare_minus.to_string().contains("expected numeric literal"));
}

#[test]
fn parses_group_by_join_and_subquery_forms() {
    let statements = parse_sql(
        "SELECT active, COUNT(*) AS total FROM users GROUP BY active ORDER BY total DESC;
         SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id WHERE o.amount > 10 ORDER BY u.name ASC;
         SELECT name FROM users WHERE id IN (SELECT user_id FROM orders WHERE amount >= 100);",
    )
    .unwrap();

    assert_eq!(
        statements,
        vec![
            Statement::Select(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![
                    SelectItem::Column("active".to_string()),
                    SelectItem::Aggregate {
                        func: AggregateFunc::Count,
                        arg: AggregateArg::Wildcard,
                        filter: None,
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
                having: None,
                compounds: vec![],
                order_by: vec![OrderBy {
                    expr: OrderByExpr::Column("total".to_string()),
                    collation: None,
                    descending: true,
                    nulls: None,
                }],
                limit: None,
                offset: None,
            }),
            Statement::Select(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![
                    SelectItem::Column("u.name".to_string()),
                    SelectItem::Column("o.amount".to_string()),
                ],
                from: FromItem::Table {
                    name: "users".to_string(),
                    alias: Some("u".to_string()),
                },
                joins: vec![JoinClause {
                    kind: JoinKind::Inner,
                    source: FromItem::Table {
                        name: "orders".to_string(),
                        alias: Some("o".to_string()),
                    },
                    on: Expr::CompareScalar {
                        left: ScalarExpr::Column("u.id".to_string()),
                        op: CompareOp::Eq,
                        right: ScalarExpr::Column("o.user_id".to_string()),
                    },
                    using_columns: Vec::new(),
                    natural: false,
                }],
                filter: Some(Expr::Compare {
                    column: "o.amount".to_string(),
                    op: CompareOp::Gt,
                    value: Value::Integer(10),
                }),
                group_by: vec![],
                having: None,
                compounds: vec![],
                order_by: vec![OrderBy {
                    expr: OrderByExpr::Column("u.name".to_string()),
                    collation: None,
                    descending: false,
                    nulls: None,
                }],
                limit: None,
                offset: None,
            }),
            Statement::Select(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![SelectItem::Column("name".to_string())],
                from: FromItem::Table {
                    name: "users".to_string(),
                    alias: None,
                },
                joins: vec![],
                filter: Some(Expr::InSubquery {
                    column: "id".to_string(),
                    query: Box::new(SelectStatement {
                        with: None,
                        distinct: false,
                        columns: vec![SelectItem::Column("user_id".to_string())],
                        from: FromItem::Table {
                            name: "orders".to_string(),
                            alias: None,
                        },
                        joins: vec![],
                        filter: Some(Expr::Compare {
                            column: "amount".to_string(),
                            op: CompareOp::Gte,
                            value: Value::Integer(100),
                        }),
                        group_by: vec![],
                        having: None,
                        compounds: vec![],
                        order_by: vec![],
                        limit: None,
                        offset: None,
                    }),
                    negated: false,
                }),
                group_by: vec![],
                having: None,
                compounds: vec![],
                order_by: vec![],
                limit: None,
                offset: None,
            }),
        ]
    );
}
