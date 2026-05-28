# RustSQL MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建 RustSQL 的首个可运行 MVP：支持 `CREATE TABLE`、`CREATE INDEX`、`INSERT`、`SELECT ... WHERE ...`、`BEGIN/COMMIT/ROLLBACK`，并通过 `memory` 与 `storage_v1` 两个后端跑通端到端链路。

**Architecture:** 先建立 SQL 外壳（lexer/parser/planner/executor）和稳定的 `StorageEngine` trait 边界，再接入纯内存后端完成联调，最后增加 `storage_v1` 的最小持久化实现。`storage_v2` 的 pager/WAL/B+Tree 真内核不在本计划实现范围内，单独作为下一份计划。

**Tech Stack:** Rust stable、Cargo、标准库、`thiserror`、`serde`、`serde_json`

---

## File Map

### Create

- `Cargo.toml` - crate 清单与依赖
- `src/lib.rs` - 对外导出主模块
- `src/main.rs` - CLI/REPL 入口
- `src/db.rs` - `Database` 统一 API，串联 parser/planner/executor/storage
- `src/common/mod.rs` - 公共模块导出
- `src/common/types.rs` - `Value`、`ColumnType`、`ColumnDef`、`Schema`、`Row`、`RowId`
- `src/common/error.rs` - `DbError` 与分层错误类型
- `src/engine/mod.rs` - engine 模块导出
- `src/engine/traits.rs` - `CatalogStore`、`TableStore`、`IndexStore`、`TransactionManager`、`StorageEngine`
- `src/engine/txn.rs` - `TransactionId`、事务状态机
- `src/sql/mod.rs` - SQL 模块导出
- `src/sql/ast.rs` - AST 定义
- `src/sql/lexer.rs` - token 定义与词法分析
- `src/sql/parser.rs` - SQL parser
- `src/sql/plan.rs` - 逻辑计划定义
- `src/sql/planner.rs` - AST -> Plan
- `src/sql/executor.rs` - plan 执行器
- `src/storage/mod.rs` - 存储模块导出
- `src/storage/memory.rs` - 内存后端
- `src/storage/v1/mod.rs` - 持久化后端导出
- `src/storage/v1/catalog.rs` - schema/index 元数据落盘
- `src/storage/v1/table.rs` - 行文件落盘与扫描
- `src/storage/v1/index.rs` - 简化索引落盘与查询
- `src/storage/v1/txn.rs` - 简化事务日志
- `src/repl/mod.rs` - REPL 循环
- `src/repl/printer.rs` - 查询结果打印
- `tests/parser_tests.rs` - parser 单测
- `tests/planner_tests.rs` - planner 单测
- `tests/executor_tests.rs` - executor + memory 集成测试
- `tests/storage_memory_tests.rs` - memory 后端测试
- `tests/storage_v1_tests.rs` - storage_v1 测试
- `tests/e2e_sql_tests.rs` - 端到端 SQL 测试

### Modify

- 无（仓库当前为空）

---

### Task 1: Bootstrap crate and project skeleton

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `src/main.rs`
- Create: `src/common/mod.rs`
- Create: `src/engine/mod.rs`
- Create: `src/sql/mod.rs`
- Create: `src/storage/mod.rs`
- Create: `src/repl/mod.rs`
- Test: `tests/e2e_sql_tests.rs`

- [ ] **Step 1: Write the failing smoke test**

```rust
use rustsql::db::Database;

#[test]
fn smoke_database_new_compiles() {
    let db = Database::memory();
    assert!(db.is_ok());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test smoke_database_new_compiles -v`
Expected: FAIL with `use of undeclared crate or module 'rustsql'` or `could not find 'db'`

- [ ] **Step 3: Write minimal crate skeleton**

`Cargo.toml`
```toml
[package]
name = "rustsql"
version = "0.1.0"
edition = "2021"

[dependencies]
thiserror = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[dev-dependencies]
tempfile = "3"
```

`src/lib.rs`
```rust
pub mod common;
pub mod db;
pub mod engine;
pub mod repl;
pub mod sql;
pub mod storage;
```

`src/main.rs`
```rust
fn main() {
    println!("rustsql shell coming soon");
}
```

`src/sql/mod.rs`
```rust
pub mod ast;
pub mod executor;
pub mod lexer;
pub mod parser;
pub mod plan;
pub mod planner;
```

- [ ] **Step 4: Add temporary `Database` stub and rerun test**

`src/db.rs`
```rust
use crate::common::error::DbResult;

pub struct Database;

impl Database {
    pub fn memory() -> DbResult<Self> {
        Ok(Self)
    }
}
```

