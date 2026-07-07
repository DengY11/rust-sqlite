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
fn database_accepts_select_all_quantifier_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'alice');
         INSERT INTO users VALUES (3, 'bob');",
    )
    .unwrap();

    let rows = db
        .query("SELECT ALL name FROM users ORDER BY id ASC;")
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice")],
            vec![Value::from("alice")],
            vec![Value::from("bob")],
        ]
    );
}

#[test]
fn database_accepts_string_literal_select_aliases_like_sqlite() {
    let db = Database::memory();

    let rows = db.query("SELECT 1 AS 'one', 2 'two';").unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(2)]]);
}

#[test]
fn database_accepts_create_temp_table_syntax_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TEMP TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         CREATE TEMPORARY TABLE IF NOT EXISTS logs (id INTEGER, message TEXT);
         INSERT INTO logs VALUES (1, 'created');",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT id, name FROM users;").unwrap(),
        vec![vec![Value::Integer(1), Value::from("alice")]]
    );
    assert_eq!(
        db.query("SELECT id, message FROM logs;").unwrap(),
        vec![vec![Value::Integer(1), Value::from("created")]]
    );
}

#[test]
fn database_accepts_boolean_keyword_select_aliases_like_sqlite() {
    let db = Database::memory();

    let rows = db.query("SELECT 1 AS TRUE, 2 FALSE;").unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(2)]]);
}

#[test]
fn database_accepts_rollback_keyword_select_alias_like_sqlite() {
    let db = Database::memory();

    let rows = db.query("SELECT 1 AS ROLLBACK;").unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn database_accepts_begin_keyword_select_alias_like_sqlite() {
    let db = Database::memory();

    let rows = db.query("SELECT 1 AS BEGIN;").unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn database_supports_top_level_values_query_like_sqlite() {
    let db = Database::memory();

    let rows = db.query("VALUES (1, 'alice'), (2, 'bob');").unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn database_selects_from_values_derived_table_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT v.column1, column2
             FROM (VALUES (2, 'bob'), (1, 'alice')) AS v
             WHERE v.column1 > 0
             ORDER BY column2 ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn database_exposes_rowid_for_rowid_tables() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (7, 'bob');",
    )
    .unwrap();

    let rows = db
        .query("SELECT rowid, id, name FROM users ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(7), Value::Integer(7), Value::from("bob")],
        ]
    );
}

#[test]
fn database_exposes_separate_rowid_for_desc_integer_primary_key_tables() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE weird (id INTEGER PRIMARY KEY DESC, name TEXT);
         INSERT INTO weird(name) VALUES ('carol');
         INSERT INTO weird(id, name) VALUES (9, 'dave');",
    )
    .unwrap();

    let rows = db
        .query("SELECT rowid, id, name FROM weird ORDER BY rowid;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Null, Value::from("carol")],
            vec![Value::Integer(2), Value::Integer(9), Value::from("dave")],
        ]
    );
}

#[test]
fn database_rejects_rowid_reference_for_without_rowid_tables() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            PRIMARY KEY(user_id, group_id)
        ) WITHOUT ROWID;
         INSERT INTO memberships VALUES (1, 10);",
    )
    .unwrap();

    let error = db
        .query("SELECT rowid, user_id FROM memberships;")
        .unwrap_err();
    assert!(
        error.to_string().contains("unknown column rowid"),
        "unexpected error: {error}"
    );
}

#[test]
fn database_accepts_composite_foreign_key_parent_primary_key_shorthand_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE parents (
            a INTEGER,
            b INTEGER,
            PRIMARY KEY(a, b)
        );
         CREATE TABLE child (
            x INTEGER,
            y INTEGER,
            FOREIGN KEY (x, y) REFERENCES parents
        );
         INSERT INTO parents VALUES (1, 2);
         INSERT INTO child VALUES (1, 2);",
    )
    .unwrap();

    let rows = db.query("SELECT x, y FROM child ORDER BY x, y;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(2)]]);
}

#[test]
fn database_exposes_rowid_alias_names() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (7, 'bob');",
    )
    .unwrap();

    let rows = db
        .query("SELECT rowid, oid, _rowid_, id, name FROM users;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(7),
            Value::Integer(7),
            Value::Integer(7),
            Value::Integer(7),
            Value::from("bob"),
        ]]
    );
}

#[test]
fn database_real_rowid_column_shadows_only_rowid_name() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE shadowed (rowid INTEGER, name TEXT);
         INSERT INTO shadowed VALUES (5, 'x');",
    )
    .unwrap();

    let rows = db
        .query("SELECT rowid, oid, _rowid_ FROM shadowed;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(5),
            Value::Integer(1),
            Value::Integer(1)
        ]]
    );
}

#[test]
fn database_real_oid_and_rowid_columns_shadow_only_their_own_names() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE oid_shadow (oid INTEGER, name TEXT);
         INSERT INTO oid_shadow VALUES (6, 'y');
         CREATE TABLE hidden_shadow (_rowid_ INTEGER, name TEXT);
         INSERT INTO hidden_shadow VALUES (8, 'z');",
    )
    .unwrap();

    let oid_rows = db
        .query("SELECT rowid, oid, _rowid_ FROM oid_shadow;")
        .unwrap();
    assert_eq!(
        oid_rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(6),
            Value::Integer(1)
        ]]
    );

    let hidden_rows = db
        .query("SELECT rowid, oid, _rowid_ FROM hidden_shadow;")
        .unwrap();
    assert_eq!(
        hidden_rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(8)
        ]]
    );
}

#[test]
fn database_exposes_sqlite_master_catalog_rows() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_name ON users(name);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT type, name, tbl_name, sql
             FROM sqlite_master
             ORDER BY type, name;",
        )
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::from("index"),
                Value::from("idx_users_name"),
                Value::from("users"),
                Value::from("CREATE INDEX idx_users_name ON users(name)"),
            ],
            vec![
                Value::from("table"),
                Value::from("users"),
                Value::from("users"),
                Value::from("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"),
            ],
        ]
    );
}

#[test]
fn database_exposes_sqlite_schema_as_catalog_alias() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_name ON users(name);",
    )
    .unwrap();

    let rows = db
        .query("SELECT type, name FROM sqlite_schema ORDER BY type, name;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("index"), Value::from("idx_users_name")],
            vec![Value::from("table"), Value::from("users")],
        ]
    );
}

#[test]
fn database_supports_pragma_table_info_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL DEFAULT 'anonymous',
            age INTEGER
        );",
    )
    .unwrap();

    let rows = db.query("PRAGMA table_info(users);").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(0),
                Value::from("id"),
                Value::from("INTEGER"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(1),
            ],
            vec![
                Value::Integer(1),
                Value::from("name"),
                Value::from("TEXT"),
                Value::Integer(1),
                Value::from("'anonymous'"),
                Value::Integer(0),
            ],
            vec![
                Value::Integer(2),
                Value::from("age"),
                Value::from("INTEGER"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(0),
            ],
        ]
    );

    let quoted_rows = db.query("PRAGMA table_info('users');").unwrap();
    assert_eq!(quoted_rows, rows);

    let main_rows = db.query("PRAGMA main.table_info('users');").unwrap();
    assert_eq!(main_rows, rows);

    let equals_rows = db.query("PRAGMA table_info = users;").unwrap();
    assert_eq!(equals_rows, rows);

    let equals_quoted_rows = db.query("PRAGMA table_info = 'users';").unwrap();
    assert_eq!(equals_quoted_rows, rows);
}

#[test]
fn database_supports_pragma_table_xinfo_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE metrics (
            base INTEGER,
            plus_one INTEGER GENERATED ALWAYS AS (base + 1) STORED,
            plus_two INTEGER AS (base + 2)
        );",
    )
    .unwrap();

    let rows = db.query("PRAGMA table_xinfo(metrics);").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(0),
                Value::from("base"),
                Value::from("INTEGER"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(0),
                Value::Integer(0),
            ],
            vec![
                Value::Integer(1),
                Value::from("plus_one"),
                Value::from("INTEGER"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(0),
                Value::Integer(3),
            ],
            vec![
                Value::Integer(2),
                Value::from("plus_two"),
                Value::from("INTEGER"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(0),
                Value::Integer(2),
            ],
        ]
    );
}

#[test]
fn database_supports_pragma_table_list_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID;",
    )
    .unwrap();

    let rows = db.query("PRAGMA table_list;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::from("main"),
                Value::from("kv"),
                Value::from("table"),
                Value::Integer(2),
                Value::Integer(1),
                Value::Integer(0),
            ],
            vec![
                Value::from("main"),
                Value::from("users"),
                Value::from("table"),
                Value::Integer(2),
                Value::Integer(0),
                Value::Integer(0),
            ],
            vec![
                Value::from("main"),
                Value::from("sqlite_schema"),
                Value::from("table"),
                Value::Integer(5),
                Value::Integer(0),
                Value::Integer(0),
            ],
            vec![
                Value::from("temp"),
                Value::from("sqlite_temp_schema"),
                Value::from("table"),
                Value::Integer(5),
                Value::Integer(0),
                Value::Integer(0),
            ],
        ]
    );

    assert_eq!(
        db.query("PRAGMA table_list(users);").unwrap(),
        vec![vec![
            Value::from("main"),
            Value::from("users"),
            Value::from("table"),
            Value::Integer(2),
            Value::Integer(0),
            Value::Integer(0),
        ]]
    );
    assert_eq!(
        db.query("PRAGMA table_list('users');").unwrap(),
        vec![vec![
            Value::from("main"),
            Value::from("users"),
            Value::from("table"),
            Value::Integer(2),
            Value::Integer(0),
            Value::Integer(0),
        ]]
    );
    assert!(db.query("PRAGMA table_list(missing);").unwrap().is_empty());

    assert_eq!(
        db.query("PRAGMA main.table_list;").unwrap(),
        vec![
            vec![
                Value::from("main"),
                Value::from("kv"),
                Value::from("table"),
                Value::Integer(2),
                Value::Integer(1),
                Value::Integer(0),
            ],
            vec![
                Value::from("main"),
                Value::from("users"),
                Value::from("table"),
                Value::Integer(2),
                Value::Integer(0),
                Value::Integer(0),
            ],
            vec![
                Value::from("main"),
                Value::from("sqlite_schema"),
                Value::from("table"),
                Value::Integer(5),
                Value::Integer(0),
                Value::Integer(0),
            ],
        ]
    );
    assert_eq!(
        db.query("PRAGMA temp.table_list;").unwrap(),
        vec![vec![
            Value::from("temp"),
            Value::from("sqlite_temp_schema"),
            Value::from("table"),
            Value::Integer(5),
            Value::Integer(0),
            Value::Integer(0),
        ]]
    );
}

#[test]
fn database_supports_pragma_index_list_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            email TEXT UNIQUE,
            name TEXT
        );
         CREATE INDEX idx_users_name ON users(name);
         CREATE UNIQUE INDEX idx_users_email_named ON users(email);",
    )
    .unwrap();

    let rows = db.query("PRAGMA index_list(users);").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(0),
                Value::from("idx_users_email_named"),
                Value::Integer(1),
                Value::from("c"),
                Value::Integer(0),
            ],
            vec![
                Value::Integer(1),
                Value::from("idx_users_name"),
                Value::Integer(0),
                Value::from("c"),
                Value::Integer(0),
            ],
            vec![
                Value::Integer(2),
                Value::from("sqlite_autoindex_users_1"),
                Value::Integer(1),
                Value::from("u"),
                Value::Integer(0),
            ],
        ]
    );
}

#[test]
fn database_supports_pragma_index_info_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            email TEXT UNIQUE,
            name TEXT,
            age INTEGER
        );
         CREATE INDEX idx_users_name_age ON users(name, age);",
    )
    .unwrap();

    let rows = db.query("PRAGMA index_info(idx_users_name_age);").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(0), Value::Integer(2), Value::from("name")],
            vec![Value::Integer(1), Value::Integer(3), Value::from("age")],
        ]
    );

    let quoted_rows = db
        .query("PRAGMA index_info('idx_users_name_age');")
        .unwrap();
    assert_eq!(quoted_rows, rows);

    let autoindex_rows = db
        .query("PRAGMA index_info(sqlite_autoindex_users_1);")
        .unwrap();
    assert_eq!(
        autoindex_rows,
        vec![vec![
            Value::Integer(0),
            Value::Integer(1),
            Value::from("email")
        ]]
    );
}

#[test]
fn database_supports_pragma_index_xinfo_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name TEXT COLLATE NOCASE,
            age INTEGER
        );
         CREATE INDEX idx_users_name_age ON users(name COLLATE NOCASE DESC, age);
         CREATE INDEX idx_users_lower_name ON users(lower(name));",
    )
    .unwrap();

    let rows = db.query("PRAGMA index_xinfo(idx_users_name_age);").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(0),
                Value::Integer(1),
                Value::from("name"),
                Value::Integer(1),
                Value::from("NOCASE"),
                Value::Integer(1),
            ],
            vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::from("age"),
                Value::Integer(0),
                Value::from("BINARY"),
                Value::Integer(1),
            ],
            vec![
                Value::Integer(2),
                Value::Integer(-1),
                Value::Null,
                Value::Integer(0),
                Value::from("BINARY"),
                Value::Integer(0),
            ],
        ]
    );

    let expr_rows = db
        .query("PRAGMA index_xinfo(idx_users_lower_name);")
        .unwrap();
    assert_eq!(
        expr_rows,
        vec![
            vec![
                Value::Integer(0),
                Value::Integer(-2),
                Value::Null,
                Value::Integer(0),
                Value::from("BINARY"),
                Value::Integer(1),
            ],
            vec![
                Value::Integer(1),
                Value::Integer(-1),
                Value::Null,
                Value::Integer(0),
                Value::from("BINARY"),
                Value::Integer(0),
            ],
        ]
    );
}

#[test]
fn database_supports_pragma_foreign_key_list_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            org_id INTEGER,
            code TEXT,
            UNIQUE(org_id, code)
        );
         CREATE TABLE posts (
            id INTEGER PRIMARY KEY,
            user_id INTEGER REFERENCES users(id) ON DELETE CASCADE ON UPDATE RESTRICT,
            org_id INTEGER,
            code TEXT,
            FOREIGN KEY(org_id, code) REFERENCES users(org_id, code)
                MATCH SIMPLE ON DELETE SET NULL ON UPDATE NO ACTION
         );",
    )
    .unwrap();

    let rows = db.query("PRAGMA foreign_key_list(posts);").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(0),
                Value::Integer(0),
                Value::from("users"),
                Value::from("org_id"),
                Value::from("org_id"),
                Value::from("NO ACTION"),
                Value::from("SET NULL"),
                Value::from("NONE"),
            ],
            vec![
                Value::Integer(0),
                Value::Integer(1),
                Value::from("users"),
                Value::from("code"),
                Value::from("code"),
                Value::from("NO ACTION"),
                Value::from("SET NULL"),
                Value::from("NONE"),
            ],
            vec![
                Value::Integer(1),
                Value::Integer(0),
                Value::from("users"),
                Value::from("user_id"),
                Value::from("id"),
                Value::from("RESTRICT"),
                Value::from("CASCADE"),
                Value::from("NONE"),
            ],
        ]
    );
}

