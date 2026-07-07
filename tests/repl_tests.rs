use std::io::Cursor;
use std::process::Command;

use rustsql::db::Database;
use rustsql::repl::{render_rows, run_with_io};
use tempfile::tempdir;

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
fn repl_schema_renders_typeless_columns_without_inventing_type_names() {
    let db = Database::memory();
    let input = Cursor::new(
        "CREATE TABLE users (\n\
             id PRIMARY KEY,\n\
             name,\n\
             created_at DEFAULT CURRENT_TIMESTAMP\n\
         );\n\
         .schema\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE users"));
    assert!(rendered.contains("id PRIMARY KEY"));
    assert!(rendered.contains("name"));
    assert!(rendered.contains("created_at DEFAULT CURRENT_TIMESTAMP"));
    assert!(!rendered.contains("name ANY"));
}

#[test]
fn repl_accepts_multiline_sql_until_statement_terminator() {
    let db = Database::memory();
    let input = Cursor::new(
        "CREATE TABLE users (\n\
             id INTEGER PRIMARY KEY,\n\
             name TEXT NOT NULL\n\
         );\n\
         INSERT INTO users VALUES (1, 'alice');\n\
         SELECT id,\n\
                name\n\
         FROM users;\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("...> "));
    assert_eq!(rendered.matches("ok").count(), 2);
    assert!(rendered.contains("id | name"));
    assert!(rendered.contains("1 | alice"));
    assert!(!rendered.contains("sql error:"));
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
fn repl_prints_help_tables_and_schema_meta_commands() {
    let db = Database::memory();
    let input = Cursor::new(
        ".help\n\
         CREATE TABLE users (id INTEGER PRIMARY KEY, age INTEGER DEFAULT 0 CHECK (age >= 0), name TEXT NOT NULL);\n\
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));\n\
         CREATE INDEX idx_users_name ON users (name);\n\
         .tables\n\
         .schema\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains(".help      Show this help message"));
    assert!(rendered.contains(".tables    List tables"));
    assert!(rendered.contains("users"));
    assert!(rendered.contains("CREATE TABLE users"));
    assert!(rendered.contains("id INTEGER PRIMARY KEY"));
    assert!(rendered.contains("age INTEGER DEFAULT 0 CHECK (age >= 0)"));
    assert!(rendered.contains("name TEXT NOT NULL"));
    assert!(rendered.contains("user_id INTEGER REFERENCES users(id)"));
    assert!(rendered.contains("CREATE INDEX idx_users_name ON users (name);"));
}

#[test]
fn repl_schema_renders_column_collations() {
    let db = Database::memory();
    let input = Cursor::new(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE, alias TEXT COLLATE BINARY);\n\
         .schema\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE users"));
    assert!(rendered.contains("name TEXT COLLATE NOCASE"));
    assert!(rendered.contains("alias TEXT COLLATE BINARY"));
}

#[test]
fn repl_schema_preserves_index_column_decorations_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("index-column-decorations-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE);
             CREATE INDEX idx_users_name_nocase ON users(name COLLATE NOCASE DESC);",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(
        rendered
            .contains("CREATE INDEX idx_users_name_nocase ON users (name COLLATE NOCASE DESC);")
    );
}

#[test]
fn repl_schema_preserves_named_check_constraints_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-check-constraints-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                age INTEGER CONSTRAINT age_nonneg CHECK (age >= 0),
                score INTEGER,
                CONSTRAINT score_cap CHECK (score <= 100)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CONSTRAINT age_nonneg CHECK (age >= 0)"));
    assert!(rendered.contains("CONSTRAINT score_cap CHECK (score <= 100)"));
}

#[test]
fn repl_schema_preserves_named_column_primary_key_and_unique_constraints_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-column-pk-unique-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER CONSTRAINT pk PRIMARY KEY,
                email TEXT CONSTRAINT uq UNIQUE
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("id INTEGER CONSTRAINT pk PRIMARY KEY"));
    assert!(rendered.contains("email TEXT CONSTRAINT uq UNIQUE"));
}

#[test]
fn repl_schema_preserves_named_not_null_constraints_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-not-null-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                name TEXT CONSTRAINT nn NOT NULL
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("name TEXT CONSTRAINT nn NOT NULL"));
}