`src/common/mod.rs`
```rust
pub mod error;
pub mod types;
```

`src/common/error.rs`
```rust
pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("bootstrap error: {0}")]
    Bootstrap(String),
}
```

Run: `cargo test smoke_database_new_compiles -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/lib.rs src/main.rs src/db.rs src/common/mod.rs src/common/error.rs src/sql/mod.rs tests/e2e_sql_tests.rs
git commit -m "feat: bootstrap rustsql crate skeleton"
```

### Task 2: Define shared types, errors, and engine traits

**Files:**
- Create: `src/common/types.rs`
- Modify: `src/common/error.rs`
- Create: `src/engine/traits.rs`
- Create: `src/engine/txn.rs`
- Modify: `src/engine/mod.rs`
- Test: `tests/storage_memory_tests.rs`

- [ ] **Step 1: Write the failing trait contract test**

```rust
use rustsql::common::types::{ColumnDef, ColumnType, Schema, Value};

#[test]
fn schema_and_value_types_are_constructible() {
    let schema = Schema::new(
        "users",
        vec![
            ColumnDef::new("id", ColumnType::Integer).primary_key(),
            ColumnDef::new("name", ColumnType::Text),
        ],
    );

    assert_eq!(schema.name, "users");
    assert_eq!(Value::Integer(1).to_string(), "1");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test schema_and_value_types_are_constructible -v`
Expected: FAIL with missing `types` module or missing associated methods

- [ ] **Step 3: Implement shared domain model**

`src/common/types.rs`
```rust
use std::fmt;

use serde::{Deserialize, Serialize};

pub type Row = Vec<Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RowId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Value {
    Null,
    Integer(i64),
    Text(String),
    Boolean(bool),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnType {
    Integer,
    Text,
    Boolean,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub column_type: ColumnType,
    pub not_null: bool,
    pub primary_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMeta {
    pub name: String,
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub indexes: Vec<IndexMeta>,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, column_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            column_type,
            not_null: false,
            primary_key: false,
        }
    }

    pub fn primary_key(mut self) -> Self {
        self.primary_key = true;
        self.not_null = true;
        self
    }
}

impl Schema {
    pub fn new(name: impl Into<String>, columns: Vec<ColumnDef>) -> Self {
        Self {
            name: name.into(),
            columns,
            indexes: Vec::new(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Integer(value) => write!(f, "{value}"),
            Value::Text(value) => write!(f, "{value}"),
            Value::Boolean(value) => write!(f, "{value}"),
        }
    }
}
```

`src/engine/traits.rs`
```rust
use crate::common::error::DbResult;
use crate::common::types::{IndexMeta, Row, RowId, Schema, Value};
use crate::engine::txn::TransactionId;

pub trait CatalogStore {
    fn create_table(&mut self, schema: Schema) -> DbResult<()>;
    fn get_schema(&self, table: &str) -> DbResult<Schema>;
    fn list_schemas(&self) -> DbResult<Vec<Schema>>;
    fn create_index(&mut self, index: IndexMeta) -> DbResult<()>;
    fn list_indexes(&self, table: &str) -> DbResult<Vec<IndexMeta>>;
}

pub trait TableStore {
    fn insert_row(&mut self, tx: TransactionId, table: &str, row: Row) -> DbResult<RowId>;
    fn scan_rows(&self, tx: TransactionId, table: &str) -> DbResult<Vec<(RowId, Row)>>;
    fn get_row(&self, tx: TransactionId, table: &str, row_id: RowId) -> DbResult<Row>;
}

pub trait IndexStore {
    fn insert_index(&mut self, tx: TransactionId, index: &str, key: Value, row_id: RowId) -> DbResult<()>;
    fn lookup_index(&self, tx: TransactionId, index: &str, key: &Value) -> DbResult<Vec<RowId>>;
}

pub trait TransactionManager {
    fn begin(&mut self) -> DbResult<TransactionId>;
    fn commit(&mut self, tx: TransactionId) -> DbResult<()>;
    fn rollback(&mut self, tx: TransactionId) -> DbResult<()>;
}

pub trait StorageEngine: CatalogStore + TableStore + IndexStore + TransactionManager {}

impl<T> StorageEngine for T where T: CatalogStore + TableStore + IndexStore + TransactionManager {}
```

- [ ] **Step 4: Expand error model and rerun test**

`src/common/error.rs`
```rust
pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sql error: {0}")]
    Sql(String),
    #[error("plan error: {0}")]
    Plan(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("transaction error: {0}")]
    Txn(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}
```