#[test]
fn database_supports_pragma_foreign_key_check_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (
            id INTEGER PRIMARY KEY,
            user_id INTEGER REFERENCES users(id)
         );
         INSERT INTO orders VALUES (10, 99);",
    )
    .unwrap();

    assert_eq!(
        db.query("PRAGMA foreign_key_check;").unwrap(),
        vec![vec![
            Value::from("orders"),
            Value::Integer(10),
            Value::from("users"),
            Value::Integer(0),
        ]]
    );
    assert_eq!(
        db.query("PRAGMA foreign_key_check(orders);").unwrap(),
        vec![vec![
            Value::from("orders"),
            Value::Integer(10),
            Value::from("users"),
            Value::Integer(0),
        ]]
    );
    assert!(
        db.query("PRAGMA foreign_key_check(users);")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn database_supports_pragma_foreign_keys_runtime_switch_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));",
    )
    .unwrap();

    assert_eq!(
        db.query("PRAGMA foreign_keys;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("INSERT INTO orders VALUES (1, 999);").unwrap();

    db.execute("PRAGMA foreign_keys = ON;").unwrap();
    assert_eq!(
        db.query("PRAGMA foreign_keys;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    let error = db
        .execute("INSERT INTO orders VALUES (2, 999);")
        .unwrap_err();
    assert!(error.to_string().contains("foreign key constraint"));
}

#[test]
fn database_supports_pragma_read_uncommitted_runtime_switch_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA read_uncommitted;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("PRAGMA read_uncommitted = ON;").unwrap();
    assert_eq!(
        db.query("PRAGMA read_uncommitted;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    db.execute("PRAGMA read_uncommitted = OFF;").unwrap();
    assert_eq!(
        db.query("PRAGMA read_uncommitted;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("PRAGMA read_uncommitted = 1;").unwrap();
    assert_eq!(
        db.query("PRAGMA read_uncommitted;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    db.execute("PRAGMA read_uncommitted = 0;").unwrap();
    assert_eq!(
        db.query("PRAGMA read_uncommitted;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn database_supports_pragma_recursive_triggers_runtime_switch_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA recursive_triggers;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("PRAGMA recursive_triggers = ON;").unwrap();
    assert_eq!(
        db.query("PRAGMA recursive_triggers;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    db.execute("PRAGMA recursive_triggers = OFF;").unwrap();
    assert_eq!(
        db.query("PRAGMA recursive_triggers;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("PRAGMA recursive_triggers = 1;").unwrap();
    assert_eq!(
        db.query("PRAGMA recursive_triggers;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    db.execute("PRAGMA recursive_triggers = 0;").unwrap();
    assert_eq!(
        db.query("PRAGMA recursive_triggers;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn database_supports_pragma_trusted_schema_runtime_switch_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA trusted_schema;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("PRAGMA trusted_schema = ON;").unwrap();
    assert_eq!(
        db.query("PRAGMA trusted_schema;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    db.execute("PRAGMA trusted_schema = OFF;").unwrap();
    assert_eq!(
        db.query("PRAGMA trusted_schema;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("PRAGMA trusted_schema = 1;").unwrap();
    assert_eq!(
        db.query("PRAGMA trusted_schema;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    db.execute("PRAGMA trusted_schema = 0;").unwrap();
    assert_eq!(
        db.query("PRAGMA trusted_schema;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn database_supports_pragma_ignore_check_constraints_like_sqlite() {
    let db = Database::memory();

    db.execute("CREATE TABLE measurements (value INTEGER CHECK(value > 0));")
        .unwrap();

    assert_eq!(
        db.query("PRAGMA ignore_check_constraints;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    let checked_error = db
        .execute("INSERT INTO measurements VALUES (-1);")
        .unwrap_err();
    assert!(checked_error.to_string().contains("check constraint"));

    db.execute("PRAGMA ignore_check_constraints = ON;").unwrap();
    assert_eq!(
        db.query("PRAGMA ignore_check_constraints;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );
    db.execute("INSERT INTO measurements VALUES (-1);").unwrap();
    assert_eq!(
        db.query("SELECT value FROM measurements;").unwrap(),
        vec![vec![Value::Integer(-1)]]
    );

    db.execute("PRAGMA ignore_check_constraints = OFF;")
        .unwrap();
    assert_eq!(
        db.query("PRAGMA ignore_check_constraints;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
    let checked_again = db
        .execute("INSERT INTO measurements VALUES (-2);")
        .unwrap_err();
    assert!(checked_again.to_string().contains("check constraint"));
}

#[test]
fn database_supports_pragma_query_only_enforcement_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA query_only;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("PRAGMA query_only = ON;").unwrap();
    assert_eq!(
        db.query("PRAGMA query_only;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    let create_error = db
        .execute("CREATE TABLE blocked (id INTEGER PRIMARY KEY);")
        .unwrap_err();
    assert!(
        create_error
            .to_string()
            .contains("attempt to write a readonly database"),
        "unexpected error: {create_error}"
    );

    db.execute("PRAGMA query_only = OFF;").unwrap();
    db.execute("CREATE TABLE allowed (id INTEGER PRIMARY KEY);")
        .unwrap();
    db.execute("INSERT INTO allowed VALUES (1);").unwrap();

    db.execute("PRAGMA query_only = 1;").unwrap();
    let insert_error = db.execute("INSERT INTO allowed VALUES (2);").unwrap_err();
    assert!(
        insert_error
            .to_string()
            .contains("attempt to write a readonly database"),
        "unexpected error: {insert_error}"
    );

    db.execute("PRAGMA query_only = 0;").unwrap();
    db.execute("INSERT INTO allowed VALUES (2);").unwrap();
    assert_eq!(
        db.query("SELECT id FROM allowed ORDER BY id;").unwrap(),
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
}

#[test]
fn database_supports_pragma_encoding_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA encoding;").unwrap(),
        vec![vec![Value::from("UTF-8")]]
    );
}

#[test]
fn database_supports_pragma_collation_list_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA collation_list;").unwrap(),
        vec![
            vec![Value::Integer(0), Value::from("BINARY")],
            vec![Value::Integer(1), Value::from("NOCASE")],
            vec![Value::Integer(2), Value::from("RTRIM")],
        ]
    );
}

#[test]
fn database_supports_pragma_data_version_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA data_version;").unwrap(),
        vec![vec![Value::Integer(2)]]
    );

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY);")
        .unwrap();
    assert_eq!(
        db.query("PRAGMA data_version;").unwrap(),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn database_supports_pragma_integrity_checks_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA quick_check;").unwrap(),
        vec![vec![Value::from("ok")]]
    );
    assert_eq!(
        db.query("PRAGMA integrity_check;").unwrap(),
        vec![vec![Value::from("ok")]]
    );
    assert_eq!(
        db.query("PRAGMA quick_check(1);").unwrap(),
        vec![vec![Value::from("ok")]]
    );
    assert_eq!(
        db.query("PRAGMA integrity_check(1);").unwrap(),
        vec![vec![Value::from("ok")]]
    );
}

#[test]
fn database_supports_pragma_function_list_for_supported_functions() {
    let db = Database::memory();

    let rows = db.query("PRAGMA function_list;").unwrap();
    assert!(rows.contains(&vec![
        Value::from("lower"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("sqlite_version"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(0),
        Value::Integer(2_097_152),
    ]));
    assert!(rows.contains(&vec![
        Value::from("count"),
        Value::Integer(1),
        Value::from("w"),
        Value::from("utf8"),
        Value::Integer(0),
        Value::Integer(2_097_152),
    ]));
    assert!(rows.contains(&vec![
        Value::from("group_concat"),
        Value::Integer(1),
        Value::from("w"),
        Value::from("utf8"),
        Value::Integer(2),
        Value::Integer(2_097_152),
    ]));
    assert!(rows.contains(&vec![
        Value::from("median"),
        Value::Integer(1),
        Value::from("w"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_097_152),
    ]));
    assert!(rows.contains(&vec![
        Value::from("percentile_cont"),
        Value::Integer(1),
        Value::from("w"),
        Value::from("utf8"),
        Value::Integer(2),
        Value::Integer(2_097_152),
    ]));
    assert!(rows.contains(&vec![
        Value::from("percentile_disc"),
        Value::Integer(1),
        Value::from("w"),
        Value::from("utf8"),
        Value::Integer(2),
        Value::Integer(2_097_152),
    ]));
    assert!(rows.contains(&vec![
        Value::from("sin"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("cos"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("tan"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("sinh"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("cosh"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("tanh"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("acos"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("acosh"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("asinh"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("atanh"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("atan2"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(2),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("timediff"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(2),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_object"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(-1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_array_length"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(2),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_valid"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(2),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_error_position"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_remove"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(-1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_set"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(-1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_insert"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(-1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_replace"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(-1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_patch"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(2),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_pretty"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(2),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_group_array"),
        Value::Integer(1),
        Value::from("w"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_097_152),
    ]));
    assert!(rows.contains(&vec![
        Value::from("json_group_object"),
        Value::Integer(1),
        Value::from("w"),
        Value::from("utf8"),
        Value::Integer(2),
        Value::Integer(2_097_152),
    ]));
    assert!(rows.contains(&vec![
        Value::from("unistr_quote"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
    assert!(rows.contains(&vec![
        Value::from("subtype"),
        Value::Integer(1),
        Value::from("s"),
        Value::from("utf8"),
        Value::Integer(1),
        Value::Integer(2_099_200),
    ]));
}

#[test]
fn database_supports_pragma_compile_options_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA compile_options;").unwrap(),
        vec![
            vec![Value::from("DEFAULT_PAGE_SIZE=4096")],
            vec![Value::from("MAX_PAGE_SIZE=65536")],
            vec![Value::from("OMIT_LOAD_EXTENSION")],
        ]
    );
}

#[test]
fn database_supports_pragma_journal_mode_for_memory_database_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA journal_mode;").unwrap(),
        vec![vec![Value::from("memory")]]
    );
}

#[test]
fn sqlite3_pragma_journal_mode_reports_delete_for_file_database() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("journal-mode.db");
    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());

    assert_eq!(
        db.query("PRAGMA journal_mode;").unwrap(),
        vec![vec![Value::from("delete")]]
    );
}

#[test]
fn database_supports_pragma_synchronous_default_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA synchronous;").unwrap(),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn database_supports_common_read_only_pragma_defaults_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA cache_size;").unwrap(),
        vec![vec![Value::Integer(2000)]]
    );
    assert_eq!(
        db.query("PRAGMA temp_store;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
    assert_eq!(
        db.query("PRAGMA locking_mode;").unwrap(),
        vec![vec![Value::from("normal")]]
    );
    assert_eq!(
        db.query("PRAGMA busy_timeout;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn database_supports_pragma_cache_size_and_busy_timeout_setters_like_sqlite() {
    let db = Database::memory();

    db.execute("PRAGMA cache_size = 123;").unwrap();
    assert_eq!(
        db.query("PRAGMA cache_size;").unwrap(),
        vec![vec![Value::Integer(123)]]
    );

    db.execute("PRAGMA cache_size = -2000;").unwrap();
    assert_eq!(
        db.query("PRAGMA cache_size;").unwrap(),
        vec![vec![Value::Integer(-2000)]]
    );

    db.execute("PRAGMA busy_timeout = 2500;").unwrap();
    assert_eq!(
        db.query("PRAGMA busy_timeout;").unwrap(),
        vec![vec![Value::Integer(2500)]]
    );

    db.execute("PRAGMA busy_timeout = -1;").unwrap();
    assert_eq!(
        db.query("PRAGMA busy_timeout;").unwrap(),
        vec![vec![Value::Integer(2500)]]
    );
}

#[test]
fn database_supports_pragma_threads_runtime_switch_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA threads;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("PRAGMA threads = 4;").unwrap();
    assert_eq!(
        db.query("PRAGMA threads;").unwrap(),
        vec![vec![Value::Integer(4)]]
    );

    db.execute("PRAGMA threads = 0;").unwrap();
    assert_eq!(
        db.query("PRAGMA threads;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn database_supports_pragma_reverse_unordered_selects_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE values_t (id INTEGER);
         INSERT INTO values_t VALUES (1), (2), (3);",
    )
    .unwrap();

    assert_eq!(
        db.query("PRAGMA reverse_unordered_selects;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
    assert_eq!(
        db.query("SELECT id FROM values_t;").unwrap(),
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );

    db.execute("PRAGMA reverse_unordered_selects = ON;")
        .unwrap();
    assert_eq!(
        db.query("PRAGMA reverse_unordered_selects;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        db.query("SELECT id FROM values_t;").unwrap(),
        vec![
            vec![Value::Integer(3)],
            vec![Value::Integer(2)],
            vec![Value::Integer(1)],
        ]
    );
    assert_eq!(
        db.query("SELECT id FROM values_t ORDER BY id;").unwrap(),
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );

    db.execute("PRAGMA reverse_unordered_selects = OFF;")
        .unwrap();
    assert_eq!(
        db.query("SELECT id FROM values_t;").unwrap(),
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );
}

#[test]
fn database_accepts_pragma_optimize_as_noop_like_sqlite() {
    let db = Database::memory();

    db.execute("PRAGMA optimize;").unwrap();
    db.execute("PRAGMA optimize = 0x10002;").unwrap();
}

#[test]
fn database_accepts_common_maintenance_statements_as_noops_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX idx_users_name ON users(name);
         INSERT INTO users VALUES (1, 'alice');
         ANALYZE;
         ANALYZE users;
         ANALYZE main.users;
         REINDEX;
         REINDEX idx_users_name;
         VACUUM;
         VACUUM main;",
    )
    .unwrap();

    let rows = db.query("SELECT name FROM users;").unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice")]]);
}

#[test]
fn database_supports_pragma_database_list_for_main_database() {
    let db = Database::memory();

    let rows = db.query("PRAGMA database_list;").unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(0),
            Value::from("main"),
            Value::from("")
        ]]
    );
}

#[test]
fn sqlite3_database_list_reports_main_file_path() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("pragma-database-list.db");
    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY);")
        .unwrap();

    let rows = db.query("PRAGMA database_list;").unwrap();
    let expected_path = path.canonicalize().unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(0),
            Value::from("main"),
            Value::from(expected_path.to_string_lossy().as_ref()),
        ]]
    );
}

#[test]
fn database_supports_pragma_page_size_like_sqlite() {
    let db = Database::memory();
    assert_eq!(
        db.query("PRAGMA page_size;").unwrap(),
        vec![vec![Value::Integer(4096)]]
    );
}

#[test]
fn sqlite3_pragma_page_size_reports_file_header_page_size() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("page-size-512.db");
    let status = Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA page_size = 512; VACUUM; CREATE TABLE users(id INTEGER PRIMARY KEY);")
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    assert_eq!(
        db.query("PRAGMA page_size;").unwrap(),
        vec![vec![Value::Integer(512)]]
    );
}

#[test]
fn database_supports_pragma_page_count_for_empty_memory_database() {
    let db = Database::memory();
    assert_eq!(
        db.query("PRAGMA page_count;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn sqlite3_pragma_page_count_reports_file_page_count() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("page-count-512.db");
    let status = Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA page_size = 512; VACUUM; CREATE TABLE users(id INTEGER PRIMARY KEY);")
        .status()
        .unwrap();
    assert!(status.success());

    let bytes = std::fs::read(&path).unwrap();
    let expected_page_count = i64::try_from(bytes.len() / 512).unwrap();
    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    assert_eq!(
        db.query("PRAGMA page_count;").unwrap(),
        vec![vec![Value::Integer(expected_page_count)]]
    );
}

#[test]
fn database_supports_pragma_freelist_count_like_sqlite_for_new_database() {
    let db = Database::memory();
    assert_eq!(
        db.query("PRAGMA freelist_count;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn database_supports_pragma_user_version_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA user_version;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("PRAGMA user_version = 123;").unwrap();
    assert_eq!(
        db.query("PRAGMA user_version;").unwrap(),
        vec![vec![Value::Integer(123)]]
    );
}

#[test]
fn sqlite3_pragma_user_version_persists_in_file_header() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("user-version.db");
    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY);")
        .unwrap();
    db.execute("PRAGMA user_version = 456;").unwrap();
    assert_eq!(
        db.query("PRAGMA user_version;").unwrap(),
        vec![vec![Value::Integer(456)]]
    );

    let reopened =
        Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    assert_eq!(
        reopened.query("PRAGMA user_version;").unwrap(),
        vec![vec![Value::Integer(456)]]
    );

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(u32::from_be_bytes(bytes[60..64].try_into().unwrap()), 456);
}

#[test]
fn database_supports_pragma_schema_version_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA schema_version;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY);")
        .unwrap();
    assert_eq!(
        db.query("PRAGMA schema_version;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    db.execute("PRAGMA schema_version = 123;").unwrap();
    assert_eq!(
        db.query("PRAGMA schema_version;").unwrap(),
        vec![vec![Value::Integer(123)]]
    );
}

#[test]
fn sqlite3_pragma_schema_version_persists_in_file_header() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("schema-version.db");
    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY);")
        .unwrap();
    assert_eq!(
        db.query("PRAGMA schema_version;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );
    db.execute("PRAGMA schema_version = 456;").unwrap();
    assert_eq!(
        db.query("PRAGMA schema_version;").unwrap(),
        vec![vec![Value::Integer(456)]]
    );

    let reopened =
        Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    assert_eq!(
        reopened.query("PRAGMA schema_version;").unwrap(),
        vec![vec![Value::Integer(456)]]
    );

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(u32::from_be_bytes(bytes[40..44].try_into().unwrap()), 456);
}

#[test]
fn database_supports_pragma_application_id_like_sqlite() {
    let db = Database::memory();

    assert_eq!(
        db.query("PRAGMA application_id;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );

    db.execute("PRAGMA application_id = 42;").unwrap();
    assert_eq!(
        db.query("PRAGMA application_id;").unwrap(),
        vec![vec![Value::Integer(42)]]
    );
}

#[test]
fn sqlite3_pragma_application_id_persists_in_file_header() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("application-id.db");
    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY);")
        .unwrap();
    db.execute("PRAGMA application_id = 42;").unwrap();
    assert_eq!(
        db.query("PRAGMA application_id;").unwrap(),
        vec![vec![Value::Integer(42)]]
    );

    let reopened =
        Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    assert_eq!(
        reopened.query("PRAGMA application_id;").unwrap(),
        vec![vec![Value::Integer(42)]]
    );

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(u32::from_be_bytes(bytes[68..72].try_into().unwrap()), 42);
}

#[test]
fn database_reports_last_insert_rowid() {
    let db = Database::memory();

    let initial = db.query("SELECT last_insert_rowid();").unwrap();
    assert_eq!(initial, vec![vec![Value::Integer(0)]]);

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users(name) VALUES ('alice');",
    )
    .unwrap();
    let first = db.query("SELECT last_insert_rowid();").unwrap();
    assert_eq!(first, vec![vec![Value::Integer(1)]]);

    db.execute(
        "CREATE TABLE weird (id INTEGER PRIMARY KEY DESC, name TEXT);
         INSERT INTO weird(name) VALUES ('bob');",
    )
    .unwrap();
    let second = db.query("SELECT last_insert_rowid();").unwrap();
    assert_eq!(second, vec![vec![Value::Integer(1)]]);
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
fn database_preserves_inline_foreign_key_parent_primary_key_shorthand_metadata() {
    let db = Database::memory();

    db.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             user_id INTEGER REFERENCES users
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
        Some(ForeignKey::to_parent_primary_key("user_id", "users"))
    );
    assert_eq!(
        orders_schema.all_foreign_keys(),
        vec![ForeignKey::to_parent_primary_key("user_id", "users")]
    );
}

#[test]
fn database_executes_generated_columns_through_sql_pipeline() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE metrics (
            base INTEGER,
            plus_one INTEGER GENERATED ALWAYS AS (base + 1) STORED
        );
         INSERT INTO metrics(base) VALUES (3);
         UPDATE metrics SET base = 5;",
    )
    .unwrap();

    let rows = db.query("SELECT base, plus_one FROM metrics;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(5), Value::Integer(6)]]);

    let error = db
        .execute("INSERT INTO metrics(base, plus_one) VALUES (7, 8);")
        .unwrap_err();
    assert!(
        error.to_string().contains("generated column"),
        "unexpected error: {error}"
    );
}

#[test]
fn database_enforces_inline_foreign_key_parent_primary_key_shorthand() {
    let db = Database::memory();

    db.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             user_id INTEGER REFERENCES users
         );
         INSERT INTO users VALUES (1);
         INSERT INTO orders VALUES (10, 1);",
    )
    .unwrap();

    let delete_error = db.execute("DELETE FROM users WHERE id = 1;").unwrap_err();
    assert!(delete_error.to_string().contains("foreign key constraint"));
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
fn database_enforces_composite_primary_keys_by_key_tuple() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            role TEXT,
            PRIMARY KEY(user_id, group_id)
        );",
    )
    .unwrap();

    db.execute("INSERT INTO memberships VALUES (1, 10, 'owner');")
        .unwrap();
    db.execute("INSERT INTO memberships VALUES (1, 11, 'member');")
        .unwrap();
    db.execute("INSERT INTO memberships VALUES (2, 10, 'member');")
        .unwrap();

    let error = db
        .execute("INSERT INTO memberships VALUES (1, 10, 'duplicate');")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("duplicate primary key value for columns (user_id, group_id)")
    );

    let indexes = db.list_indexes("memberships").unwrap();
    assert_eq!(indexes.len(), 1);
    assert!(indexes[0].unique);
    assert_eq!(
        indexes[0].columns,
        vec!["user_id".to_string(), "group_id".to_string()]
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
         CREATE INDEX idx_logs_level_created_at ON logs (level, created_at);
         INSERT INTO users VALUES (1, 'Alice', 30, 'alice@example.com');
         INSERT INTO users VALUES (2, 'alicia', 24, NULL);
         INSERT INTO users VALUES (3, 'bob', 19, 'bob@example.com');",
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
    assert_eq!(like_plan[0][0], Value::from("SeqScan"));
    assert_eq!(like_plan[0][1], Value::from("table=users"));

    let like_rows = db
        .query("SELECT name FROM users WHERE name LIKE 'ali%' ORDER BY name ASC;")
        .unwrap();
    assert_eq!(
        like_rows,
        vec![vec![Value::from("Alice")], vec![Value::from("alicia")]]
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
fn database_explains_without_rowid_primary_key_lookup_as_index_scan() {
    let fixture_dir = tempdir().unwrap();
    let path = fixture_dir.path().join("without-rowid-explain.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                name TEXT,
                PRIMARY KEY(user_id, group_id)
             ) WITHOUT ROWID;
             INSERT INTO memberships VALUES (1, 10, 'alpha'), (2, 20, 'beta');",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT name FROM memberships WHERE user_id = 2 AND group_id = 20;",
        )
        .unwrap();

    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=memberships index=sqlite_autoindex_memberships_1 mode=lookup key_prefix=[2, 20]"
        )
    );
}

#[test]
fn database_explains_without_rowid_secondary_index_lookup_as_index_scan() {
    let fixture_dir = tempdir().unwrap();
    let path = fixture_dir
        .path()
        .join("without-rowid-secondary-index-explain.db");

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            role TEXT,
            PRIMARY KEY(user_id, group_id)
         ) WITHOUT ROWID;
         CREATE INDEX idx_memberships_role ON memberships(role);
         INSERT INTO memberships VALUES (1, 10, 'owner');
         INSERT INTO memberships VALUES (2, 20, 'member');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT user_id FROM memberships WHERE role = 'member';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=memberships index=idx_memberships_role mode=lookup key_prefix=[member]")
    );

    let rows = db
        .query("SELECT user_id FROM memberships WHERE role = 'member';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn database_uses_without_rowid_secondary_index_for_range_scan() {
    let fixture_dir = tempdir().unwrap();
    let path = fixture_dir
        .path()
        .join("without-rowid-secondary-index-range.db");

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            role TEXT,
            PRIMARY KEY(user_id, group_id)
         ) WITHOUT ROWID;
         CREATE INDEX idx_memberships_role ON memberships(role);
         INSERT INTO memberships VALUES (1, 10, 'alpha');
         INSERT INTO memberships VALUES (2, 20, 'bravo');
         INSERT INTO memberships VALUES (3, 30, 'charlie');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT user_id FROM memberships WHERE role > 'alpha' AND role < 'charlie';",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=memberships index=idx_memberships_role mode=range key_prefix=[] range=role:Gt alpha..Lt charlie"
        )
    );

    let rows = db
        .query(
            "SELECT user_id FROM memberships \
             WHERE role > 'alpha' AND role < 'charlie' ORDER BY user_id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn database_explains_without_rowid_composite_secondary_index_scan() {
    let fixture_dir = tempdir().unwrap();
    let path = fixture_dir
        .path()
        .join("without-rowid-composite-secondary-index-explain.db");

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    db.execute(
        "CREATE TABLE memberships (
            user_id INTEGER,
            group_id INTEGER,
            active INTEGER,
            role TEXT,
            PRIMARY KEY(user_id, group_id)
         ) WITHOUT ROWID;
         CREATE INDEX idx_memberships_active_role ON memberships(active, role);
         INSERT INTO memberships VALUES (1, 10, 1, 'alpha');
         INSERT INTO memberships VALUES (2, 20, 1, 'bravo');
         INSERT INTO memberships VALUES (3, 30, 0, 'charlie');",
    )
    .unwrap();

    let plan = db
        .query(
            "EXPLAIN QUERY PLAN \
             SELECT user_id FROM memberships \
             WHERE active = 1 AND role > 'alpha' AND role < 'charlie';",
        )
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from(
            "table=memberships index=idx_memberships_active_role mode=range key_prefix=[1] range=role:Gt alpha..Lt charlie"
        )
    );

    let rows = db
        .query(
            "SELECT user_id FROM memberships \
             WHERE active = 1 AND role > 'alpha' AND role < 'charlie' \
             ORDER BY user_id;",
        )
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn database_supports_indexed_by_and_not_indexed_table_hints_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE INDEX idx_users_name ON users(name);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');",
    )
    .unwrap();

    let indexed_rows = db
        .query("SELECT id FROM users INDEXED BY idx_users_name WHERE name = 'bob';")
        .unwrap();
    assert_eq!(indexed_rows, vec![vec![Value::Integer(2)]]);

    let not_indexed_rows = db
        .query("SELECT id FROM users NOT INDEXED WHERE name = 'alice';")
        .unwrap();
    assert_eq!(not_indexed_rows, vec![vec![Value::Integer(1)]]);

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM users NOT INDEXED WHERE name = 'alice';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("SeqScan"));
}

#[test]
fn database_rejects_indexed_by_unknown_index_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    let error = db
        .query("SELECT id FROM users INDEXED BY missing_idx WHERE name = 'alice';")
        .unwrap_err();
    assert_eq!(error.to_string(), "plan error: no such index: missing_idx");
}

#[test]
fn database_uses_blob_index_for_equality_filters() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE files (id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
         CREATE INDEX idx_files_payload ON files (payload);
         INSERT INTO files VALUES (1, X'0001FEFF');
         INSERT INTO files VALUES (2, X'ABCD');
         INSERT INTO files VALUES (3, X'0001FE00');",
    )
    .unwrap();

    let plan = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM files WHERE payload = X'0001FEFF';")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0][0], Value::from("IndexScan"));
    assert_eq!(
        plan[0][1],
        Value::from("table=files index=idx_files_payload mode=lookup key_prefix=[X'0001FEFF']")
    );

    let rows = db
        .query("SELECT id FROM files WHERE payload = X'0001FEFF';")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
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

    db.execute(
        "BEGIN ISOLATION LEVEL SERIALIZABLE; INSERT INTO users VALUES (3, 'carol'); COMMIT;",
    )
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

    setup_db
        .execute(
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

    assert!(
        writer_done_rx
            .recv_timeout(Duration::from_millis(150))
            .is_err()
    );
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
fn binary_repl_can_select_sqlite3_storage_engine() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cli-sqlite3.db");

    run_rustsql_binary_with_args(
        &["--engine", "sqlite3", path.to_str().unwrap()],
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\n\
         INSERT INTO users(name) VALUES ('alice');\n\
         CREATE INDEX idx_users_name ON users (name);\n\
         .quit\n",
    );

    let output = run_rustsql_binary_with_args(
        &["--engine", "sqlite3", path.to_str().unwrap()],
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
fn database_supports_if_exists_and_if_not_exists_for_ddl() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE INDEX IF NOT EXISTS idx_users_name ON users (name);
         CREATE INDEX IF NOT EXISTS idx_users_name ON users (name);
         INSERT INTO users VALUES (1, 'alice');
         DROP INDEX IF EXISTS missing_idx;
         DROP TABLE IF EXISTS missing_table;",
    )
    .unwrap();

    let indexes = db.list_indexes("users").unwrap();
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx_users_name");

    assert_eq!(
        db.query("SELECT id, name FROM users ORDER BY id;").unwrap(),
        vec![vec![Value::Integer(1), Value::from("alice")]]
    );

    db.execute("DROP INDEX IF EXISTS idx_users_name;").unwrap();
    assert_eq!(db.list_indexes("users").unwrap(), Vec::new());

    db.execute("DROP TABLE IF EXISTS users;").unwrap();
    assert_eq!(db.list_schemas().unwrap(), Vec::new());
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
fn database_applies_current_timestamp_defaults_for_missing_insert_columns() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP
        );",
    )
    .unwrap();

    db.execute("INSERT INTO users (id) VALUES (1);").unwrap();

    let rows = db
        .query("SELECT created_at FROM users WHERE id = 1;")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 1);

    match &rows[0][0] {
        Value::Text(value) => {
            assert_eq!(value.len(), 19, "unexpected timestamp: {value}");
            assert_eq!(&value[4..5], "-");
            assert_eq!(&value[7..8], "-");
            assert_eq!(&value[10..11], " ");
            assert_eq!(&value[13..14], ":");
            assert_eq!(&value[16..17], ":");
        }
        other => panic!("expected text timestamp, got {other:?}"),
    }
}

#[test]
fn database_insert_default_values_uses_declared_defaults() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL DEFAULT 'anonymous',
            active BOOLEAN NOT NULL DEFAULT true
        );",
    )
    .unwrap();

    db.execute("INSERT INTO users DEFAULT VALUES;").unwrap();

    let rows = db
        .query("SELECT id, name, active FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("anonymous"),
            Value::Boolean(true),
        ]]
    );
}

#[test]
fn database_allows_typeless_columns_to_store_mixed_value_kinds() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
            id PRIMARY KEY,
            payload
        );",
    )
    .unwrap();

    db.execute("INSERT INTO users VALUES (1, 'alice');")
        .unwrap();
    db.execute("INSERT INTO users VALUES (2, X'0102');")
        .unwrap();
    db.execute("INSERT INTO users VALUES (3, 42);").unwrap();

    let rows = db
        .query("SELECT id, payload FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![Value::Integer(1), Value::from("alice")]);
    assert_eq!(
        rows[1],
        vec![Value::Integer(2), Value::Blob(vec![0x01, 0x02])]
    );
    assert_eq!(rows[2], vec![Value::Integer(3), Value::Integer(42)]);
}

#[test]
fn database_applies_current_date_and_time_defaults_for_missing_insert_columns() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            created_date TEXT DEFAULT CURRENT_DATE,
            created_time TEXT DEFAULT CURRENT_TIME
        );",
    )
    .unwrap();

    db.execute("INSERT INTO users (id) VALUES (1);").unwrap();

    let rows = db
        .query("SELECT created_date, created_time FROM users WHERE id = 1;")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 2);

    match &rows[0][0] {
        Value::Text(value) => {
            assert_eq!(value.len(), 10, "unexpected current_date: {value}");
            assert_eq!(&value[4..5], "-");
            assert_eq!(&value[7..8], "-");
        }
        other => panic!("expected text current_date, got {other:?}"),
    }

    match &rows[0][1] {
        Value::Text(value) => {
            assert_eq!(value.len(), 8, "unexpected current_time: {value}");
            assert_eq!(&value[2..3], ":");
            assert_eq!(&value[5..6], ":");
        }
        other => panic!("expected text current_time, got {other:?}"),
    }
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
fn database_supports_check_constraints_with_in_lists_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE tasks (
             id INTEGER PRIMARY KEY,
             status TEXT CHECK (status IN ('todo', 'done')),
             code INTEGER CHECK (code NOT IN (13, 99))
         );",
    )
    .unwrap();

    db.execute("INSERT INTO tasks VALUES (1, 'todo', 1);")
        .unwrap();
    db.execute("INSERT INTO tasks VALUES (2, NULL, NULL);")
        .unwrap();

    let status_error = db
        .execute("INSERT INTO tasks VALUES (3, 'blocked', 2);")
        .unwrap_err();
    assert!(status_error.to_string().contains("check constraint"));

    let code_error = db
        .execute("INSERT INTO tasks VALUES (4, 'done', 13);")
        .unwrap_err();
    assert!(code_error.to_string().contains("check constraint"));
}

#[test]
fn database_supports_check_constraints_with_between_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE ranges (
             id INTEGER PRIMARY KEY,
             score INTEGER CHECK (score BETWEEN 1 AND 3),
             excluded INTEGER CHECK (excluded NOT BETWEEN 7 AND 9)
         );",
    )
    .unwrap();

    db.execute("INSERT INTO ranges VALUES (1, 2, 6);").unwrap();
    db.execute("INSERT INTO ranges VALUES (2, NULL, NULL);")
        .unwrap();

    let score_error = db
        .execute("INSERT INTO ranges VALUES (3, 4, 6);")
        .unwrap_err();
    assert!(score_error.to_string().contains("check constraint"));

    let excluded_error = db
        .execute("INSERT INTO ranges VALUES (4, 2, 8);")
        .unwrap_err();
    assert!(excluded_error.to_string().contains("check constraint"));
}

#[test]
fn database_supports_check_constraints_with_is_bool_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE flags (
             id INTEGER PRIMARY KEY,
             active INTEGER CHECK (active IS TRUE),
             archived INTEGER CHECK (archived IS NOT TRUE),
             disabled INTEGER CHECK (disabled IS FALSE)
         );",
    )
    .unwrap();

    db.execute("INSERT INTO flags VALUES (1, 1, 0, 0);")
        .unwrap();
    db.execute("INSERT INTO flags VALUES (2, 2, NULL, 0);")
        .unwrap();

    let active_error = db
        .execute("INSERT INTO flags VALUES (3, 0, 0, 0);")
        .unwrap_err();
    assert!(active_error.to_string().contains("check constraint"));

    let archived_error = db
        .execute("INSERT INTO flags VALUES (4, 1, 1, 0);")
        .unwrap_err();
    assert!(archived_error.to_string().contains("check constraint"));

    let disabled_error = db
        .execute("INSERT INTO flags VALUES (5, 1, 0, 1);")
        .unwrap_err();
    assert!(disabled_error.to_string().contains("check constraint"));

    let disabled_null_error = db
        .execute("INSERT INTO flags VALUES (6, 1, 0, NULL);")
        .unwrap_err();
    assert!(disabled_null_error.to_string().contains("check constraint"));
}

#[test]
fn database_supports_check_constraints_with_bare_truthy_expression_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE flags (
             id INTEGER PRIMARY KEY,
             enabled INTEGER CHECK (enabled),
             archived INTEGER CHECK (NOT archived)
         );",
    )
    .unwrap();

    db.execute("INSERT INTO flags VALUES (1, 1, 0);").unwrap();
    db.execute("INSERT INTO flags VALUES (2, NULL, NULL);")
        .unwrap();

    let enabled_error = db
        .execute("INSERT INTO flags VALUES (3, 0, 0);")
        .unwrap_err();
    assert!(enabled_error.to_string().contains("check constraint"));

    let archived_error = db
        .execute("INSERT INTO flags VALUES (4, 1, 1);")
        .unwrap_err();
    assert!(archived_error.to_string().contains("check constraint"));
}

#[test]
fn database_supports_check_constraints_with_distinct_from_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE samples (
             id INTEGER PRIMARY KEY,
             marker INTEGER CHECK (marker IS DISTINCT FROM 0),
             zeroish INTEGER CHECK (zeroish IS NOT DISTINCT FROM 0)
         );",
    )
    .unwrap();

    db.execute("INSERT INTO samples VALUES (1, 1, 0);").unwrap();
    db.execute("INSERT INTO samples VALUES (2, NULL, 0);")
        .unwrap();

    let marker_error = db
        .execute("INSERT INTO samples VALUES (3, 0, 0);")
        .unwrap_err();
    assert!(marker_error.to_string().contains("check constraint"));

    let zeroish_error = db
        .execute("INSERT INTO samples VALUES (4, 1, NULL);")
        .unwrap_err();
    assert!(zeroish_error.to_string().contains("check constraint"));
}

#[test]
fn database_enforces_basic_foreign_keys() {
    let db = Database::memory();
    db.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY);
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
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY);
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
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY);
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
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY);
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
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY);
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
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY);
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

#[test]
fn database_accepts_unary_plus_and_hex_integer_literals() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT TYPEOF(+1),
                    +1,
                    TYPEOF(+.5),
                    +.5,
                    TYPEOF(0x10),
                    0x10,
                    TYPEOF(-0x10),
                    -0x10;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("integer"),
            Value::Integer(1),
            Value::from("real"),
            Value::Real(0.5),
            Value::from("integer"),
            Value::Integer(16),
            Value::from("integer"),
            Value::Integer(-16),
        ]]
    );

    db.execute(
        "CREATE TABLE nums (
            id INTEGER PRIMARY KEY,
            whole INTEGER DEFAULT (+1),
            frac REAL DEFAULT (+.5),
            mask INTEGER DEFAULT (0x10),
            delta INTEGER DEFAULT (-0x10)
        );
         INSERT INTO nums(id) VALUES (1);
         INSERT INTO nums VALUES (2, +2, +.25, 0x20, -0x20);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT whole, frac, mask, delta, TYPEOF(whole), TYPEOF(frac), TYPEOF(mask), TYPEOF(delta)
             FROM nums
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(1),
                Value::Real(0.5),
                Value::Integer(16),
                Value::Integer(-16),
                Value::from("integer"),
                Value::from("real"),
                Value::from("integer"),
                Value::from("integer"),
            ],
            vec![
                Value::Integer(2),
                Value::Real(0.25),
                Value::Integer(32),
                Value::Integer(-32),
                Value::from("integer"),
                Value::from("real"),
                Value::from("integer"),
                Value::from("integer"),
            ],
        ]
    );
}

#[test]
fn database_accepts_underscored_numeric_literals() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT TYPEOF(1_000),
                    1_000,
                    TYPEOF(1_234.5_6),
                    1_234.5_6,
                    TYPEOF(1_7e+1),
                    1_7e+1,
                    DATETIME(1_704_067_200, 'auto'),
                    JULIANDAY(2_460_310.5, 'auto');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("integer"),
            Value::Integer(1000),
            Value::from("real"),
            Value::Real(1234.56),
            Value::from("real"),
            Value::Real(170.0),
            Value::from("2024-01-01 00:00:00"),
            Value::Real(2460310.5),
        ]]
    );

    db.execute(
        "CREATE TABLE nums (
            id INTEGER PRIMARY KEY,
            whole INTEGER DEFAULT (1_000),
            frac REAL DEFAULT (1_234.5_6),
            sci REAL DEFAULT (1_7e+1)
        );
         INSERT INTO nums(id) VALUES (1);
         INSERT INTO nums VALUES (2, 2_000, 2_345.6_7, 2_5e+1);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT whole, frac, sci, TYPEOF(whole), TYPEOF(frac), TYPEOF(sci)
             FROM nums
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(1000),
                Value::Real(1234.56),
                Value::Real(170.0),
                Value::from("integer"),
                Value::from("real"),
                Value::from("real"),
            ],
            vec![
                Value::Integer(2000),
                Value::Real(2345.67),
                Value::Real(250.0),
                Value::from("integer"),
                Value::from("real"),
                Value::from("real"),
            ],
        ]
    );
}

#[test]
fn database_accepts_sql_comments() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT 1/*inline*/+/*x*/2
             -- trailing line comment
             ;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(3)]]);

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); /* schema comment */
         INSERT INTO users VALUES (1, 'alice'); -- first row
         INSERT INTO users VALUES (2, 'bob'); /* second row */",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT id, name
             FROM users /* source comment */
             WHERE id >= 1 -- filter comment
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
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
fn database_insert_or_ignore_skips_duplicate_primary_key_insert() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    db.execute("INSERT INTO users VALUES (1, 'alice');")
        .unwrap();
    db.execute("INSERT OR IGNORE INTO users VALUES (1, 'bob');")
        .unwrap();

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);
}

#[test]
fn database_supports_insert_values_returning_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    let returned = db
        .query("INSERT INTO users(name) VALUES ('alice') RETURNING id, name;")
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![Value::Integer(1), Value::from("alice")]]
    );

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, returned);
}

#[test]
fn database_supports_multi_row_insert_values_returning_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    let returned = db
        .query("INSERT INTO users(name) VALUES ('alice'), ('bob') RETURNING id, name;")
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, returned);
}

#[test]
fn database_supports_multi_row_insert_expr_returning_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'seed');").unwrap();

    let returned = db
        .query(
            "INSERT OR IGNORE INTO users(id, name)
             VALUES (1, 'ignored'), (2, LOWER('BOB')), (3, COALESCE(NULL, 'carol'))
             RETURNING id, name;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![Value::Integer(2), Value::from("bob")],
            vec![Value::Integer(3), Value::from("carol")],
        ]
    );

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(2), Value::Integer(3)]]);

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("seed")],
            vec![Value::Integer(2), Value::from("bob")],
            vec![Value::Integer(3), Value::from("carol")],
        ]
    );
}

