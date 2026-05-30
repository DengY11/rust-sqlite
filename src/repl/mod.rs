use std::io::{self, BufRead, Write};

use crate::common::error::Result;
use crate::db::{Database, StatementBatchKind};
use crate::engine::PlanningStorageEngine;
use crate::sql::ast::{SelectItem, Statement};
use crate::sql::parse_sql;

mod printer;

pub use printer::render_rows;

const PROMPT: &str = "rustsql> ";

pub fn run_memory_repl() -> Result<()> {
    let db = Database::memory();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    run_with_io(&db, stdin.lock(), &mut output)
}

pub fn run_with_io<S, R, W>(db: &Database<S>, mut input: R, output: &mut W) -> Result<()>
where
    S: PlanningStorageEngine,
    R: BufRead,
    W: Write,
{
    let mut line = String::new();

    loop {
        write!(output, "{PROMPT}")?;
        output.flush()?;

        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }

        let sql = line.trim();
        if sql.is_empty() {
            continue;
        }

        if matches!(sql, ".exit" | ".quit") {
            break;
        }

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
    }

    Ok(())
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
                        crate::sql::ast::AggregateArg::Column(name) => name.clone(),
                    }
                )
            }),
        })
        .collect()
}

fn generic_headers(width: usize) -> Vec<String> {
    (1..=width).map(|index| format!("col{index}")).collect()
}