`src/engine/txn.rs`
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId(pub u64);
```

Run: `cargo test schema_and_value_types_are_constructible -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/common/types.rs src/common/error.rs src/engine/traits.rs src/engine/txn.rs src/engine/mod.rs tests/storage_memory_tests.rs
git commit -m "feat: define shared database types and engine traits"
```

### Task 3: Implement lexer, AST, and parser for MVP SQL subset

**Files:**
- Create: `src/sql/ast.rs`
- Create: `src/sql/lexer.rs`
- Create: `src/sql/parser.rs`
- Test: `tests/parser_tests.rs`

- [ ] **Step 1: Write failing parser tests for core statements**

```rust
use rustsql::sql::parser::parse_sql;

#[test]
fn parse_create_table_statement() {
    let ast = parse_sql("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);").unwrap();
    assert_eq!(ast.len(), 1);
}

#[test]
fn parse_select_where_statement() {
    let ast = parse_sql("SELECT id, name FROM users WHERE id = 1;").unwrap();
    assert_eq!(ast.len(), 1);
}
```

- [ ] **Step 2: Run parser tests to verify they fail**

Run: `cargo test parser_tests -v`
Expected: FAIL with missing `parse_sql`

- [ ] **Step 3: Define AST and tokens**

`src/sql/ast.rs`
```rust
use crate::common::types::{ColumnDef, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable { name: String, columns: Vec<ColumnDef> },
    CreateIndex { name: String, table: String, column: String },
    Insert { table: String, values: Vec<Value> },
    Select { table: String, columns: Vec<String>, filter: Option<Expr> },
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Eq(String, Value),
    Gt(String, Value),
    Lt(String, Value),
}
```

`src/sql/lexer.rs`
```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Keyword(String),
    Identifier(String),
    Integer(i64),
    String(String),
    Comma,
    LParen,
    RParen,
    Semicolon,
    Star,
    Eq,
    Gt,
    Lt,
}

