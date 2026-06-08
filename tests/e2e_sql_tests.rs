use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rustsql::common::types::{ForeignKey, Value};
use rustsql::db::Database;
use rustsql::storage::v2::FileStorage as V2FileStorage;
use tempfile::tempdir;

#[test]
fn smoke_database_new_compiles() {
    let _db = Database::new();
}

#[test]
fn database_execute_and_query_run_full_sql_pipeline() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active BOOLEAN);
         CREATE INDEX idx_users_name ON users (name);
         INSERT INTO users VALUES (1, 'alice', true);
         INSERT INTO users VALUES (2, 'bob', false);",
    )
    .unwrap();

    let rows = db
        .query("SELECT * FROM users WHERE name = 'alice';")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("alice"),
            Value::Boolean(true)
        ]]
    );

    let projected = db
        .query("SELECT name, id FROM users WHERE id = 2;")
        .unwrap();
    assert_eq!(projected, vec![vec![Value::from("bob"), Value::Integer(2)]]);
}

#[test]
fn database_preserves_create_table_foreign_key_metadata() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             user_id INTEGER,
             FOREIGN KEY (user_id) REFERENCES users(id)
         );",
    )
    .unwrap();

    let schemas = db.list_schemas().unwrap();
    let orders_schema = schemas
        .iter()
        .find(|schema| schema.name == "orders")
        .expect("orders schema should be stored");

    assert_eq!(
        orders_schema.foreign_keys,
        vec![ForeignKey::single_column("user_id", "users", "id")]
    );
}

#[test]
fn database_preserves_inline_foreign_key_metadata() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             user_id INTEGER REFERENCES users(id)
         );",
    )
    .unwrap();

    let schemas = db.list_schemas().unwrap();
    let orders_schema = schemas
        .iter()
        .find(|schema| schema.name == "orders")
        .expect("orders schema should be stored");
    let user_id_column = orders_schema
        .columns
        .iter()
        .find(|column| column.name == "user_id")
        .expect("user_id column should be stored");

    assert_eq!(
        user_id_column.foreign_key,
        Some(ForeignKey::single_column("user_id", "users", "id"))
    );
    assert_eq!(
        orders_schema.all_foreign_keys(),
        vec![ForeignKey::single_column("user_id", "users", "id")]
    );
}

#[test]
fn database_rejects_check_constraint_that_references_unknown_column() {
    let db = Database::memory();

    let error = db
        .execute("CREATE TABLE users (id INTEGER PRIMARY KEY, CHECK (missing > 0));")
        .unwrap_err();
    let message = error.to_string();

    assert!(
        message.contains("unknown column") || message.contains("CHECK"),
        "unexpected error: {message}"
    );
}

#[test]
fn database_rejects_foreign_key_that_references_unknown_child_column() {
    let db = Database::memory();

    let error = db
        .execute(
            "CREATE TABLE orders (
                id INTEGER PRIMARY KEY,
                FOREIGN KEY (missing) REFERENCES users(id)
            );",
        )
        .unwrap_err();
    let message = error.to_string();

    assert!(
        message.contains("unknown column") || message.contains("FOREIGN KEY"),
        "unexpected error: {message}"
    );
}

#[test]
fn database_rejects_foreign_key_that_references_unknown_parent() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY);")
        .unwrap();

    let missing_column_error = db
        .execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(missing));")
        .unwrap_err();
    let missing_column_message = missing_column_error.to_string();
    assert!(
        missing_column_message.contains("unknown column")
            || missing_column_message.contains("FOREIGN KEY"),
        "unexpected error: {missing_column_message}"
    );

    let missing_table_error = db
        .execute("CREATE TABLE payments (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES missing(id));")
        .unwrap_err();
    let missing_table_message = missing_table_error.to_string();
    assert!(
        missing_table_message.contains("unknown table")
            || missing_table_message.contains("FOREIGN KEY"),
        "unexpected error: {missing_table_message}"
    );
}

#[test]
fn database_explains_optimized_query_plan() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER, email TEXT);
         CREATE TABLE logs (id INTEGER PRIMARY KEY, level TEXT NOT NULL, created_at INTEGER NOT NULL);
         CREATE INDEX idx_users_id ON users (id);
         CREATE INDEX idx_users_age ON users (age);
         CREATE INDEX idx_users_name ON users (name);
         CREATE INDEX idx_users_email ON users (email);
         CREATE INDEX idx_logs_level_created_at ON logs (level, created_at);",
    )
    .unwrap();

    let indexed_plan = db
        .query("EXPLAIN QUERY PLAN SELECT name FROM users WHERE id = 1;")
        .unwrap();
    assert_eq!(indexed_plan.len(), 1);
    assert_eq!(indexed_plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        indexed_plan[0][1],
        Value::from("table=users index=idx_users_id mode=lookup key_prefix=[1]")
    );

    let seq_plan = db
        .query("EXPLAIN QUERY PLAN SELECT name FROM users WHERE name != 'alice';")
        .unwrap();
    assert_eq!(seq_plan.len(), 1);
    assert_eq!(seq_plan[0][0], Value::from("SeqScan"));
    assert_eq!(seq_plan[0][1], Value::from("table=users"));

    let range_plan = db
        .query("EXPLAIN QUERY PLAN SELECT name FROM users WHERE age BETWEEN 18 AND 30;")
        .unwrap();
    assert_eq!(range_plan.len(), 1);
    assert_eq!(range_plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        range_plan[0][1],
        Value::from(
            "table=users index=idx_users_age mode=range key_prefix=[] range=age:Gte 18..Lte 30"
        )
    );

    let like_plan = db
        .query("EXPLAIN QUERY PLAN SELECT name FROM users WHERE name LIKE 'ali%';")
        .unwrap();
    assert_eq!(like_plan.len(), 1);
    assert_eq!(like_plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        like_plan[0][1],
        Value::from(
            "table=users index=idx_users_name mode=range key_prefix=[] range=name:Gte ali..Lt alj"
        )
    );

    let null_plan = db
        .query("EXPLAIN QUERY PLAN SELECT name FROM users WHERE email IS NULL;")
        .unwrap();
    assert_eq!(null_plan.len(), 1);
    assert_eq!(null_plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        null_plan[0][1],
        Value::from("table=users index=idx_users_email mode=lookup key_prefix=[NULL]")
    );

    let prefix_plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM logs WHERE level = 'info';")
        .unwrap();
    assert_eq!(prefix_plan.len(), 1);
    assert_eq!(prefix_plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        prefix_plan[0][1],
        Value::from("table=logs index=idx_logs_level_created_at mode=prefix key_prefix=[info]")
    );
}