#[test]
fn repl_schema_preserves_on_conflict_clauses_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("on-conflict-preserved-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY ON CONFLICT REPLACE,
                email TEXT UNIQUE ON CONFLICT IGNORE,
                name TEXT CONSTRAINT nn NOT NULL ON CONFLICT FAIL,
                nickname TEXT,
                CONSTRAINT uq UNIQUE(name, nickname) ON CONFLICT ABORT
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("id INTEGER PRIMARY KEY ON CONFLICT REPLACE"));
    assert!(rendered.contains("email TEXT UNIQUE ON CONFLICT IGNORE"));
    assert!(rendered.contains("name TEXT CONSTRAINT nn NOT NULL ON CONFLICT FAIL"));
    assert!(rendered.contains("CONSTRAINT uq UNIQUE(name, nickname) ON CONFLICT ABORT"));
}

#[test]
fn repl_schema_renders_default_then_collate_columns() {
    let db = Database::memory();
    let input = Cursor::new(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, nickname TEXT DEFAULT ('guest') COLLATE NOCASE);\n\
         .schema\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE users"));
    assert!(rendered.contains("nickname TEXT COLLATE NOCASE DEFAULT 'guest'"));
}

#[test]
fn repl_schema_preserves_parent_primary_key_foreign_key_shorthand_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("foreign-key-parent-primary-key-shorthand-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (id INTEGER PRIMARY KEY);
             CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users);",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE orders"));
    assert!(rendered.contains("user_id INTEGER REFERENCES users"));
    assert!(!rendered.contains("user_id INTEGER REFERENCES users(id)"));
}

#[test]
fn repl_schema_preserves_named_foreign_keys_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-foreign-keys-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER CONSTRAINT fk_user REFERENCES users(id),
                author_id INTEGER,
                CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("user_id INTEGER CONSTRAINT fk_user REFERENCES users(id)"));
    assert!(rendered.contains("CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id)"));
}

#[test]
fn repl_schema_preserves_foreign_key_actions_and_deferrable_clauses_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("foreign-key-actions-deferrable-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER REFERENCES users(id) ON DELETE CASCADE ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED,
                author_id INTEGER,
                CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE SET NULL ON UPDATE NO ACTION NOT DEFERRABLE INITIALLY IMMEDIATE
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains(
        "user_id INTEGER REFERENCES users(id) ON DELETE CASCADE ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED"
    ));
    assert!(rendered.contains(
        "CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE SET NULL ON UPDATE NO ACTION NOT DEFERRABLE INITIALLY IMMEDIATE"
    ));
}

#[test]
fn repl_schema_preserves_foreign_key_match_clauses_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("foreign-key-match-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                user_id INTEGER REFERENCES users(id) MATCH FULL,
                author_id INTEGER,
                CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id) MATCH SIMPLE
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("user_id INTEGER REFERENCES users(id) MATCH FULL"));
    assert!(rendered.contains(
        "CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES users(id) MATCH SIMPLE"
    ));
}

#[test]
fn repl_schema_preserves_named_unique_constraints_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-unique-constraints-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                name TEXT,
                nickname TEXT,
                CONSTRAINT uq_user_names UNIQUE(name, nickname)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CONSTRAINT uq_user_names UNIQUE(name, nickname)"));
}

#[test]
fn repl_schema_preserves_decorated_table_constraint_columns_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("decorated-table-constraints-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                name TEXT,
                email TEXT,
                CONSTRAINT uq UNIQUE(name COLLATE NOCASE DESC, email ASC),
                PRIMARY KEY(name COLLATE BINARY ASC)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CONSTRAINT uq UNIQUE(name COLLATE NOCASE DESC, email ASC)"));
    assert!(rendered.contains("PRIMARY KEY(name COLLATE BINARY ASC)"));
}

#[test]
fn repl_schema_preserves_primary_key_on_conflict_clause_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("primary-key-on-conflict-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE users (
                name TEXT,
                email TEXT,
                PRIMARY KEY(name, email) ON CONFLICT FAIL
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("PRIMARY KEY(name, email) ON CONFLICT FAIL"));
}

