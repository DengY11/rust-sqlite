# rustsql

`rustsql` is a small educational SQLite-style database written in Rust. It includes a SQL parser, logical planner, query executor, in-memory storage, file-backed storage, a REPL, indexes, transactions, joins, aggregates, and subqueries.

## Quick start

Run an in-memory REPL:

```bash
cargo run
```

Run a persistent file-backed REPL:

```bash
cargo run -- demo.db
```

Select a storage engine explicitly:

```bash
cargo run -- --engine v1 demo.db
cargo run -- --engine v2 demo-v2.db
cargo run -- --engine sqlite3 demo-sqlite3.db
```

`v1` is the default file-backed storage engine. `v2` is experimental and exposes the page-based B+Tree/Pager/WAL engine for testing. `sqlite3` is the SQLite-file-compatible engine under active development.

Example session:

```sql
rustsql> CREATE TABLE users (
...> id INTEGER PRIMARY KEY,
...> name TEXT NOT NULL,
...> active BOOLEAN
...> );
ok
rustsql> CREATE INDEX idx_users_name ON users (name);
ok
rustsql> INSERT INTO users VALUES (1, 'alice', true);
ok
rustsql> SELECT id,
...> name
...> FROM users
...> WHERE active = true;
id | name
--------
1 | alice
rustsql> .tables
users
rustsql> .schema
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, active BOOLEAN);
CREATE INDEX idx_users_name ON users (name);
rustsql> .quit
```

## REPL commands

```text
.help      Show help
.tables    List tables
.schema    Show table schemas and indexes
.exit      Exit
.quit      Exit
```

## Supported SQL subset

DDL and writes:

- `CREATE TABLE`
- `CREATE INDEX`
- `DROP TABLE`
- `DROP INDEX`
- `ALTER TABLE ... ADD COLUMN ...`
- `ALTER TABLE ... RENAME TO ...`
- `ALTER TABLE ... RENAME COLUMN ... TO ...`
- `INSERT INTO ... VALUES ...`
- `INSERT INTO ... (columns...) VALUES ...`
- `UPDATE ... SET ... WHERE ...`
- `DELETE FROM ... WHERE ...`
- `BEGIN`, `COMMIT`, `ROLLBACK`
- literal `DEFAULT` values
- `CHECK` constraints
- basic single-column foreign keys with immediate enforcement

Queries:

- `SELECT *` and explicit projections
- scalar projection expressions with integer arithmetic (`+`, `-`, `*`, `/`) and text concatenation (`||`)
- column aliases and table aliases
- `WHERE` with `=`, `!=`, `>`, `>=`, `<`, `<=`
- `LIKE`, `NOT LIKE`, `BETWEEN`, and `NOT BETWEEN`
- boolean expressions with `AND`, `OR`, `NOT`, parentheses
- `IS NULL` / `IS NOT NULL`
- `ORDER BY` by column, alias, or select-list position, including `NULLS FIRST` / `NULLS LAST`, and `LIMIT`
- `GROUP BY`
- aggregates: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, including `DISTINCT` column arguments
- `HAVING`
- `INNER JOIN` and `LEFT JOIN`
- `DISTINCT`
- `IN (subquery)`, `EXISTS (subquery)`, `NOT EXISTS (subquery)`, and scalar comparison subqueries

## Storage engines

`rustsql` currently has three storage paths:

- `MemoryStorage`: default for `cargo run` without a database path.
- `storage::v1::FileStorage`: current default file-backed engine used by `Database::open`, `cargo run -- demo.db`, and `cargo run -- --engine v1 demo.db`.
- `storage::v2::FileStorage`: experimental page-based engine with B+Tree-style table/index structures, a pager, WAL-backed commits, and secondary indexes. It is available through `cargo run -- --engine v2 demo-v2.db`.
- `storage::sqlite3::FileStorage`: SQLite 3 file-format-compatible engine that can read real SQLite databases and now supports a growing write subset through `cargo run -- --engine sqlite3 demo-sqlite3.db`.

The v2 engine has B+Tree-style leaf/internal pages and leaf-chain scans, but it is not yet a full production database storage engine. In particular, it does not yet include a complete buffer pool manager with clean page caching, frame eviction, pin/unpin, or latch management.

## Test and quality checks

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

## Current limitations

- The v2 storage engine is experimental and not the default file-backed engine, but it can be selected with `--engine v2`.
- No complete buffer pool manager yet.
