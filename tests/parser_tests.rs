use rustsql::common::types::{ColumnDef, ColumnType, Value};
use rustsql::sql::ast::{Expr, SelectItem, Statement};
use rustsql::sql::lexer::{TokenKind, lex};
use rustsql::sql::parser::parse_sql;

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
            column: "id".to_string(),
        }]
    );
}

#[test]
fn rejects_multi_column_create_index_statement() {
    let error = parse_sql("CREATE INDEX idx_users_id_name ON users (id, name);").unwrap_err();

    assert!(error.to_string().contains("expected )"));
}

#[test]
fn parses_insert_statement() {
    let statements = parse_sql("INSERT INTO users VALUES (1, 'alice');").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Insert {
            table: "users".to_string(),
            values: vec![Value::Integer(1), Value::Text("alice".to_string())],
        }]
    );
}

#[test]
fn parses_select_statement_with_where_clause() {
    let statements = parse_sql("SELECT id, name FROM users WHERE id = 1;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select {
            columns: vec![
                SelectItem::Column("id".to_string()),
                SelectItem::Column("name".to_string()),
            ],
            table: "users".to_string(),
            filter: Some(Expr::Eq("id".to_string(), Value::Integer(1))),
        }]
    );
}

#[test]
fn parses_select_all_statement() {
    let statements = parse_sql("SELECT * FROM users;").unwrap();

    assert_eq!(
        statements,
        vec![Statement::Select {
            columns: vec![SelectItem::Wildcard],
            table: "users".to_string(),
            filter: None,
        }]
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
        vec![Statement::Select {
            columns: vec![SelectItem::Column("active".to_string())],
            table: "users".to_string(),
            filter: Some(Expr::Eq("active".to_string(), Value::Boolean(true))),
        }]
    );

    let statements = parse_sql("SELECT name FROM users WHERE name = NULL;").unwrap();
    assert_eq!(
        statements,
        vec![Statement::Select {
            columns: vec![SelectItem::Column("name".to_string())],
            table: "users".to_string(),
            filter: Some(Expr::Eq("name".to_string(), Value::Null)),
        }]
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
        vec![Statement::Select {
            columns: vec![SelectItem::Column("id".to_string())],
            table: "users".to_string(),
            filter: Some(Expr::Gt("id".to_string(), Value::Integer(1))),
        }]
    );

    let statements = parse_sql("SELECT id FROM users WHERE id < 9;").unwrap();
    assert_eq!(
        statements,
        vec![Statement::Select {
            columns: vec![SelectItem::Column("id".to_string())],
            table: "users".to_string(),
            filter: Some(Expr::Lt("id".to_string(), Value::Integer(9))),
        }]
    );
}