#[test]
fn database_explicit_transactions_rollback_and_commit() {
    let db = Database::memory();

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);")
        .unwrap();

    db.execute("BEGIN; INSERT INTO users VALUES (1, 'alice'); ROLLBACK;")
        .unwrap();
    assert_eq!(
        db.query("SELECT * FROM users;").unwrap(),
        Vec::<Vec<Value>>::new()
    );

    db.execute("BEGIN; INSERT INTO users VALUES (2, 'bob'); COMMIT;")
        .unwrap();
    assert_eq!(
        db.query("SELECT * FROM users WHERE id = 2;").unwrap(),
        vec![vec![Value::Integer(2), Value::from("bob")]]
    );

    db.execute("BEGIN ISOLATION LEVEL SERIALIZABLE; INSERT INTO users VALUES (3, 'carol'); COMMIT;")
        .unwrap();
    assert_eq!(
        db.query("SELECT * FROM users WHERE id = 3;").unwrap(),
        vec![vec![Value::Integer(3), Value::from("carol")]]
    );

    db.execute(
        "START TRANSACTION ISOLATION LEVEL READ COMMITTED; INSERT INTO users VALUES (4, 'dave'); ROLLBACK;",
    )
    .unwrap();
    assert_eq!(
        db.query("SELECT * FROM users WHERE id = 4;").unwrap(),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn database_with_storage_v2_serializable_select_blocks_conflicting_insert() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let setup_db = Database::with_storage(V2FileStorage::open(&path).unwrap());

    setup_db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active BOOLEAN);
         CREATE INDEX idx_users_active_name ON users (active, name);",
    )
    .unwrap();

    let (reader_locked_tx, reader_locked_rx) = mpsc::channel();
    let (release_reader_tx, release_reader_rx) = mpsc::channel();
    let (writer_done_tx, writer_done_rx) = mpsc::channel();

    let reader_path = path.clone();
    let reader = thread::spawn(move || {
        let db = Database::with_storage(V2FileStorage::open(&reader_path).unwrap());
        db.execute("BEGIN ISOLATION LEVEL SERIALIZABLE;").unwrap();
        assert_eq!(
            db.query(
                "SELECT id FROM users WHERE active = true AND name >= 'alice' AND name <= 'carol';",
            )
            .unwrap(),
            Vec::<Vec<Value>>::new()
        );
        reader_locked_tx.send(()).unwrap();
        release_reader_rx.recv().unwrap();
        db.execute("ROLLBACK;").unwrap();
    });

    reader_locked_rx.recv().unwrap();

    let writer_path = path.clone();
    let writer = thread::spawn(move || {
        let db = Database::with_storage(V2FileStorage::open(&writer_path).unwrap());
        db.execute("INSERT INTO users VALUES (1, 'bob', true);")
            .unwrap();
        writer_done_tx.send(()).unwrap();
    });

    assert!(writer_done_rx.recv_timeout(Duration::from_millis(150)).is_err());
    release_reader_tx.send(()).unwrap();
    writer_done_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    reader.join().unwrap();
    writer.join().unwrap();
}

#[test]
fn database_execute_rejects_mixed_batches_without_partial_commit() {
    let db = Database::memory();

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);")
        .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (1, 'alice'); SELECT * FROM users;")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sql error: SELECT statements must use Database::query"
    );
    assert_eq!(
        db.query("SELECT * FROM users;").unwrap(),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn database_query_rejects_non_select_batches_without_side_effects() {
    let db = Database::memory();

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);")
        .unwrap();

    let error = db
        .query("INSERT INTO users VALUES (1, 'alice'); SELECT * FROM users;")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sql error: Database::query only accepts SELECT statements"
    );
    assert_eq!(
        db.query("SELECT * FROM users;").unwrap(),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn database_open_runs_persistent_sql_pipeline_across_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let db = Database::open(&path).unwrap();
        db.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active BOOLEAN);
             CREATE INDEX idx_users_name ON users (name);
             INSERT INTO users VALUES (1, 'alice', true);
             BEGIN;
             INSERT INTO users VALUES (2, 'bob', false);
             ROLLBACK;
             BEGIN;
             INSERT INTO users VALUES (3, 'carol', true);
             COMMIT;",
        )
        .unwrap();
    }

    let reopened = Database::open(&path).unwrap();
    assert_eq!(
        reopened
            .query("SELECT * FROM users WHERE name = 'alice';")
            .unwrap(),
        vec![vec![
            Value::Integer(1),
            Value::from("alice"),
            Value::Boolean(true)
        ]]
    );
    assert_eq!(
        reopened
            .query("SELECT id FROM users WHERE name = 'carol';")
            .unwrap(),
        vec![vec![Value::Integer(3)]]
    );
    assert_eq!(
        reopened
            .query("SELECT * FROM users WHERE name = 'bob';")
            .unwrap(),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn binary_repl_uses_database_path_for_persistent_file_mode() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cli.db");

    run_rustsql_binary(
        &path,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\n\
         INSERT INTO users VALUES (1, 'alice');\n\
         .quit\n",
    );

    let output = run_rustsql_binary(&path, "SELECT id, name FROM users;\n.quit\n");
    assert!(output.contains("id | name"));
    assert!(output.contains("1 | alice"));
}

#[test]
fn binary_repl_can_select_experimental_v2_storage_engine() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cli-v2.db");

    run_rustsql_binary_with_args(
        &["--engine", "v2", path.to_str().unwrap()],
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\n\
         CREATE INDEX idx_users_name ON users (name);\n\
         INSERT INTO users VALUES (1, 'alice');\n\
         .quit\n",
    );

    let output = run_rustsql_binary_with_args(
        &["--engine", "v2", path.to_str().unwrap()],
        "SELECT id, name FROM users WHERE name = 'alice';\n.schema\n.quit\n",
    );
    assert!(output.contains("id | name"));
    assert!(output.contains("1 | alice"));
    assert!(output.contains("CREATE INDEX idx_users_name ON users (name);"));
}

#[test]
fn database_supports_drop_index_and_drop_table() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_name ON users (name);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    db.execute("DROP INDEX idx_users_name;").unwrap();
    assert_eq!(db.list_indexes("users").unwrap(), Vec::new());
    assert_eq!(
        db.query("SELECT id FROM users WHERE name = 'alice';")
            .unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    db.execute("DROP TABLE users;").unwrap();
    assert_eq!(db.list_schemas().unwrap(), Vec::new());
    let error = db.query("SELECT * FROM users;").unwrap_err();
    assert!(error.to_string().contains("unknown table: users"));
}