#[test]
fn database_execute_rejects_multi_row_insert_returning_like_other_returning_statements() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    let error = db
        .execute("INSERT INTO users(name) VALUES ('alice'), ('bob') RETURNING id, name;")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sql error: RETURNING statements must use Database::query"
    );
}

#[test]
fn database_supports_insert_returning_wildcard_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             nick TEXT GENERATED ALWAYS AS (UPPER(name)) STORED
         );",
    )
    .unwrap();

    let returned = db
        .query("INSERT INTO users(name) VALUES ('alice') RETURNING *;")
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![
            Value::Integer(1),
            Value::from("alice"),
            Value::from("ALICE"),
        ]]
    );
}

#[test]
fn database_supports_insert_returning_wildcard_mixed_with_rowid_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    let returned = db
        .query("INSERT INTO users(name) VALUES ('alice'), ('bob') RETURNING *, rowid;")
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![Value::Integer(1), Value::from("alice"), Value::Integer(1)],
            vec![Value::Integer(2), Value::from("bob"), Value::Integer(2)],
        ]
    );
}

#[test]
fn database_supports_insert_on_conflict_do_nothing_returning_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'seed');")
        .unwrap();

    let returned = db
        .query(
            "INSERT INTO users VALUES
                 (1, 'ignored@example.com', 'dup-id'),
                 (2, 'b@example.com', 'bob'),
                 (3, 'a@example.com', 'dup-email')
             ON CONFLICT DO NOTHING
             RETURNING id, email, name;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![
            Value::Integer(2),
            Value::from("b@example.com"),
            Value::from("bob"),
        ]]
    );

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(1), Value::Integer(2)]]);

    let rows = db
        .query("SELECT id, email, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(1),
                Value::from("a@example.com"),
                Value::from("seed"),
            ],
            vec![
                Value::Integer(2),
                Value::from("b@example.com"),
                Value::from("bob"),
            ],
        ]
    );
}

#[test]
fn database_supports_insert_on_conflict_target_do_nothing_returning_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'seed');")
        .unwrap();

    let returned = db
        .query(
            "INSERT INTO users VALUES
                 (2, 'a@example.com', 'dup-email'),
                 (3, 'c@example.com', 'carol')
             ON CONFLICT(email) DO NOTHING
             RETURNING *, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![
            Value::Integer(3),
            Value::from("c@example.com"),
            Value::from("carol"),
            Value::Integer(3),
        ]]
    );
}

#[test]
fn database_insert_on_conflict_target_do_nothing_returning_does_not_ignore_other_unique_conflicts()
{
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'seed');")
        .unwrap();

    let error = db
        .query(
            "INSERT INTO users VALUES (1, 'b@example.com', 'dup-id')
             ON CONFLICT(email) DO NOTHING
             RETURNING id;",
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("duplicate primary key value for column 'id'")
    );
}

#[test]
fn database_execute_rejects_insert_on_conflict_do_nothing_returning_like_other_returning_statements()
 {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    let error = db
        .execute("INSERT INTO users VALUES (1, 'alice') ON CONFLICT DO NOTHING RETURNING id;")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sql error: RETURNING statements must use Database::query"
    );
}

#[test]
fn database_supports_insert_select_returning_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE src (id INTEGER, name TEXT NOT NULL);")
        .unwrap();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    db.execute("INSERT INTO src VALUES (1, 'alice'), (2, 'bob');")
        .unwrap();

    let returned = db
        .query("INSERT INTO users SELECT id, name FROM src RETURNING id, name, rowid;")
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![Value::Integer(1), Value::from("alice"), Value::Integer(1)],
            vec![Value::Integer(2), Value::from("bob"), Value::Integer(2)],
        ]
    );

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn database_supports_insert_select_returning_wildcard_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE src (id INTEGER, name TEXT NOT NULL);")
        .unwrap();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    db.execute("INSERT INTO src VALUES (1, 'alice'), (2, 'bob');")
        .unwrap();

    let returned = db
        .query("INSERT INTO users(id, name) SELECT id, UPPER(name) FROM src RETURNING *, rowid;")
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![Value::Integer(1), Value::from("ALICE"), Value::Integer(1)],
            vec![Value::Integer(2), Value::from("BOB"), Value::Integer(2)],
        ]
    );
}

#[test]
fn database_supports_insert_select_on_conflict_do_nothing_returning_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE src (
             id INTEGER,
             email TEXT,
             name TEXT NOT NULL
         );",
    )
    .unwrap();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'seed');")
        .unwrap();
    db.execute(
        "INSERT INTO src VALUES
             (1, 'ignored@example.com', 'dup-id'),
             (2, 'b@example.com', 'bob'),
             (3, 'a@example.com', 'dup-email');",
    )
    .unwrap();

    let returned = db
        .query(
            "INSERT INTO users
             SELECT id, email, name FROM src WHERE true
             ON CONFLICT DO NOTHING
             RETURNING id, email, name;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![
            Value::Integer(2),
            Value::from("b@example.com"),
            Value::from("bob"),
        ]]
    );

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(1), Value::Integer(2)]]);
}

#[test]
fn database_supports_insert_default_values_returning_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             name TEXT DEFAULT 'anon',
             active BOOLEAN DEFAULT TRUE
         );",
    )
    .unwrap();

    let returned = db
        .query("INSERT INTO users DEFAULT VALUES RETURNING id, name, active, rowid;")
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![
            Value::Integer(1),
            Value::from("anon"),
            Value::Boolean(true),
            Value::Integer(1),
        ]]
    );

    let rows = db.query("SELECT id, name, active FROM users;").unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("anon"),
            Value::Boolean(true),
        ]]
    );
}

#[test]
fn database_execute_rejects_insert_default_values_returning_like_other_returning_statements() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT DEFAULT 'anon');")
        .unwrap();

    let error = db
        .execute("INSERT INTO users DEFAULT VALUES RETURNING id, name;")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sql error: RETURNING statements must use Database::query"
    );
}

#[test]
fn database_supports_insert_on_conflict_do_update_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice', 1);")
        .unwrap();

    db.execute(
        "INSERT INTO users(id, email, name, hits)
         VALUES (2, 'a@example.com', 'alice2', 3)
         ON CONFLICT(email) DO UPDATE
         SET name = excluded.name, hits = users.hits + excluded.hits;",
    )
    .unwrap();

    let rows = db
        .query("SELECT id, email, name, hits FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("alice2"),
            Value::Integer(4),
        ]]
    );

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(1), Value::Integer(1)]]);
}

#[test]
fn database_supports_insert_on_conflict_do_update_returning_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice', 1);")
        .unwrap();

    let updated = db
        .query(
            "INSERT INTO users(id, email, name, hits)
             VALUES (2, 'a@example.com', 'alice2', 3)
             ON CONFLICT(email) DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert_eq!(
        updated,
        vec![vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("alice2"),
            Value::Integer(4),
            Value::Integer(1),
        ]]
    );

    let inserted = db
        .query(
            "INSERT INTO users(id, email, name, hits)
             VALUES (2, 'b@example.com', 'bob', 3)
             ON CONFLICT(email) DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             RETURNING *, rowid;",
        )
        .unwrap();
    assert_eq!(
        inserted,
        vec![vec![
            Value::Integer(2),
            Value::from("b@example.com"),
            Value::from("bob"),
            Value::Integer(3),
            Value::Integer(2),
        ]]
    );
}

#[test]
fn database_supports_targetless_insert_on_conflict_do_update_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice', 1);")
        .unwrap();

    let email_conflict = db
        .query(
            "INSERT INTO users(id, email, name, hits)
             VALUES (2, 'a@example.com', 'alice2', 3)
             ON CONFLICT DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert_eq!(
        email_conflict,
        vec![vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("alice2"),
            Value::Integer(4),
            Value::Integer(1),
        ]]
    );

    let id_conflict = db
        .query(
            "INSERT INTO users(id, email, name, hits)
             VALUES (1, 'b@example.com', 'id-conflict', 4)
             ON CONFLICT DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert_eq!(
        id_conflict,
        vec![vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("id-conflict"),
            Value::Integer(8),
            Value::Integer(1),
        ]]
    );
}

#[test]
fn database_supports_targetless_multi_row_insert_on_conflict_do_update_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();

    let returned = db
        .query(
            "INSERT INTO users(id, email, name, hits)
             VALUES
                 (1, 'a@example.com', 'alice', 1),
                 (2, 'a@example.com', 'alice2', 2)
             ON CONFLICT DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![
                Value::Integer(1),
                Value::from("a@example.com"),
                Value::from("alice"),
                Value::Integer(1),
                Value::Integer(1),
            ],
            vec![
                Value::Integer(1),
                Value::from("a@example.com"),
                Value::from("alice2"),
                Value::Integer(3),
                Value::Integer(1),
            ],
        ]
    );
}

#[test]
fn database_insert_on_conflict_do_update_where_false_skips_update_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice', 5);")
        .unwrap();

    let returned = db
        .query(
            "INSERT INTO users(id, email, name, hits)
             VALUES (2, 'a@example.com', 'alice2', 3)
             ON CONFLICT(email) DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             WHERE excluded.hits > users.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert!(returned.is_empty());

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(0), Value::Integer(1)]]);

    let rows = db
        .query("SELECT id, email, name, hits FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("alice"),
            Value::Integer(5),
        ]]
    );
}

#[test]
fn database_insert_on_conflict_do_update_where_true_updates_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice', 5);")
        .unwrap();

    let returned = db
        .query(
            "INSERT INTO users(id, email, name, hits)
             VALUES (2, 'a@example.com', 'alice2', 3)
             ON CONFLICT(email) DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             WHERE excluded.hits < users.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("alice2"),
            Value::Integer(8),
            Value::Integer(1),
        ]]
    );
}

#[test]
fn database_supports_multi_row_insert_on_conflict_do_update_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice', 1);")
        .unwrap();

    let returned = db
        .query(
            "INSERT INTO users(id, email, name, hits)
             VALUES
                 (2, 'a@example.com', 'alice2', 3),
                 (3, 'b@example.com', 'bob', 5)
             ON CONFLICT(email) DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![
                Value::Integer(1),
                Value::from("a@example.com"),
                Value::from("alice2"),
                Value::Integer(4),
                Value::Integer(1),
            ],
            vec![
                Value::Integer(3),
                Value::from("b@example.com"),
                Value::from("bob"),
                Value::Integer(5),
                Value::Integer(3),
            ],
        ]
    );

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(2), Value::Integer(3)]]);
}

#[test]
fn database_multi_row_insert_on_conflict_do_update_sees_prior_rows_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();

    let returned = db
        .query(
            "INSERT INTO users(id, email, name, hits)
             VALUES
                 (1, 'a@example.com', 'alice', 1),
                 (2, 'a@example.com', 'alice2', 2)
             ON CONFLICT(email) DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![
                Value::Integer(1),
                Value::from("a@example.com"),
                Value::from("alice"),
                Value::Integer(1),
                Value::Integer(1),
            ],
            vec![
                Value::Integer(1),
                Value::from("a@example.com"),
                Value::from("alice2"),
                Value::Integer(3),
                Value::Integer(1),
            ],
        ]
    );
}

#[test]
fn database_multi_row_insert_on_conflict_do_update_where_skips_rows_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice', 5);")
        .unwrap();

    let returned = db
        .query(
            "INSERT INTO users(id, email, name, hits)
             VALUES
                 (2, 'a@example.com', 'low', 3),
                 (3, 'a@example.com', 'high', 7)
             ON CONFLICT(email) DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             WHERE excluded.hits > users.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("high"),
            Value::Integer(12),
            Value::Integer(1),
        ]]
    );

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(1), Value::Integer(1)]]);
}

#[test]
fn database_supports_insert_select_on_conflict_do_update_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE src (
             id INTEGER,
             email TEXT,
             name TEXT NOT NULL,
             hits INTEGER
         );",
    )
    .unwrap();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice', 1);")
        .unwrap();
    db.execute(
        "INSERT INTO src VALUES
             (2, 'a@example.com', 'alice2', 3),
             (3, 'b@example.com', 'bob', 5);",
    )
    .unwrap();

    let returned = db
        .query(
            "INSERT INTO users
             SELECT id, email, name, hits FROM src WHERE true
             ON CONFLICT(email) DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![
                Value::Integer(1),
                Value::from("a@example.com"),
                Value::from("alice2"),
                Value::Integer(4),
                Value::Integer(1),
            ],
            vec![
                Value::Integer(3),
                Value::from("b@example.com"),
                Value::from("bob"),
                Value::Integer(5),
                Value::Integer(3),
            ],
        ]
    );

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(2), Value::Integer(3)]]);
}

#[test]
fn database_insert_select_on_conflict_do_update_where_skips_rows_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE src (
             id INTEGER,
             email TEXT,
             name TEXT NOT NULL,
             hits INTEGER
         );",
    )
    .unwrap();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL,
             hits INTEGER DEFAULT 0
         );",
    )
    .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice', 5);")
        .unwrap();
    db.execute(
        "INSERT INTO src VALUES
             (2, 'a@example.com', 'low', 3),
             (3, 'a@example.com', 'high', 7);",
    )
    .unwrap();

    let returned = db
        .query(
            "INSERT INTO users
             SELECT id, email, name, hits FROM src WHERE true
             ON CONFLICT(email) DO UPDATE
             SET name = excluded.name, hits = hits + excluded.hits
             WHERE excluded.hits > users.hits
             RETURNING id, email, name, hits, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("high"),
            Value::Integer(12),
            Value::Integer(1),
        ]]
    );

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(1), Value::Integer(2)]]);
}

#[test]
fn database_supports_update_returning_updated_rows_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER);")
        .unwrap();
    db.execute(
        "INSERT INTO users VALUES
             (1, 'alice', 10),
             (2, 'bob', 20),
             (3, 'carol', 30);",
    )
    .unwrap();

    let returned = db
        .query(
            "UPDATE users
             SET score = score + 5, name = UPPER(name)
             WHERE score >= 20
             RETURNING id, name, score, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![
                Value::Integer(2),
                Value::from("BOB"),
                Value::Integer(25),
                Value::Integer(2),
            ],
            vec![
                Value::Integer(3),
                Value::from("CAROL"),
                Value::Integer(35),
                Value::Integer(3),
            ],
        ]
    );

    let rows = db
        .query("SELECT id, name, score FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice"), Value::Integer(10)],
            vec![Value::Integer(2), Value::from("BOB"), Value::Integer(25)],
            vec![Value::Integer(3), Value::from("CAROL"), Value::Integer(35)],
        ]
    );
}

#[test]
fn database_supports_update_returning_wildcard_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    db.execute("INSERT INTO users(name) VALUES ('alice'), ('bob');")
        .unwrap();

    let returned = db
        .query("UPDATE users SET name = UPPER(name) RETURNING *, rowid;")
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![Value::Integer(1), Value::from("ALICE"), Value::Integer(1)],
            vec![Value::Integer(2), Value::from("BOB"), Value::Integer(2)],
        ]
    );
}

#[test]
fn database_supports_update_returning_order_by_limit_offset_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER);")
        .unwrap();
    db.execute(
        "INSERT INTO users VALUES
             (1, 'alice', 20),
             (2, 'bob', 10),
             (3, 'carol', 30);",
    )
    .unwrap();

    let returned = db
        .query(
            "UPDATE users
             SET name = UPPER(name)
             RETURNING id, name
             ORDER BY score ASC
             LIMIT 1 OFFSET 1;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![Value::Integer(1), Value::from("ALICE")]]
    );

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("ALICE")],
            vec![Value::Integer(2), Value::from("bob")],
            vec![Value::Integer(3), Value::from("carol")],
        ]
    );
}

#[test]
fn database_execute_rejects_update_returning_like_other_returning_statements() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    db.execute("INSERT INTO users(name) VALUES ('alice');")
        .unwrap();

    let error = db
        .execute("UPDATE users SET name = 'ALICE' RETURNING id, name;")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sql error: RETURNING statements must use Database::query"
    );
}

#[test]
fn database_supports_delete_returning_deleted_rows_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER);")
        .unwrap();
    db.execute(
        "INSERT INTO users VALUES
             (1, 'alice', 10),
             (2, 'bob', 20),
             (3, 'carol', 30);",
    )
    .unwrap();

    let returned = db
        .query("DELETE FROM users WHERE score >= 20 RETURNING id, name, score, rowid;")
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![
                Value::Integer(2),
                Value::from("bob"),
                Value::Integer(20),
                Value::Integer(2),
            ],
            vec![
                Value::Integer(3),
                Value::from("carol"),
                Value::Integer(30),
                Value::Integer(3),
            ],
        ]
    );

    let rows = db
        .query("SELECT id, name, score FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("alice"),
            Value::Integer(10),
        ]]
    );
}

#[test]
fn database_supports_delete_returning_wildcard_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    db.execute("INSERT INTO users(name) VALUES ('alice'), ('bob');")
        .unwrap();

    let returned = db.query("DELETE FROM users RETURNING *, rowid;").unwrap();
    assert_eq!(
        returned,
        vec![
            vec![Value::Integer(1), Value::from("alice"), Value::Integer(1)],
            vec![Value::Integer(2), Value::from("bob"), Value::Integer(2)],
        ]
    );

    assert_eq!(
        db.query("SELECT COUNT(*) FROM users;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn database_supports_delete_returning_order_by_limit_offset_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER);")
        .unwrap();
    db.execute(
        "INSERT INTO users VALUES
             (1, 'alice', 20),
             (2, 'bob', 10),
             (3, 'carol', 30);",
    )
    .unwrap();

    let returned = db
        .query(
            "DELETE FROM users
             RETURNING id, name
             ORDER BY score ASC
             LIMIT 1 OFFSET 1;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![Value::Integer(1), Value::from("alice")]]
    );

    let remaining = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(
        remaining,
        vec![
            vec![Value::Integer(2), Value::from("bob")],
            vec![Value::Integer(3), Value::from("carol")],
        ]
    );
}

#[test]
fn database_execute_rejects_delete_returning_like_other_returning_statements() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    db.execute("INSERT INTO users(name) VALUES ('alice');")
        .unwrap();

    let error = db
        .execute("DELETE FROM users RETURNING id, name;")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sql error: RETURNING statements must use Database::query"
    );
}

#[test]
fn database_insert_on_conflict_do_nothing_skips_duplicate_primary_key_insert() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    db.execute("INSERT INTO users VALUES (1, 'alice');")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'bob') ON CONFLICT DO NOTHING;")
        .unwrap();

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);
}

#[test]
fn database_insert_on_conflict_target_do_nothing_skips_only_matching_conflict() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );",
    )
    .unwrap();

    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice');")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'b@example.com', 'bob') ON CONFLICT(id) DO NOTHING;")
        .unwrap();

    let rows = db
        .query("SELECT id, email, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("alice"),
        ]]
    );
}

#[test]
fn database_insert_on_conflict_target_do_nothing_does_not_ignore_other_unique_conflicts() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );",
    )
    .unwrap();

    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice');")
        .unwrap();
    let error = db
        .execute("INSERT INTO users VALUES (2, 'a@example.com', 'bob') ON CONFLICT(id) DO NOTHING;")
        .unwrap_err();
    assert!(error.to_string().contains("unique index"));
}

#[test]
fn database_insert_select_on_conflict_do_nothing_skips_duplicate_primary_key_rows() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE archive_users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
    )
    .unwrap();

    db.execute(
        "INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO archive_users VALUES (1, 'existing');",
    )
    .unwrap();
    db.execute(
        "INSERT INTO archive_users
         SELECT id, name FROM users
         ON CONFLICT DO NOTHING;",
    )
    .unwrap();

    let rows = db
        .query("SELECT id, name FROM archive_users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("existing")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn database_insert_select_on_conflict_target_do_nothing_does_not_ignore_other_unique_conflicts() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE source_users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );
         CREATE TABLE archive_users (
             id INTEGER PRIMARY KEY,
             email TEXT UNIQUE,
             name TEXT NOT NULL
         );",
    )
    .unwrap();

    db.execute(
        "INSERT INTO source_users VALUES (2, 'a@example.com', 'bob');
         INSERT INTO archive_users VALUES (1, 'a@example.com', 'alice');",
    )
    .unwrap();

    let error = db
        .execute(
            "INSERT INTO archive_users
             SELECT id, email, name FROM source_users
             ON CONFLICT(id) DO NOTHING;",
        )
        .unwrap_err();
    assert!(error.to_string().contains("unique index"));
}

#[test]
fn database_insert_or_ignore_still_reports_foreign_key_failures() {
    let db = Database::memory();
    db.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY);
         CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             user_id INTEGER REFERENCES users(id)
         );",
    )
    .unwrap();

    let error = db
        .execute("INSERT OR IGNORE INTO orders VALUES (1, 404);")
        .unwrap_err();
    assert!(error.to_string().contains("foreign key constraint failed"));

    let rows = db.query("SELECT id, user_id FROM orders;").unwrap();
    assert!(
        rows.is_empty(),
        "unexpected rows after failed insert: {rows:?}"
    );
}

#[test]
fn database_insert_or_replace_replaces_conflicting_primary_key_row() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    db.execute("INSERT INTO users VALUES (1, 'alice');")
        .unwrap();
    db.execute("INSERT OR REPLACE INTO users VALUES (1, 'bob');")
        .unwrap();

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("bob")]]);
}

#[test]
fn database_replace_into_replaces_conflicting_primary_key_row() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    db.execute("INSERT INTO users VALUES (1, 'alice');")
        .unwrap();
    db.execute("REPLACE INTO users VALUES (1, 'bob');").unwrap();

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("bob")]]);
}

#[test]
fn database_insert_or_replace_replaces_conflicting_unique_index_row() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, name TEXT NOT NULL);
         CREATE UNIQUE INDEX idx_users_email_unique ON users(email);",
    )
    .unwrap();

    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'alice');")
        .unwrap();
    db.execute("INSERT OR REPLACE INTO users VALUES (2, 'a@example.com', 'bob');")
        .unwrap();

    let rows = db
        .query("SELECT id, email, name FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(2),
            Value::from("a@example.com"),
            Value::from("bob"),
        ]]
    );
}

#[test]
fn database_insert_or_replace_reports_foreign_key_dependents() {
    let db = Database::memory();
    db.execute(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE);
         CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             user_id INTEGER REFERENCES users(id)
         );
         INSERT INTO users VALUES (1, 'a@example.com');
         INSERT INTO orders VALUES (10, 1);",
    )
    .unwrap();

    let error = db
        .execute("INSERT OR REPLACE INTO users VALUES (1, 'b@example.com');")
        .unwrap_err();
    assert!(error.to_string().contains("foreign key constraint failed"));

    let rows = db
        .query("SELECT id, email FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Integer(1), Value::from("a@example.com")]]
    );
}

#[test]
fn database_insert_or_rollback_aborts_explicit_transaction_on_constraint_failure() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'alice');")
        .unwrap();

    let error = db
        .execute(
            "BEGIN;
             INSERT INTO users VALUES (2, 'bob');
             INSERT OR ROLLBACK INTO users VALUES (1, 'dupe');",
        )
        .unwrap_err();
    assert!(error.to_string().contains("duplicate primary key"));

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("alice")]]);

    let txn_error = db.execute("COMMIT;").unwrap_err();
    assert!(txn_error.to_string().contains("no active transaction"));
}

#[test]
fn database_insert_or_abort_keeps_explicit_transaction_open_on_constraint_failure() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'alice');")
        .unwrap();

    db.execute("BEGIN;").unwrap();
    db.execute("INSERT INTO users VALUES (2, 'bob');").unwrap();

    let error = db
        .execute("INSERT OR ABORT INTO users VALUES (1, 'dupe');")
        .unwrap_err();
    assert!(error.to_string().contains("duplicate primary key"));

    db.execute("INSERT INTO users VALUES (3, 'carol');")
        .unwrap();
    db.execute("COMMIT;").unwrap();

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
            vec![Value::Integer(3), Value::from("carol")],
        ]
    );
}

#[test]
fn database_insert_or_fail_keeps_explicit_transaction_open_on_constraint_failure() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'alice');")
        .unwrap();

    db.execute("BEGIN;").unwrap();
    db.execute("INSERT INTO users VALUES (2, 'bob');").unwrap();

    let error = db
        .execute("INSERT OR FAIL INTO users VALUES (1, 'dupe');")
        .unwrap_err();
    assert!(error.to_string().contains("duplicate primary key"));

    db.execute("INSERT INTO users VALUES (3, 'carol');")
        .unwrap();
    db.execute("COMMIT;").unwrap();

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
            vec![Value::Integer(3), Value::from("carol")],
        ]
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
fn database_supports_alter_table_drop_column() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
         INSERT INTO users VALUES (1, 'alice', 30);
         ALTER TABLE users DROP COLUMN age;",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT * FROM users;").unwrap(),
        vec![vec![Value::Integer(1), Value::from("alice")]]
    );
}

#[test]
fn database_supports_insert_select() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE archive_users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO archive_users SELECT id, name FROM users WHERE id >= 2;",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT id, name FROM archive_users ORDER BY id;")
            .unwrap(),
        vec![vec![Value::Integer(2), Value::from("bob")]]
    );
}

#[test]
fn database_supports_create_table_as_select_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         CREATE TABLE archive_users AS
         SELECT id, UPPER(name) AS name FROM users WHERE id >= 2;",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT id, name FROM archive_users ORDER BY id;")
            .unwrap(),
        vec![vec![Value::Integer(2), Value::from("BOB")]]
    );
    assert_eq!(
        db.query("PRAGMA table_info(archive_users);").unwrap(),
        vec![
            vec![
                Value::Integer(0),
                Value::from("id"),
                Value::from("ANY"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(0),
            ],
            vec![
                Value::Integer(1),
                Value::from("name"),
                Value::from("ANY"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(0),
            ],
        ]
    );
}

#[test]
fn database_supports_create_table_if_not_exists_as_select_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         CREATE TABLE IF NOT EXISTS archive_users AS SELECT id, name FROM users;",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT id, name FROM archive_users;").unwrap(),
        vec![vec![Value::Integer(1), Value::from("alice")]]
    );

    db.execute(
        "CREATE TABLE IF NOT EXISTS archive_users AS
         SELECT missing FROM no_such_source;",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT id, name FROM archive_users;").unwrap(),
        vec![vec![Value::Integer(1), Value::from("alice")]]
    );
}

#[test]
fn database_renames_duplicate_create_table_as_select_columns_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics AS
         SELECT 1 AS value, 2 AS value, 3 AS \"value:1\", 4 AS value;",
    )
        .unwrap();

    assert_eq!(
        db.query("PRAGMA table_info(metrics);").unwrap(),
        vec![
            vec![
                Value::Integer(0),
                Value::from("value"),
                Value::from("ANY"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(0),
            ],
            vec![
                Value::Integer(1),
                Value::from("value:1"),
                Value::from("ANY"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(0),
            ],
            vec![
                Value::Integer(2),
                Value::from("value:2"),
                Value::from("ANY"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(0),
            ],
            vec![
                Value::Integer(3),
                Value::from("value:3"),
                Value::from("ANY"),
                Value::Integer(0),
                Value::Null,
                Value::Integer(0),
            ],
        ]
    );
    assert_eq!(
        db.query("SELECT value, \"value:1\", \"value:2\", \"value:3\" FROM metrics;")
            .unwrap(),
        vec![vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(3),
            Value::Integer(4),
        ]]
    );
}

#[test]
fn database_supports_insert_select_with_explicit_column_list_and_defaults() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE archive_users (
             id INTEGER PRIMARY KEY,
             name TEXT,
             active BOOLEAN DEFAULT true
         );
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO archive_users (id, name)
         SELECT id, name FROM users WHERE id >= 2;",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT id, name, active FROM archive_users ORDER BY id;")
            .unwrap(),
        vec![vec![
            Value::Integer(2),
            Value::from("bob"),
            Value::Boolean(true),
        ]]
    );
}