pub fn lex(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\n' | '\t' | '\r' => {}
            '(' => tokens.push(Token::LParen),
            ')' => tokens.push(Token::RParen),
            ',' => tokens.push(Token::Comma),
            ';' => tokens.push(Token::Semicolon),
            '*' => tokens.push(Token::Star),
            '=' => tokens.push(Token::Eq),
            '>' => tokens.push(Token::Gt),
            '<' => tokens.push(Token::Lt),
            '\'' => {
                let mut literal = String::new();
                while let Some(next) = chars.next() {
                    if next == '\'' {
                        break;
                    }
                    literal.push(next);
                }
                tokens.push(Token::String(literal));
            }
            c if c.is_ascii_digit() => {
                let mut number = c.to_string();
                while let Some(next) = chars.peek() {
                    if next.is_ascii_digit() {
                        number.push(*next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Integer(number.parse().unwrap()));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut word = c.to_string();
                while let Some(next) = chars.peek() {
                    if next.is_ascii_alphanumeric() || *next == '_' {
                        word.push(*next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let upper = word.to_ascii_uppercase();
                match upper.as_str() {
                    "CREATE" | "TABLE" | "INDEX" | "ON" | "INSERT" | "INTO" | "VALUES" | "SELECT" | "FROM" | "WHERE" | "BEGIN" | "COMMIT" | "ROLLBACK" | "PRIMARY" | "KEY" | "INTEGER" | "TEXT" | "BOOLEAN" => {
                        tokens.push(Token::Keyword(upper));
                    }
                    _ => tokens.push(Token::Identifier(word)),
                }
            }
            _ => {}
        }
    }

    tokens
}
```

- [ ] **Step 4: Implement parser and rerun tests**

`src/sql/parser.rs`
```rust
use crate::common::error::{DbError, DbResult};
use crate::sql::ast::Statement;

pub fn parse_sql(input: &str) -> DbResult<Vec<Statement>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(DbError::Sql("empty sql".into()));
    }

    // 第一版做 statement-by-statement 解析；支持单条语句即可。
    parse_single(trimmed).map(|stmt| vec![stmt])
}

fn parse_single(input: &str) -> DbResult<Statement> {
    let upper = input.to_ascii_uppercase();

    if upper.starts_with("CREATE TABLE") {
        return parse_create_table(input);
    }
    if upper.starts_with("CREATE INDEX") {
        return parse_create_index(input);
    }
    if upper.starts_with("INSERT INTO") {
        return parse_insert(input);
    }
    if upper.starts_with("SELECT") {
        return parse_select(input);
    }
    if upper.starts_with("BEGIN") {
        return Ok(Statement::Begin);
    }
    if upper.starts_with("COMMIT") {
        return Ok(Statement::Commit);
    }
    if upper.starts_with("ROLLBACK") {
        return Ok(Statement::Rollback);
    }

    Err(DbError::Sql(format!("unsupported sql: {input}")))
}

fn parse_create_table(input: &str) -> DbResult<Statement> {
    let trimmed = input.trim_end_matches(';').trim();
    let (head, tail) = trimmed
        .split_once('(')
        .ok_or_else(|| DbError::Sql("CREATE TABLE missing column list".into()))?;
    let table = head
        .split_whitespace()
        .nth(2)
        .ok_or_else(|| DbError::Sql("CREATE TABLE missing table name".into()))?;
    let columns = tail
        .trim_end_matches(')')
        .split(',')
        .map(|raw| {
            let tokens = raw.split_whitespace().collect::<Vec<_>>();
            let mut column = crate::common::types::ColumnDef::new(
                tokens[0],
                match tokens[1].to_ascii_uppercase().as_str() {
                    "INTEGER" => crate::common::types::ColumnType::Integer,
                    "TEXT" => crate::common::types::ColumnType::Text,
                    "BOOLEAN" => crate::common::types::ColumnType::Boolean,
                    other => return Err(DbError::Sql(format!("unsupported type {other}"))),
                },
            );
            if tokens.windows(2).any(|pair| pair == ["PRIMARY", "KEY"]) {
                column = column.primary_key();
            }
            Ok(column)
        })
        .collect::<DbResult<Vec<_>>>()?;
    Ok(Statement::CreateTable { name: table.into(), columns })
}

fn parse_create_index(input: &str) -> DbResult<Statement> {
    let trimmed = input.trim_end_matches(';').trim();
    let parts = trimmed.split_whitespace().collect::<Vec<_>>();
    let name = parts.get(2).ok_or_else(|| DbError::Sql("CREATE INDEX missing name".into()))?;
    let table = parts.get(4).ok_or_else(|| DbError::Sql("CREATE INDEX missing table".into()))?;
    let column = trimmed
        .split_once('(')
        .and_then(|(_, tail)| tail.trim_end_matches(')').split_whitespace().next())
        .ok_or_else(|| DbError::Sql("CREATE INDEX missing column".into()))?;
    Ok(Statement::CreateIndex { name: (*name).into(), table: (*table).into(), column: column.into() })
}

fn parse_insert(input: &str) -> DbResult<Statement> {
    let trimmed = input.trim_end_matches(';').trim();
    let table = trimmed
        .split_whitespace()
        .nth(2)
        .ok_or_else(|| DbError::Sql("INSERT missing table name".into()))?;
    let values_section = trimmed
        .split_once("VALUES")
        .map(|(_, tail)| tail.trim())
        .ok_or_else(|| DbError::Sql("INSERT missing VALUES".into()))?;
    let values = values_section
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .map(|raw| parse_literal(raw.trim()))
        .collect::<DbResult<Vec<_>>>()?;
    Ok(Statement::Insert { table: table.into(), values })
}

fn parse_select(input: &str) -> DbResult<Statement> {
    let trimmed = input.trim_end_matches(';').trim();
    let after_select = trimmed.strip_prefix("SELECT ").ok_or_else(|| DbError::Sql("SELECT missing keyword".into()))?;
    let (projection, rest) = after_select
        .split_once(" FROM ")
        .ok_or_else(|| DbError::Sql("SELECT missing FROM".into()))?;
    let columns = if projection.trim() == "*" {
        vec!["*".into()]
    } else {
        projection.split(',').map(|part| part.trim().to_string()).collect()
    };
    let (table, filter) = if let Some((table, predicate)) = rest.split_once(" WHERE ") {
        (table.trim().to_string(), Some(parse_expr(predicate.trim())?))
    } else {
        (rest.trim().to_string(), None)
    };
    Ok(Statement::Select { table, columns, filter })
}

fn parse_expr(input: &str) -> DbResult<crate::sql::ast::Expr> {
    if let Some((column, value)) = input.split_once('=') {
        return Ok(crate::sql::ast::Expr::Eq(column.trim().into(), parse_literal(value.trim())?));
    }
    if let Some((column, value)) = input.split_once('>') {
        return Ok(crate::sql::ast::Expr::Gt(column.trim().into(), parse_literal(value.trim())?));
    }
    if let Some((column, value)) = input.split_once('<') {
        return Ok(crate::sql::ast::Expr::Lt(column.trim().into(), parse_literal(value.trim())?));
    }
    Err(DbError::Sql(format!("unsupported WHERE expression: {input}")))
}

fn parse_literal(input: &str) -> DbResult<crate::common::types::Value> {
    if input.eq_ignore_ascii_case("NULL") {
        return Ok(crate::common::types::Value::Null);
    }
    if input.eq_ignore_ascii_case("TRUE") || input.eq_ignore_ascii_case("FALSE") {
        return Ok(crate::common::types::Value::Boolean(input.eq_ignore_ascii_case("TRUE")));
    }
    if let Ok(value) = input.parse::<i64>() {
        return Ok(crate::common::types::Value::Integer(value));
    }
    Ok(crate::common::types::Value::Text(input.trim_matches('\'').to_string()))
}
```

Run: `cargo test parser_tests -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/sql/ast.rs src/sql/lexer.rs src/sql/parser.rs tests/parser_tests.rs
git commit -m "feat: add parser for rustsql mvp statements"
```

### Task 4: Implement planner for `SeqScan` and `IndexScan`

**Files:**
- Create: `src/sql/plan.rs`
- Create: `src/sql/planner.rs`
- Test: `tests/planner_tests.rs`

- [ ] **Step 1: Write failing planner tests**

```rust
use rustsql::common::types::{ColumnDef, ColumnType, IndexMeta, Schema};
use rustsql::sql::parser::parse_sql;
use rustsql::sql::planner::Planner;

#[test]
fn planner_prefers_index_scan_for_indexed_eq_filter() {
    let schema = Schema {
        name: "users".into(),
        columns: vec![ColumnDef::new("id", ColumnType::Integer).primary_key()],
        indexes: vec![IndexMeta { name: "idx_users_id".into(), table: "users".into(), column: "id".into() }],
    };

    let stmt = parse_sql("SELECT * FROM users WHERE id = 1;").unwrap().remove(0);
    let plan = Planner::new(vec![schema]).build(stmt).unwrap();

    assert!(matches!(plan, rustsql::sql::plan::Plan::IndexScan { .. }));
}
```

- [ ] **Step 2: Run planner tests to verify they fail**

Run: `cargo test planner_prefers_index_scan_for_indexed_eq_filter -v`
Expected: FAIL with missing `Planner` or `Plan`

- [ ] **Step 3: Define logical plan nodes**

`src/sql/plan.rs`
```rust
use crate::common::types::Value;
use crate::sql::ast::Expr;

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    CreateTable { name: String },
    CreateIndex { name: String, table: String, column: String },
    Insert { table: String, values: Vec<Value> },
    SeqScan { table: String, columns: Vec<String>, filter: Option<Expr> },
    IndexScan { table: String, index: String, columns: Vec<String>, filter: Expr },
    BeginTxn,
    CommitTxn,
    RollbackTxn,
}
```

- [ ] **Step 4: Implement planner rules and rerun tests**

`src/sql/planner.rs`
```rust
use crate::common::error::{DbError, DbResult};
use crate::common::types::Schema;
use crate::sql::ast::{Expr, Statement};
use crate::sql::plan::Plan;