#[test]
fn database_supports_unique_index_constraints() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
         CREATE UNIQUE INDEX idx_users_email ON users (email);
         INSERT INTO users VALUES (1, 'alice@example.com');",
    )
    .unwrap();

    let indexes = db.list_indexes("users").unwrap();
    assert_eq!(indexes.len(), 1);
    assert!(indexes[0].unique);

    let error = db
        .execute("INSERT INTO users VALUES (2, 'alice@example.com');")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "storage error: unique index idx_users_email constraint failed"
    );
    assert_eq!(
        db.query("SELECT id FROM users WHERE email = 'alice@example.com';")
            .unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    db.execute("INSERT INTO users VALUES (3, NULL); INSERT INTO users VALUES (4, NULL);")
        .unwrap();
    assert_eq!(
        db.query("SELECT id FROM users WHERE email IS NULL ORDER BY id ASC;")
            .unwrap(),
        vec![vec![Value::Integer(3)], vec![Value::Integer(4)]]
    );

    db.execute(
        "CREATE TABLE contacts (id INTEGER PRIMARY KEY, email TEXT);
         INSERT INTO contacts VALUES (1, 'dupe@example.com');
         INSERT INTO contacts VALUES (2, 'dupe@example.com');",
    )
    .unwrap();
    let backfill_error = db
        .execute("CREATE UNIQUE INDEX idx_contacts_email ON contacts (email);")
        .unwrap_err();
    assert_eq!(
        backfill_error.to_string(),
        "storage error: unique index idx_contacts_email constraint failed"
    );
}

#[test]
fn database_enforces_default_and_check_constraints() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER DEFAULT 0 CHECK (age >= 0));",
    )
    .unwrap();
    db.execute("INSERT INTO users (id) VALUES (1);").unwrap();
    assert_eq!(
        db.query("SELECT age FROM users WHERE id = 1;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    let error = db.execute("INSERT INTO users VALUES (2, -1);").unwrap_err();
    assert!(error.to_string().contains("check constraint"));
}

#[test]
fn database_applies_defaults_for_missing_insert_columns() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT DEFAULT 'anonymous', active BOOLEAN DEFAULT true);",
    )
    .unwrap();

    db.execute("INSERT INTO users (id) VALUES (1);").unwrap();

    assert_eq!(
        db.query("SELECT name, active FROM users WHERE id = 1;")
            .unwrap(),
        vec![vec![Value::from("anonymous"), Value::Boolean(true)]]
    );
}

#[test]
fn database_enforces_check_constraints_on_insert_and_update() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER CHECK (age >= 0));")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 1);").unwrap();

    let insert_error = db.execute("INSERT INTO users VALUES (2, -1);").unwrap_err();
    assert!(insert_error.to_string().contains("check constraint"));

    let update_error = db
        .execute("UPDATE users SET age = -2 WHERE id = 1;")
        .unwrap_err();
    assert!(update_error.to_string().contains("check constraint"));
}

#[test]
fn database_enforces_basic_foreign_keys() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));
         INSERT INTO users VALUES (1);",
    )
    .unwrap();

    db.execute("INSERT INTO orders VALUES (10, 1);").unwrap();

    let child_error = db
        .execute("INSERT INTO orders VALUES (11, 404);")
        .unwrap_err();
    assert!(child_error.to_string().contains("foreign key constraint"));

    db.execute("INSERT INTO orders VALUES (12, NULL);").unwrap();
    let update_error = db
        .execute("UPDATE orders SET user_id = 404 WHERE id = 12;")
        .unwrap_err();
    assert!(update_error.to_string().contains("foreign key constraint"));
    db.execute("UPDATE orders SET user_id = 1 WHERE id = 12;")
        .unwrap();

    let parent_error = db.execute("DELETE FROM users WHERE id = 1;").unwrap_err();
    assert!(parent_error.to_string().contains("foreign key constraint"));
}

#[test]
fn database_rejects_update_that_would_orphan_foreign_key_child() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));
         INSERT INTO users VALUES (1);
         INSERT INTO orders VALUES (10, 1);",
    )
    .unwrap();

    let error = db
        .execute("UPDATE users SET id = 2 WHERE id = 1;")
        .unwrap_err();
    assert!(error.to_string().contains("foreign key constraint"));

    assert_eq!(
        db.query("SELECT id FROM users ORDER BY id ASC;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        db.query("SELECT user_id FROM orders WHERE id = 10;")
            .unwrap(),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn database_does_not_partially_delete_when_foreign_key_delete_fails_inside_explicit_transaction() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));
         INSERT INTO users VALUES (1);
         INSERT INTO users VALUES (2);
         INSERT INTO orders VALUES (10, 2);",
    )
    .unwrap();

    db.execute("BEGIN;").unwrap();
    let error = db.execute("DELETE FROM users WHERE id >= 1;").unwrap_err();
    assert!(error.to_string().contains("foreign key constraint"));
    db.execute("COMMIT;").unwrap();

    assert_eq!(
        db.query("SELECT id FROM users ORDER BY id ASC;").unwrap(),
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
}

#[test]
fn database_enforces_table_level_foreign_key_on_parent_delete() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             user_id INTEGER,
             FOREIGN KEY (user_id) REFERENCES users(id)
         );
         INSERT INTO users VALUES (1);
         INSERT INTO orders VALUES (10, 1);",
    )
    .unwrap();

    let error = db.execute("DELETE FROM users WHERE id = 1;").unwrap_err();
    assert!(error.to_string().contains("foreign key constraint"));

    assert_eq!(
        db.query("SELECT id FROM users ORDER BY id ASC;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn database_updates_foreign_keys_when_parent_table_is_renamed() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));
         INSERT INTO users VALUES (1);
         INSERT INTO orders VALUES (10, 1);
         ALTER TABLE users RENAME TO people;",
    )
    .unwrap();

    let delete_error = db.execute("DELETE FROM people WHERE id = 1;").unwrap_err();
    assert!(delete_error.to_string().contains("foreign key constraint"));

    db.execute(
        "INSERT INTO people VALUES (2);
         INSERT INTO orders VALUES (11, 2);",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT user_id FROM orders WHERE id = 11;")
            .unwrap(),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn database_updates_foreign_keys_when_parent_column_is_renamed() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));
         INSERT INTO users VALUES (1);
         ALTER TABLE users RENAME COLUMN id TO uid;",
    )
    .unwrap();

    db.execute("INSERT INTO orders VALUES (10, 1);").unwrap();

    let delete_error = db.execute("DELETE FROM users WHERE uid = 1;").unwrap_err();
    assert!(delete_error.to_string().contains("foreign key constraint"));
}

#[test]
fn database_does_not_lose_row_when_update_violates_check_inside_explicit_transaction() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER CHECK (age >= 0));
         INSERT INTO users VALUES (1, 10);",
    )
    .unwrap();

    db.execute("BEGIN;").unwrap();
    let error = db
        .execute("UPDATE users SET age = -1 WHERE id = 1;")
        .unwrap_err();
    assert!(error.to_string().contains("check constraint"));
    db.execute("COMMIT;").unwrap();

    assert_eq!(
        db.query("SELECT age FROM users WHERE id = 1;").unwrap(),
        vec![vec![Value::Integer(10)]]
    );
}