#[test]
fn repl_schema_matches_sqlite_catalog_behavior_for_if_not_exists_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("if-not-exists-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT);
             CREATE INDEX IF NOT EXISTS idx_users_name ON users(name);",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);"));
    assert!(rendered.contains("CREATE INDEX idx_users_name ON users (name);"));
    assert!(!rendered.contains("IF NOT EXISTS"));
}

#[test]
fn repl_schema_renders_autoincrement_primary_keys() {
    let db = Database::memory();
    let input = Cursor::new(
        "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);\n\
         .schema\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE users"));
    assert!(rendered.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"));
}

#[test]
fn repl_schema_renders_desc_primary_keys() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("desc-primary-key-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg("CREATE TABLE users (id INTEGER PRIMARY KEY DESC, name TEXT);")
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE users"));
    assert!(rendered.contains("id INTEGER PRIMARY KEY DESC"));
}

#[test]
fn repl_schema_renders_strict_tables() {
    let db = Database::memory();
    let input = Cursor::new(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT) STRICT;\n\
         .schema\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE users"));
    assert!(rendered.contains("STRICT"));
}

#[test]
fn repl_schema_renders_current_date_and_time_defaults() {
    let db = Database::memory();
    let input = Cursor::new(
        "CREATE TABLE users (\n\
         id INTEGER PRIMARY KEY,\n\
         created_date TEXT DEFAULT CURRENT_DATE,\n\
         created_time TEXT DEFAULT CURRENT_TIME\n\
         );\n\
         .schema\n\
         .quit\n",
    );
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("created_date TEXT DEFAULT CURRENT_DATE"));
    assert!(rendered.contains("created_time TEXT DEFAULT CURRENT_TIME"));
}

#[test]
fn repl_schema_lists_without_rowid_tables_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("without-rowid-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                PRIMARY KEY(user_id, group_id)
            ) WITHOUT ROWID;",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("WITHOUT ROWID"));
}

#[test]
fn repl_schema_preserves_named_composite_primary_key_constraints_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named-composite-primary-key-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                role TEXT,
                CONSTRAINT pk_memberships PRIMARY KEY(user_id, group_id)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CONSTRAINT pk_memberships PRIMARY KEY(user_id, group_id)"));
}

#[test]
fn repl_schema_renders_generated_columns_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("generated-column-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER GENERATED ALWAYS AS (base + 1) STORED
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE metrics"));
    assert!(rendered.contains("plus_one INTEGER GENERATED ALWAYS AS (base + 1) STORED"));
}

#[test]
fn repl_schema_renders_virtual_generated_columns_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("generated-column-virtual-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER GENERATED ALWAYS AS (base + 1) VIRTUAL
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE metrics"));
    assert!(rendered.contains("plus_one INTEGER GENERATED ALWAYS AS (base + 1) VIRTUAL"));
}

#[test]
fn repl_schema_preserves_implicit_virtual_generated_columns_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("generated-column-implicit-virtual-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER GENERATED ALWAYS AS (base + 1)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE metrics"));
    assert!(rendered.contains("plus_one INTEGER GENERATED ALWAYS AS (base + 1)"));
    assert!(!rendered.contains("plus_one INTEGER GENERATED ALWAYS AS (base + 1) VIRTUAL"));
}

#[test]
fn repl_schema_preserves_as_generated_columns_from_sqlite3_storage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("generated-column-as-repl.db");

    let status = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "CREATE TABLE metrics (
                base INTEGER,
                plus_one INTEGER AS (base + 1)
            );",
        )
        .status()
        .unwrap();
    assert!(status.success());

    let db = Database::with_storage(rustsql::storage::sqlite3::FileStorage::open(&path).unwrap());
    let input = Cursor::new(".schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("CREATE TABLE metrics"));
    assert!(rendered.contains("plus_one INTEGER AS (base + 1)"));
    assert!(!rendered.contains("plus_one INTEGER GENERATED ALWAYS AS (base + 1)"));
}

#[test]
fn repl_reports_no_tables_for_empty_database() {
    let db = Database::memory();
    let input = Cursor::new(".tables\n.schema\n.quit\n");
    let mut output = Vec::new();

    run_with_io(&db, input, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("(no tables)"));
    assert!(rendered.contains("(no schema)"));
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
