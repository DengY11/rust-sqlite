use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use crate::common::error::{DbError, Result};
use crate::common::types::{ColumnDef, IndexMeta, Schema};
use crate::db::{Database, StatementBatchKind};
use crate::engine::PlanningStorageEngine;
use crate::sql::ast::{SelectItem, Statement};
use crate::sql::parse_sql;

mod printer;

pub use printer::render_rows;

const PROMPT: &str = "rustsql> ";
const CONTINUATION_PROMPT: &str = "...> ";

const HELP_TEXT: &str = "rustsql commands:\n  .help      Show this help message\n  .tables    List tables\n  .schema    Show table schemas and indexes\n  .exit      Exit\n  .quit      Exit";

pub fn run_from_args(args: impl IntoIterator<Item = OsString>) -> Result<()> {
    let args = args.into_iter().skip(1).collect::<Vec<_>>();

    match args.as_slice() {
        [] => run_memory_repl(),
        [path] => run_file_repl(PathBuf::from(path)),
        [flag, engine, path] if flag == OsStr::new("--engine") => {
            run_engine_file_repl(engine, PathBuf::from(path))
        }
        _ => Err(DbError::sql(
            "usage: rustsql [database-path] | rustsql --engine <v1|v2> <database-path>",
        )),
    }
}

pub fn run_memory_repl() -> Result<()> {
    let db = Database::memory();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    run_with_io(&db, stdin.lock(), &mut output)
}

pub fn run_file_repl(path: impl Into<PathBuf>) -> Result<()> {
    let db = Database::open(path.into())?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    run_with_io(&db, stdin.lock(), &mut output)
}

fn run_engine_file_repl(engine: &OsStr, path: PathBuf) -> Result<()> {
    match engine.to_str() {
        Some("v1") => run_file_repl(path),
        Some("v2") => {
            let db = Database::with_storage(crate::storage::v2::FileStorage::open(path)?);
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut output = stdout.lock();

            run_with_io(&db, stdin.lock(), &mut output)
        }
        Some(other) => Err(DbError::sql(format!(
            "unknown storage engine '{other}'; expected v1 or v2"
        ))),
        None => Err(DbError::sql("storage engine must be valid UTF-8")),
    }
}

pub fn run_with_io<S, R, W>(db: &Database<S>, mut input: R, output: &mut W) -> Result<()>
where
    S: PlanningStorageEngine,
    R: BufRead,
    W: Write,
{
    let mut line = String::new();
    let mut pending_sql = String::new();

    loop {
        if pending_sql.is_empty() {
            write!(output, "{PROMPT}")?;
        } else {
            write!(output, "{CONTINUATION_PROMPT}")?;
        }
        output.flush()?;

        line.clear();
        if input.read_line(&mut line)? == 0 {
            if !pending_sql.trim().is_empty() {
                execute_repl_sql(db, pending_sql.trim(), output)?;
            }
            break;
        }

        let input_line = line.trim();
        if input_line.is_empty() {
            continue;
        }

        if pending_sql.is_empty() && matches!(input_line, ".exit" | ".quit") {
            break;
        }

        if pending_sql.is_empty() && handle_meta_command(db, input_line, output)? {
            continue;
        }

        let was_pending = !pending_sql.is_empty();
        append_sql_line(&mut pending_sql, input_line);

        if should_wait_for_more_sql(was_pending, input_line, &pending_sql) {
            continue;
        }

        execute_repl_sql(db, pending_sql.trim(), output)?;
        pending_sql.clear();
    }

    Ok(())
}

fn append_sql_line(pending_sql: &mut String, line: &str) {
    if !pending_sql.is_empty() {
        pending_sql.push('\n');
    }
    pending_sql.push_str(line);
}

fn should_wait_for_more_sql(was_pending: bool, line: &str, sql: &str) -> bool {
    if sql.trim_end().ends_with(';') {
        return false;
    }

    was_pending || looks_like_incomplete_sql(line, sql)
}

fn looks_like_incomplete_sql(line: &str, sql: &str) -> bool {
    paren_balance(sql) > 0 || line.trim_end().ends_with(',')
}

fn paren_balance(sql: &str) -> i32 {
    let mut balance = 0;
    let mut in_string = false;
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if in_string && chars.peek() == Some(&'\'') => {
                chars.next();
            }
            '\'' => in_string = !in_string,
            '(' if !in_string => balance += 1,
            ')' if !in_string => balance -= 1,
            _ => {}
        }
    }

    balance
}