#[test]
fn database_does_not_lose_row_when_update_violates_unique_index_inside_explicit_transaction() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
         CREATE UNIQUE INDEX idx_users_email ON users(email);
         INSERT INTO users VALUES (1, 'a');
         INSERT INTO users VALUES (2, 'b');",
    )
    .unwrap();

    db.execute("BEGIN;").unwrap();
    let error = db
        .execute("UPDATE users SET email = 'b' WHERE id = 1;")
        .unwrap_err();
    assert!(error.to_string().contains("unique index"));
    db.execute("COMMIT;").unwrap();

    assert_eq!(
        db.query("SELECT id, email FROM users ORDER BY id ASC;")
            .unwrap(),
        vec![
            vec![Value::Integer(1), Value::from("a")],
            vec![Value::Integer(2), Value::from("b")],
        ]
    );
}

#[test]
fn database_does_not_lose_row_when_update_violates_primary_key_inside_explicit_transaction() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
         INSERT INTO users VALUES (1, 'a');
         INSERT INTO users VALUES (2, 'b');",
    )
    .unwrap();

    db.execute("BEGIN;").unwrap();
    let error = db
        .execute("UPDATE users SET id = 2 WHERE id = 1;")
        .unwrap_err();
    assert!(error.to_string().contains("duplicate primary key"));
    db.execute("COMMIT;").unwrap();

    assert_eq!(
        db.query("SELECT id, email FROM users ORDER BY id ASC;")
            .unwrap(),
        vec![
            vec![Value::Integer(1), Value::from("a")],
            vec![Value::Integer(2), Value::from("b")],
        ]
    );
}

fn run_rustsql_binary(path: &std::path::Path, input: &str) -> String {
    run_rustsql_binary_with_args(&[path.to_str().unwrap()], input)
}

fn run_rustsql_binary_with_args(args: &[&str], input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rustsql"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "rustsql failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn database_memory_reports_constraint_violations() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    let not_null_error = db
        .execute("INSERT INTO users VALUES (1, NULL);")
        .unwrap_err();
    assert!(
        not_null_error
            .to_string()
            .contains("column 'name' cannot be NULL")
    );

    db.execute("INSERT INTO users VALUES (1, 'alice');")
        .unwrap();

    let duplicate_error = db
        .execute("INSERT INTO users VALUES (1, 'bob');")
        .unwrap_err();
    assert!(
        duplicate_error
            .to_string()
            .contains("duplicate primary key value for column 'id'")
    );
}

#[test]
fn database_open_reports_constraint_violations() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'alice');")
            .unwrap();

        let type_error = db
            .execute("INSERT INTO users VALUES ('oops', 'bob');")
            .unwrap_err();
        assert!(
            type_error
                .to_string()
                .contains("column 'id' expected INTEGER but got TEXT")
        );
    }

    let reopened = Database::open(&path).unwrap();
    assert_eq!(
        reopened.query("SELECT * FROM users;").unwrap(),
        vec![vec![Value::Integer(1), Value::from("alice")]]
    );

    let duplicate_error = reopened
        .execute("INSERT INTO users VALUES (1, 'carol');")
        .unwrap_err();
    assert!(
        duplicate_error
            .to_string()
            .contains("duplicate primary key value for column 'id'")
    );
}

#[test]
fn database_supports_alter_table_add_column_and_renames() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         ALTER TABLE users ADD COLUMN age INTEGER DEFAULT 0;
         ALTER TABLE users RENAME COLUMN name TO full_name;
         ALTER TABLE users RENAME TO customers;",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT full_name, age FROM customers WHERE id = 1;")
            .unwrap(),
        vec![vec![Value::from("alice"), Value::Integer(0)]]
    );
}

#[test]
fn database_rejects_add_column_that_violates_existing_rows() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let not_null_error = db
        .execute("ALTER TABLE users ADD COLUMN age INTEGER NOT NULL;")
        .unwrap_err();
    assert!(not_null_error.to_string().contains("cannot be NULL"));

    assert_eq!(
        db.query("SELECT * FROM users;").unwrap(),
        vec![vec![Value::Integer(1), Value::from("alice")]]
    );

    let check_error = db
        .execute("ALTER TABLE users ADD COLUMN score INTEGER DEFAULT -1 CHECK (score >= 0);")
        .unwrap_err();
    assert!(check_error.to_string().contains("check constraint"));

    assert_eq!(
        db.query("SELECT * FROM users;").unwrap(),
        vec![vec![Value::Integer(1), Value::from("alice")]]
    );
}

#[test]
fn database_supports_insert_column_list_delete_update_order_by_limit_and_aliases() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, active BOOLEAN, email TEXT);
         INSERT INTO users (id, name) VALUES (1, 'alice');
         INSERT INTO users (id, name, active, email) VALUES (2, 'bob', true, 'b@example.com');
         INSERT INTO users VALUES (3, 'carol', true, 'c@example.com');
         UPDATE users AS u SET name = 'bobby', email = 'bb@example.com' WHERE u.id = 2;
         DELETE FROM users u WHERE u.id = 1;",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT u.name AS username, u.id user_id FROM users AS u WHERE u.active = TRUE ORDER BY username DESC, u.id ASC LIMIT 1;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("carol"), Value::Integer(3)]]);

    let all_rows = db.query("SELECT * FROM users ORDER BY id ASC;").unwrap();
    assert_eq!(
        all_rows,
        vec![
            vec![
                Value::Integer(2),
                Value::from("bobby"),
                Value::Boolean(true),
                Value::from("bb@example.com"),
            ],
            vec![
                Value::Integer(3),
                Value::from("carol"),
                Value::Boolean(true),
                Value::from("c@example.com"),
            ],
        ]
    );
}