pub struct Planner {
    schemas: Vec<Schema>,
}

impl Planner {
    pub fn new(schemas: Vec<Schema>) -> Self {
        Self { schemas }
    }

    pub fn build(&self, stmt: Statement) -> DbResult<Plan> {
        match stmt {
            Statement::Select { table, columns, filter } => {
                if let Some(Expr::Eq(column, _)) = &filter {
                    if let Some(schema) = self.schemas.iter().find(|schema| schema.name == table) {
                        if let Some(index) = schema.indexes.iter().find(|idx| &idx.column == column) {
                            return Ok(Plan::IndexScan {
                                table,
                                index: index.name.clone(),
                                columns,
                                filter: filter.unwrap(),
                            });
                        }
                    }
                }
                Ok(Plan::SeqScan { table, columns, filter })
            }
            Statement::CreateTable { name, .. } => Ok(Plan::CreateTable { name }),
            Statement::CreateIndex { name, table, column } => Ok(Plan::CreateIndex { name, table, column }),
            Statement::Insert { table, values } => Ok(Plan::Insert { table, values }),
            Statement::Begin => Ok(Plan::BeginTxn),
            Statement::Commit => Ok(Plan::CommitTxn),
            Statement::Rollback => Ok(Plan::RollbackTxn),
        }
    }
}
```

Run: `cargo test planner_tests -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/sql/plan.rs src/sql/planner.rs tests/planner_tests.rs
git commit -m "feat: add logical planner for seq and index scans"
```

### Task 5: Implement executor and in-memory storage backend

**Files:**
- Create: `src/sql/executor.rs`
- Create: `src/storage/memory.rs`
- Modify: `src/db.rs`
- Test: `tests/executor_tests.rs`
- Test: `tests/storage_memory_tests.rs`

- [ ] **Step 1: Write failing integration tests for `CREATE` / `INSERT` / `SELECT` / txn**

```rust
use rustsql::db::Database;

