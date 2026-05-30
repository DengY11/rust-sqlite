use rustsql::common::types::{ColumnDef, ColumnType, Value};
use rustsql::sql::ast::{
    AggregateArg, AggregateFunc, Assignment, CompareOp, Expr, JoinClause, JoinKind, OrderBy,
    SelectItem, SelectStatement, Statement,
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
        distinct: false,
        columns,
        table: table.to_string(),
        table_alias: table_alias.map(str::to_string),
        joins: vec![],
        filter,
        group_by: vec![],
        having: None,
        order_by,
        limit,
    })
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
        }]
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
        }]
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
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
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
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
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
                        value: Value::from("bob"),
                    },
                    Assignment {
                        column: "active".to_string(),
                        value: Value::Boolean(false),
                    },
                ],
                filter: Some(Expr::Compare {
                    column: "u.id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(2),
                }),
            },
            Statement::Select(SelectStatement {
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
                table: "users".to_string(),
                table_alias: Some("u".to_string()),
                joins: vec![],
                filter: Some(Expr::Compare {
                    column: "u.active".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Boolean(true),
                }),
                group_by: vec![],
                order_by: vec![
                    OrderBy {
                        column: "username".to_string(),
                        descending: true,
                    },
                    OrderBy {
                        column: "u.id".to_string(),
                        descending: false,
                    },
                ],
                limit: Some(5),
                distinct: false,
                having: None,
            }),
        ]
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
    assert_eq!(parse_sql("BEGIN;").unwrap(), vec![Statement::Begin]);
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

    assert_eq!(statements, vec![Statement::Begin, Statement::Commit]);
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
}

#[test]
fn parse_rejects_empty_input_and_missing_column_values() {
    let empty = parse_sql("   ").unwrap_err();
    assert_eq!(empty.to_string(), "sql error: empty SQL input");

    let missing_values = parse_sql("INSERT INTO users VALUES (); ").unwrap_err();
    assert!(missing_values.to_string().contains("expected literal"));
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
    assert!(dangling_not.to_string().contains("expected identifier"));

    let dangling_is = parse_sql("SELECT id FROM users WHERE name IS;").unwrap_err();
    assert!(dangling_is.to_string().contains("expected NULL or NOT"));

    let bare_minus = parse_sql("SELECT id FROM users WHERE id = -;").unwrap_err();
    assert!(bare_minus.to_string().contains("expected integer literal"));
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
                distinct: false,
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
                having: None,
                order_by: vec![OrderBy {
                    column: "total".to_string(),
                    descending: true,
                }],
                limit: None,
            }),
            Statement::Select(SelectStatement {
                distinct: false,
                columns: vec![
                    SelectItem::Column("u.name".to_string()),
                    SelectItem::Column("o.amount".to_string()),
                ],
                table: "users".to_string(),
                table_alias: Some("u".to_string()),
                joins: vec![JoinClause {
                    kind: JoinKind::Inner,
                    table: "orders".to_string(),
                    table_alias: Some("o".to_string()),
                    on: Expr::CompareColumns {
                        left: "u.id".to_string(),
                        op: CompareOp::Eq,
                        right: "o.user_id".to_string(),
                    },
                }],
                filter: Some(Expr::Compare {
                    column: "o.amount".to_string(),
                    op: CompareOp::Gt,
                    value: Value::Integer(10),
                }),
                group_by: vec![],
                having: None,
                order_by: vec![OrderBy {
                    column: "u.name".to_string(),
                    descending: false,
                }],
                limit: None,
            }),
            Statement::Select(SelectStatement {
                distinct: false,
                columns: vec![SelectItem::Column("name".to_string())],
                table: "users".to_string(),
                table_alias: None,
                joins: vec![],
                filter: Some(Expr::InSubquery {
                    column: "id".to_string(),
                    query: Box::new(SelectStatement {
                        distinct: false,
                        columns: vec![SelectItem::Column("user_id".to_string())],
                        table: "orders".to_string(),
                        table_alias: None,
                        joins: vec![],
                        filter: Some(Expr::Compare {
                            column: "amount".to_string(),
                            op: CompareOp::Gte,
                            value: Value::Integer(100),
                        }),
                        group_by: vec![],
                        having: None,
                        order_by: vec![],
                        limit: None,
                    }),
                    negated: false,
                }),
                group_by: vec![],
                having: None,
                order_by: vec![],
                limit: None,
            }),
        ]
    );
}