#[test]
fn database_supports_group_by_join_and_subqueries() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, active BOOLEAN NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO users VALUES (1, 'alice', true);
         INSERT INTO users VALUES (2, 'bob', false);
         INSERT INTO users VALUES (3, 'carol', true);
         INSERT INTO orders VALUES (1, 1, 40);
         INSERT INTO orders VALUES (2, 1, 120);
         INSERT INTO orders VALUES (3, 3, 200);
         INSERT INTO orders VALUES (4, 2, 5);",
    )
    .unwrap();

    let grouped = db
        .query("SELECT active, COUNT(*) AS total FROM users GROUP BY active ORDER BY active ASC;")
        .unwrap();
    assert_eq!(
        grouped,
        vec![
            vec![Value::Boolean(false), Value::Integer(1)],
            vec![Value::Boolean(true), Value::Integer(2)],
        ]
    );

    let joined = db
        .query(
            "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id WHERE o.amount > 10 ORDER BY u.name ASC, o.amount ASC;",
        )
        .unwrap();
    assert_eq!(
        joined,
        vec![
            vec![Value::from("alice"), Value::Integer(40)],
            vec![Value::from("alice"), Value::Integer(120)],
            vec![Value::from("carol"), Value::Integer(200)],
        ]
    );

    let in_subquery = db
        .query(
            "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders WHERE amount >= 100) ORDER BY name ASC;",
        )
        .unwrap();
    assert_eq!(
        in_subquery,
        vec![vec![Value::from("alice")], vec![Value::from("carol")]]
    );

    let scalar_subquery = db
        .query("SELECT name FROM users WHERE id = (SELECT user_id FROM orders WHERE id = 4);")
        .unwrap();
    assert_eq!(scalar_subquery, vec![vec![Value::from("bob")]]);

    let correlated_in_subquery = db
        .query(
            "SELECT name FROM users u WHERE id IN (SELECT user_id FROM orders o WHERE o.user_id = u.id AND o.amount >= 100) ORDER BY name ASC;",
        )
        .unwrap();
    assert_eq!(
        correlated_in_subquery,
        vec![vec![Value::from("alice")], vec![Value::from("carol")]]
    );

    let correlated_scalar_subquery = db
        .query(
            "SELECT name FROM users u WHERE id = (SELECT user_id FROM orders o WHERE o.user_id = u.id AND o.amount >= 100) ORDER BY name ASC;",
        )
        .unwrap();
    assert_eq!(
        correlated_scalar_subquery,
        vec![vec![Value::from("alice")], vec![Value::from("carol")]]
    );
}

#[test]
fn database_supports_having_left_join_and_distinct() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, active BOOLEAN NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO users VALUES (1, 'alice', true);
         INSERT INTO users VALUES (2, 'bob', false);
         INSERT INTO users VALUES (3, 'carol', true);
         INSERT INTO orders VALUES (1, 1, 40);
         INSERT INTO orders VALUES (2, 1, 120);
         INSERT INTO orders VALUES (3, 3, 200);
         INSERT INTO orders VALUES (4, 2, 5);",
    )
    .unwrap();

    // HAVING filters group rows after aggregation.
    let having = db
        .query(
            "SELECT user_id, COUNT(*) AS total FROM orders GROUP BY user_id HAVING total > 1 ORDER BY user_id ASC;",
        )
        .unwrap();
    assert_eq!(having, vec![vec![Value::Integer(1), Value::Integer(2)]]);

    // LEFT JOIN preserves left rows without matches and pads with NULL.
    let left_joined = db
        .query(
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount >= 100 ORDER BY u.name ASC, o.amount ASC;",
        )
        .unwrap();
    assert_eq!(
        left_joined,
        vec![
            vec![Value::from("alice"), Value::Integer(120)],
            vec![Value::from("bob"), Value::Null],
            vec![Value::from("carol"), Value::Integer(200)],
        ]
    );

    // DISTINCT removes duplicate result rows.
    let distinct = db
        .query("SELECT DISTINCT user_id FROM orders ORDER BY user_id ASC;")
        .unwrap();
    assert_eq!(
        distinct,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );
}

#[test]
fn database_union_deduplicates_rows() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE left_values (id INTEGER PRIMARY KEY, value INTEGER);
         CREATE TABLE right_values (id INTEGER PRIMARY KEY, value INTEGER);
         INSERT INTO left_values VALUES (1, 1);
         INSERT INTO left_values VALUES (2, 2);
         INSERT INTO right_values VALUES (10, 2);
         INSERT INTO right_values VALUES (11, 2);
         INSERT INTO right_values VALUES (12, 3);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT value FROM left_values
             UNION
             SELECT value FROM right_values
             ORDER BY value ASC;",
        )
        .unwrap();

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
fn database_union_all_preserves_duplicate_rows() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE left_values (id INTEGER PRIMARY KEY, value INTEGER);
         CREATE TABLE right_values (id INTEGER PRIMARY KEY, value INTEGER);
         INSERT INTO left_values VALUES (1, 1);
         INSERT INTO left_values VALUES (2, 2);
         INSERT INTO right_values VALUES (10, 2);
         INSERT INTO right_values VALUES (11, 2);
         INSERT INTO right_values VALUES (12, 3);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT value FROM left_values
             UNION ALL
             SELECT value FROM right_values
             ORDER BY value ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(2)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );
}

#[test]
fn database_applies_order_by_and_limit_to_entire_union_all_result() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE left_values (id INTEGER PRIMARY KEY, value INTEGER);
         CREATE TABLE right_values (id INTEGER PRIMARY KEY, value INTEGER);
         INSERT INTO left_values VALUES (1, 1);
         INSERT INTO left_values VALUES (2, 4);
         INSERT INTO right_values VALUES (10, 2);
         INSERT INTO right_values VALUES (11, 3);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT value FROM left_values
             UNION ALL
             SELECT value FROM right_values
             ORDER BY value DESC
             LIMIT 2;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(4)], vec![Value::Integer(3)]]);
}

#[test]
fn database_joins_with_derived_source_on_right() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 20);
         INSERT INTO users VALUES (2, 'bob', 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT u.name, t.bucket
             FROM users u
             JOIN (SELECT id, age + 1 AS bucket FROM users) t ON u.id = t.id
             ORDER BY u.id ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::Integer(21)],
            vec![Value::from("bob"), Value::Integer(31)],
        ]
    );
}

#[test]
fn database_left_joins_aggregate_derived_source() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, 'carol');
         INSERT INTO orders VALUES (1, 1, 40);
         INSERT INTO orders VALUES (2, 1, 120);
         INSERT INTO orders VALUES (3, 3, 200);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT u.id, t.total
             FROM users u
             LEFT JOIN (
                 SELECT user_id, COUNT(*) AS total
                 FROM orders
                 GROUP BY user_id
             ) t ON u.id = t.user_id
             ORDER BY u.id ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(2), Value::Null],
            vec![Value::Integer(3), Value::Integer(1)],
        ]
    );
}

#[test]
fn database_rejects_reference_to_unexposed_inner_column_from_joined_derived_source() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 20);",
    )
    .unwrap();

    let error = db
        .query(
            "SELECT u.name
             FROM users u
             JOIN (SELECT id, age + 1 AS bucket FROM users) t ON u.id = t.id
             WHERE t.age > 20;",
        )
        .unwrap_err();

    assert_eq!(error.to_string(), "plan error: unknown column t.age");
}