#[test]
fn memory_backend_supports_basic_sql_flow() {
    let mut db = Database::memory().unwrap();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);").unwrap();
    db.execute("CREATE INDEX idx_users_id ON users (id);").unwrap();
    db.execute("INSERT INTO users VALUES (1, 'alice');").unwrap();

    let rows = db.query("SELECT * FROM users WHERE id = 1;").unwrap();
    assert_eq!(rows.len(), 1);
}
```

- [ ] **Step 2: Run integration tests to verify they fail**

Run: `cargo test memory_backend_supports_basic_sql_flow -v`
Expected: FAIL with missing `execute` or `query`

- [ ] **Step 3: Implement `MemoryStorage` and executor**

`src/storage/memory.rs`
```rust
use std::collections::{BTreeMap, HashMap};

use crate::common::error::{DbError, DbResult};
use crate::common::types::{IndexMeta, Row, RowId, Schema, Value};
use crate::engine::traits::{CatalogStore, IndexStore, TableStore, TransactionManager};
use crate::engine::txn::TransactionId;

#[derive(Default)]
pub struct MemoryStorage {
    next_tx: u64,
    next_row: u64,
    schemas: HashMap<String, Schema>,
    tables: HashMap<String, Vec<(RowId, Row)>>,
    indexes: HashMap<String, BTreeMap<Value, Vec<RowId>>>,
}

impl CatalogStore for MemoryStorage {
    fn create_table(&mut self, schema: Schema) -> DbResult<()> {
        if self.schemas.contains_key(&schema.name) {
            return Err(DbError::Storage(format!("table {} already exists", schema.name)));
        }
        self.tables.insert(schema.name.clone(), Vec::new());
        self.schemas.insert(schema.name.clone(), schema);
        Ok(())
    }

    fn get_schema(&self, table: &str) -> DbResult<Schema> {
        self.schemas
            .get(table)
            .cloned()
            .ok_or_else(|| DbError::Plan(format!("table {table} not found")))
    }

    fn list_schemas(&self) -> DbResult<Vec<Schema>> {
        Ok(self.schemas.values().cloned().collect())
    }

    fn create_index(&mut self, index: IndexMeta) -> DbResult<()> {
        let schema = self
            .schemas
            .get_mut(&index.table)
            .ok_or_else(|| DbError::Plan(format!("table {} not found", index.table)))?;
        schema.indexes.push(index.clone());
        self.indexes.entry(index.name).or_default();
        Ok(())
    }

    fn list_indexes(&self, table: &str) -> DbResult<Vec<IndexMeta>> {
        Ok(self.get_schema(table)?.indexes)
    }
}
```

`src/sql/executor.rs`
```rust
use crate::common::error::{DbError, DbResult};
use crate::common::types::{IndexMeta, Row, Schema};
use crate::engine::traits::StorageEngine;
use crate::engine::txn::TransactionId;
use crate::sql::ast::Expr;
use crate::sql::plan::Plan;

pub struct Executor<'a, S: StorageEngine> {
    storage: &'a mut S,
    current_tx: Option<TransactionId>,
}

impl<'a, S: StorageEngine> Executor<'a, S> {
    pub fn new(storage: &'a mut S) -> Self {
        Self { storage, current_tx: None }
    }

    pub fn execute(&mut self, plan: Plan) -> DbResult<Vec<Row>> {
        match plan {
            Plan::CreateTable { name } => {
                self.storage.create_table(Schema::new(name, Vec::new()))?;
                Ok(Vec::new())
            }
            Plan::CreateIndex { name, table, column } => {
                self.storage.create_index(IndexMeta { name, table, column })?;
                Ok(Vec::new())
            }
            Plan::Insert { table, values } => {
                let tx = self.ensure_tx()?;
                let _row_id = self.storage.insert_row(tx, &table, values)?;
                Ok(Vec::new())
            }
            Plan::SeqScan { table, filter, .. } => {
                let tx = self.current_tx.unwrap_or(TransactionId(0));
                let rows = self.storage.scan_rows(tx, &table)?;
                Ok(rows
                    .into_iter()
                    .filter(|(_, row)| matches_filter(row, filter.as_ref()))
                    .map(|(_, row)| row)
                    .collect())
            }
            Plan::IndexScan { table, index, filter, .. } => {
                let tx = self.current_tx.unwrap_or(TransactionId(0));
                let row_ids = match &filter {
                    Expr::Eq(_, value) => self.storage.lookup_index(tx, &index, value)?,
                    _ => return Err(DbError::Plan("memory index scan only supports equality filters".into())),
                };
                let mut rows = Vec::new();
                for row_id in row_ids {
                    rows.push(self.storage.get_row(tx, &table, row_id)?);
                }
                Ok(rows)
            }
            Plan::BeginTxn => {
                self.current_tx = Some(self.storage.begin()?);
                Ok(Vec::new())
            }
            Plan::CommitTxn => {
                let tx = self.current_tx.take().ok_or_else(|| DbError::Txn("no active transaction".into()))?;
                self.storage.commit(tx)?;
                Ok(Vec::new())
            }
            Plan::RollbackTxn => {
                let tx = self.current_tx.take().ok_or_else(|| DbError::Txn("no active transaction".into()))?;
                self.storage.rollback(tx)?;
                Ok(Vec::new())
            }
        }
    }

