use rustsql::common::types::Value;
use rustsql::db::Database;
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