#[test]
fn database_supports_common_sql_predicates_order_positions_and_distinct_aggregates() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER, active BOOLEAN, email TEXT);
         CREATE INDEX idx_users_name ON users (name);
         CREATE INDEX idx_users_email ON users (email);
         INSERT INTO users VALUES (1, 'alice', 30, true, 'alice@example.com');
         INSERT INTO users VALUES (2, 'alicia', 24, true, NULL);
         INSERT INTO users VALUES (3, 'bob', 19, false, 'bob@example.com');
         INSERT INTO users VALUES (4, 'carol', 41, true, NULL);
         INSERT INTO users VALUES (5, 'dave', NULL, false, 'dave@example.com');",
    )
    .unwrap();

    let like_rows = db
        .query("SELECT name FROM users WHERE name LIKE 'ali%' ORDER BY 1 ASC;")
        .unwrap();
    assert_eq!(
        like_rows,
        vec![vec![Value::from("alice")], vec![Value::from("alicia")]]
    );

    let not_like_rows = db
        .query("SELECT name FROM users WHERE name NOT LIKE '%a%' ORDER BY name ASC;")
        .unwrap();
    assert_eq!(not_like_rows, vec![vec![Value::from("bob")]]);

    let between_rows = db
        .query("SELECT name, age FROM users WHERE age BETWEEN 20 AND 40 ORDER BY 2 DESC;")
        .unwrap();
    assert_eq!(
        between_rows,
        vec![
            vec![Value::from("alice"), Value::Integer(30)],
            vec![Value::from("alicia"), Value::Integer(24)],
        ]
    );

    let not_between_rows = db
        .query("SELECT name FROM users WHERE age NOT BETWEEN 20 AND 40 ORDER BY 1 ASC;")
        .unwrap();
    assert_eq!(
        not_between_rows,
        vec![vec![Value::from("bob")], vec![Value::from("carol")]]
    );

    let null_email_rows = db
        .query("SELECT name FROM users WHERE email IS NULL ORDER BY 1 ASC;")
        .unwrap();
    assert_eq!(
        null_email_rows,
        vec![vec![Value::from("alicia")], vec![Value::from("carol")]]
    );

    let distinct_count = db
        .query("SELECT COUNT(DISTINCT active) AS active_values FROM users;")
        .unwrap();
    assert_eq!(distinct_count, vec![vec![Value::Integer(2)]]);
}

#[test]
fn database_supports_explicit_null_ordering() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 30);
         INSERT INTO users VALUES (2, 'bob', NULL);
         INSERT INTO users VALUES (3, 'carol', 20);
         INSERT INTO users VALUES (4, 'dave', NULL);",
    )
    .unwrap();

    let nulls_first = db
        .query("SELECT name, age FROM users ORDER BY age NULLS FIRST, name DESC NULLS LAST;")
        .unwrap();
    assert_eq!(
        nulls_first,
        vec![
            vec![Value::from("dave"), Value::Null],
            vec![Value::from("bob"), Value::Null],
            vec![Value::from("carol"), Value::Integer(20)],
            vec![Value::from("alice"), Value::Integer(30)],
        ]
    );

    let nulls_last_desc = db
        .query("SELECT name, age FROM users ORDER BY age DESC NULLS LAST, name ASC;")
        .unwrap();
    assert_eq!(
        nulls_last_desc,
        vec![
            vec![Value::from("alice"), Value::Integer(30)],
            vec![Value::from("carol"), Value::Integer(20)],
            vec![Value::from("bob"), Value::Null],
            vec![Value::from("dave"), Value::Null],
        ]
    );
}

#[test]
fn database_supports_expression_projection() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 30);
         INSERT INTO users VALUES (2, 'bob', 19);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT id + 10 AS shifted_id,
                    age * 2 AS doubled_age,
                    name || '_user' AS label,
                    (age - 1) / 2 AS half_minus_one,
                    1 + 2 * 3 AS constant_value
             FROM users
             ORDER BY shifted_id DESC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(12),
                Value::Integer(38),
                Value::from("bob_user"),
                Value::Integer(9),
                Value::Integer(7),
            ],
            vec![
                Value::Integer(11),
                Value::Integer(60),
                Value::from("alice_user"),
                Value::Integer(14),
                Value::Integer(7),
            ],
        ]
    );
}

#[test]
fn database_supports_scalar_functions() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, nickname TEXT, delta INTEGER);
         INSERT INTO users VALUES (1, 'Alice', NULL, -7);
         INSERT INTO users VALUES (2, 'Bob', 'bobby', 4);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT LENGTH(name) AS name_len,
                    LOWER(name) AS lower_name,
                    UPPER(nickname) AS upper_nickname,
                    ABS(delta) AS abs_delta,
                    COALESCE(nickname, name, 'anonymous') AS display_name,
                    IFNULL(nickname, name) AS fallback_name,
                    LENGTH(name || '!') AS excited_len
             FROM users
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(5),
                Value::from("alice"),
                Value::Null,
                Value::Integer(7),
                Value::from("Alice"),
                Value::from("Alice"),
                Value::Integer(6),
            ],
            vec![
                Value::Integer(3),
                Value::from("bob"),
                Value::from("BOBBY"),
                Value::Integer(4),
                Value::from("bobby"),
                Value::from("bobby"),
                Value::Integer(4),
            ],
        ]
    );
}

#[test]
fn database_rejects_invalid_scalar_function_calls() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 30);",
    )
    .unwrap();

    let unknown = db.query("SELECT MYSTERY(name) FROM users;").unwrap_err();
    assert!(unknown.to_string().contains("unsupported scalar function"));

    let wrong_arity = db.query("SELECT IFNULL(name) FROM users;").unwrap_err();
    assert!(
        wrong_arity
            .to_string()
            .contains("IFNULL expects 2 arguments")
    );

    let wrong_type = db.query("SELECT LOWER(age) FROM users;").unwrap_err();
    assert!(wrong_type.to_string().contains("LOWER expects TEXT"));
}

#[test]
fn database_short_circuits_coalesce_and_ifnull() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 30);
         INSERT INTO users VALUES (2, NULL, 40);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT COALESCE(name, LOWER(age), 'fallback') AS display_name,
                    IFNULL(name, 1 / 0) AS fallback_name
             FROM users
             WHERE id = 1;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("alice"), Value::from("alice")]]);

    let error = db
        .query("SELECT COALESCE(name, LOWER(age), 'fallback') FROM users WHERE id = 2;")
        .unwrap_err();
    assert!(error.to_string().contains("LOWER expects TEXT"));
}

#[test]
fn database_orders_by_scalar_expressions() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, nickname TEXT);
         INSERT INTO users VALUES (1, 'bob', NULL);
         INSERT INTO users VALUES (2, 'alice', 'ally');
         INSERT INTO users VALUES (3, 'carol', 'c');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT name
             FROM users
             ORDER BY LENGTH(COALESCE(nickname, name)) DESC, LOWER(name) ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice")],
            vec![Value::from("bob")],
            vec![Value::from("carol")],
        ]
    );
}

#[test]
fn database_orders_aggregate_rows_by_scalar_expression() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active BOOLEAN);
         INSERT INTO users VALUES (1, 'bob', true);
         INSERT INTO users VALUES (2, 'alice', true);
         INSERT INTO users VALUES (3, 'carol', false);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT active, COUNT(*) AS total
             FROM users
             GROUP BY active
             ORDER BY total + 1 DESC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Boolean(true), Value::Integer(2)],
            vec![Value::Boolean(false), Value::Integer(1)],
        ]
    );
}