#[test]
fn database_supports_replace_into_select() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE archive_users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO archive_users VALUES (1, 'stale');
         REPLACE INTO archive_users
         SELECT id, name FROM users WHERE id = 1;",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT id, name FROM archive_users ORDER BY id;")
            .unwrap(),
        vec![vec![Value::Integer(1), Value::from("alice")]]
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
fn database_supports_delete_order_by_limit_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER);
         INSERT INTO users VALUES (1, 'alice', 30);
         INSERT INTO users VALUES (2, 'bob', 10);
         INSERT INTO users VALUES (3, 'carol', 20);",
    )
    .unwrap();

    db.execute("DELETE FROM users ORDER BY score ASC LIMIT 2;")
        .unwrap();
    let rows = db
        .query("SELECT id, name, score FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("alice"),
            Value::Integer(30)
        ]]
    );
}

#[test]
fn database_supports_delete_order_by_limit_offset_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER);
         INSERT INTO users VALUES (1, 'alice', 30);
         INSERT INTO users VALUES (2, 'bob', 10);
         INSERT INTO users VALUES (3, 'carol', 20);",
    )
    .unwrap();

    db.execute("DELETE FROM users ORDER BY score ASC LIMIT 1 OFFSET 1;")
        .unwrap();
    let rows = db
        .query("SELECT id, name, score FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice"), Value::Integer(30)],
            vec![Value::Integer(2), Value::from("bob"), Value::Integer(10)],
        ]
    );
}

#[test]
fn database_supports_update_order_by_limit_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER);
         INSERT INTO users VALUES (1, 'alice', 30);
         INSERT INTO users VALUES (2, 'bob', 10);
         INSERT INTO users VALUES (3, 'carol', 20);",
    )
    .unwrap();

    db.execute("UPDATE users SET name = UPPER(name) ORDER BY score ASC LIMIT 2;")
        .unwrap();
    let rows = db
        .query("SELECT id, name, score FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice"), Value::Integer(30)],
            vec![Value::Integer(2), Value::from("BOB"), Value::Integer(10)],
            vec![Value::Integer(3), Value::from("CAROL"), Value::Integer(20)],
        ]
    );

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(2), Value::Integer(3)]]);
}

#[test]
fn database_supports_update_order_by_limit_offset_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER);
         INSERT INTO users VALUES (1, 'alice', 30);
         INSERT INTO users VALUES (2, 'bob', 10);
         INSERT INTO users VALUES (3, 'carol', 20);",
    )
    .unwrap();

    db.execute("UPDATE users SET name = UPPER(name) ORDER BY score ASC LIMIT 1 OFFSET 1;")
        .unwrap();
    let rows = db
        .query("SELECT id, name, score FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice"), Value::Integer(30)],
            vec![Value::Integer(2), Value::from("bob"), Value::Integer(10)],
            vec![Value::Integer(3), Value::from("CAROL"), Value::Integer(20)],
        ]
    );

    let metadata = db.query("SELECT changes(), last_insert_rowid();").unwrap();
    assert_eq!(metadata, vec![vec![Value::Integer(1), Value::Integer(3)]]);
}

#[test]
fn database_supports_limit_offset_forms_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, 'carol');
         INSERT INTO users VALUES (4, 'dave');",
    )
    .unwrap();

    let explicit_offset = db
        .query("SELECT name FROM users ORDER BY id ASC LIMIT 2 OFFSET 1;")
        .unwrap();
    assert_eq!(
        explicit_offset,
        vec![vec![Value::from("bob")], vec![Value::from("carol")]]
    );

    let real_offset = db
        .query("SELECT name FROM users ORDER BY id ASC LIMIT 2.0 OFFSET 1.0;")
        .unwrap();
    assert_eq!(
        real_offset,
        vec![vec![Value::from("bob")], vec![Value::from("carol")]]
    );

    let comma_offset = db
        .query("SELECT name FROM users ORDER BY id ASC LIMIT 1, 2;")
        .unwrap();
    assert_eq!(
        comma_offset,
        vec![vec![Value::from("bob")], vec![Value::from("carol")]]
    );

    let negative_limit = db
        .query("SELECT name FROM users ORDER BY id ASC LIMIT -1 OFFSET 1;")
        .unwrap();
    assert_eq!(
        negative_limit,
        vec![
            vec![Value::from("bob")],
            vec![Value::from("carol")],
            vec![Value::from("dave")],
        ]
    );

    let negative_offset = db
        .query("SELECT name FROM users ORDER BY id ASC LIMIT 1 OFFSET -2;")
        .unwrap();
    assert_eq!(negative_offset, vec![vec![Value::from("alice")]]);
}

#[test]
fn database_supports_update_assignment_scalar_expressions() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value INTEGER);
         INSERT INTO metrics VALUES (1, 10);
         INSERT INTO metrics VALUES (2, 20);
         UPDATE metrics SET value = value + 1 WHERE id = 2;",
    )
    .unwrap();

    let rows = db
        .query("SELECT id, value FROM metrics ORDER BY id;")
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Integer(10)],
            vec![Value::Integer(2), Value::Integer(21)],
        ]
    );
}

#[test]
fn database_supports_qualified_update_assignment_scalar_expressions() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value INTEGER);
         INSERT INTO metrics VALUES (1, 10);
         UPDATE metrics AS m SET value = m.value + 1 WHERE m.id = 1;",
    )
    .unwrap();

    let rows = db.query("SELECT id, value FROM metrics;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(11)]]);
}

#[test]
fn database_updates_multiple_assignment_expressions_from_original_row_values() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER);
         INSERT INTO metrics VALUES (1, 10, 100);
         UPDATE metrics SET a = a + 1, b = a + 1 WHERE id = 1;",
    )
    .unwrap();

    let rows = db.query("SELECT a, b FROM metrics;").unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(11), Value::Integer(11)]]);
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

    let ordinal_grouped = db
        .query("SELECT active, COUNT(*) AS total FROM users GROUP BY 1 ORDER BY active ASC;")
        .unwrap();
    assert_eq!(ordinal_grouped, grouped);

    let empty_arg_count = db.query("SELECT COUNT(), COUNT(*) FROM users;").unwrap();
    assert_eq!(
        empty_arg_count,
        vec![vec![Value::Integer(3), Value::Integer(3)]]
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

    let cross_join_on = db
        .query(
            "SELECT u.name, o.amount
             FROM users u CROSS JOIN orders o ON u.id = o.user_id
             WHERE o.amount > 10
             ORDER BY u.name ASC, o.amount ASC;",
        )
        .unwrap();
    assert_eq!(cross_join_on, joined);

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
fn database_supports_right_and_full_outer_join_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE a (id INTEGER, av TEXT);
         CREATE TABLE b (id INTEGER, bv TEXT);
         INSERT INTO a VALUES (1, 'a1');
         INSERT INTO a VALUES (2, 'a2');
         INSERT INTO b VALUES (2, 'b2');
         INSERT INTO b VALUES (3, 'b3');",
    )
    .unwrap();

    let right_joined = db
        .query(
            "SELECT a.id, av, b.id, bv
             FROM a RIGHT JOIN b ON a.id = b.id
             ORDER BY b.id;",
        )
        .unwrap();
    assert_eq!(
        right_joined,
        vec![
            vec![
                Value::Integer(2),
                Value::from("a2"),
                Value::Integer(2),
                Value::from("b2"),
            ],
            vec![
                Value::Null,
                Value::Null,
                Value::Integer(3),
                Value::from("b3")
            ],
        ]
    );

    let full_joined = db
        .query(
            "SELECT a.id, av, b.id, bv
             FROM a FULL OUTER JOIN b ON a.id = b.id
             ORDER BY COALESCE(a.id, b.id);",
        )
        .unwrap();
    assert_eq!(
        full_joined,
        vec![
            vec![
                Value::Integer(1),
                Value::from("a1"),
                Value::Null,
                Value::Null
            ],
            vec![
                Value::Integer(2),
                Value::from("a2"),
                Value::Integer(2),
                Value::from("b2"),
            ],
            vec![
                Value::Null,
                Value::Null,
                Value::Integer(3),
                Value::from("b3")
            ],
        ]
    );

    let right_using = db
        .query(
            "SELECT *
             FROM a RIGHT JOIN b USING(id)
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(
        right_using,
        vec![
            vec![Value::Integer(2), Value::from("a2"), Value::from("b2")],
            vec![Value::Integer(3), Value::Null, Value::from("b3")],
        ]
    );

    let full_using = db
        .query(
            "SELECT *
             FROM a FULL OUTER JOIN b USING(id)
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(
        full_using,
        vec![
            vec![Value::Integer(1), Value::from("a1"), Value::Null],
            vec![Value::Integer(2), Value::from("a2"), Value::from("b2")],
            vec![Value::Integer(3), Value::Null, Value::from("b3")],
        ]
    );
}

#[test]
fn database_supports_join_using_clause_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE a (id INTEGER, av TEXT);
         CREATE TABLE b (id INTEGER, bv TEXT);
         INSERT INTO a VALUES (1, 'a1');
         INSERT INTO a VALUES (2, 'a2');
         INSERT INTO b VALUES (2, 'b2');
         INSERT INTO b VALUES (3, 'b3');",
    )
    .unwrap();

    let inner_joined = db
        .query(
            "SELECT a.id, av, b.id, bv
             FROM a JOIN b USING(id)
             ORDER BY a.id;",
        )
        .unwrap();
    assert_eq!(
        inner_joined,
        vec![vec![
            Value::Integer(2),
            Value::from("a2"),
            Value::Integer(2),
            Value::from("b2"),
        ]]
    );

    let left_joined = db
        .query(
            "SELECT a.id, av, b.id, bv
             FROM a LEFT JOIN b USING(id)
             ORDER BY a.id;",
        )
        .unwrap();
    assert_eq!(
        left_joined,
        vec![
            vec![
                Value::Integer(1),
                Value::from("a1"),
                Value::Null,
                Value::Null
            ],
            vec![
                Value::Integer(2),
                Value::from("a2"),
                Value::Integer(2),
                Value::from("b2"),
            ],
        ]
    );

    let wildcard_joined = db
        .query(
            "SELECT *
             FROM a JOIN b USING(id);",
        )
        .unwrap();
    assert_eq!(
        wildcard_joined,
        vec![vec![
            Value::Integer(2),
            Value::from("a2"),
            Value::from("b2")
        ]]
    );

    let unqualified_joined = db
        .query(
            "SELECT id, av, bv
             FROM a JOIN b USING(id);",
        )
        .unwrap();
    assert_eq!(
        unqualified_joined,
        vec![vec![
            Value::Integer(2),
            Value::from("a2"),
            Value::from("b2")
        ]]
    );

    let right_joined = db
        .query(
            "SELECT *
             FROM a NATURAL RIGHT JOIN b
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(
        right_joined,
        vec![
            vec![Value::Integer(2), Value::from("a2"), Value::from("b2")],
            vec![Value::Integer(3), Value::Null, Value::from("b3")],
        ]
    );

    let full_joined = db
        .query(
            "SELECT *
             FROM a NATURAL FULL OUTER JOIN b
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(
        full_joined,
        vec![
            vec![Value::Integer(1), Value::from("a1"), Value::Null],
            vec![Value::Integer(2), Value::from("a2"), Value::from("b2")],
            vec![Value::Integer(3), Value::Null, Value::from("b3")],
        ]
    );
}

#[test]
fn database_supports_natural_join_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE a (id INTEGER, av TEXT);
         CREATE TABLE b (id INTEGER, bv TEXT);
         INSERT INTO a VALUES (1, 'a1');
         INSERT INTO a VALUES (2, 'a2');
         INSERT INTO b VALUES (2, 'b2');
         INSERT INTO b VALUES (3, 'b3');",
    )
    .unwrap();

    let inner_joined = db
        .query(
            "SELECT *
             FROM a NATURAL JOIN b
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(
        inner_joined,
        vec![vec![
            Value::Integer(2),
            Value::from("a2"),
            Value::from("b2")
        ]]
    );

    let left_joined = db
        .query(
            "SELECT *
             FROM a NATURAL LEFT JOIN b
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(
        left_joined,
        vec![
            vec![Value::Integer(1), Value::from("a1"), Value::Null],
            vec![Value::Integer(2), Value::from("a2"), Value::from("b2")],
        ]
    );

    let unqualified_joined = db
        .query(
            "SELECT id, av, bv
             FROM a NATURAL JOIN b;",
        )
        .unwrap();
    assert_eq!(
        unqualified_joined,
        vec![vec![
            Value::Integer(2),
            Value::from("a2"),
            Value::from("b2")
        ]]
    );

    let cross_using = db
        .query(
            "SELECT *
             FROM a CROSS JOIN b USING(id);",
        )
        .unwrap();
    assert_eq!(
        cross_using,
        vec![vec![
            Value::Integer(2),
            Value::from("a2"),
            Value::from("b2")
        ]]
    );

    let natural_cross = db
        .query(
            "SELECT *
             FROM a NATURAL CROSS JOIN b
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(
        natural_cross,
        vec![vec![
            Value::Integer(2),
            Value::from("a2"),
            Value::from("b2")
        ]]
    );

    db.execute(
        "CREATE TABLE c (cx INTEGER);
         CREATE TABLE d (dy INTEGER);
         INSERT INTO c VALUES (1), (2);
         INSERT INTO d VALUES (10), (20);",
    )
    .unwrap();
    let natural_cross_without_common_columns = db
        .query(
            "SELECT *
             FROM c NATURAL CROSS JOIN d
             ORDER BY cx, dy;",
        )
        .unwrap();
    assert_eq!(
        natural_cross_without_common_columns,
        vec![
            vec![Value::Integer(1), Value::Integer(10)],
            vec![Value::Integer(1), Value::Integer(20)],
            vec![Value::Integer(2), Value::Integer(10)],
            vec![Value::Integer(2), Value::Integer(20)],
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
fn database_supports_intersect_and_except_compound_selects_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE left_values (value TEXT);
         CREATE TABLE right_values (value TEXT);
         INSERT INTO left_values VALUES ('a');
         INSERT INTO left_values VALUES ('b');
         INSERT INTO left_values VALUES ('b');
         INSERT INTO left_values VALUES ('c');
         INSERT INTO right_values VALUES ('b');
         INSERT INTO right_values VALUES ('d');",
    )
    .unwrap();

    let except_rows = db
        .query(
            "SELECT value FROM left_values
             EXCEPT
             SELECT value FROM right_values
             ORDER BY value ASC;",
        )
        .unwrap();
    assert_eq!(
        except_rows,
        vec![vec![Value::from("a")], vec![Value::from("c")]]
    );

    let intersect_rows = db
        .query(
            "SELECT value FROM left_values
             INTERSECT
             SELECT value FROM right_values
             ORDER BY value ASC;",
        )
        .unwrap();
    assert_eq!(intersect_rows, vec![vec![Value::from("b")]]);
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

    let glob_rows = db
        .query("SELECT name FROM users WHERE name GLOB 'ali*' ORDER BY 1 ASC;")
        .unwrap();
    assert_eq!(
        glob_rows,
        vec![vec![Value::from("alice")], vec![Value::from("alicia")]]
    );

    let glob_class_rows = db
        .query("SELECT name FROM users WHERE name GLOB 'a[lb]i*' ORDER BY 1 ASC;")
        .unwrap();
    assert_eq!(
        glob_class_rows,
        vec![vec![Value::from("alice")], vec![Value::from("alicia")]]
    );

    let glob_range_rows = db
        .query("SELECT name FROM users WHERE name GLOB 'a[a-z]i*' ORDER BY 1 ASC;")
        .unwrap();
    assert_eq!(
        glob_range_rows,
        vec![vec![Value::from("alice")], vec![Value::from("alicia")]]
    );

    let glob_negated_class_rows = db
        .query("SELECT name FROM users WHERE name GLOB '[^bd]*' ORDER BY 1 ASC;")
        .unwrap();
    assert_eq!(
        glob_negated_class_rows,
        vec![
            vec![Value::from("alice")],
            vec![Value::from("alicia")],
            vec![Value::from("carol")],
        ]
    );

    let not_glob_rows = db
        .query("SELECT name FROM users WHERE name NOT GLOB '*a*' ORDER BY name ASC;")
        .unwrap();
    assert_eq!(not_glob_rows, vec![vec![Value::from("bob")]]);

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

    let is_bool_rows = db
        .query(
            "SELECT 1 IS TRUE,
                    0 IS TRUE,
                    NULL IS TRUE,
                    1 IS FALSE,
                    0 IS FALSE,
                    NULL IS FALSE,
                    1 IS NOT TRUE,
                    0 IS NOT TRUE,
                    NULL IS NOT TRUE,
                    1 IS NOT FALSE,
                    0 IS NOT FALSE,
                    NULL IS NOT FALSE,
                    'abc' IS TRUE,
                    'abc' IS FALSE,
                    '1abc' IS TRUE,
                    '0abc' IS FALSE,
                    '0.1abc' IS TRUE,
                    X'31' IS TRUE,
                    X'30' IS FALSE;",
        )
        .unwrap();
    assert_eq!(
        is_bool_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
        ]]
    );

    let not_rows = db
        .query(
            "SELECT NOT 0,
                    NOT 1,
                    NOT NULL,
                    NOT 'abc',
                    NOT '';",
        )
        .unwrap();
    assert_eq!(
        not_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Null,
            Value::Boolean(true),
            Value::Boolean(true),
        ]]
    );

    let bare_truthy = db
        .query("SELECT name FROM users WHERE '1abc' ORDER BY name ASC;")
        .unwrap();
    assert_eq!(
        bare_truthy,
        vec![
            vec![Value::from("alice")],
            vec![Value::from("alicia")],
            vec![Value::from("bob")],
            vec![Value::from("carol")],
            vec![Value::from("dave")],
        ]
    );

    let bare_false = db.query("SELECT name FROM users WHERE 'abc';").unwrap();
    assert!(bare_false.is_empty());

    let bare_null = db.query("SELECT name FROM users WHERE NULL;").unwrap();
    assert!(bare_null.is_empty());

    let is_rows = db
        .query(
            "SELECT 1 IS 1,
                    1 IS 2,
                    NULL IS NULL,
                    NULL IS 1,
                    1 IS NULL,
                    1 IS NOT 1,
                    1 IS NOT 2,
                    NULL IS NOT NULL,
                    NULL IS NOT 1,
                    1 IS NOT NULL;",
        )
        .unwrap();
    assert_eq!(
        is_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
        ]]
    );

    let is_distinct_rows = db
        .query(
            "SELECT 1 IS DISTINCT FROM NULL,
                    NULL IS DISTINCT FROM NULL,
                    1 IS DISTINCT FROM 1,
                    1 IS NOT DISTINCT FROM 1,
                    NULL IS NOT DISTINCT FROM NULL,
                    1 IS NOT DISTINCT FROM NULL;",
        )
        .unwrap();
    assert_eq!(
        is_distinct_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
        ]]
    );

    let null_suffix_rows = db
        .query(
            "SELECT 1 ISNULL,
                    NULL ISNULL,
                    1 NOTNULL,
                    NULL NOTNULL,
                    1 NOT NULL,
                    NULL NOT NULL;",
        )
        .unwrap();
    assert_eq!(
        null_suffix_rows,
        vec![vec![
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(false),
        ]]
    );

    let in_null_semantics_rows = db
        .query(
            "SELECT 1 IN (),
                    NULL IN (),
                    1 NOT IN (),
                    NULL NOT IN (),
                    1 IN (1, 2),
                    3 IN (1, 2),
                    NULL IN (1, 2),
                    1 IN (NULL, 2),
                    3 IN (NULL, 2),
                    NULL IN (NULL, 2),
                    1 NOT IN (NULL, 2),
                    3 NOT IN (NULL, 2),
                    NULL NOT IN (NULL, 2);",
        )
        .unwrap();
    assert_eq!(
        in_null_semantics_rows,
        vec![vec![
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );

    let scalar_compare_rows = db
        .query(
            "SELECT 1 = 1,
                    1 = 2,
                    1 == 1,
                    1 == 2,
                    NULL = NULL,
                    1 <> 2,
                    1 <> 1,
                    2 > 1,
                    2 >= 2,
                    1 < 2,
                    1 <= 1;",
        )
        .unwrap();
    assert_eq!(
        scalar_compare_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Null,
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
        ]]
    );

    let scalar_predicate_rows = db
        .query(
            "SELECT 'abc' LIKE 'a%',
                    'abc' GLOB 'a*',
                    2 BETWEEN 1 AND 3,
                    2 NOT BETWEEN 1 AND 3,
                    NULL LIKE 'a%',
                    NULL GLOB 'a*',
                    NULL BETWEEN 1 AND 3;",
        )
        .unwrap();
    assert_eq!(
        scalar_predicate_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );

    let null_compare_filter_rows = db
        .query(
            "SELECT name
             FROM users
             WHERE email = NULL
                OR email <> NULL
                OR NULL = NULL
                OR NULL <> NULL
             ORDER BY 1 ASC;",
        )
        .unwrap();
    assert_eq!(null_compare_filter_rows, Vec::<Vec<Value>>::new());

    let distinct_count = db
        .query("SELECT COUNT(DISTINCT active) AS active_values FROM users;")
        .unwrap();
    assert_eq!(distinct_count, vec![vec![Value::Integer(2)]]);
}

#[test]
fn database_supports_single_column_values_in_rhs_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT 2 IN (VALUES (1), (2)),
                    3 NOT IN (VALUES (1), (2)),
                    2 IN (VALUES (1), (NULL)),
                    3 NOT IN (VALUES (1), (NULL));",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Null,
            Value::Null,
        ]]
    );

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, 'carol');",
    )
    .unwrap();

    let filtered = db
        .query(
            "SELECT name
             FROM users
             WHERE id IN (VALUES (1), (3))
             ORDER BY name ASC;",
        )
        .unwrap();

    assert_eq!(
        filtered,
        vec![vec![Value::from("alice")], vec![Value::from("carol")]]
    );
}

#[test]
fn database_supports_row_value_values_in_rhs_like_sqlite() {
    let db = Database::memory();

    let scalar_rows = db
        .query(
            "SELECT (1, 2) IN (VALUES (1, 2), (3, 4)),
                    (1, 3) IN (VALUES (1, 2), (3, 4)),
                    (1, NULL) IN (VALUES (1, NULL)),
                    (NULL, NULL) IN (VALUES (NULL, NULL)),
                    (1, NULL) IN (VALUES (1, 2)),
                    (1, 2) IN (VALUES (1, NULL)),
                    (1, NULL) NOT IN (VALUES (1, NULL));",
        )
        .unwrap();
    assert_eq!(
        scalar_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );

    db.execute(
        "CREATE TABLE pairs (a INTEGER, b INTEGER, label TEXT);
         INSERT INTO pairs VALUES (1, 2, 'one-two');
         INSERT INTO pairs VALUES (1, 3, 'one-three');
         INSERT INTO pairs VALUES (2, 2, 'two-two');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT label
             FROM pairs
             WHERE (a, b) IN (VALUES (1, 2), (2, 2))
             ORDER BY label ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("one-two")], vec![Value::from("two-two")]]
    );
}

#[test]
fn database_supports_row_value_comparisons_like_sqlite() {
    let db = Database::memory();

    let scalar_rows = db
        .query(
            "SELECT (1, 2) = (1, 2),
                    (1, 2) <> (1, 3),
                    (1, 2) < (1, 3),
                    (1, 4) < (1, 3),
                    (1, 2) < (2, NULL),
                    (1, NULL) = (1, 2),
                    (1, NULL, 3) = (1, 2, 4),
                    (1, NULL, 3) < (1, NULL, 4),
                    (1, NULL) < (1, 2),
                    (1, NULL) IS (1, NULL),
                    (1, NULL) IS (1, 2),
                    (1, NULL) IS NOT (1, NULL),
                    (NULL, NULL) IS (NULL, NULL),
                    (1, 2) IS (1, 2),
                    (1, NULL) IS DISTINCT FROM (1, NULL),
                    (1, NULL) IS DISTINCT FROM (1, 2),
                    (1, NULL) IS NOT DISTINCT FROM (1, NULL),
                    (NULL, NULL) IS DISTINCT FROM (NULL, NULL),
                    (1, 2) IS NOT DISTINCT FROM (1, 2);",
        )
        .unwrap();
    assert_eq!(
        scalar_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Null,
            Value::Boolean(false),
            Value::Null,
            Value::Null,
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
        ]]
    );

    db.execute(
        "CREATE TABLE versions (major INTEGER, minor INTEGER, label TEXT);
         INSERT INTO versions VALUES (1, 0, 'v1.0');
         INSERT INTO versions VALUES (1, 2, 'v1.2');
         INSERT INTO versions VALUES (2, 0, 'v2.0');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT label
             FROM versions
             WHERE (major, minor) >= (1, 2)
             ORDER BY major, minor;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("v1.2")], vec![Value::from("v2.0")]]
    );
}

#[test]
fn database_supports_row_value_between_like_sqlite() {
    let db = Database::memory();

    let scalar_rows = db
        .query(
            "SELECT (1, 2) BETWEEN (1, 1) AND (1, 3),
                    (1, 4) BETWEEN (1, 1) AND (1, 3),
                    (1, NULL) BETWEEN (1, 1) AND (1, 3),
                    (1, NULL, 3) BETWEEN (1, 1, 1) AND (1, NULL, 4),
                    (2, NULL) BETWEEN (1, 1) AND (1, 3);",
        )
        .unwrap();
    assert_eq!(
        scalar_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Null,
            Value::Null,
            Value::Boolean(false),
        ]]
    );

    db.execute(
        "CREATE TABLE versions (major INTEGER, minor INTEGER, label TEXT);
         INSERT INTO versions VALUES (1, 0, 'v1.0');
         INSERT INTO versions VALUES (1, 2, 'v1.2');
         INSERT INTO versions VALUES (1, 4, 'v1.4');
         INSERT INTO versions VALUES (2, 0, 'v2.0');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT label
             FROM versions
             WHERE (major, minor) BETWEEN (1, 1) AND (1, 4)
             ORDER BY major, minor;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("v1.2")], vec![Value::from("v1.4")]]
    );
}

#[test]
fn database_supports_row_value_subquery_predicates_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE lhs (a INTEGER, b INTEGER, label TEXT);
         INSERT INTO lhs VALUES (1, 2, 'one-two');
         INSERT INTO lhs VALUES (1, 3, 'one-three');
         INSERT INTO lhs VALUES (2, 2, 'two-two');
         CREATE TABLE rhs (x INTEGER, y INTEGER);
         INSERT INTO rhs VALUES (1, 2);
         INSERT INTO rhs VALUES (2, 2);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT label
             FROM lhs
             WHERE (a, b) IN (SELECT x, y FROM rhs)
             ORDER BY label;",
        )
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::from("one-two")], vec![Value::from("two-two")]]
    );

    let scalar_rows = db
        .query(
            "SELECT (1, 2) IN (SELECT x, y FROM rhs),
                    (1, 3) IN (SELECT x, y FROM rhs),
                    (1, 2) = (SELECT x, y FROM rhs WHERE x = 1),
                    (1, 3) < (SELECT x, y FROM rhs WHERE x = 2);",
        )
        .unwrap();
    assert_eq!(
        scalar_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
        ]]
    );
}

#[test]
fn database_rejects_row_value_misuse_like_sqlite() {
    let db = Database::memory();

    for sql in [
        "SELECT (1, 2) IS NULL;",
        "SELECT (1, 2) IS TRUE;",
        "SELECT (1, 2) LIKE 'x';",
        "SELECT (1, 2) GLOB 'x';",
    ] {
        let error = db.query(sql).unwrap_err();
        assert!(
            error.to_string().contains("row value misused"),
            "unexpected error for {sql}: {error}"
        );
    }
}

#[test]
fn database_supports_expression_lists_in_in_predicates_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, 'carol');
         INSERT INTO users VALUES (4, 'dave');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT name
             FROM users
             WHERE id IN (1 + 1, ABS(-3), NULLIF(4, 4))
                OR id NOT IN (ABS(-1), 1 + 1, NULLIF(5, 5))
             ORDER BY id ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("bob")], vec![Value::from("carol")]]
    );
}

#[test]
fn database_compares_integer_and_real_values_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT 5 = 5.0,
                    5 IN (5.0, 6),
                    5.0 IN (5, 6),
                    5 BETWEEN 4.5 AND 5.5,
                    5.0 BETWEEN 4 AND 6;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
        ]]
    );
}