fn execute_repl_sql<S, W>(db: &Database<S>, sql: &str, output: &mut W) -> Result<()>
where
    S: PlanningStorageEngine,
    W: Write,
{
    match parse_sql(sql) {
        Ok(statements) => match Database::<S>::classify_batch(&statements) {
            StatementBatchKind::Query => match db.query_parsed(&statements) {
                Ok(rows) => {
                    let headers = infer_headers(&statements, rows.first().map_or(0, Vec::len));
                    writeln!(output, "{}", render_rows(&headers, &rows))?;
                }
                Err(error) => {
                    writeln!(output, "{error}")?;
                }
            },
            StatementBatchKind::Execute => match db.execute_parsed(&statements) {
                Ok(()) => {
                    writeln!(output, "ok")?;
                }
                Err(error) => {
                    writeln!(output, "{error}")?;
                }
            },
        },
        Err(error) => {
            writeln!(output, "{error}")?;
        }
    }

    Ok(())
}

fn handle_meta_command<S, W>(db: &Database<S>, command: &str, output: &mut W) -> Result<bool>
where
    S: PlanningStorageEngine,
    W: Write,
{
    match command {
        ".help" => {
            writeln!(output, "{HELP_TEXT}")?;
            Ok(true)
        }
        ".tables" => {
            write_tables(db, output)?;
            Ok(true)
        }
        ".schema" => {
            write_schema(db, output)?;
            Ok(true)
        }
        _ if command.starts_with('.') => {
            writeln!(output, "unknown command: {command}")?;
            writeln!(output, "Run .help for available commands.")?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn write_tables<S, W>(db: &Database<S>, output: &mut W) -> Result<()>
where
    S: PlanningStorageEngine,
    W: Write,
{
    let mut schemas = db.list_schemas()?;
    schemas.sort_by(|left, right| left.name.cmp(&right.name));

    if schemas.is_empty() {
        writeln!(output, "(no tables)")?;
    } else {
        writeln!(
            output,
            "{}",
            schemas
                .iter()
                .map(|schema| schema.name.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )?;
    }

    Ok(())
}

fn write_schema<S, W>(db: &Database<S>, output: &mut W) -> Result<()>
where
    S: PlanningStorageEngine,
    W: Write,
{
    let mut schemas = db.list_schemas()?;
    schemas.sort_by(|left, right| left.name.cmp(&right.name));

    if schemas.is_empty() {
        writeln!(output, "(no schema)")?;
        return Ok(());
    }

    for schema in schemas {
        writeln!(output, "{}", render_create_table(&schema))?;

        let mut indexes = db.list_indexes(&schema.name)?;
        indexes.sort_by(|left, right| left.name.cmp(&right.name));
        for index in indexes {
            writeln!(output, "{}", render_create_index(&schema.name, &index))?;
        }
    }

    Ok(())
}

fn render_create_table(schema: &Schema) -> String {
    let columns = schema
        .columns
        .iter()
        .map(render_column_def)
        .collect::<Vec<_>>()
        .join(", ");
    format!("CREATE TABLE {} ({});", schema.name, columns)
}

fn render_column_def(column: &ColumnDef) -> String {
    let mut rendered = format!("{} {}", column.name, column.column_type.name());
    if column.primary_key {
        rendered.push_str(" PRIMARY KEY");
    }
    if !column.nullable && !column.primary_key {
        rendered.push_str(" NOT NULL");
    }
    rendered
}

fn render_create_index(table: &str, index: &IndexMeta) -> String {
    format!(
        "CREATE INDEX {} ON {} ({});",
        index.name,
        table,
        index.columns.join(", ")
    )
}

fn infer_headers(statements: &[Statement], row_width: usize) -> Vec<String> {
    let Some(Statement::Select(select)) = statements.last() else {
        return generic_headers(row_width);
    };

    let columns = &select.columns;

    if columns.len() == 1 && matches!(columns.first(), Some(SelectItem::Wildcard)) {
        return generic_headers(row_width);
    }

    columns
        .iter()
        .map(|column| match column {
            SelectItem::Wildcard => "*".to_string(),
            SelectItem::Column(name) => name.clone(),
            SelectItem::AliasedColumn { alias, .. } => alias.clone(),
            SelectItem::Aggregate { func, arg, alias } => alias.clone().unwrap_or_else(|| {
                format!(
                    "{}({})",
                    match func {
                        crate::sql::ast::AggregateFunc::Count => "COUNT",
                        crate::sql::ast::AggregateFunc::Sum => "SUM",
                        crate::sql::ast::AggregateFunc::Avg => "AVG",
                        crate::sql::ast::AggregateFunc::Min => "MIN",
                        crate::sql::ast::AggregateFunc::Max => "MAX",
                    },
                    match arg {
                        crate::sql::ast::AggregateArg::Wildcard => "*".to_string(),
                        crate::sql::ast::AggregateArg::Column { name, distinct } => {
                            if *distinct {
                                format!("DISTINCT {name}")
                            } else {
                                name.clone()
                            }
                        }
                    }
                )
            }),
        })
        .collect()
}

fn generic_headers(width: usize) -> Vec<String> {
    (1..=width).map(|index| format!("col{index}")).collect()
}
