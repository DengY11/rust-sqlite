use std::io::Cursor;

use rustsql::db::Database;
use rustsql::repl::{render_rows, run_with_io};

#[test]
fn render_rows_includes_headers_and_values() {
    let output = render_rows(
        &["id".to_string(), "name".to_string()],
        &[
            vec![1_i64.into(), "alice".into()],
            vec![2_i64.into(), "bob".into()],
        ],
    );

    assert!(output.contains("id | name"));
    assert!(output.contains("1 | alice"));
    assert!(output.contains("2 | bob"));
}

#[test]
fn repl_runs_basic_commands_and_quits() {
    let db = Database::memory();
    let input = Cursor::new(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\n\
         INSERT INTO users VALUES (1, 'alice');\n\
         SELECT id, name FROM users;\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("rustsql> "));
    assert!(rendered.contains("ok"));
    assert!(rendered.contains("id | name"));
    assert!(rendered.contains("1 | alice"));
}

#[test]
fn repl_ignores_empty_lines_and_reprompts() {
    let db = Database::memory();
    let input = Cursor::new("\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert_eq!(rendered.matches("rustsql> ").count(), 2);
    assert!(!rendered.contains("error"));
}

#[test]
fn repl_exit_command_quits() {
    let db = Database::memory();
    let input = Cursor::new(".exit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert_eq!(rendered.matches("rustsql> ").count(), 1);
}

#[test]
fn repl_prints_sql_errors_and_continues() {
    let db = Database::memory();
    let input = Cursor::new(
        "BROKEN SQL\n\
         CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("sql error:"));
    assert!(rendered.contains("expected statement"));
    assert!(rendered.contains("ok"));
    assert_eq!(rendered.matches("rustsql> ").count(), 3);
}

#[test]
fn repl_rejects_mixed_query_and_execute_batches_with_execute_error() {
    let db = Database::memory();
    let input = Cursor::new(
        "SELECT id FROM users; INSERT INTO users VALUES (1, 'alice');\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("sql error: SELECT statements must use Database::query"));
    assert!(!rendered.contains("Database::query only accepts SELECT statements"));
}