#[test]
fn database_compares_boolean_and_numeric_values_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT 1 = TRUE,
                    0 = FALSE,
                    TRUE IN (1, 2),
                    FALSE IN (0.0, 2.0),
                    TRUE BETWEEN 1 AND 1,
                    FALSE BETWEEN 0 AND 0,
                    TRUE = 1.0,
                    FALSE = 0.0;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
        ]]
    );
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
fn database_orders_with_collate_nocase_like_sqlite() {
    let db = Database::memory();

    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'b');
         INSERT INTO users VALUES (2, 'A');
         INSERT INTO users VALUES (3, 'a');
         INSERT INTO users VALUES (4, NULL);",
    )
    .unwrap();

    let rows = db
        .query("SELECT COALESCE(name, 'NULL') FROM users ORDER BY name COLLATE NOCASE ASC;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("NULL")],
            vec![Value::from("A")],
            vec![Value::from("a")],
            vec![Value::from("b")],
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
fn database_supports_modulo_expression_projection() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT 7 % 3,
                    -7 % 3,
                    7 % -3,
                    NULL % 3,
                    7 % NULL;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(-1),
            Value::Integer(1),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_mod_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT MOD(10, 3),
                    MOD(10.5, 3),
                    MOD(NULL, 3),
                    TYPEOF(MOD(10, 3)),
                    MOD('12', '5');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Real(1.0),
            Value::Real(1.5),
            Value::Null,
            Value::from("real"),
            Value::Real(2.0),
        ]]
    );
}

#[test]
fn database_supports_sqlite_numeric_coercion_in_arithmetic_expressions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT '5' + 2,
                    '5.5' + 2,
                    'abc' + 2,
                    '5' * 2,
                    '6' / 2,
                    'abc' / 2,
                    '5' % 2,
                    'abc' % 2,
                    -'6',
                    -'abc';",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(7),
            Value::Real(7.5),
            Value::Integer(2),
            Value::Integer(10),
            Value::Integer(3),
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(0),
            Value::Integer(-6),
            Value::Integer(0),
        ]]
    );
}

#[test]
fn database_returns_null_for_division_and_modulo_by_zero_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT 1 / 0,
                    1.0 / 0,
                    1 % 0,
                    '1' / 0,
                    '1' % 0;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_supports_boolean_coercion_in_arithmetic_expressions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT 1 + TRUE,
                    1 + FALSE,
                    TRUE * 2,
                    TRUE / 2,
                    TRUE % 2;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(2),
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(0),
            Value::Integer(1),
        ]]
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
                    TYPEOF(name) AS name_type,
                    CAST(delta AS TEXT) AS delta_text,
                    LOWER(name) AS lower_name,
                    UPPER(nickname) AS upper_nickname,
                    ABS(delta) AS abs_delta,
                    COALESCE(nickname, name, 'anonymous') AS display_name,
                    IFNULL(nickname, name) AS fallback_name,
                    NULLIF(name, nickname) AS nullif_name,
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
                Value::from("text"),
                Value::from("-7"),
                Value::from("alice"),
                Value::Null,
                Value::Integer(7),
                Value::from("Alice"),
                Value::from("Alice"),
                Value::from("Alice"),
                Value::Integer(6),
            ],
            vec![
                Value::Integer(3),
                Value::from("text"),
                Value::from("4"),
                Value::from("bob"),
                Value::from("BOBBY"),
                Value::Integer(4),
                Value::from("bobby"),
                Value::from("bobby"),
                Value::from("Bob"),
                Value::Integer(4),
            ],
        ]
    );

    let ascii_only_case = db
        .query("SELECT LOWER('AÄB'), UPPER('aäb'), LOWER('ÄÉ你好'), UPPER('äé你好');")
        .unwrap();
    assert_eq!(
        ascii_only_case,
        vec![vec![
            Value::from("aÄb"),
            Value::from("AäB"),
            Value::from("ÄÉ你好"),
            Value::from("äé你好"),
        ]]
    );
}

#[test]
fn database_evaluates_like_and_glob_scalar_functions_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT LIKE('a%', 'abc'),
                    LIKE('a!_', 'a_', '!'),
                    GLOB('a*', 'abc'),
                    LIKE(NULL, 'abc'),
                    GLOB('a*', NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_unistr_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT UNISTR('A\\0042'),
                    UNISTR('\\u0041'),
                    UNISTR('\\U00000041'),
                    UNISTR('\\+000041'),
                    UNISTR('\\\\'),
                    UNISTR(NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("AB"),
            Value::from("A"),
            Value::from("A"),
            Value::from("A"),
            Value::from("\\"),
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_unistr_quote_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT UNISTR_QUOTE('abc'),
                    UNISTR_QUOTE('a''b'),
                    UNISTR_QUOTE('a\\b'),
                    UNISTR_QUOTE(CHAR(1)),
                    UNISTR_QUOTE('line' || CHAR(10) || 'break'),
                    UNISTR_QUOTE(NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("'abc'"),
            Value::from("'a''b'"),
            Value::from("'a\\b'"),
            Value::from("unistr('\\u0001')"),
            Value::from("unistr('line\\u000abreak')"),
            Value::from("NULL"),
        ]]
    );
}

#[test]
fn database_evaluates_json_scalar_functions_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_VALID('{\"a\":1}'),
                    JSON_VALID('bad'),
                    JSON_VALID(1),
                    JSON_VALID(NULL),
                    JSON_QUOTE('a''b'),
                    JSON_QUOTE(NULL),
                    JSON_QUOTE(1),
                    JSON_QUOTE(TRUE);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(0),
            Value::Integer(1),
            Value::Null,
            Value::from("\"a'b\""),
            Value::from("null"),
            Value::from("1"),
            Value::from("1"),
        ]]
    );
}

#[test]
fn database_evaluates_json_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON('{ \"a\" : 1, \"b\" : [2,3] }'),
                    JSON('[1,2]'),
                    JSON('1'),
                    JSON(1),
                    JSON(1.5),
                    JSON(TRUE),
                    JSON(NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"a\":1,\"b\":[2,3]}"),
            Value::from("[1,2]"),
            Value::from("1"),
            Value::from("1"),
            Value::from("1.5"),
            Value::from("1"),
            Value::Null,
        ]]
    );
}

#[test]
fn database_rejects_json_scalar_function_malformed_json_like_sqlite() {
    let db = Database::memory();

    let error = db.query("SELECT JSON('bad');").unwrap_err();
    assert!(
        error.to_string().contains("malformed JSON"),
        "unexpected error: {error}"
    );
}

#[test]
fn database_accepts_json5_unquoted_object_keys_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON('{a:1,b:[2,3]}'),
                    JSON_EXTRACT('{a:1,b:[2,3]}', '$.b[1]'),
                    JSON_TYPE('{a:1}', '$.a'),
                    JSON_VALID('{a:1}');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"a\":1,\"b\":[2,3]}"),
            Value::Integer(3),
            Value::from("integer"),
            Value::Integer(0),
        ]]
    );
}

#[test]
fn database_accepts_json5_unicode_unquoted_object_keys_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON('{é:1,ключ:2}'),
                    JSON_EXTRACT('{é:1,ключ:2}', '$.é'),
                    JSON_EXTRACT('{é:1,ключ:2}', '$.ключ'),
                    JSON_VALID('{é:1,ключ:2}', 2),
                    JSON_VALID('{é:1,ключ:2}');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"é\":1,\"ключ\":2}"),
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(1),
            Value::Integer(0),
        ]]
    );
}

#[test]
fn database_evaluates_json_error_position_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_ERROR_POSITION('{\"a\":1}'),
                    JSON_ERROR_POSITION('{a:1}'),
                    JSON_ERROR_POSITION('bad'),
                    JSON_ERROR_POSITION('{\"a\":}'),
                    JSON_ERROR_POSITION(NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(0),
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(6),
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_json_pretty_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_PRETTY('{\"a\":1,\"b\":[2,{\"c\":3}]}'),
                    JSON_PRETTY('{\"a\":1,\"b\":[2]}', '--'),
                    JSON_PRETTY('{a:1,b:[2,]}', '  '),
                    JSON_PRETTY(NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from(
                "{\n    \"a\": 1,\n    \"b\": [\n        2,\n        {\n            \"c\": 3\n        }\n    ]\n}"
            ),
            Value::from("{\n--\"a\": 1,\n--\"b\": [\n----2\n--]\n}"),
            Value::from("{\n  \"a\": 1,\n  \"b\": [\n    2\n  ]\n}"),
            Value::Null,
        ]]
    );

    let malformed = db.query("SELECT JSON_PRETTY('bad');").unwrap_err();
    assert!(
        malformed.to_string().contains("malformed JSON"),
        "unexpected error: {malformed}"
    );
}

#[test]
fn database_accepts_json5_single_quoted_strings_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON('{a:''x'',b:[''y'',2]}'),
                    JSON_EXTRACT('{a:''x''}', '$.a'),
                    JSON_TYPE('{a:''x''}', '$.a'),
                    JSON_VALID('{a:''x''}', 2),
                    JSON_VALID('{a:''x''}');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"a\":\"x\",\"b\":[\"y\",2]}"),
            Value::from("x"),
            Value::from("text"),
            Value::Integer(1),
            Value::Integer(0),
        ]]
    );
}

#[test]
fn database_accepts_json5_trailing_commas_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON('{a:1,}'),
                    JSON('[1,2,]'),
                    JSON_EXTRACT('{a:[1,2,]}', '$.a[1]'),
                    JSON_VALID('[1,2,]', 2),
                    JSON_VALID('[1,2,]');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"a\":1}"),
            Value::from("[1,2]"),
            Value::Integer(2),
            Value::Integer(1),
            Value::Integer(0),
        ]]
    );

    let malformed = db.query("SELECT JSON('{a:1,,}');").unwrap_err();
    assert!(
        malformed.to_string().contains("malformed JSON"),
        "unexpected error: {malformed}"
    );
}

#[test]
fn database_accepts_json5_numeric_literals_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON('{a:0x10,b:+2,c:-3,d:.5,e:5.}'),
                    JSON_EXTRACT('{a:0x10,b:+2,c:-3,d:.5,e:5.}', '$.a'),
                    JSON_EXTRACT('{a:0x10,b:+2,c:-3,d:.5,e:5.}', '$.d'),
                    JSON_VALID('{a:0x10,b:+2,c:-3,d:.5,e:5.}', 2),
                    JSON_VALID('{a:0x10,b:+2,c:-3,d:.5,e:5.}');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"a\":16,\"b\":2,\"c\":-3,\"d\":0.5,\"e\":5.0}"),
            Value::Integer(16),
            Value::Real(0.5),
            Value::Integer(1),
            Value::Integer(0),
        ]]
    );
}

#[test]
fn database_accepts_json5_comments_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON('{a:/*x*/1}'),
                    JSON('{a:1,//line
b:2}'),
                    JSON_EXTRACT('{a:/*x*/[1,2]}', '$.a[1]'),
                    JSON_VALID('{a:/*x*/1}', 2),
                    JSON_VALID('{a:/*x*/1}');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"a\":1}"),
            Value::from("{\"a\":1,\"b\":2}"),
            Value::Integer(2),
            Value::Integer(1),
            Value::Integer(0),
        ]]
    );

    let malformed = db
        .query("SELECT JSON('{a:1 /* unterminated }');")
        .unwrap_err();
    assert!(
        malformed.to_string().contains("malformed JSON"),
        "unexpected error: {malformed}"
    );
}

#[test]
fn database_accepts_json5_nan_values_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON('{a:NaN,b:QNaN,c:SNaN}'),
                    JSON_VALID('{a:NaN,b:QNaN,c:SNaN}', 2),
                    JSON_VALID('{a:NaN,b:QNaN,c:SNaN}');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"a\":null,\"b\":null,\"c\":null}"),
            Value::Integer(1),
            Value::Integer(0),
        ]]
    );
}

#[test]
fn database_evaluates_json_valid_flags_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_VALID('{a:1}'),
                    JSON_VALID('{a:1}', 2),
                    JSON_VALID('{\"a\":1}', 1),
                    JSON_VALID('{\"a\":1}', 2),
                    JSON_VALID('{a:1}', 3),
                    JSON_VALID('bad', 2),
                    JSON_VALID(NULL, 2),
                    JSON_VALID('{a:1}', '2');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(0),
            Value::Null,
            Value::Integer(1),
        ]]
    );

    let bad_flags = db.query("SELECT JSON_VALID('{a:1}', 0);").unwrap_err();
    assert!(
        bad_flags
            .to_string()
            .contains("FLAGS parameter to json_valid() must be between 1 and 15"),
        "unexpected error: {bad_flags}"
    );
}

#[test]
fn database_evaluates_json_extract_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_EXTRACT('{\"a\":1,\"b\":\"x\",\"c\":true,\"d\":null,\"e\":[10,20]}', '$.a'),
                    TYPEOF(JSON_EXTRACT('{\"a\":1}', '$.a')),
                    JSON_EXTRACT('{\"b\":\"x\"}', '$.b'),
                    JSON_EXTRACT('{\"c\":true}', '$.c'),
                    JSON_EXTRACT('{\"d\":null}', '$.d'),
                    JSON_EXTRACT('{\"e\":[10,20]}', '$.e[1]'),
                    JSON_EXTRACT('{\"z\":1}', '$.missing');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::from("integer"),
            Value::from("x"),
            Value::Integer(1),
            Value::Null,
            Value::Integer(20),
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_json_arrow_operators_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT '{\"a\":1,\"b\":\"x\",\"c\":null,\"d\":[10,20]}' -> '$.a',
                    TYPEOF('{\"a\":1}' -> '$.a'),
                    '{\"b\":\"x\"}' -> '$.b',
                    TYPEOF('{\"b\":\"x\"}' -> '$.b'),
                    '{\"a\":1}' ->> '$.a',
                    TYPEOF('{\"a\":1}' ->> '$.a'),
                    '{\"b\":\"x\"}' ->> '$.b',
                    TYPEOF('{\"b\":\"x\"}' ->> '$.b'),
                    '{\"c\":null}' -> '$.c',
                    '{\"c\":null}' ->> '$.c',
                    '{\"d\":[10,20]}' -> '$.d[1]',
                    '{\"a\":1,\"b\":[10,20]}' -> 'a',
                    '{\"a\":1,\"b\":[10,20]}' ->> 'a',
                    '[10,20]' -> 1,
                    '[10,20]' ->> 1,
                    '{\"b\":[10,20]}' -> 'b' -> 1,
                    '{\"b\":[10,20]}' -> 'b' ->> 1,
                    '{\"missing\":1}' -> '$.absent';",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("1"),
            Value::from("text"),
            Value::from("\"x\""),
            Value::from("text"),
            Value::Integer(1),
            Value::from("integer"),
            Value::from("x"),
            Value::from("text"),
            Value::from("null"),
            Value::Null,
            Value::from("20"),
            Value::from("1"),
            Value::Integer(1),
            Value::from("20"),
            Value::Integer(20),
            Value::from("20"),
            Value::Integer(20),
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_json_tail_array_indexes_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_EXTRACT('[10,20,30]', '$[#-1]'),
                    JSON_EXTRACT('[10,20,30]', '$[#-2]'),
                    JSON_EXTRACT('[10,20,30]', '$[#-4]'),
                    JSON_EXTRACT('[10,20,30]', '$[#]'),
                    JSON_TYPE('[10,20,30]', '$[#-1]'),
                    JSON_ARRAY_LENGTH('{\"a\":[1,2,3]}', '$.a[#-1]');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(30),
            Value::Integer(20),
            Value::Null,
            Value::Null,
            Value::from("integer"),
            Value::Integer(0),
        ]]
    );

    let bad_negative = db
        .query("SELECT JSON_EXTRACT('[10,20,30]', '$[-1]');")
        .unwrap_err();
    assert!(
        bad_negative.to_string().contains("bad JSON path")
            || bad_negative
                .to_string()
                .contains("invalid JSON array index"),
        "unexpected error: {bad_negative}"
    );
}

#[test]
fn database_evaluates_json_patch_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_PATCH('{\"a\":1,\"b\":2}', '{\"b\":9,\"c\":3}'),
                    JSON_PATCH('{\"a\":1,\"b\":2}', '{\"b\":null}'),
                    JSON_PATCH('{\"a\":{\"x\":1},\"b\":2}', '{\"a\":{\"y\":2}}'),
                    JSON_PATCH('[1,2]', '{\"a\":1}'),
                    JSON_PATCH('{\"a\":1}', '[2]'),
                    JSON_PATCH(NULL, '{\"a\":1}'),
                    JSON_PATCH('{\"a\":1}', NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"a\":1,\"b\":9,\"c\":3}"),
            Value::from("{\"a\":1}"),
            Value::from("{\"a\":{\"x\":1,\"y\":2},\"b\":2}"),
            Value::from("{\"a\":1}"),
            Value::from("[2]"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_rejects_json_patch_malformed_json_like_sqlite() {
    let db = Database::memory();

    let malformed_left = db
        .query("SELECT JSON_PATCH('bad', '{\"a\":1}');")
        .unwrap_err();
    assert!(
        malformed_left.to_string().contains("malformed JSON"),
        "unexpected error: {malformed_left}"
    );

    let malformed_patch = db
        .query("SELECT JSON_PATCH('{\"a\":1}', 'bad');")
        .unwrap_err();
    assert!(
        malformed_patch.to_string().contains("malformed JSON"),
        "unexpected error: {malformed_patch}"
    );
}

#[test]
fn database_evaluates_json_type_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_TYPE('{\"a\":1}', '$.a'),
                    JSON_TYPE('{\"a\":1.5}', '$.a'),
                    JSON_TYPE('{\"a\":\"x\"}', '$.a'),
                    JSON_TYPE('{\"a\":true}', '$.a'),
                    JSON_TYPE('{\"a\":false}', '$.a'),
                    JSON_TYPE('{\"a\":null}', '$.a'),
                    JSON_TYPE('{\"a\":[1]}', '$.a'),
                    JSON_TYPE('{\"a\":{}}', '$.a'),
                    JSON_TYPE('{\"z\":1}', '$.missing'),
                    JSON_TYPE('[1,2]');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("integer"),
            Value::from("real"),
            Value::from("text"),
            Value::from("true"),
            Value::from("false"),
            Value::from("null"),
            Value::from("array"),
            Value::from("object"),
            Value::Null,
            Value::from("array"),
        ]]
    );
}

#[test]
fn database_evaluates_json_array_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query("SELECT JSON_ARRAY(), JSON_ARRAY(1, 'x', NULL, TRUE, FALSE);")
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("[]"), Value::from("[1,\"x\",null,1,0]")]]
    );
}

#[test]
fn database_evaluates_json_object_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_OBJECT(),
                    JSON_OBJECT('a', 1, 'b', 'x', 'c', NULL, 'd', TRUE);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{}"),
            Value::from("{\"a\":1,\"b\":\"x\",\"c\":null,\"d\":1}"),
        ]]
    );
}

#[test]
fn database_rejects_json_object_invalid_arguments_like_sqlite() {
    let db = Database::memory();

    let odd_args = db.query("SELECT JSON_OBJECT('a');").unwrap_err();
    assert!(
        odd_args
            .to_string()
            .contains("json_object() requires an even number of arguments"),
        "unexpected error: {odd_args}"
    );

    let null_label = db.query("SELECT JSON_OBJECT(NULL, 1);").unwrap_err();
    assert!(
        null_label
            .to_string()
            .contains("json_object() labels must be TEXT"),
        "unexpected error: {null_label}"
    );

    let numeric_label = db.query("SELECT JSON_OBJECT(1, 2);").unwrap_err();
    assert!(
        numeric_label
            .to_string()
            .contains("json_object() labels must be TEXT"),
        "unexpected error: {numeric_label}"
    );
}

#[test]
fn database_evaluates_json_array_length_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_ARRAY_LENGTH('[1,2,3]'),
                    JSON_ARRAY_LENGTH('{\"a\":[1,2],\"b\":5}', '$.a'),
                    JSON_ARRAY_LENGTH('{\"a\":[1,2],\"b\":5}', '$.b'),
                    JSON_ARRAY_LENGTH('{\"a\":[1,2]}', '$.missing'),
                    JSON_ARRAY_LENGTH('1'),
                    JSON_ARRAY_LENGTH(NULL),
                    JSON_ARRAY_LENGTH('[1]', NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(3),
            Value::Integer(2),
            Value::Integer(0),
            Value::Null,
            Value::Integer(0),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_rejects_json_array_length_malformed_json_like_sqlite() {
    let db = Database::memory();

    let error = db.query("SELECT JSON_ARRAY_LENGTH('bad');").unwrap_err();
    assert!(
        error.to_string().contains("malformed JSON"),
        "unexpected error: {error}"
    );
}

#[test]
fn database_evaluates_json_remove_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_REMOVE('{\"a\":1,\"b\":2}', '$.a'),
                    JSON_REMOVE('[10,20,30]', '$[1]'),
                    JSON_REMOVE('{\"a\":1}', '$.missing'),
                    JSON_REMOVE('{\"a\":1}', '$'),
                    JSON_REMOVE('{\"a\":1,\"b\":2,\"c\":3}', '$.a', '$.c'),
                    JSON_REMOVE('[10,20,30,40]', '$[1]', '$[2]'),
                    JSON_REMOVE(NULL, '$.a'),
                    JSON_REMOVE('{\"a\":1}', NULL),
                    JSON_REMOVE('{\"a\":1}');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"b\":2}"),
            Value::from("[10,30]"),
            Value::from("{\"a\":1}"),
            Value::Null,
            Value::from("{\"b\":2}"),
            Value::from("[10,30]"),
            Value::Null,
            Value::Null,
            Value::from("{\"a\":1}"),
        ]]
    );
}

#[test]
fn database_rejects_json_remove_invalid_inputs_like_sqlite() {
    let db = Database::memory();

    let malformed = db.query("SELECT JSON_REMOVE('bad', '$.a');").unwrap_err();
    assert!(
        malformed.to_string().contains("malformed JSON"),
        "unexpected error: {malformed}"
    );

    let bad_path = db
        .query("SELECT JSON_REMOVE('{\"a\":1}', 'bad');")
        .unwrap_err();
    assert!(
        bad_path.to_string().contains("bad JSON path"),
        "unexpected error: {bad_path}"
    );
}

#[test]
fn database_evaluates_json_set_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_SET('{\"a\":1}', '$.a', 2),
                    JSON_SET('{\"a\":1}', '$.b', 'x'),
                    JSON_SET('[10,20]', '$[1]', 99),
                    JSON_SET('[10,20]', '$[2]', 30),
                    JSON_SET('[10,20]', '$[3]', 30),
                    JSON_SET('{\"a\":1}', '$.b.c', 2),
                    JSON_SET('{\"a\":1}', '$', 9),
                    JSON_SET('{\"a\":1}', NULL, 2),
                    JSON_SET('{\"a\":1}', '$.b', NULL),
                    JSON_SET('{\"a\":1}', '$.b', TRUE),
                    JSON_SET('{\"a\":1}', '$.b', 2, '$.c', 3),
                    JSON_SET(NULL, '$.a', 1);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"a\":2}"),
            Value::from("{\"a\":1,\"b\":\"x\"}"),
            Value::from("[10,99]"),
            Value::from("[10,20,30]"),
            Value::from("[10,20]"),
            Value::from("{\"a\":1,\"b\":{\"c\":2}}"),
            Value::from("9"),
            Value::from("{\"a\":1}"),
            Value::from("{\"a\":1,\"b\":null}"),
            Value::from("{\"a\":1,\"b\":1}"),
            Value::from("{\"a\":1,\"b\":2,\"c\":3}"),
            Value::Null,
        ]]
    );
}

#[test]
fn database_rejects_json_set_invalid_inputs_like_sqlite() {
    let db = Database::memory();

    let odd_args = db
        .query("SELECT JSON_SET('{\"a\":1}', '$.b');")
        .unwrap_err();
    assert!(
        odd_args
            .to_string()
            .contains("json_set() needs an odd number of arguments"),
        "unexpected error: {odd_args}"
    );

    let malformed = db.query("SELECT JSON_SET('bad', '$.a', 1);").unwrap_err();
    assert!(
        malformed.to_string().contains("malformed JSON"),
        "unexpected error: {malformed}"
    );

    let bad_path = db
        .query("SELECT JSON_SET('{\"a\":1}', 'bad', 2);")
        .unwrap_err();
    assert!(
        bad_path.to_string().contains("bad JSON path"),
        "unexpected error: {bad_path}"
    );
}

#[test]
fn database_evaluates_json_insert_and_replace_scalar_functions_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JSON_INSERT('{\"a\":1}', '$.a', 2),
                    JSON_INSERT('{\"a\":1}', '$.b', 2),
                    JSON_INSERT('[10,20]', '$[1]', 99),
                    JSON_INSERT('[10,20]', '$[2]', 30),
                    JSON_INSERT('{\"a\":1}', '$', 9),
                    JSON_INSERT('{\"a\":1}', NULL, 2),
                    JSON_REPLACE('{\"a\":1}', '$.a', 2),
                    JSON_REPLACE('{\"a\":1}', '$.b', 2),
                    JSON_REPLACE('[10,20]', '$[1]', 99),
                    JSON_REPLACE('[10,20]', '$[2]', 30),
                    JSON_REPLACE('{\"a\":1}', '$', 9),
                    JSON_REPLACE('{\"a\":1}', NULL, 2);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"a\":1}"),
            Value::from("{\"a\":1,\"b\":2}"),
            Value::from("[10,20]"),
            Value::from("[10,20,30]"),
            Value::from("{\"a\":1}"),
            Value::from("{\"a\":1}"),
            Value::from("{\"a\":2}"),
            Value::from("{\"a\":1}"),
            Value::from("[10,99]"),
            Value::from("[10,20]"),
            Value::from("9"),
            Value::from("{\"a\":1}"),
        ]]
    );
}

#[test]
fn database_rejects_json_insert_and_replace_invalid_inputs_like_sqlite() {
    let db = Database::memory();

    let insert_odd_args = db
        .query("SELECT JSON_INSERT('{\"a\":1}', '$.b');")
        .unwrap_err();
    assert!(
        insert_odd_args
            .to_string()
            .contains("json_insert() needs an odd number of arguments"),
        "unexpected error: {insert_odd_args}"
    );

    let replace_odd_args = db
        .query("SELECT JSON_REPLACE('{\"a\":1}', '$.b');")
        .unwrap_err();
    assert!(
        replace_odd_args
            .to_string()
            .contains("json_replace() needs an odd number of arguments"),
        "unexpected error: {replace_odd_args}"
    );

    let malformed = db
        .query("SELECT JSON_INSERT('bad', '$.a', 1);")
        .unwrap_err();
    assert!(
        malformed.to_string().contains("malformed JSON"),
        "unexpected error: {malformed}"
    );

    let bad_path = db
        .query("SELECT JSON_REPLACE('{\"a\":1}', 'bad', 2);")
        .unwrap_err();
    assert!(
        bad_path.to_string().contains("bad JSON path"),
        "unexpected error: {bad_path}"
    );
}

#[test]
fn database_supports_round_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query("SELECT ROUND(1.6), ROUND(-1.6), ROUND(NULL), ROUND(1.234, 2);")
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Real(2.0),
            Value::Real(-2.0),
            Value::Null,
            Value::Real(1.23),
        ]]
    );
}

#[test]
fn database_supports_abs_and_round_text_numeric_coercion() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT ABS('12'),
                    ABS('abc'),
                    ROUND('1.23', 1),
                    ROUND('abc', 1);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Real(12.0),
            Value::Real(0.0),
            Value::Real(1.2),
            Value::Real(0.0),
        ]]
    );
}

#[test]
fn database_supports_text_coercion_for_common_string_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT LOWER(123),
                    UPPER(123),
                    TRIM(123),
                    SUBSTR(12345, 2, 2),
                    LENGTH(123);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("123"),
            Value::from("123"),
            Value::from("123"),
            Value::from("23"),
            Value::Integer(3),
        ]]
    );
}

#[test]
fn database_supports_text_coercion_for_instr_replace_and_unicode() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT INSTR(12345, 23),
                    REPLACE(12345, 23, 'x'),
                    UNICODE(123),
                    INSTR(X'3132333435', X'3233'),
                    REPLACE(X'3132333435', X'3233', 'x'),
                    UNICODE(X'313233');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(2),
            Value::from("1x45"),
            Value::Integer(49),
            Value::Integer(2),
            Value::from("1x45"),
            Value::Integer(49),
        ]]
    );
}