#[test]
fn database_groups_by_scalar_expression_and_orders_by_aggregate_expression() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 20);
         INSERT INTO users VALUES (2, 'bob', 20);
         INSERT INTO users VALUES (3, 'carol', 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT age + 1 AS bucket, COUNT(*) AS total
             FROM users
             GROUP BY age + 1
             HAVING bucket > 20
             ORDER BY total + 1 DESC, bucket DESC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(21), Value::Integer(2)],
            vec![Value::Integer(31), Value::Integer(1)],
        ]
    );
}

#[test]
fn database_aggregates_scalar_expression_arguments() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 20);
         INSERT INTO users VALUES (2, 'bob', 20);
         INSERT INTO users VALUES (3, 'carol', 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT SUM(age + 1) AS total, AVG(age + 1) AS avg_total, COUNT(DISTINCT age + 1) AS distinct_total
             FROM users;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(73),
            Value::Integer(24),
            Value::Integer(2),
        ]]
    );
}

#[test]
fn database_groups_with_scalar_projection_and_scalar_aggregate_argument() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO users VALUES (2, 20);
         INSERT INTO users VALUES (3, 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT age + 1 AS bucket, SUM(age + 1) AS total
             FROM users
             GROUP BY age + 1
             ORDER BY bucket ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(21), Value::Integer(42)],
            vec![Value::Integer(31), Value::Integer(31)],
        ]
    );
}

#[test]
fn database_selects_from_subquery_and_filters_on_derived_alias() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO users VALUES (2, 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT bucket
             FROM (SELECT age + 1 AS bucket FROM users) t
             WHERE bucket > 21
             ORDER BY bucket ASC;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(31)]]);
}

#[test]
fn database_consumes_aggregate_subquery_outputs_as_regular_columns() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO users VALUES (2, 20);
         INSERT INTO users VALUES (3, 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT bucket, total
             FROM (
                 SELECT age + 1 AS bucket, COUNT(*) AS total
                 FROM users
                 GROUP BY age + 1
             ) t
             WHERE total > 1
             ORDER BY bucket ASC;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(21), Value::Integer(2)]]);
}

#[test]
fn database_aggregates_over_derived_source() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO users VALUES (2, 20);
         INSERT INTO users VALUES (3, 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT bucket, COUNT(*) AS total
             FROM (SELECT age + 1 AS bucket FROM users) t
             GROUP BY bucket
             HAVING bucket > 20
             ORDER BY bucket ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(21), Value::Integer(2)],
            vec![Value::Integer(31), Value::Integer(1)],
        ]
    );
}

#[test]
fn database_aggregates_over_joined_derived_source() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO users VALUES (2, 20);
         INSERT INTO users VALUES (3, 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT t.bucket, COUNT(*) AS total
             FROM users u
             JOIN (SELECT id, age + 1 AS bucket FROM users) t ON u.id = t.id
             GROUP BY t.bucket
             HAVING t.bucket > 20
             ORDER BY t.bucket ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(21), Value::Integer(2)],
            vec![Value::Integer(31), Value::Integer(1)],
        ]
    );
}

#[test]
fn database_selects_from_wildcard_subquery_columns() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO users VALUES (2, 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT age
             FROM (SELECT * FROM users) t
             ORDER BY age ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::Integer(20)], vec![Value::Integer(30)]]
    );
}

#[test]
fn database_selects_from_qualified_column_subquery_using_unqualified_output_name() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO users VALUES (2, 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT age
             FROM (SELECT u.age FROM users u) t
             ORDER BY age ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::Integer(20)], vec![Value::Integer(30)]]
    );
}

#[test]
fn database_rejects_unqualified_duplicate_output_from_joined_wildcard_derived_source() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO orders VALUES (10, 1);",
    )
    .unwrap();

    let error = db
        .query("SELECT id FROM (SELECT * FROM users u JOIN orders o ON u.id = o.user_id) t;")
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "plan error: ambiguous column reference: id"
    );
}

#[test]
fn database_rejects_qualified_duplicate_output_from_joined_wildcard_derived_source() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO orders VALUES (10, 1);",
    )
    .unwrap();

    let error = db
        .query("SELECT t.id FROM (SELECT * FROM users u JOIN orders o ON u.id = o.user_id) t;")
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "plan error: ambiguous column reference: t.id"
    );
}

#[test]
fn database_having_and_order_by_can_reference_group_expression_without_alias() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO users VALUES (2, 20);
         INSERT INTO users VALUES (3, 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT age + 1 AS bucket, COUNT(*) AS total
             FROM users
             GROUP BY age + 1
             HAVING age + 1 > 20
             ORDER BY age + 1 DESC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(31), Value::Integer(1)],
            vec![Value::Integer(21), Value::Integer(2)],
        ]
    );
}

#[test]
fn database_joins_with_scalar_expression_on_clause() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, amount INTEGER);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO orders VALUES (10, 2, 80);
         INSERT INTO orders VALUES (11, 4, 90);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT u.name, o.amount
             FROM users u
             JOIN orders o ON u.id + 1 = o.user_id
             ORDER BY u.name ASC;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(80)]]);
}

#[test]
fn database_filters_with_scalar_expression_comparisons() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, nickname TEXT, age INTEGER);
         INSERT INTO users VALUES (1, 'Alice', NULL, 20);
         INSERT INTO users VALUES (2, 'bob', 'bobby', 17);
         INSERT INTO users VALUES (3, 'carol', NULL, 40);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT name
             FROM users
             WHERE LENGTH(name) > 3
               AND (age + 1) >= 21
               AND LOWER(name) = 'alice';",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("Alice")]]);

    let coalesce_rows = db
        .query(
            "SELECT name
             FROM users
             WHERE COALESCE(nickname, name) = 'bobby';",
        )
        .unwrap();

    assert_eq!(coalesce_rows, vec![vec![Value::from("bob")]]);

    let short_circuit_rows = db
        .query(
            "SELECT name
             FROM users
             WHERE COALESCE(name, 1 / 0) = 'Alice'
                OR IFNULL(name, 1 / 0) = 'Alice';",
        )
        .unwrap();

    assert_eq!(short_circuit_rows, vec![vec![Value::from("Alice")]]);

    db.execute(
        "CREATE TABLE aliases (id INTEGER PRIMARY KEY, code TEXT NOT NULL);
         INSERT INTO aliases VALUES (1, 'alice');
         INSERT INTO aliases VALUES (2, 'carol');",
    )
    .unwrap();

    let correlated_rows = db
        .query(
            "SELECT name
             FROM users u
             WHERE EXISTS (SELECT id FROM aliases a WHERE LOWER(u.name) = a.code)
             ORDER BY name ASC;",
        )
        .unwrap();

    assert_eq!(
        correlated_rows,
        vec![vec![Value::from("Alice")], vec![Value::from("carol")]]
    );
}