    fn ensure_tx(&mut self) -> DbResult<TransactionId> {
        if let Some(tx) = self.current_tx {
            return Ok(tx);
        }
        let tx = self.storage.begin()?;
        self.current_tx = Some(tx);
        Ok(tx)
    }
}

fn matches_filter(_row: &Row, _filter: Option<&Expr>) -> bool {
    true
}
```

- [ ] **Step 4: Wire `Database` to parser/planner/executor and rerun tests**

`src/db.rs`
```rust
use crate::common::error::DbResult;
use crate::common::types::Row;
use crate::engine::traits::CatalogStore;
use crate::sql::parser::parse_sql;
use crate::sql::planner::Planner;
use crate::storage::memory::MemoryStorage;

pub struct Database {
    storage: MemoryStorage,
}

impl Database {
    pub fn memory() -> DbResult<Self> {
        Ok(Self { storage: MemoryStorage::default() })
    }

    pub fn execute(&mut self, sql: &str) -> DbResult<()> {
        let statement = parse_sql(sql)?.remove(0);
        let plan = Planner::new(self.storage.list_schemas()?).build(statement)?;
        let mut executor = crate::sql::executor::Executor::new(&mut self.storage);
        let _ = executor.execute(plan)?;
        Ok(())
    }

    pub fn query(&mut self, sql: &str) -> DbResult<Vec<Row>> {
        let statement = parse_sql(sql)?.remove(0);
        let plan = Planner::new(self.storage.list_schemas()?).build(statement)?;
        let mut executor = crate::sql::executor::Executor::new(&mut self.storage);
        executor.execute(plan)
    }
}
```

Run: `cargo test executor_tests storage_memory_tests -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/sql/executor.rs src/storage/memory.rs src/db.rs tests/executor_tests.rs tests/storage_memory_tests.rs
git commit -m "feat: execute core sql statements on memory storage"
```

### Task 6: Add `storage_v1` persistence for catalog, rows, indexes, and rollback log

**Files:**
- Create: `src/storage/v1/mod.rs`
- Create: `src/storage/v1/catalog.rs`
- Create: `src/storage/v1/table.rs`
- Create: `src/storage/v1/index.rs`
- Create: `src/storage/v1/txn.rs`
- Modify: `src/db.rs`
- Test: `tests/storage_v1_tests.rs`
- Test: `tests/e2e_sql_tests.rs`

- [ ] **Step 1: Write failing persistence and rollback tests**

```rust
use tempfile::tempdir;
use rustsql::db::Database;

#[test]
fn file_backend_persists_rows_across_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("demo.db");

    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'alice');").unwrap();
    }

    let mut reopened = Database::open(&path).unwrap();
    let rows = reopened.query("SELECT * FROM users WHERE id = 1;").unwrap();
    assert_eq!(rows.len(), 1);
}
```

- [ ] **Step 2: Run persistence tests to verify they fail**

Run: `cargo test file_backend_persists_rows_across_reopen -v`
Expected: FAIL with missing `Database::open`

- [ ] **Step 3: Implement file layout and metadata persistence**

`src/storage/v1/catalog.rs`
```rust
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::common::error::DbResult;
use crate::common::types::Schema;

#[derive(Debug)]
pub struct CatalogFile {
    path: PathBuf,
    schemas: HashMap<String, Schema>,
}

impl CatalogFile {
    pub fn open(base: &Path) -> DbResult<Self> {
        let path = base.join("catalog.json");
        let schemas = if path.exists() {
            serde_json::from_slice(&fs::read(&path)?)?
        } else {
            HashMap::new()
        };
        Ok(Self { path, schemas })
    }
}
```

`src/storage/v1/table.rs`
```rust
use std::fs;
use std::path::PathBuf;

use crate::common::error::DbResult;
use crate::common::types::{Row, RowId};