#[test]
fn database_supports_numeric_text_coercion_for_char_and_blob_builders() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT CHAR('65'),
                    LENGTH(ZEROBLOB('3')),
                    TYPEOF(ZEROBLOB('3')),
                    LENGTH(RANDOMBLOB('3')),
                    TYPEOF(RANDOMBLOB('3'));",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("A"),
            Value::Integer(3),
            Value::from("blob"),
            Value::Integer(3),
            Value::from("blob"),
        ]]
    );
}

#[test]
fn database_evaluates_cast_scalar_expressions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT CAST(123 AS TEXT),
                    TYPEOF(CAST(123 AS TEXT)),
                    CAST('00123' AS INTEGER),
                    TYPEOF(CAST('00123' AS INTEGER)),
                    CAST('123abc' AS INTEGER),
                    CAST('  -12xyz' AS INTEGER),
                    CAST('3.9abc' AS INTEGER),
                    CAST('3.5xyz' AS REAL),
                    CAST('abc' AS INTEGER),
                    CAST(X'31' AS INTEGER),
                    CAST('AB' AS BLOB),
                    TYPEOF(CAST('AB' AS BLOB)),
                    CAST(X'4142' AS TEXT),
                    TYPEOF(CAST(X'4142' AS TEXT)),
                    CAST(3.0 AS TEXT),
                    HEX(CAST(3.0 AS BLOB)),
                    3.0 || 'x',
                    CAST(NULL AS TEXT),
                    TYPEOF(CAST(NULL AS TEXT)),
                    CAST('123' AS NUMERIC),
                    TYPEOF(CAST('123' AS NUMERIC)),
                    CAST('123.5' AS NUMERIC),
                    TYPEOF(CAST('123.5' AS NUMERIC)),
                    CAST('123abc' AS NUMERIC),
                    TYPEOF(CAST('123abc' AS NUMERIC)),
                    CAST('abc' AS NUMERIC),
                    TYPEOF(CAST('abc' AS NUMERIC)),
                    CAST('123.0abc' AS NUMERIC),
                    TYPEOF(CAST('123.0abc' AS NUMERIC)),
                    CAST('123.1abc' AS NUMERIC),
                    TYPEOF(CAST('123.1abc' AS NUMERIC)),
                    CAST('1e2abc' AS NUMERIC),
                    TYPEOF(CAST('1e2abc' AS NUMERIC)),
                    TYPEOF(CAST('9223372036854775808' AS NUMERIC));",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("123"),
            Value::from("text"),
            Value::Integer(123),
            Value::from("integer"),
            Value::Integer(123),
            Value::Integer(-12),
            Value::Integer(3),
            Value::Real(3.5),
            Value::Integer(0),
            Value::Integer(1),
            Value::Blob(vec![0x41, 0x42]),
            Value::from("blob"),
            Value::from("AB"),
            Value::from("text"),
            Value::from("3.0"),
            Value::from("332E30"),
            Value::from("3.0x"),
            Value::Null,
            Value::from("null"),
            Value::Integer(123),
            Value::from("integer"),
            Value::Real(123.5),
            Value::from("real"),
            Value::Integer(123),
            Value::from("integer"),
            Value::Integer(0),
            Value::from("integer"),
            Value::Integer(123),
            Value::from("integer"),
            Value::Real(123.1),
            Value::from("real"),
            Value::Integer(100),
            Value::from("integer"),
            Value::from("real"),
        ]]
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

    let nullif_wrong_arity = db.query("SELECT NULLIF(name) FROM users;").unwrap_err();
    assert!(
        nullif_wrong_arity
            .to_string()
            .contains("NULLIF expects 2 arguments")
    );

    let coalesce_wrong_arity = db.query("SELECT COALESCE(name) FROM users;").unwrap_err();
    assert!(
        coalesce_wrong_arity
            .to_string()
            .contains("COALESCE expects at least 2 arguments")
    );

    let trim_wrong_arity = db
        .query("SELECT TRIM(name, 'x', 'y') FROM users;")
        .unwrap_err();
    assert!(
        trim_wrong_arity
            .to_string()
            .contains("TRIM expects 1 or 2 arguments")
    );

    let substr_wrong_arity = db.query("SELECT SUBSTR(name) FROM users;").unwrap_err();
    assert!(
        substr_wrong_arity
            .to_string()
            .contains("SUBSTR expects 2 or 3 arguments")
    );

    let instr_wrong_arity = db.query("SELECT INSTR(name) FROM users;").unwrap_err();
    assert!(
        instr_wrong_arity
            .to_string()
            .contains("INSTR expects 2 arguments")
    );

    let replace_wrong_arity = db
        .query("SELECT REPLACE(name, 'a') FROM users;")
        .unwrap_err();
    assert!(
        replace_wrong_arity
            .to_string()
            .contains("REPLACE expects 3 arguments")
    );

    let quote_wrong_arity = db.query("SELECT QUOTE(name, 'x') FROM users;").unwrap_err();
    assert!(
        quote_wrong_arity
            .to_string()
            .contains("QUOTE expects 1 arguments")
    );

    let unicode_wrong_arity = db
        .query("SELECT UNICODE(name, 'x') FROM users;")
        .unwrap_err();
    assert!(
        unicode_wrong_arity
            .to_string()
            .contains("UNICODE expects 1 arguments")
    );

    let round_wrong_arity = db.query("SELECT ROUND() FROM users;").unwrap_err();
    assert!(
        round_wrong_arity
            .to_string()
            .contains("ROUND expects 1 or 2 arguments")
    );

    let round_text_precision = db.query("SELECT ROUND(age, name) FROM users;").unwrap();
    assert_eq!(round_text_precision, vec![vec![Value::Real(30.0)]]);

    let char_text_argument = db.query("SELECT HEX(CHAR(name)) FROM users;").unwrap();
    assert_eq!(char_text_argument, vec![vec![Value::from("00")]]);

    let zeroblob_wrong_arity = db.query("SELECT ZEROBLOB() FROM users;").unwrap_err();
    assert!(
        zeroblob_wrong_arity
            .to_string()
            .contains("ZEROBLOB expects 1 arguments")
    );

    let zeroblob_negative_length = db.query("SELECT ZEROBLOB(-1) FROM users;").unwrap_err();
    assert!(
        zeroblob_negative_length
            .to_string()
            .contains("ZEROBLOB length must be non-negative")
    );

    let sign_wrong_arity = db.query("SELECT SIGN() FROM users;").unwrap_err();
    assert!(
        sign_wrong_arity
            .to_string()
            .contains("SIGN expects 1 arguments")
    );

    let likelihood_wrong_arity = db.query("SELECT LIKELIHOOD(name) FROM users;").unwrap_err();
    assert!(
        likelihood_wrong_arity
            .to_string()
            .contains("LIKELIHOOD expects 2 arguments")
    );

    let ceil_wrong_arity = db.query("SELECT CEIL() FROM users;").unwrap_err();
    assert!(
        ceil_wrong_arity
            .to_string()
            .contains("CEIL expects 1 arguments")
    );

    let pi_wrong_arity = db.query("SELECT PI(name) FROM users;").unwrap_err();
    assert!(
        pi_wrong_arity
            .to_string()
            .contains("PI expects 0 arguments")
    );

    let sqrt_wrong_arity = db.query("SELECT SQRT() FROM users;").unwrap_err();
    assert!(
        sqrt_wrong_arity
            .to_string()
            .contains("SQRT expects 1 arguments")
    );

    let power_wrong_arity = db.query("SELECT POWER(age) FROM users;").unwrap_err();
    assert!(
        power_wrong_arity
            .to_string()
            .contains("POWER expects 2 arguments")
    );

    let log_wrong_arity = db.query("SELECT LOG() FROM users;").unwrap_err();
    assert!(
        log_wrong_arity
            .to_string()
            .contains("LOG expects 1 or 2 arguments")
    );

    let lower_integer = db.query("SELECT LOWER(age) FROM users;").unwrap();
    assert_eq!(lower_integer, vec![vec![Value::from("30")]]);
}

#[test]
fn database_supports_format_scalar_function_aliasing_printf() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (7, 'alice');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT FORMAT('member-%03d-%s', id, name),
                    FORMAT('%04d-', id)
             FROM users;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("member-007-alice"), Value::from("0007-"),]]
    );
}

#[test]
fn database_evaluates_likelihood_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT LIKELIHOOD(1, 0.25),
                    LIKELIHOOD(0, 0.25),
                    TYPEOF(LIKELIHOOD(1, 0.25)),
                    LIKELIHOOD(NULL, 0.5);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(0),
            Value::from("integer"),
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_rounding_and_pi_scalar_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT CEIL(1.2),
                    CEILING(-1.2),
                    FLOOR(1.8),
                    TRUNC(-1.8),
                    PI(),
                    TYPEOF(PI()),
                    CEIL(NULL),
                    FLOOR('abc'),
                    CEIL('1.2'),
                    FLOOR('1.8'),
                    TRUNC('1.8');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Real(2.0),
            Value::Real(-1.0),
            Value::Real(1.0),
            Value::Real(-1.0),
            Value::Real(3.141592653589793),
            Value::from("real"),
            Value::Null,
            Value::Null,
            Value::Real(2.0),
            Value::Real(1.0),
            Value::Real(1.0),
        ]]
    );
}

#[test]
fn database_evaluates_math_scalar_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT SQRT(9),
                    TYPEOF(SQRT(9)),
                    SQRT(-1),
                    POWER(2, 3),
                    POW(2, 3),
                    EXP(1),
                    LN(EXP(1)),
                    LOG10(1000),
                    LOG2(8),
                    LOG(100),
                    LOG(2, 8),
                    SIN(0),
                    COS(0),
                    TAN(0),
                    SINH(0),
                    COSH(0),
                    TANH(0),
                    ACOS(1),
                    ASIN(0),
                    ATAN(1),
                    ATAN2(1, 1),
                    ACOSH(1),
                    ASINH(0),
                    ATANH(0),
                    ACOS(2),
                    ASIN(2),
                    ACOSH(0),
                    ATANH(2),
                    DEGREES(PI()),
                    RADIANS(180),
                    SQRT('9'),
                    SQRT('abc'),
                    SIN(NULL),
                    SIN('bad'),
                    SINH(NULL),
                    SINH('bad'),
                    ASINH(NULL),
                    ASINH('bad'),
                    ATAN(NULL),
                    ATAN('bad'),
                    POWER(NULL, 2),
                    DEGREES(NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Real(3.0),
            Value::from("real"),
            Value::Null,
            Value::Real(8.0),
            Value::Real(8.0),
            Value::Real(2.718281828459045),
            Value::Real(1.0),
            Value::Real(3.0),
            Value::Real(3.0),
            Value::Real(2.0),
            Value::Real(3.0),
            Value::Real(0.0),
            Value::Real(1.0),
            Value::Real(0.0),
            Value::Real(0.0),
            Value::Real(1.0),
            Value::Real(0.0),
            Value::Real(0.0),
            Value::Real(0.0),
            Value::Real(std::f64::consts::FRAC_PI_4),
            Value::Real(std::f64::consts::FRAC_PI_4),
            Value::Real(0.0),
            Value::Real(0.0),
            Value::Real(0.0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Real(180.0),
            Value::Real(std::f64::consts::PI),
            Value::Real(3.0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_typeof_scalar_function() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE valueset (id INTEGER PRIMARY KEY, name TEXT, active BOOLEAN, payload BLOB);
        INSERT INTO valueset VALUES (1, 'alice', true, X'ABCD');
         INSERT INTO valueset VALUES (2, NULL, false, NULL);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT TYPEOF(name), TYPEOF(active), TYPEOF(payload), TYPEOF(NULL)
             FROM valueset
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![
                Value::from("text"),
                Value::from("integer"),
                Value::from("blob"),
                Value::from("null"),
            ],
            vec![
                Value::from("null"),
                Value::from("integer"),
                Value::from("null"),
                Value::from("null"),
            ],
        ]
    );
}

#[test]
fn database_evaluates_subtype_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT SUBTYPE(1),
                    SUBTYPE('x'),
                    SUBTYPE(NULL),
                    SUBTYPE(JSON('{\"a\":1}')),
                    SUBTYPE(JSON_ARRAY(1, 2));",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(0),
            Value::Integer(0),
            Value::Integer(0),
            Value::Integer(0),
            Value::Integer(0),
        ]]
    );
}

#[test]
fn database_evaluates_blob_scalar_functions() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE files (id INTEGER PRIMARY KEY, payload BLOB);
         INSERT INTO files VALUES (1, X'0001FEFF');
         INSERT INTO files VALUES (2, NULL);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT LENGTH(payload) AS payload_len,
                    HEX(payload) AS payload_hex
             FROM files
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(4), Value::from("0001FEFF")],
            vec![Value::Null, Value::Null],
        ]
    );
}

#[test]
fn database_evaluates_hex_scalar_function_for_text_and_integer_inputs() {
    let db = Database::memory();

    let rows = db
        .query("SELECT HEX('A'), HEX('你好'), HEX(123), HEX(NULL);")
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("41"),
            Value::from("E4BDA0E5A5BD"),
            Value::from("313233"),
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_octet_length_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query("SELECT OCTET_LENGTH('A'), OCTET_LENGTH('你好'), OCTET_LENGTH(X'0001'), OCTET_LENGTH(123), OCTET_LENGTH(3.0), OCTET_LENGTH(NULL);")
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(6),
            Value::Integer(2),
            Value::Integer(3),
            Value::Integer(3),
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_min_and_max_scalar_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT MAX(1, 2, 3),
                    MIN(1, 2, 3),
                    MAX(NULL, 2, 3),
                    MIN(NULL, 2, 3),
                    MAX('2', '10'),
                    MIN('2', '10');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(3),
            Value::Integer(1),
            Value::Null,
            Value::Null,
            Value::from("2"),
            Value::from("10"),
        ]]
    );
}

#[test]
fn database_evaluates_min_and_max_with_mixed_numeric_types_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT MIN(5, 5.0),
                    MAX(5, 5.0),
                    MIN(TRUE, 1),
                    MAX(FALSE, 0),
                    MIN(5, 4.5),
                    MAX(5, 4.5);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Real(5.0),
            Value::Integer(5),
            Value::Integer(1),
            Value::Integer(0),
            Value::Real(4.5),
            Value::Integer(5),
        ]]
    );
}

#[test]
fn database_evaluates_date_time_and_datetime_scalar_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-01-02 03:04:05'),
                    TIME('2024-01-02'),
                    DATETIME('2024-01-02'),
                    TIME('2024-01-02 03:04:05.678', 'subsec'),
                    DATETIME('2024-01-02 03:04:05.678', 'subsec'),
                    DATETIME('2024-01-02T03:04:05Z'),
                    DATETIME('2024-01-02 03:04:05+02:00'),
                    DATETIME('2024-01-02T03:04:05-02:30'),
                    DATETIME('2024-01-02    03:04:05'),
                    DATETIME('2024-01-02TT03:04:05'),
                    DATETIME('2024-01-02 03:04:05 +02:00'),
                    TIME('24:59:59'),
                    DATETIME('2024-01-01 24:59:59'),
                    DATE('bad'),
                    TIME(NULL),
                    DATETIME('03:04:05');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-02"),
            Value::from("00:00:00"),
            Value::from("2024-01-02 00:00:00"),
            Value::from("03:04:05.678"),
            Value::from("2024-01-02 03:04:05.678"),
            Value::from("2024-01-02 03:04:05"),
            Value::from("2024-01-02 01:04:05"),
            Value::from("2024-01-02 05:34:05"),
            Value::from("2024-01-02 03:04:05"),
            Value::from("2024-01-02 03:04:05"),
            Value::from("2024-01-02 01:04:05"),
            Value::from("24:59:59"),
            Value::from("2024-01-01 24:59:59"),
            Value::Null,
            Value::Null,
            Value::from("2000-01-01 03:04:05"),
        ]]
    );
}

#[test]
fn database_evaluates_timediff_scalar_function_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT TIMEDIFF('2024-01-02 03:04:05', '2024-01-01 01:02:03'),
                    TIMEDIFF('2024-01-01', '2024-01-02'),
                    TIMEDIFF('2024-01-01', '2024-01-01'),
                    TIMEDIFF('2024-01-01 00:00:00.500', '2024-01-01 00:00:00.250'),
                    TIMEDIFF('2024-02-29', '2024-01-31'),
                    TIMEDIFF('2024-01-31', '2024-02-29'),
                    TIMEDIFF('2025-03-01', '2024-02-29'),
                    TIMEDIFF('2024-03-31', '2024-01-30'),
                    TIMEDIFF('2024-01-30', '2024-03-31'),
                    TIMEDIFF(NULL, '2024-01-01'),
                    TIMEDIFF('bad', '2024-01-01');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("+0000-00-01 02:02:02.000"),
            Value::from("-0000-00-01 00:00:00.000"),
            Value::from("+0000-00-00 00:00:00.000"),
            Value::from("+0000-00-00 00:00:00.250"),
            Value::from("+0000-00-29 00:00:00.000"),
            Value::from("-0000-00-29 00:00:00.000"),
            Value::from("+0001-00-00 00:00:00.000"),
            Value::from("+0000-02-01 00:00:00.000"),
            Value::from("-0000-02-01 00:00:00.000"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_current_date_time_and_timestamp_special_literals() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT TYPEOF(CURRENT_DATE),
                    LENGTH(CURRENT_DATE),
                    CURRENT_DATE GLOB '????-??-??',
                    TYPEOF(CURRENT_TIME),
                    LENGTH(CURRENT_TIME),
                    CURRENT_TIME GLOB '??:??:??',
                    TYPEOF(CURRENT_TIMESTAMP),
                    LENGTH(CURRENT_TIMESTAMP),
                    CURRENT_TIMESTAMP GLOB '????-??-?? ??:??:??',
                    DATE('now') GLOB '????-??-??',
                    DATE() GLOB '????-??-??',
                    TIME() GLOB '??:??:??',
                    DATETIME() GLOB '????-??-?? ??:??:??',
                    TYPEOF(JULIANDAY()),
                    TYPEOF(UNIXEPOCH()),
                    UNIXEPOCH() > 0;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("text"),
            Value::Integer(10),
            Value::Boolean(true),
            Value::from("text"),
            Value::Integer(8),
            Value::Boolean(true),
            Value::from("text"),
            Value::Integer(19),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::from("real"),
            Value::from("integer"),
            Value::Boolean(true),
        ]]
    );
}

#[test]
fn database_evaluates_strftime_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT STRFTIME('%Y-%m', '2024-01-02'),
                    STRFTIME('%H:%M:%S', '2024-01-02'),
                    STRFTIME('%F %T', '03:04:05'),
                    STRFTIME('%J', '2024-01-02'),
                    STRFTIME('%s', '1970-01-02'),
                    STRFTIME('%s', '2024-01-02 03:04:05.678', 'subsec'),
                    STRFTIME('%j', '2024-01-02'),
                    STRFTIME('%e', '2024-01-02'),
                    STRFTIME('%w', '2024-01-07'),
                    STRFTIME('%u', '2024-01-08'),
                    STRFTIME('%U', '2024-01-07'),
                    STRFTIME('%W', '2024-01-08'),
                    STRFTIME('%V', '2021-01-01'),
                    STRFTIME('%G', '2021-01-01'),
                    STRFTIME('%g', '2021-01-01'),
                    STRFTIME('%R', '2024-01-02 03:04:05'),
                    STRFTIME('%f', '2024-01-02 03:04:05'),
                    STRFTIME('%f', '2024-01-02 03:04:05.678'),
                    STRFTIME('%f', '2024-01-02T03:04:05.678Z'),
                    STRFTIME('%f', '2024-01-02   03:04:05.678   Z'),
                    STRFTIME('%F %T', '2024-01-02 00:00:00+02:30'),
                    STRFTIME('%I', '2024-01-02 15:04:05'),
                    STRFTIME('%p', '2024-01-02 15:04:05'),
                    STRFTIME('%P', '2024-01-02 03:04:05'),
                    STRFTIME('%k', '2024-01-02 03:04:05'),
                    STRFTIME('%l', '2024-01-02 15:04:05'),
                    STRFTIME('%Y', 'bad'),
                    STRFTIME('%m', NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01"),
            Value::from("00:00:00"),
            Value::from("2000-01-01 03:04:05"),
            Value::from("2460311.5"),
            Value::from("86400"),
            Value::from("1704164645.678"),
            Value::from("002"),
            Value::from(" 2"),
            Value::from("0"),
            Value::from("1"),
            Value::from("01"),
            Value::from("02"),
            Value::from("53"),
            Value::from("2020"),
            Value::from("20"),
            Value::from("03:04"),
            Value::from("05.000"),
            Value::from("05.678"),
            Value::from("05.678"),
            Value::from("05.678"),
            Value::from("2024-01-01 21:30:00"),
            Value::from("03"),
            Value::from("PM"),
            Value::from("am"),
            Value::from(" 3"),
            Value::from(" 3"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_julianday_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT JULIANDAY('2024-01-02'),
                    TYPEOF(JULIANDAY('2024-01-02')),
                    JULIANDAY('03:04:05'),
                    JULIANDAY('2024-01-01 24:00:00'),
                    JULIANDAY('bad'),
                    JULIANDAY(NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Real(2460311.5),
            Value::from("real"),
            Value::Real(2451544.627835648),
            Value::Real(2460311.5),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_unixepoch_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT UNIXEPOCH('2024-01-02'),
                    TYPEOF(UNIXEPOCH('2024-01-02')),
                    UNIXEPOCH('2024-01-02 03:04:05.678', 'subsec'),
                    UNIXEPOCH('03:04:05'),
                    UNIXEPOCH('bad'),
                    UNIXEPOCH(NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1704153600),
            Value::from("integer"),
            Value::Real(1704164645.678),
            Value::Integer(946695845),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_day_modifiers_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-01-02', '+1 day'),
                    DATE('2024-01-02', '-2 day'),
                    DATETIME('2024-01-02 03:04:05', '+1 day'),
                    STRFTIME('%F', '2024-01-02 03:04:05', '+1 day'),
                    UNIXEPOCH('2024-01-02', '+1 day'),
                    JULIANDAY('2024-01-02', '+1 day'),
                    DATE('bad', '+1 day'),
                    DATE(NULL, '+1 day');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-03"),
            Value::from("2023-12-31"),
            Value::from("2024-01-03 03:04:05"),
            Value::from("2024-01-03"),
            Value::Integer(1704240000),
            Value::Real(2460312.5),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_start_of_day_modifier_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-01-02 03:04:05', 'start of day'),
                    TIME('2024-01-02 03:04:05', 'start of day'),
                    DATETIME('2024-01-02 03:04:05', 'start of day'),
                    STRFTIME('%F %T', '2024-01-02 03:04:05', 'start of day'),
                    UNIXEPOCH('2024-01-02 03:04:05', 'start of day'),
                    JULIANDAY('2024-01-02 03:04:05', 'start of day'),
                    DATE('bad', 'start of day'),
                    TIME(NULL, 'start of day');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-02"),
            Value::from("00:00:00"),
            Value::from("2024-01-02 00:00:00"),
            Value::from("2024-01-02 00:00:00"),
            Value::Integer(1704153600),
            Value::Real(2460311.5),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_hour_modifiers_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-01-02', '+1 hour'),
                    TIME('2024-01-02 03:04:05', '+2 hour'),
                    DATETIME('2024-01-02 23:04:05', '+2 hour'),
                    DATETIME('2024-01-02 01:04:05', '-2 hour'),
                    STRFTIME('%F %T', '2024-01-02 23:04:05', '+2 hour'),
                    UNIXEPOCH('2024-01-02 23:04:05', '+2 hour'),
                    JULIANDAY('2024-01-02 23:04:05', '+2 hour'),
                    TIME('03:04:05', '+2 hour'),
                    DATE('bad', '+1 hour'),
                    TIME(NULL, '+1 hour');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-02"),
            Value::from("05:04:05"),
            Value::from("2024-01-03 01:04:05"),
            Value::from("2024-01-01 23:04:05"),
            Value::from("2024-01-03 01:04:05"),
            Value::Integer(1704243845),
            Value::Real(2460312.5445023146),
            Value::from("05:04:05"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_minute_modifiers_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-01-02', '+1 minute'),
                    TIME('2024-01-02 03:04:05', '+2 minute'),
                    DATETIME('2024-01-02 23:59:05', '+2 minute'),
                    DATETIME('2024-01-02 00:01:05', '-2 minute'),
                    STRFTIME('%F %T', '2024-01-02 23:59:05', '+2 minute'),
                    UNIXEPOCH('2024-01-02 23:59:05', '+2 minute'),
                    JULIANDAY('2024-01-02 23:59:05', '+2 minute'),
                    TIME('03:04:05', '+2 minute'),
                    DATE('bad', '+1 minute'),
                    TIME(NULL, '+1 minute');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-02"),
            Value::from("03:06:05"),
            Value::from("2024-01-03 00:01:05"),
            Value::from("2024-01-01 23:59:05"),
            Value::from("2024-01-03 00:01:05"),
            Value::Integer(1704240065),
            Value::Real(2460312.500752315),
            Value::from("03:06:05"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_second_modifiers_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-01-02', '+1 second'),
                    TIME('2024-01-02 03:04:05', '+2 second'),
                    DATETIME('2024-01-02 23:59:59', '+2 second'),
                    DATETIME('2024-01-02 00:00:01', '-2 second'),
                    STRFTIME('%F %T', '2024-01-02 23:59:59', '+2 second'),
                    UNIXEPOCH('2024-01-02 23:59:59', '+2 second'),
                    JULIANDAY('2024-01-02 23:59:59', '+2 second'),
                    STRFTIME('%f', '2024-01-01 00:00:00.250', '+0.5 second'),
                    DATETIME('2024-01-01 23:59:59.750', '+0.5 second', 'subsec'),
                    TIME('03:04:05', '+2 second'),
                    DATE('bad', '+1 second'),
                    TIME(NULL, '+1 second');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-02"),
            Value::from("03:04:07"),
            Value::from("2024-01-03 00:00:01"),
            Value::from("2024-01-01 23:59:59"),
            Value::from("2024-01-03 00:00:01"),
            Value::Integer(1704240001),
            Value::Real(2460312.500011574),
            Value::from("00.750"),
            Value::from("2024-01-02 00:00:00.250"),
            Value::from("03:04:07"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_start_of_month_modifier_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-01-15 03:04:05', 'start of month'),
                    TIME('2024-01-15 03:04:05', 'start of month'),
                    DATETIME('2024-01-15 03:04:05', 'start of month'),
                    STRFTIME('%F %T', '2024-01-15 03:04:05', 'start of month'),
                    UNIXEPOCH('2024-01-15 03:04:05', 'start of month'),
                    JULIANDAY('2024-01-15 03:04:05', 'start of month'),
                    DATE('bad', 'start of month'),
                    TIME(NULL, 'start of month');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-01"),
            Value::from("00:00:00"),
            Value::from("2024-01-01 00:00:00"),
            Value::from("2024-01-01 00:00:00"),
            Value::Integer(1704067200),
            Value::Real(2460310.5),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_start_of_year_modifier_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-07-15 03:04:05', 'start of year'),
                    TIME('2024-07-15 03:04:05', 'start of year'),
                    DATETIME('2024-07-15 03:04:05', 'start of year'),
                    STRFTIME('%F %T', '2024-07-15 03:04:05', 'start of year'),
                    UNIXEPOCH('2024-07-15 03:04:05', 'start of year'),
                    JULIANDAY('2024-07-15 03:04:05', 'start of year'),
                    DATE('bad', 'start of year'),
                    TIME(NULL, 'start of year');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-01"),
            Value::from("00:00:00"),
            Value::from("2024-01-01 00:00:00"),
            Value::from("2024-01-01 00:00:00"),
            Value::Integer(1704067200),
            Value::Real(2460310.5),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_month_modifiers_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-01-15', '+1 month'),
                    DATE('2024-01-31', '+1 month'),
                    DATE('2024-01-31', '+1 month', 'floor'),
                    DATE('2024-01-31', '+1 month', 'ceiling'),
                    DATE('2024-03-31', '-1 month'),
                    DATETIME('2024-01-31 23:04:05', '+1 month'),
                    STRFTIME('%F %T', '2024-01-31 23:04:05', '+1 month'),
                    UNIXEPOCH('2024-01-31 23:04:05', '+1 month'),
                    JULIANDAY('2024-01-31 23:04:05', '+1 month'),
                    TIME('03:04:05', '+1 month'),
                    DATETIME('03:04:05', '+1 month'),
                    DATE('bad', '+1 month'),
                    TIME(NULL, '+1 month');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-02-15"),
            Value::from("2024-03-02"),
            Value::from("2024-02-29"),
            Value::from("2024-03-02"),
            Value::from("2024-03-02"),
            Value::from("2024-03-02 23:04:05"),
            Value::from("2024-03-02 23:04:05"),
            Value::Integer(1709420645),
            Value::Real(2460372.4611689816),
            Value::from("03:04:05"),
            Value::from("2000-02-01 03:04:05"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_year_modifiers_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-02-29', '+1 year'),
                    DATE('2024-02-29', '+1 year', 'floor'),
                    DATE('2024-02-29', '+1 year', 'ceiling'),
                    DATE('2024-02-29', '-1 year'),
                    DATE('2023-02-28', '+1 year'),
                    DATETIME('2024-02-29 23:04:05', '+1 year'),
                    STRFTIME('%F %T', '2024-02-29 23:04:05', '+1 year'),
                    UNIXEPOCH('2024-02-29 23:04:05', '+1 year'),
                    JULIANDAY('2024-02-29 23:04:05', '+1 year'),
                    TIME('03:04:05', '+1 year'),
                    DATETIME('03:04:05', '+1 year'),
                    DATE('bad', '+1 year'),
                    TIME(NULL, '+1 year');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2025-03-01"),
            Value::from("2025-02-28"),
            Value::from("2025-03-01"),
            Value::from("2023-03-01"),
            Value::from("2024-02-28"),
            Value::from("2025-03-01 23:04:05"),
            Value::from("2025-03-01 23:04:05"),
            Value::Integer(1740870245),
            Value::Real(2460736.4611689816),
            Value::from("03:04:05"),
            Value::from("2001-01-01 03:04:05"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_weekday_modifier_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-07-05', 'weekday 0'),
                    DATE('2024-07-05', 'weekday 1'),
                    DATE('2024-07-07', 'weekday 0'),
                    DATETIME('2024-07-05 03:04:05', 'weekday 1'),
                    STRFTIME('%F %T', '2024-07-05 03:04:05', 'weekday 1'),
                    UNIXEPOCH('2024-07-05 03:04:05', 'weekday 1'),
                    JULIANDAY('2024-07-05 03:04:05', 'weekday 1'),
                    TIME('03:04:05', 'weekday 1'),
                    DATETIME('03:04:05', 'weekday 1'),
                    DATE('2024-07-05', 'weekday 7'),
                    DATE('bad', 'weekday 1'),
                    TIME(NULL, 'weekday 1');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-07-07"),
            Value::from("2024-07-08"),
            Value::from("2024-07-07"),
            Value::from("2024-07-08 03:04:05"),
            Value::from("2024-07-08 03:04:05"),
            Value::Integer(1720407845),
            Value::Real(2460499.627835648),
            Value::from("03:04:05"),
            Value::from("2000-01-03 03:04:05"),
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_multiple_modifiers_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE('2024-01-31', 'start of month', '+1 month', '-1 day'),
                    DATETIME('2024-07-05 03:04:05', 'weekday 1', '+1 day'),
                    STRFTIME('%F %T', '2024-07-05 03:04:05', 'start of month', '+1 month', '-1 second'),
                    UNIXEPOCH('2024-01-31 23:04:05', 'start of month', '+1 month', '-1 day'),
                    JULIANDAY('2024-01-31 23:04:05', 'start of month', '+1 month', '-1 day'),
                    DATE('2024-01-31', '+1 month', 'start of month'),
                    DATE('2024-01-31', 'start of month', '+1 month'),
                    DATE('2024-01-31', 'weekday 1', 'weekday 5'),
                    DATE('bad', 'start of month', '+1 month'),
                    DATE('2024-01-31', 'start of month', 'bogus');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-31"),
            Value::from("2024-07-09 03:04:05"),
            Value::from("2024-07-31 23:59:59"),
            Value::Integer(1706659200),
            Value::Real(2460340.5),
            Value::from("2024-03-01"),
            Value::from("2024-02-01"),
            Value::from("2024-02-09"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_unixepoch_modifier_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE(1704067200, 'unixepoch'),
                    TIME(1704067200, 'unixepoch'),
                    DATETIME(1704067200, 'unixepoch'),
                    STRFTIME('%F %T', 1704067200, 'unixepoch'),
                    UNIXEPOCH(1704067200, 'unixepoch'),
                    JULIANDAY(1704067200, 'unixepoch'),
                    DATETIME(1704067200, 'unixepoch', '+1 day'),
                    DATETIME(1704067200, '+1 day', 'unixepoch'),
                    DATETIME('1704067200', 'unixepoch'),
                    DATETIME(1704067200.5, 'unixepoch', 'subsec'),
                    TIME(1704067200.5, 'unixepoch', 'subsec'),
                    STRFTIME('%f', 1704067200.5, 'unixepoch'),
                    UNIXEPOCH(1704067200.5, 'unixepoch', 'subsec'),
                    DATETIME(1704067200, 'bogus'),
                    DATETIME(1704067200, 'weekday 1', 'unixepoch');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-01"),
            Value::from("00:00:00"),
            Value::from("2024-01-01 00:00:00"),
            Value::from("2024-01-01 00:00:00"),
            Value::Integer(1704067200),
            Value::Real(2460310.5),
            Value::from("2024-01-02 00:00:00"),
            Value::Null,
            Value::from("2024-01-01 00:00:00"),
            Value::from("2024-01-01 00:00:00.500"),
            Value::from("00:00:00.500"),
            Value::from("00.500"),
            Value::Real(1704067200.5),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_supports_blob_text_coercion_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATETIME(X'31373034303637323030', X'756E697865706F6368'),
                    DATETIME(1704067200, 'unixepoch', X'2B3120646179'),
                    DATE(X'31373034303637323030', 'unixepoch'),
                    UNIXEPOCH(X'31373034303637323030', 'unixepoch');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-01 00:00:00"),
            Value::from("2024-01-02 00:00:00"),
            Value::from("2024-01-01"),
            Value::Integer(1704067200),
        ]]
    );
}

#[test]
fn database_evaluates_auto_modifier_for_date_time_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT DATE(1704067200, 'auto'),
                    TIME(1704067200, 'auto'),
                    DATETIME(1704067200, 'auto'),
                    STRFTIME('%F %T', 1704067200, 'auto'),
                    UNIXEPOCH(1704067200, 'auto'),
                    JULIANDAY(1704067200, 'auto'),
                    DATETIME(1704067200, 'auto', '+1 day'),
                    DATETIME(1704067200, '+1 day', 'auto'),
                    DATETIME(2460310.5, 'auto', '+1 day'),
                    DATETIME('1704067200', 'auto'),
                    DATETIME('2460310.5', 'auto'),
                    DATETIME(1704067200.5, 'auto', 'subsec'),
                    TIME(1704067200.5, 'auto', 'subsec'),
                    STRFTIME('%f', 1704067200.5, 'auto'),
                    UNIXEPOCH(1704067200.5, 'auto', 'subsec'),
                    DATETIME(2460310.500005787, 'auto', 'subsec'),
                    STRFTIME('%f', 2460310.500005787, 'auto'),
                    DATETIME(1704067200, 'bogus'),
                    DATETIME(1704067200, 'weekday 1', 'auto');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01-01"),
            Value::from("00:00:00"),
            Value::from("2024-01-01 00:00:00"),
            Value::from("2024-01-01 00:00:00"),
            Value::Integer(1704067200),
            Value::Real(2460310.5),
            Value::from("2024-01-02 00:00:00"),
            Value::Null,
            Value::from("2024-01-02 00:00:00"),
            Value::from("2024-01-01 00:00:00"),
            Value::from("2024-01-01 00:00:00"),
            Value::from("2024-01-01 00:00:00.500"),
            Value::from("00:00:00.500"),
            Value::from("00.500"),
            Value::Real(1704067200.5),
            Value::from("2024-01-01 00:00:00.500"),
            Value::from("00.500"),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_likely_and_unlikely_scalar_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT LIKELY(1), LIKELY(0), UNLIKELY(1), UNLIKELY(2), LIKELY(NULL), UNLIKELY(NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(2),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_sqlite_version_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query("SELECT SQLITE_VERSION(), TYPEOF(SQLITE_VERSION());")
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("3.46.0"), Value::from("text"),]]
    );
}

#[test]
fn database_evaluates_sign_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query("SELECT SIGN(-3), SIGN(0), SIGN(2), SIGN(0.0), SIGN(-0.0), SIGN(NULL), SIGN(true), SIGN(false), SIGN('12'), SIGN('-7'), SIGN('5.5'), SIGN('abc'), SIGN(X'01');")
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(-1),
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(0),
            Value::Integer(0),
            Value::Null,
            Value::Integer(1),
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(-1),
            Value::Integer(1),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn database_evaluates_sqlite_source_id_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query("SELECT LENGTH(SQLITE_SOURCE_ID()), TYPEOF(SQLITE_SOURCE_ID());")
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(84), Value::from("text"),]]);
}