#[test]
fn database_filters_with_scalar_expression_is_null() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, nickname TEXT);
         INSERT INTO users VALUES (1, 'alice', NULL);
         INSERT INTO users VALUES (2, NULL, NULL);
         INSERT INTO users VALUES (3, 'carol', 'c');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT id
             FROM users
             WHERE COALESCE(nickname, name) IS NOT NULL
             ORDER BY id ASC;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);

    let null_rows = db
        .query(
            "SELECT id
             FROM users
             WHERE LENGTH(COALESCE(nickname, name)) IS NULL;",
        )
        .unwrap();

    assert_eq!(null_rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn database_filters_with_scalar_expression_like() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, nickname TEXT);
         INSERT INTO users VALUES (1, 'Alice', NULL);
         INSERT INTO users VALUES (2, 'bob', 'bobby');
         INSERT INTO users VALUES (3, 'carol', 'c');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT id
             FROM users
             WHERE LOWER(name) LIKE 'a%'
               AND COALESCE(nickname, name) NOT LIKE 'x%';",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn database_filters_with_scalar_expression_between() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
         INSERT INTO users VALUES (1, 'Alice', 17);
         INSERT INTO users VALUES (2, 'Bo', 29);
         INSERT INTO users VALUES (3, 'Carol', 30);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT id
             FROM users
             WHERE age + 1 BETWEEN 18 AND 30
               AND LENGTH(name) NOT BETWEEN 1 AND 3
             ORDER BY id ASC;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    let scalar_bounds_rows = db
        .query(
            "SELECT id
             FROM users
             WHERE age BETWEEN 17 + 1 AND 40 - 10
             ORDER BY id ASC;",
        )
        .unwrap();

    assert_eq!(
        scalar_bounds_rows,
        vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]
    );
}

#[test]
fn database_filters_with_scalar_expression_in_subquery() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, alias_id INTEGER);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO users VALUES (1, 'alice', NULL);
         INSERT INTO users VALUES (2, 'bob', 99);
         INSERT INTO users VALUES (3, 'carol', NULL);
         INSERT INTO orders VALUES (1, 1, 120);
         INSERT INTO orders VALUES (2, 2, 5);
         INSERT INTO orders VALUES (3, 3, 200);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT name
             FROM users u
             WHERE COALESCE(alias_id, id) IN (
                 SELECT user_id
                 FROM orders o
                 WHERE o.user_id = u.id AND o.amount >= 100
             )
             ORDER BY name ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("alice")], vec![Value::from("carol")]]
    );

    let not_in_rows = db
        .query(
            "SELECT name
             FROM users u
             WHERE COALESCE(alias_id, id) NOT IN (
                 SELECT user_id
                 FROM orders o
                 WHERE o.user_id = u.id AND o.amount >= 100
             )
             ORDER BY name ASC;",
        )
        .unwrap();

    assert_eq!(not_in_rows, vec![vec![Value::from("bob")]]);
}

#[test]
fn database_supports_cte_in_in_subquery() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, 'carol');
         INSERT INTO orders VALUES (1, 1, 120);
         INSERT INTO orders VALUES (2, 2, 5);
         INSERT INTO orders VALUES (3, 3, 200);",
    )
    .unwrap();

    let rows = db
        .query(
            "WITH high_spenders AS (
                 SELECT user_id
                 FROM orders
                 WHERE amount >= 100
             )
             SELECT name
             FROM users
             WHERE id IN (SELECT user_id FROM high_spenders)
             ORDER BY name ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("alice")], vec![Value::from("carol")]]
    );
}

#[test]
fn database_cte_shadows_base_table_name_in_outer_query() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE archived_users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (1, 'active_alice');
         INSERT INTO users VALUES (2, 'active_bob');
         INSERT INTO archived_users VALUES (10, 'archived_zoe');
         INSERT INTO archived_users VALUES (11, 'archived_yuki');",
    )
    .unwrap();

    let rows = db
        .query(
            "WITH users AS (
                 SELECT id, name
                 FROM archived_users
             )
             SELECT name
             FROM users
             ORDER BY id ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::from("archived_zoe")],
            vec![Value::from("archived_yuki")],
        ]
    );
}

#[test]
fn database_filters_with_scalar_expression_compare_subquery() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, alias_id INTEGER);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO users VALUES (1, 'alice', NULL);
         INSERT INTO users VALUES (2, 'bob', 99);
         INSERT INTO users VALUES (3, 'carol', NULL);
         INSERT INTO orders VALUES (1, 1, 120);
         INSERT INTO orders VALUES (2, 2, 5);
         INSERT INTO orders VALUES (3, 3, 200);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT name
             FROM users u
             WHERE COALESCE(alias_id, id) = (
                 SELECT user_id
                 FROM orders o
                 WHERE o.user_id = u.id AND o.amount >= 100
             )
             ORDER BY name ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("alice")], vec![Value::from("carol")]]
    );
}

#[test]
fn database_supports_exists_and_not_exists_subqueries() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, 'carol');
         INSERT INTO orders VALUES (1, 1, 120);
         INSERT INTO orders VALUES (2, 2, 5);",
    )
    .unwrap();

    let exists_rows = db
        .query(
            "SELECT name FROM users WHERE EXISTS (SELECT id FROM orders WHERE amount > 100) ORDER BY name ASC;",
        )
        .unwrap();
    assert_eq!(
        exists_rows,
        vec![
            vec![Value::from("alice")],
            vec![Value::from("bob")],
            vec![Value::from("carol")]
        ]
    );

    let not_exists_rows = db
        .query(
            "SELECT name FROM users WHERE NOT EXISTS (SELECT id FROM orders WHERE amount > 999) ORDER BY name ASC;",
        )
        .unwrap();
    assert_eq!(
        not_exists_rows,
        vec![
            vec![Value::from("alice")],
            vec![Value::from("bob")],
            vec![Value::from("carol")]
        ]
    );

    let empty_exists_rows = db
        .query("SELECT name FROM users WHERE EXISTS (SELECT id FROM orders WHERE amount > 999);")
        .unwrap();
    assert_eq!(empty_exists_rows, Vec::<Vec<Value>>::new());

    let correlated_exists_rows = db
        .query(
            "SELECT name FROM users u WHERE EXISTS (SELECT id FROM orders o WHERE o.user_id = u.id) ORDER BY name ASC;",
        )
        .unwrap();
    assert_eq!(
        correlated_exists_rows,
        vec![vec![Value::from("alice")], vec![Value::from("bob")]]
    );

    let correlated_not_exists_rows = db
        .query(
            "SELECT name FROM users u WHERE NOT EXISTS (SELECT id FROM orders o WHERE o.user_id = u.id) ORDER BY name ASC;",
        )
        .unwrap();
    assert_eq!(correlated_not_exists_rows, vec![vec![Value::from("carol")]]);
}