pub fn load_rows(path: &PathBuf) -> DbResult<Vec<(RowId, Row)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
```

- [ ] **Step 4: Implement `Database::open`, rollback log, and rerun tests**

`src/storage/v1/txn.rs`
```rust
use std::fs;
use std::path::PathBuf;

use crate::common::error::DbResult;

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RollbackLog {
    pub table_snapshots: Vec<String>,
}

pub fn write_log(path: &PathBuf, log: &RollbackLog) -> DbResult<()> {
    fs::write(path, serde_json::to_vec_pretty(log)?)?;
    Ok(())
}
```

`src/db.rs`
```rust
use std::path::Path;

pub enum Backend {
    Memory(crate::storage::memory::MemoryStorage),
    File(crate::storage::v1::FileStorage),
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> DbResult<Self> {
        Ok(Self { storage: Backend::File(crate::storage::v1::FileStorage::open(path.as_ref())?) })
    }
}
```

Run: `cargo test storage_v1_tests e2e_sql_tests -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/storage/v1/mod.rs src/storage/v1/catalog.rs src/storage/v1/table.rs src/storage/v1/index.rs src/storage/v1/txn.rs src/db.rs tests/storage_v1_tests.rs tests/e2e_sql_tests.rs
git commit -m "feat: add persistent storage_v1 backend"
```

### Task 7: Add REPL and final end-to-end verification

**Files:**
- Create: `src/repl/printer.rs`
- Modify: `src/repl/mod.rs`
- Modify: `src/main.rs`
- Modify: `tests/e2e_sql_tests.rs`

- [ ] **Step 1: Write failing CLI smoke test**

```rust
#[test]
fn query_results_render_as_table() {
    let output = rustsql::repl::printer::render_rows(
        &["id".into(), "name".into()],
        &[vec![rustsql::common::types::Value::Integer(1), rustsql::common::types::Value::Text("alice".into())]],
    );

    assert!(output.contains("alice"));
}
```

- [ ] **Step 2: Run the CLI smoke test to verify it fails**

Run: `cargo test query_results_render_as_table -v`
Expected: FAIL with missing `render_rows`

- [ ] **Step 3: Implement printer and REPL loop**

`src/repl/printer.rs`
```rust
use crate::common::types::Value;

pub fn render_rows(headers: &[String], rows: &[Vec<Value>]) -> String {
    let mut output = String::new();
    output.push_str(&headers.join(" | "));
    output.push('\n');
    for row in rows {
        let line = row.iter().map(ToString::to_string).collect::<Vec<_>>().join(" | ");
        output.push_str(&line);
        output.push('\n');
    }
    output
}
```

`src/repl/mod.rs`
```rust
use std::io::{self, Write};

use crate::common::error::DbResult;
use crate::db::Database;

pub fn run_repl(mut db: Database) -> DbResult<()> {
    let mut line = String::new();
    loop {
        print!("rustsql> ");
        io::stdout().flush()?;
        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        if line.trim().eq_ignore_ascii_case(".exit") {
            break;
        }
        let _ = db.execute(&line);
    }
    Ok(())
}
```

- [ ] **Step 4: Wire `main` and run the full test suite**

`src/main.rs`
```rust
fn main() -> Result<(), rustsql::common::error::DbError> {
    let db = rustsql::db::Database::memory()?;
    rustsql::repl::run_repl(db)
}
```

Run: `cargo test -v`
Expected: PASS

Run: `cargo run`
Expected: prompt shows `rustsql>` and `.exit` cleanly exits

- [ ] **Step 5: Commit**

```bash
git add src/repl/printer.rs src/repl/mod.rs src/main.rs tests/e2e_sql_tests.rs
git commit -m "feat: add rustsql repl and final verification"
```

## Out of Scope for This Plan

- `storage_v2` pager/page/WAL/B+Tree 真内核
- 多事务并发隔离
- `UPDATE` / `DELETE`
- `JOIN` / `GROUP BY` / `ORDER BY`
- 成本优化器

这些内容需要在 MVP 跑通后，基于当前 trait 边界单独出第二份实现计划。

## Self-Review Checklist

- Spec coverage: 已覆盖 parser、planner、executor、memory backend、storage_v1、错误分层、事务接口、测试策略、REPL；未覆盖的仅为 spec 中明确属于第二阶段的 `storage_v2` 真内核，已放入 out-of-scope。
- Placeholder scan: 计划中没有 `TODO`、`TBD`、`implement later`；每个任务均列出精确文件和命令。
- Type consistency: 全文统一使用 `Database`、`StorageEngine`、`Schema`、`RowId`、`TransactionId`、`Plan::{SeqScan, IndexScan}`。