#[test]
fn database_evaluates_sqlite_compileoption_scalar_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT
                SQLITE_COMPILEOPTION_USED('OMIT_LOAD_EXTENSION'),
                SQLITE_COMPILEOPTION_USED('sqlite_default_page_size'),
                SQLITE_COMPILEOPTION_USED('not_a_real_option'),
                SQLITE_COMPILEOPTION_GET(0),
                SQLITE_COMPILEOPTION_GET(1),
                SQLITE_COMPILEOPTION_GET(9999),
                TYPEOF(SQLITE_COMPILEOPTION_GET(9999));",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(0),
            Value::from("DEFAULT_PAGE_SIZE=4096"),
            Value::from("MAX_PAGE_SIZE=65536"),
            Value::Null,
            Value::from("null"),
        ]]
    );
}

#[test]
fn database_evaluates_randomblob_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT LENGTH(RANDOMBLOB(4)),
                    TYPEOF(RANDOMBLOB(4)),
                    LENGTH(RANDOMBLOB(0)),
                    LENGTH(RANDOMBLOB(-2)),
                    TYPEOF(RANDOMBLOB(0));",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(4),
            Value::from("blob"),
            Value::Integer(1),
            Value::Integer(1),
            Value::from("blob"),
        ]]
    );
}

#[test]
fn database_evaluates_random_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query("SELECT TYPEOF(RANDOM()), TYPEOF(ABS(RANDOM()));")
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::from("integer"), Value::from("integer"),]]
    );
}

#[test]
fn database_evaluates_unhex_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT UNHEX('4142'),
                    UNHEX('41-42', '-'),
                    TYPEOF(UNHEX('4142')),
                    UNHEX(NULL),
                    UNHEX('4G'),
                    UNHEX('41 42', ' ');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Blob(vec![0x41, 0x42]),
            Value::Blob(vec![0x41, 0x42]),
            Value::from("blob"),
            Value::Null,
            Value::Null,
            Value::Blob(vec![0x41, 0x42]),
        ]]
    );
}

#[test]
fn database_supports_unhex_text_coercion() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT UNHEX(4142),
                    TYPEOF(UNHEX(4142)),
                    UNHEX(X'34313432'),
                    TYPEOF(UNHEX(X'34313432'));",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Blob(vec![0x41, 0x42]),
            Value::from("blob"),
            Value::Blob(vec![0x41, 0x42]),
            Value::from("blob"),
        ]]
    );
}

#[test]
fn database_evaluates_concat_scalar_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT CONCAT('a', 'b'),
                    CONCAT('a', NULL, 'b'),
                    CONCAT(NULL, NULL),
                    CONCAT(1, 2),
                    CONCAT(TRUE, 'x'),
                    CONCAT(X'4142', 'c'),
                    CONCAT_WS('-', 'a', 'b', 'c'),
                    CONCAT_WS('-', 'a', NULL, 'c'),
                    CONCAT_WS('-', NULL, 'a', 'b'),
                    CONCAT_WS('-', 1, 2),
                    CONCAT_WS('-', TRUE, FALSE),
                    CONCAT_WS('-', X'4142', 'c'),
                    CONCAT_WS(NULL, 'a', 'b'),
                    TYPEOF(CONCAT('a', 'b')),
                    TYPEOF(CONCAT_WS('-', 'a', 'b')),
                    TYPEOF(CONCAT(NULL, NULL)),
                    1 || 2,
                    'a' || 1 || 2,
                    TRUE || 'x',
                    X'4142' || 'c',
                    NULL || 2,
                    TYPEOF(1 || 2);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("ab"),
            Value::from("ab"),
            Value::from(""),
            Value::from("12"),
            Value::from("1x"),
            Value::from("ABc"),
            Value::from("a-b-c"),
            Value::from("a-c"),
            Value::from("a-b"),
            Value::from("1-2"),
            Value::from("1-0"),
            Value::from("AB-c"),
            Value::Null,
            Value::from("text"),
            Value::from("text"),
            Value::from("text"),
            Value::from("12"),
            Value::from("a12"),
            Value::from("1x"),
            Value::from("ABc"),
            Value::Null,
            Value::from("text"),
        ]]
    );
}

#[test]
fn database_evaluates_printf_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT PRINTF('member-%03d-%s', 7, 'AB'),
                    TYPEOF(PRINTF('member-%03d-%s', 7, 'AB')),
                    PRINTF('%04d-', 12),
                    PRINTF('%s', NULL),
                    PRINTF('%03d', NULL);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("member-007-AB"),
            Value::from("text"),
            Value::from("0012-"),
            Value::from(""),
            Value::from("000"),
        ]]
    );
}

#[test]
fn database_evaluates_extended_printf_and_format_specifiers() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT PRINTF('%f|%x|%X|%o|%c', 1.25, 255, 255, 8, 65),
                    PRINTF('%f|%x|%X|%o|%c', NULL, NULL, NULL, NULL, NULL),
                    FORMAT('%08.2f|%04x', 3.5, 15),
                    PRINTF('%i|%u|%08u|%u|%i', -1, -1, 15, NULL, NULL),
                    PRINTF('%e|%E|%g|%G', 1234.5, 1234.5, 1234.5, 1234.5),
                    PRINTF('%.2e|%.2E|%.3g|%.3G', 1234.5, 1234.5, 1234.5, 1234.5),
                    PRINTF('%e|%g', NULL, NULL),
                    PRINTF('%08.2e|%010.3g', 3.5, 1234.5),
                    PRINTF('%p|%p|%n|%d', 255, NULL, 123, 7),
                    PRINTF('%08p|%p', 15, -1),
                    PRINTF('%ld|%lld|%li|%lli|%lx|%llX', 7, 8, 9, 10, 255, 255),
                    PRINTF('%#x|%#X|%#o|%#f|%#.0f', 255, 255, 8, 1.0, 1.0),
                    PRINTF('%#08x|%#8x|%-#8x', 255, 255, 255),
                    PRINTF('%*s|%0*d|%.*f|%*.*f', 5, 'x', 4, 12, 2, 1.234, 8, 2, 3.5),
                    PRINTF('%*s|%*d|%.*f', -5, 'x', -5, 12, -1, 1.234),
                    PRINTF('%*.*s|%*.*d', 6, 2, 'abcd', 6, 3, 12);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("1.250000|ff|FF|10|6"),
            Value::from("0.000000|0|0|0|"),
            Value::from("00003.50|000f"),
            Value::from("-1|18446744073709551615|00000015|0|0"),
            Value::from("1.234500e+03|1.234500E+03|1234.5|1234.5"),
            Value::from("1.23e+03|1.23E+03|1.23e+03|1.23E+03"),
            Value::from("0.000000e+00|0"),
            Value::from("3.50e+00|001.23e+03"),
            Value::from("FF|0||7"),
            Value::from("0000000F|FFFFFFFFFFFFFFFF"),
            Value::from("7|8|9|10|ff|FF"),
            Value::from("0xff|0XFF|010|1.000000|1."),
            Value::from("0x000000ff|    0xff|0xff    "),
            Value::from("    x|0012|1.23|    3.50"),
            Value::from("x    |12   |1.2"),
            Value::from("    ab|   012"),
        ]]
    );

    let escaped_rows = db
        .query(
            "SELECT PRINTF('%q|%Q|%w|%z', 'O''Reilly', 'O''Reilly', 'a\"\"b', 'ztext'),
                    PRINTF('%q|%Q|%w|%z', NULL, NULL, NULL, NULL);",
        )
        .unwrap();

    assert_eq!(
        escaped_rows,
        vec![vec![
            Value::from("O''Reilly|'O''Reilly'|a\"\"\"\"b|ztext"),
            Value::from("(NULL)|NULL|(NULL)|"),
        ]]
    );

    let flag_rows = db
        .query(
            "SELECT PRINTF('%-6s|%-06d|%+d|% d|%,d|%,.2f', 'x', 12, 7, 7, 1234567, 1234567.89),
                    PRINTF('%+d|% d|%,d|%,.2f', -7, -7, -1234567, -1234567.89),
                    PRINTF('%-,10d|%-10s', 1234567, 'x');",
        )
        .unwrap();

    assert_eq!(
        flag_rows,
        vec![vec![
            Value::from("x     |000012|+7| 7|1,234,567|1,234,567.89"),
            Value::from("-7|-7|-1,234,567|-1,234,567.89"),
            Value::from("1,234,567 |x         "),
        ]]
    );
}

#[test]
fn database_evaluates_nullif_with_mixed_numeric_types_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT NULLIF(5, 5.0),
                    NULLIF(TRUE, 1),
                    NULLIF(FALSE, 0.0),
                    NULLIF(5, 4.5);",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Integer(5),
        ]]
    );
}

#[test]
fn database_supports_text_coercion_for_strftime_and_printf_formats() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT STRFTIME(X'25592D256D', '2024-01-02'),
                    TYPEOF(STRFTIME(X'25592D256D', '2024-01-02')),
                    PRINTF(123, 456),
                    TYPEOF(PRINTF(123, 456));",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("2024-01"),
            Value::from("text"),
            Value::from("123"),
            Value::from("text"),
        ]]
    );
}

#[test]
fn database_evaluates_iif_and_if_scalar_functions() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT IIF(1, 'yes', 'no'),
                    IIF(0, 'yes', 'no'),
                    TYPEOF(IIF(1, 'yes', 'no')),
                    IIF(NULL, 'yes', 'no'),
                    IF(1, 'a', 'b'),
                    IF(0, 'a', 'b'),
                    IIF(1, 'ok', 1 / 0),
                    IF(0, 1 / 0, 'fallback'),
                    IIF(1, 'short'),
                    IIF(0, 'short'),
                    IF(NULL, 'short'),
                    IIF(0, 'a', 1, 'b', 'c'),
                    IIF(0, 'a', 0, 'b', 'c'),
                    IIF(1, 'a', 0, 'b', 'c'),
                    IF(0, 'a', 1, 'b', 'c');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("yes"),
            Value::from("no"),
            Value::from("text"),
            Value::from("no"),
            Value::from("a"),
            Value::from("b"),
            Value::from("ok"),
            Value::from("fallback"),
            Value::from("short"),
            Value::Null,
            Value::Null,
            Value::from("b"),
            Value::from("c"),
            Value::from("a"),
            Value::from("b"),
        ]]
    );
}

#[test]
fn database_evaluates_case_scalar_expressions() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, nickname TEXT, active BOOLEAN, role TEXT);
         INSERT INTO users VALUES (1, 'alice', 'ally', true, 'admin');
         INSERT INTO users VALUES (2, 'bob', 'b', false, 'staff');
         INSERT INTO users VALUES (3, 'carol', NULL, NULL, 'guest');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT CASE WHEN active THEN name ELSE nickname END,
                    CASE role WHEN 'admin' THEN 1 WHEN 'staff' THEN 2 ELSE 0 END,
                    CASE WHEN 1 THEN 'a' ELSE 1 / 0 END,
                    CASE WHEN 0 THEN 1 / 0 ELSE 'b' END,
                    CASE WHEN NULL THEN 'yes' ELSE 'no' END
             FROM users
             WHERE id = 1;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("alice"),
            Value::Integer(1),
            Value::from("a"),
            Value::from("b"),
            Value::from("no"),
        ]]
    );

    let fallback_rows = db
        .query(
            "SELECT CASE WHEN active THEN name ELSE nickname END,
                    CASE role WHEN 'admin' THEN 1 WHEN 'staff' THEN 2 ELSE 0 END
             FROM users
             WHERE id IN (2, 3)
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        fallback_rows,
        vec![
            vec![Value::from("b"), Value::Integer(2)],
            vec![Value::Null, Value::Integer(0)],
        ]
    );
}

#[test]
fn database_reports_changes_and_total_changes_scalar_functions() {
    let db = Database::memory();

    let initial = db.query("SELECT CHANGES(), TOTAL_CHANGES();").unwrap();
    assert_eq!(initial, vec![vec![Value::Integer(0), Value::Integer(0)]]);

    db.execute(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, value INTEGER);
         INSERT INTO items VALUES (1, 10);
         INSERT INTO items VALUES (2, 20);",
    )
    .unwrap();
    let after_insert = db.query("SELECT CHANGES(), TOTAL_CHANGES();").unwrap();
    assert_eq!(
        after_insert,
        vec![vec![Value::Integer(1), Value::Integer(2)]]
    );

    db.execute("UPDATE items SET value = 21 WHERE id = 2;")
        .unwrap();
    let after_update = db.query("SELECT CHANGES(), TOTAL_CHANGES();").unwrap();
    assert_eq!(
        after_update,
        vec![vec![Value::Integer(1), Value::Integer(3)]]
    );

    db.execute("DELETE FROM items WHERE id = 999;").unwrap();
    let after_noop_delete = db.query("SELECT CHANGES(), TOTAL_CHANGES();").unwrap();
    assert_eq!(
        after_noop_delete,
        vec![vec![Value::Integer(0), Value::Integer(3)]]
    );
}

#[test]
fn database_evaluates_trim_scalar_functions() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, '  alice  ');
         INSERT INTO users VALUES (2, 'xxbobx ');
         INSERT INTO users VALUES (3, '  carol');
         INSERT INTO users VALUES (4, NULL);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT TRIM(name),
                    LTRIM(name),
                    RTRIM(name),
                    TRIM(name, ' x'),
                    LTRIM(name, ' x'),
                    RTRIM(name, ' x')
             FROM users
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![
                Value::from("alice"),
                Value::from("alice  "),
                Value::from("  alice"),
                Value::from("alice"),
                Value::from("alice  "),
                Value::from("  alice"),
            ],
            vec![
                Value::from("xxbobx"),
                Value::from("xxbobx "),
                Value::from("xxbobx"),
                Value::from("bob"),
                Value::from("bobx "),
                Value::from("xxbob"),
            ],
            vec![
                Value::from("carol"),
                Value::from("carol"),
                Value::from("  carol"),
                Value::from("carol"),
                Value::from("carol"),
                Value::from("  carol"),
            ],
            vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ]
    );
}

#[test]
fn database_evaluates_substr_scalar_function() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, NULL);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT SUBSTR(name, 2), SUBSTR(name, 2, 3)
             FROM users
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::from("lice"), Value::from("lic")],
            vec![Value::from("ob"), Value::from("ob")],
            vec![Value::Null, Value::Null],
        ]
    );

    let negative_lengths = db
        .query(
            "SELECT SUBSTR('abcdef', 2, -1),
                    SUBSTR('abcdef', 2, -2),
                    SUBSTR('abcdef', -2, -2),
                    SUBSTR('abcdef', 0, -1);",
        )
        .unwrap();
    assert_eq!(
        negative_lengths,
        vec![vec![
            Value::from("a"),
            Value::from("a"),
            Value::from("cd"),
            Value::from(""),
        ]]
    );

    let blob_rows = db
        .query(
            "SELECT SUBSTR(X'41424344', 2, 2),
                    TYPEOF(SUBSTR(X'41424344', 2, 2)),
                    SUBSTR(X'41424344', 2, -1);",
        )
        .unwrap();
    assert_eq!(
        blob_rows,
        vec![vec![
            Value::Blob(vec![0x42, 0x43]),
            Value::from("blob"),
            Value::Blob(vec![0x41]),
        ]]
    );
}

#[test]
fn database_evaluates_instr_scalar_function() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO users VALUES (3, NULL);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT INSTR(name, 'li'), INSTR(name, 'z')
             FROM users
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(2), Value::Integer(0)],
            vec![Value::Integer(0), Value::Integer(0)],
            vec![Value::Null, Value::Null],
        ]
    );
}

#[test]
fn database_evaluates_replace_scalar_function() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'xoxo');
         INSERT INTO users VALUES (3, NULL);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT REPLACE(name, 'ice', 'ICE'), REPLACE(name, 'x', '')
             FROM users
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::from("alICE"), Value::from("alice")],
            vec![Value::from("xoxo"), Value::from("oo")],
            vec![Value::Null, Value::Null],
        ]
    );
}

#[test]
fn database_evaluates_quote_scalar_function() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, payload BLOB);
         INSERT INTO users VALUES (1, 'ali''ce', X'ABCD');
         INSERT INTO users VALUES (2, NULL, NULL);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT QUOTE(name), QUOTE(payload), QUOTE(id), QUOTE(NULL)
             FROM users
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![
                Value::from("'ali''ce'"),
                Value::from("X'ABCD'"),
                Value::from("1"),
                Value::from("NULL"),
            ],
            vec![
                Value::from("NULL"),
                Value::from("NULL"),
                Value::from("2"),
                Value::from("NULL"),
            ],
        ]
    );

    let rows = db
        .query("SELECT QUOTE(TRUE), QUOTE(FALSE), QUOTE(1.25);")
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("1"),
            Value::from("0"),
            Value::from("1.25"),
        ]]
    );

    let rows = db
        .query(
            "SELECT QUOTE(3.0),
                    LOWER(3.0),
                    HEX(3.0),
                    LENGTH(3.0),
                    SUBSTR(3.0, 1, 3),
                    REPLACE(3.0, '.', '_');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("3.0"),
            Value::from("3.0"),
            Value::from("332E30"),
            Value::Integer(3),
            Value::from("3.0"),
            Value::from("3_0"),
        ]]
    );
}

#[test]
fn database_evaluates_unicode_scalar_function() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, nickname TEXT);
         INSERT INTO users VALUES (1, 'Alice', '你好');
         INSERT INTO users VALUES (2, '', NULL);
         INSERT INTO users VALUES (3, NULL, 'z');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT UNICODE(name), UNICODE(nickname)
             FROM users
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(65), Value::Integer(20320)],
            vec![Value::Null, Value::Null],
            vec![Value::Null, Value::Integer(122)],
        ]
    );
}

#[test]
fn database_evaluates_scientific_notation_real_literals() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT TYPEOF(1e3),
                    1e3,
                    TYPEOF(1.5e2),
                    1.5e2,
                    TYPEOF(-2.5e-1),
                    -2.5e-1,
                    DATETIME(1.7040672e9, 'auto'),
                    JULIANDAY(2.4603105e6, 'auto');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("real"),
            Value::Real(1000.0),
            Value::from("real"),
            Value::Real(150.0),
            Value::from("real"),
            Value::Real(-0.25),
            Value::from("2024-01-01 00:00:00"),
            Value::Real(2460310.5),
        ]]
    );
}

#[test]
fn database_evaluates_leading_and_trailing_dot_real_literals() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT TYPEOF(.5),
                    .5,
                    TYPEOF(1.),
                    1.,
                    TYPEOF(-.25),
                    -.25,
                    TYPEOF(+.5),
                    +.5,
                    DATETIME(.24603105e7, 'auto'),
                    DATETIME(2460310., 'auto'),
                    DATETIME(.24603105e7, 'auto', '+1 day');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("real"),
            Value::Real(0.5),
            Value::from("real"),
            Value::Real(1.0),
            Value::from("real"),
            Value::Real(-0.25),
            Value::from("real"),
            Value::Real(0.5),
            Value::from("2024-01-01 00:00:00"),
            Value::from("2023-12-31 12:00:00"),
            Value::from("2024-01-02 00:00:00"),
        ]]
    );
}

#[test]
fn database_evaluates_char_scalar_function() {
    let db = Database::memory();

    let rows = db
        .query("SELECT CHAR(65), CHAR(20320, 22909), CHAR(65, 32, 66);")
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("A"),
            Value::from("你好"),
            Value::from("A B"),
        ]]
    );
}

#[test]
fn database_evaluates_zeroblob_scalar_function() {
    let db = Database::memory();

    let rows = db.query("SELECT ZEROBLOB(4), ZEROBLOB(0);").unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::Blob(vec![0, 0, 0, 0]), Value::Blob(vec![]),]]
    );
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

    let fallback = db
        .query("SELECT COALESCE(name, LOWER(age), 'fallback') FROM users WHERE id = 2;")
        .unwrap();
    assert_eq!(fallback, vec![vec![Value::from("40")]]);
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
            Value::Real(24.333_333_333_333_332),
            Value::Integer(2),
        ]]
    );
}

#[test]
fn database_accepts_aggregate_all_modifier_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         INSERT INTO users VALUES (1, 20);
         INSERT INTO users VALUES (2, 20);
         INSERT INTO users VALUES (3, NULL);",
    )
    .unwrap();

    let rows = db
        .query("SELECT COUNT(ALL age), SUM(ALL age), AVG(ALL age) FROM users;")
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(2),
            Value::Integer(40),
            Value::Real(20.0)
        ]]
    );
}

#[test]
fn database_supports_total_aggregate() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER);
         INSERT INTO users VALUES (1, 1);
         INSERT INTO users VALUES (2, 2);
         INSERT INTO users VALUES (3, NULL);",
    )
    .unwrap();

    let rows = db
        .query("SELECT TOTAL(age), TOTAL(NULL) FROM users;")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Real(3.0), Value::Real(0.0)]]);

    let empty_rows = db
        .query("SELECT TOTAL(age) FROM users WHERE id > 10;")
        .unwrap();

    assert_eq!(empty_rows, vec![vec![Value::Real(0.0)]]);
}

#[test]
fn database_aggregates_real_values_with_sum_and_avg() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, score REAL);
         INSERT INTO metrics VALUES (1, 1.5);
         INSERT INTO metrics VALUES (2, 2.5);
         INSERT INTO metrics VALUES (3, NULL);",
    )
    .unwrap();

    let rows = db
        .query("SELECT SUM(score), AVG(score) FROM metrics;")
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Real(4.0), Value::Real(2.0)]]);
}

#[test]
fn database_supports_median_aggregate_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, score);
         INSERT INTO metrics VALUES (1, 1);
         INSERT INTO metrics VALUES (2, 3);
         INSERT INTO metrics VALUES (3, 2);
         INSERT INTO metrics VALUES (4, NULL);",
    )
    .unwrap();

    let rows = db.query("SELECT MEDIAN(score) FROM metrics;").unwrap();
    assert_eq!(rows, vec![vec![Value::Real(2.0)]]);

    db.execute("INSERT INTO metrics VALUES (5, 4);").unwrap();
    let rows = db.query("SELECT MEDIAN(score) FROM metrics;").unwrap();
    assert_eq!(rows, vec![vec![Value::Real(2.5)]]);

    let empty = db
        .query("SELECT MEDIAN(score) FROM metrics WHERE id > 10;")
        .unwrap();
    assert_eq!(empty, vec![vec![Value::Null]]);

    db.execute("INSERT INTO metrics VALUES (6, 'bad');")
        .unwrap();
    let error = db.query("SELECT MEDIAN(score) FROM metrics;").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("input to median() is not numeric"),
        "unexpected error: {error}"
    );
}

#[test]
fn database_allows_scalar_functions_to_consume_aggregate_results_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, score);
         INSERT INTO metrics VALUES (1, 1);
         INSERT INTO metrics VALUES (2, 2);
         INSERT INTO metrics VALUES (3, 3);
         INSERT INTO metrics VALUES (4, 4);",
    )
    .unwrap();

    let rows = db
        .query("SELECT TYPEOF(MEDIAN(score)), ROUND(AVG(score), 1) FROM metrics;")
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("real"), Value::Real(2.5)]]);
}

#[test]
fn database_allows_having_and_order_by_to_consume_aggregate_results_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics (category TEXT, score);
         INSERT INTO metrics VALUES ('low', 1);
         INSERT INTO metrics VALUES ('low', 2);
         INSERT INTO metrics VALUES ('high', 3);
         INSERT INTO metrics VALUES ('high', 4);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT category
             FROM metrics
             GROUP BY category
             HAVING ROUND(AVG(score), 1) > 2.0
             ORDER BY ROUND(AVG(score), 1) DESC;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("high")]]);
}

#[test]
fn database_rejects_nested_aggregate_functions_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, score);
         INSERT INTO metrics VALUES (1, 1);
         INSERT INTO metrics VALUES (2, 2);",
    )
    .unwrap();

    let error = db
        .query("SELECT SUM(AVG(score)) FROM metrics;")
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("misuse of aggregate function AVG"),
        "unexpected error: {error}"
    );
}

#[test]
fn database_supports_percentile_aggregates_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, score);
         INSERT INTO metrics VALUES (1, 1);
         INSERT INTO metrics VALUES (2, 2);
         INSERT INTO metrics VALUES (3, 3);
         INSERT INTO metrics VALUES (4, 4);
         INSERT INTO metrics VALUES (5, NULL);",
    )
    .unwrap();

    let empty = db
        .query(
            "SELECT PERCENTILE_CONT(score, 0.5), PERCENTILE_DISC(score, 0.5), PERCENTILE(score, 50)
             FROM metrics
             WHERE id > 10;",
        )
        .unwrap();
    assert_eq!(empty, vec![vec![Value::Null, Value::Null, Value::Null]]);

    let rows = db
        .query(
            "SELECT PERCENTILE_CONT(score, 0.5),
                    PERCENTILE_DISC(score, 0.5),
                    PERCENTILE(score, 50),
                    PERCENTILE_CONT(score, 0.25),
                    PERCENTILE_DISC(score, 0.25)
             FROM metrics;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Real(2.5),
            Value::Real(2.0),
            Value::Real(2.5),
            Value::Real(1.75),
            Value::Real(1.0),
        ]]
    );

    db.execute("INSERT INTO metrics VALUES (6, 'bad');")
        .unwrap();
    let error = db
        .query("SELECT PERCENTILE_CONT(score, 0.5) FROM metrics;")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("input to percentile_cont() is not numeric"),
        "unexpected error: {error}"
    );
}

#[test]
fn database_aggregates_distinct_real_values() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, score REAL);
         INSERT INTO metrics VALUES (1, 1.5);
         INSERT INTO metrics VALUES (2, 1.5);
         INSERT INTO metrics VALUES (3, 2.5);
         INSERT INTO metrics VALUES (4, NULL);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT SUM(DISTINCT score), AVG(DISTINCT score), TOTAL(DISTINCT score)
             FROM metrics;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::Real(4.0), Value::Real(2.0), Value::Real(4.0)]]
    );
}

#[test]
fn database_supports_group_concat_aggregate() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT, sep TEXT);
         INSERT INTO tags VALUES (1, 'a', '|');
         INSERT INTO tags VALUES (2, 'a', '|');
         INSERT INTO tags VALUES (3, 'b', ':');
         INSERT INTO tags VALUES (4, NULL, '/');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT GROUP_CONCAT(name),
                    GROUP_CONCAT(name, sep),
                    GROUP_CONCAT(DISTINCT name)
             FROM tags;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("a,a,b"),
            Value::from("a|a:b"),
            Value::from("a,b"),
        ]]
    );
}

#[test]
fn database_supports_group_concat_order_by_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE tags (name TEXT, rank INTEGER, sep TEXT);
         INSERT INTO tags VALUES ('b', 2, '|');
         INSERT INTO tags VALUES ('a', 1, ':');
         INSERT INTO tags VALUES ('c', 3, '-');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT GROUP_CONCAT(name ORDER BY rank),
                    GROUP_CONCAT(name ORDER BY rank DESC),
                    GROUP_CONCAT(name, sep ORDER BY rank)
             FROM tags;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("a,b,c"),
            Value::from("c,b,a"),
            Value::from("a|b-c"),
        ]]
    );
}

#[test]
fn database_supports_json_group_array_aggregate_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE items (value INTEGER, rank INTEGER, active INTEGER, label TEXT);
         INSERT INTO items VALUES (2, 2, 1, 'b');
         INSERT INTO items VALUES (1, 1, 1, 'a');
         INSERT INTO items VALUES (NULL, 3, 0, NULL);
         INSERT INTO items VALUES (4, 4, 1, 'x');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT JSON_GROUP_ARRAY(value),
                    JSON_GROUP_ARRAY(value ORDER BY rank),
                    JSON_GROUP_ARRAY(value) FILTER (WHERE active = 1),
                    JSON_GROUP_ARRAY(label)
             FROM items;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("[2,1,null,4]"),
            Value::from("[1,2,null,4]"),
            Value::from("[2,1,4]"),
            Value::from("[\"b\",\"a\",null,\"x\"]"),
        ]]
    );

    let empty = db
        .query("SELECT JSON_GROUP_ARRAY(value) FROM items WHERE active = 9;")
        .unwrap();
    assert_eq!(empty, vec![vec![Value::from("[]")]]);
}

#[test]
fn database_supports_json_group_object_aggregate_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE pairs (name TEXT, value INTEGER, rank INTEGER, active INTEGER);
         INSERT INTO pairs VALUES ('b', 2, 2, 1);
         INSERT INTO pairs VALUES ('a', 1, 1, 1);
         INSERT INTO pairs VALUES (NULL, 9, 3, 1);
         INSERT INTO pairs VALUES ('n', NULL, 4, 0);
         INSERT INTO pairs VALUES ('a', 5, 5, 1);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT JSON_GROUP_OBJECT(name, value),
                    JSON_GROUP_OBJECT(name, value ORDER BY rank),
                    JSON_GROUP_OBJECT(name, value) FILTER (WHERE active = 1)
             FROM pairs;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("{\"b\":2,\"a\":1,\"n\":null,\"a\":5}"),
            Value::from("{\"a\":1,\"b\":2,\"n\":null,\"a\":5}"),
            Value::from("{\"b\":2,\"a\":1,\"a\":5}"),
        ]]
    );

    let empty = db
        .query("SELECT JSON_GROUP_OBJECT(name, value) FROM pairs WHERE active = 9;")
        .unwrap();
    assert_eq!(empty, vec![vec![Value::from("{}")]]);
}

#[test]
fn database_supports_string_agg_as_group_concat_alias() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT, sep TEXT);
         INSERT INTO tags VALUES (1, 'a', '|');
         INSERT INTO tags VALUES (2, 'a', '|');
         INSERT INTO tags VALUES (3, 'b', ':');
         INSERT INTO tags VALUES (4, NULL, '/');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT STRING_AGG(name, sep),
                    STRING_AGG(DISTINCT name)
             FROM tags;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("a|a:b"), Value::from("a,b"),]]);
}

#[test]
fn database_supports_aggregate_filter_clause_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics (value INTEGER, active INTEGER);
         INSERT INTO metrics VALUES (1, 1);
         INSERT INTO metrics VALUES (2, 0);
         INSERT INTO metrics VALUES (3, 1);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT SUM(value) FILTER (WHERE active = 1),
                    COUNT(*) FILTER (WHERE active = 0),
                    GROUP_CONCAT(value, ':') FILTER (WHERE active = 1)
             FROM metrics;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(4),
            Value::Integer(1),
            Value::from("1:3"),
        ]]
    );
}

#[test]
fn database_accepts_order_by_inside_numeric_aggregates_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics (value INTEGER, rank INTEGER);
         INSERT INTO metrics VALUES (1, 2);
         INSERT INTO metrics VALUES (2, 1);
         INSERT INTO metrics VALUES (3, 3);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT SUM(value ORDER BY rank),
                    COUNT(value ORDER BY rank DESC),
                    AVG(value ORDER BY rank)
             FROM metrics;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![Value::Integer(6), Value::Integer(3), Value::Real(2.0)]]
    );
}

#[test]
fn database_group_concat_coerces_non_text_values_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value INTEGER, sep TEXT);
         INSERT INTO metrics VALUES (1, 1, '|');
         INSERT INTO metrics VALUES (2, 1, '|');
         INSERT INTO metrics VALUES (3, 2, ':');
         INSERT INTO metrics VALUES (4, NULL, '/');",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT GROUP_CONCAT(value),
                    GROUP_CONCAT(DISTINCT value),
                    STRING_AGG(value, sep)
             FROM metrics;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::from("1,1,2"),
            Value::from("1,2"),
            Value::from("1|1:2"),
        ]]
    );
}

#[test]
fn database_aggregates_text_numbers_with_sqlite_numeric_coercion() {
    let db = Database::memory();

    let rows = db
        .query(
            "SELECT sum('1'),
                    avg('1'),
                    total('1'),
                    sum('abc'),
                    avg('abc'),
                    total('abc');",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::Real(1.0),
            Value::Real(1.0),
            Value::Real(0.0),
            Value::Real(0.0),
            Value::Real(0.0),
        ]]
    );
}

#[test]
fn database_supports_multi_row_insert_values() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'alice'), (2, 'bob');",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT id, name FROM users ORDER BY id;").unwrap(),
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn database_supports_insert_values_scalar_expressions() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, score INTEGER, created TEXT);
         INSERT INTO users VALUES
            (1 + 1, LOWER('ALICE'), 10 * 2, SUBSTR('2024-01-02', 1, 4)),
            (3, COALESCE(NULL, 'BOB'), ABS(-7), DATE('2024-01-02'));",
    )
    .unwrap();

    assert_eq!(
        db.query("SELECT id, name, score, created FROM users ORDER BY id;")
            .unwrap(),
        vec![
            vec![
                Value::Integer(2),
                Value::from("alice"),
                Value::Integer(20),
                Value::from("2024"),
            ],
            vec![
                Value::Integer(3),
                Value::from("BOB"),
                Value::Integer(7),
                Value::from("2024-01-02"),
            ],
        ]
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
fn database_selects_from_subquery_without_alias_like_sqlite() {
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
             FROM (SELECT age + 1 AS bucket FROM users)
             WHERE bucket > 21
             ORDER BY bucket ASC;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(31)]]);
}

#[test]
fn database_does_not_expose_missing_subquery_alias_as_qualifier_like_sqlite() {
    let db = Database::memory();

    let error = db.query("SELECT t.x FROM (SELECT 1 AS x);").unwrap_err();

    assert!(
        error.to_string().contains("unknown column t.x"),
        "unexpected error: {error}"
    );
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

    let cross_rows = db
        .query(
            "SELECT u.name, o.amount
             FROM users u
             CROSS JOIN orders o
             WHERE u.id + 1 = o.user_id
             ORDER BY u.name ASC;",
        )
        .unwrap();

    assert_eq!(
        cross_rows,
        vec![vec![Value::from("alice"), Value::Integer(80)]]
    );
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

    let collate_rows = db
        .query(
            "SELECT 'A' = 'a' COLLATE NOCASE,
                    'A' = 'a' COLLATE BINARY,
                    'B' > 'a' COLLATE NOCASE,
                    'a' = 'a  ' COLLATE RTRIM,
                    'a x' = 'a' COLLATE RTRIM,
                    name
             FROM users
             WHERE name = 'alice' COLLATE NOCASE;",
        )
        .unwrap();
    assert_eq!(
        collate_rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::from("Alice"),
        ]]
    );

    db.execute(
        "CREATE TABLE collated_users (
            id INTEGER PRIMARY KEY,
            name TEXT COLLATE NOCASE,
            alias TEXT COLLATE BINARY
         );
         CREATE INDEX idx_collated_users_name ON collated_users(name);
         INSERT INTO collated_users VALUES (1, 'Alice', 'Alice');",
    )
    .unwrap();

    let column_collation_plan = db
        .query("EXPLAIN QUERY PLAN SELECT name FROM collated_users WHERE name = 'alice';")
        .unwrap();
    assert_eq!(column_collation_plan[0][0], Value::from("SeqScan"));

    let column_collation_rows = db
        .query("SELECT name FROM collated_users WHERE name = 'alice';")
        .unwrap();
    assert_eq!(column_collation_rows, vec![vec![Value::from("Alice")]]);

    let binary_collation_rows = db
        .query("SELECT alias FROM collated_users WHERE alias = 'alice';")
        .unwrap();
    assert_eq!(binary_collation_rows, Vec::<Vec<Value>>::new());

    let coalesce_rows = db
        .query(
            "SELECT name
             FROM users
             WHERE COALESCE(nickname, name) = 'bobby';",
        )
        .unwrap();

    assert_eq!(coalesce_rows, vec![vec![Value::from("bob")]]);

    let bitwise_rows = db
        .query(
            "SELECT 1 + 2 << 1,
                    1 << 2 + 1,
                    8 >> 1 + 1,
                    5 & 3 + 1,
                    5 | 2 & 1,
                    ~5,
                    '5' & '3',
                    NULL & 3;",
        )
        .unwrap();
    assert_eq!(
        bitwise_rows,
        vec![vec![
            Value::Integer(6),
            Value::Integer(8),
            Value::Integer(2),
            Value::Integer(4),
            Value::Integer(1),
            Value::Integer(-6),
            Value::Integer(1),
            Value::Null,
        ]]
    );

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
             WHERE name LIKE 'ali%'
               AND COALESCE(nickname, name) NOT LIKE 'x%';",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1)]]);

    db.execute("PRAGMA case_sensitive_like = ON;").unwrap();
    assert_eq!(
        db.query("PRAGMA case_sensitive_like;").unwrap(),
        vec![vec![Value::Integer(1)]]
    );
    let sensitive_rows = db
        .query("SELECT id FROM users WHERE name LIKE 'ali%' ORDER BY id;")
        .unwrap();
    assert_eq!(sensitive_rows, Vec::<Vec<Value>>::new());

    db.execute("PRAGMA case_sensitive_like = OFF;").unwrap();
    assert_eq!(
        db.query("PRAGMA case_sensitive_like;").unwrap(),
        vec![vec![Value::Integer(0)]]
    );
    let insensitive_rows = db
        .query("SELECT id FROM users WHERE name LIKE 'ali%' ORDER BY id;")
        .unwrap();
    assert_eq!(insensitive_rows, vec![vec![Value::Integer(1)]]);

    db.execute("INSERT INTO users VALUES (4, 'Ægir', NULL);")
        .unwrap();

    let non_ascii_rows = db
        .query("SELECT id FROM users WHERE name LIKE 'æg%' ORDER BY id;")
        .unwrap();
    assert_eq!(non_ascii_rows, Vec::<Vec<Value>>::new());

    db.execute(
        "CREATE TABLE patterns (id INTEGER PRIMARY KEY, code TEXT);
         INSERT INTO patterns VALUES (1, 'a_');
         INSERT INTO patterns VALUES (2, 'ab');
         INSERT INTO patterns VALUES (3, 'a%');
         INSERT INTO patterns VALUES (4, 'a!');",
    )
    .unwrap();

    let escaped_rows = db
        .query(
            "SELECT id
             FROM patterns
             WHERE code LIKE 'a!_' ESCAPE '!'
                OR code LIKE 'a!%' ESCAPE '!'
                OR code LIKE 'a!!' ESCAPE '!'
             ORDER BY id ASC;",
        )
        .unwrap();
    assert_eq!(
        escaped_rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(3)],
            vec![Value::Integer(4)],
        ]
    );
}

#[test]
fn database_applies_case_sensitive_like_to_check_constraints() {
    let db = Database::memory();

    db.execute("CREATE TABLE names (name TEXT CHECK(name LIKE 'a%'));")
        .unwrap();
    db.execute("INSERT INTO names VALUES ('Alice');").unwrap();

    db.execute("PRAGMA case_sensitive_like = ON;").unwrap();
    let error = db
        .execute("INSERT INTO names VALUES ('Alice');")
        .unwrap_err();
    assert!(
        error.to_string().contains("check constraint"),
        "unexpected error: {error}"
    );
    db.execute("INSERT INTO names VALUES ('alice');").unwrap();

    assert_eq!(
        db.query("SELECT name FROM names ORDER BY name;").unwrap(),
        vec![vec![Value::from("Alice")], vec![Value::from("alice")]]
    );
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
fn database_selects_from_values_cte_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "WITH vals AS (VALUES (2, 'bob'), (1, 'alice'))
             SELECT column1, column2
             FROM vals
             ORDER BY column2 ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn database_selects_from_values_cte_with_column_names_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "WITH vals(c1, c2) AS (VALUES (2, 'bob'), (1, 'alice'))
             SELECT c1, c2
             FROM vals
             ORDER BY c2 ASC;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn database_supports_with_top_level_values_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query("WITH vals(x, y) AS (VALUES (1, 'a')) VALUES (2, 'b'), (3, 'c');")
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(2), Value::from("b")],
            vec![Value::Integer(3), Value::from("c")],
        ]
    );
}

#[test]
fn database_supports_with_top_level_values_multiple_ctes_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "WITH first_cte AS (VALUES (1)),
                  second_cte AS (VALUES (2))
             VALUES (4, 'picked');",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(4), Value::from("picked")]]);
}

#[test]
fn database_supports_scalar_subquery_projection_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');",
    )
    .unwrap();

    let rows = db
        .query("SELECT (SELECT name FROM users WHERE id = 1), (SELECT COUNT(*) FROM users);")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice"), Value::Integer(2)]]);
}

#[test]
fn database_scalar_subquery_uses_first_row_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');",
    )
    .unwrap();

    let rows = db
        .query("SELECT (SELECT name FROM users ORDER BY id);")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("alice")]]);
}

#[test]
fn database_supports_correlated_scalar_subquery_projection_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO orders VALUES (10, 1, 120);
         INSERT INTO orders VALUES (11, 2, 5);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT name,
                    (SELECT amount FROM orders o WHERE o.user_id = users.id)
             FROM users
             ORDER BY id;",
        )
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::from("alice"), Value::Integer(120)],
            vec![Value::from("bob"), Value::Integer(5)],
        ]
    );
}

#[test]
fn database_supports_scalar_subquery_in_top_level_values_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');",
    )
    .unwrap();

    let rows = db
        .query("VALUES ((SELECT name FROM users WHERE id = 2));")
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from("bob")]]);
}

#[test]
fn database_supports_scalar_subquery_in_with_values_like_sqlite() {
    let db = Database::memory();

    let rows = db
        .query(
            "WITH vals(x, y) AS (VALUES (2, 'bob'), (1, 'alice'))
             VALUES ((SELECT x FROM vals WHERE y = 'alice'), 'picked');",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(1), Value::from("picked")]]);
}

#[test]
fn database_supports_scalar_subquery_inside_scalar_expressions_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');
         INSERT INTO orders VALUES (10, 1, 120);
         INSERT INTO orders VALUES (11, 2, 5);",
    )
    .unwrap();

    let rows = db
        .query(
            "SELECT (SELECT COUNT(*) FROM users) + 2,
                    UPPER((SELECT name FROM users WHERE id = 1)),
                    name || ':' || (SELECT amount FROM orders o WHERE o.user_id = users.id)
             FROM users
             ORDER BY id;",
        )
        .unwrap();

    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(4),
                Value::from("ALICE"),
                Value::from("alice:120"),
            ],
            vec![
                Value::Integer(4),
                Value::from("ALICE"),
                Value::from("bob:5")
            ],
        ]
    );
}

#[test]
fn database_supports_scalar_subquery_inside_values_expression_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (1, 'alice');",
    )
    .unwrap();

    let rows = db
        .query("VALUES ((SELECT id FROM users) + 9, UPPER((SELECT name FROM users)));")
        .unwrap();

    assert_eq!(rows, vec![vec![Value::Integer(10), Value::from("ALICE")]]);
}

#[test]
fn database_supports_with_insert_select_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    db.execute(
        "WITH vals(id, name) AS (VALUES (2, 'bob'), (1, 'alice'))
         INSERT INTO users
         SELECT id, name FROM vals;",
    )
    .unwrap();

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn database_supports_with_insert_select_returning_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    let returned = db
        .query(
            "WITH vals(id, name) AS (VALUES (2, 'bob'), (1, 'alice'))
             INSERT INTO users
             SELECT id, name FROM vals
             RETURNING id, name, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![Value::Integer(2), Value::from("bob"), Value::Integer(2)],
            vec![Value::Integer(1), Value::from("alice"), Value::Integer(1)],
        ]
    );
}

#[test]
fn database_supports_with_insert_values_returning_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();

    let returned = db
        .query(
            "WITH ignored AS (VALUES (1))
             INSERT INTO users
             VALUES (2, 'bob'), (1, 'alice')
             RETURNING id, name, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![Value::Integer(2), Value::from("bob"), Value::Integer(2)],
            vec![Value::Integer(1), Value::from("alice"), Value::Integer(1)],
        ]
    );

    let rows = db.query("SELECT id, name FROM users ORDER BY id;").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice")],
            vec![Value::Integer(2), Value::from("bob")],
        ]
    );
}

#[test]
fn database_supports_with_insert_default_values_returning_like_sqlite() {
    let db = Database::memory();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT DEFAULT 'anon');")
        .unwrap();

    let returned = db
        .query(
            "WITH ignored AS (VALUES (1))
             INSERT INTO users DEFAULT VALUES
             RETURNING id, name, rowid;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![vec![
            Value::Integer(1),
            Value::from("anon"),
            Value::Integer(1)
        ]]
    );
}

#[test]
fn database_supports_with_update_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, active INTEGER);
         INSERT INTO users VALUES (1, 'alice', 1);
         INSERT INTO users VALUES (2, 'bob', 0);
         INSERT INTO users VALUES (3, 'carol', 1);",
    )
    .unwrap();

    db.execute(
        "WITH visible AS (
             SELECT id FROM users WHERE active = 1
         )
         UPDATE users
         SET name = UPPER(name)
         WHERE id IN (SELECT id FROM visible);",
    )
    .unwrap();

    let rows = db
        .query("SELECT id, name, active FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("ALICE"), Value::Integer(1)],
            vec![Value::Integer(2), Value::from("bob"), Value::Integer(0)],
            vec![Value::Integer(3), Value::from("CAROL"), Value::Integer(1)],
        ]
    );
}

#[test]
fn database_supports_with_update_returning_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, active INTEGER);
         INSERT INTO users VALUES (1, 'alice', 1);
         INSERT INTO users VALUES (2, 'bob', 0);
         INSERT INTO users VALUES (3, 'carol', 1);",
    )
    .unwrap();

    let returned = db
        .query(
            "WITH visible AS (
                 SELECT id FROM users WHERE active = 1
             )
             UPDATE users
             SET name = UPPER(name)
             WHERE id IN (SELECT id FROM visible)
             RETURNING id, name;",
        )
        .unwrap();
    assert_eq!(
        returned,
        vec![
            vec![Value::Integer(1), Value::from("ALICE")],
            vec![Value::Integer(3), Value::from("CAROL")],
        ]
    );
}

#[test]
fn database_supports_with_delete_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, active INTEGER);
         INSERT INTO users VALUES (1, 'alice', 1);
         INSERT INTO users VALUES (2, 'bob', 0);
         INSERT INTO users VALUES (3, 'carol', 1);",
    )
    .unwrap();

    db.execute(
        "WITH doomed AS (
             SELECT id FROM users WHERE active = 0
         )
         DELETE FROM users
         WHERE id IN (SELECT id FROM doomed);",
    )
    .unwrap();

    let rows = db
        .query("SELECT id, name, active FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::from("alice"), Value::Integer(1)],
            vec![Value::Integer(3), Value::from("carol"), Value::Integer(1)],
        ]
    );
}

#[test]
fn database_accepts_with_recursive_for_non_recursive_cte_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');",
    )
    .unwrap();

    let rows = db
        .query(
            "WITH RECURSIVE visible_users AS (
                 SELECT id, name
                 FROM users
                 WHERE id >= 2
             )
             SELECT name
             FROM visible_users;",
        )
        .unwrap();

    assert_eq!(rows, vec![vec![Value::from("bob")]]);
}

#[test]
fn database_accepts_cte_materialization_hints_like_sqlite() {
    let db = Database::memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES (1, 'alice');
         INSERT INTO users VALUES (2, 'bob');",
    )
    .unwrap();

    let materialized = db
        .query(
            "WITH visible AS MATERIALIZED (
                 SELECT id, name FROM users WHERE id = 1
             )
             SELECT name FROM visible;",
        )
        .unwrap();
    assert_eq!(materialized, vec![vec![Value::from("alice")]]);

    let not_materialized = db
        .query(
            "WITH visible AS NOT MATERIALIZED (
                 SELECT id, name FROM users WHERE id = 2
             )
             SELECT name FROM visible;",
        )
        .unwrap();
    assert_eq!(not_materialized, vec![vec![Value::from("bob")]]);
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
