use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use crate::common::error::{DbError, Result};
use crate::common::types::{
    CheckConstraint, CheckExpr, CheckOp, ColumnDef, ColumnDefault, ColumnType, ForeignKey,
    IndexMeta, PrimaryKeyConstraint, Schema, SortOrder, TableConstraintOrder, Value,
};
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
            "usage: rustsql [database-path] | rustsql --engine <v1|v2|sqlite3> <database-path>",
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
        Some("sqlite3") => {
            let db = Database::with_storage(crate::storage::sqlite3::FileStorage::open(path)?);
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut output = stdout.lock();

            run_with_io(&db, stdin.lock(), &mut output)
        }
        Some(other) => Err(DbError::sql(format!(
            "unknown storage engine '{other}'; expected v1, v2, or sqlite3"
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
    let mut definitions = schema
        .columns
        .iter()
        .map(|column| render_column_def(schema, column))
        .collect::<Vec<_>>();
    definitions.extend(render_table_constraints(schema));
    let strict = if schema.strict { " STRICT" } else { "" };
    let without_rowid = if schema.without_rowid {
        " WITHOUT ROWID"
    } else {
        ""
    };
    format!(
        "CREATE TABLE {} ({}){}{};",
        schema.name,
        definitions.join(", "),
        strict,
        without_rowid
    )
}

fn render_table_constraints(schema: &Schema) -> Vec<String> {
    if schema.table_constraint_order.is_empty() {
        let mut definitions = schema
            .primary_key_constraint
            .iter()
            .map(render_primary_key_constraint)
            .collect::<Vec<_>>();
        definitions.extend(schema.checks.iter().map(render_check_constraint));
        definitions.extend(
            schema
                .unique_constraints
                .iter()
                .map(render_unique_constraint),
        );
        definitions.extend(schema.foreign_keys.iter().map(render_foreign_key));
        return definitions;
    }

    schema
        .table_constraint_order
        .iter()
        .filter_map(|entry| match entry {
            TableConstraintOrder::Check(index) => {
                schema.checks.get(*index).map(render_check_constraint)
            }
            TableConstraintOrder::ForeignKey(index) => {
                schema.foreign_keys.get(*index).map(render_foreign_key)
            }
            TableConstraintOrder::PrimaryKey => schema
                .primary_key_constraint
                .as_ref()
                .map(render_primary_key_constraint),
            TableConstraintOrder::Unique(index) => schema
                .unique_constraints
                .get(*index)
                .map(render_unique_constraint),
        })
        .collect()
}

fn render_column_def(schema: &Schema, column: &ColumnDef) -> String {
    let mut rendered = if matches!(column.column_type, ColumnType::Any) {
        column.name.clone()
    } else {
        format!("{} {}", column.name, column.column_type.name())
    };
    if let Some(collation) = &column.collation {
        rendered.push_str(" COLLATE ");
        rendered.push_str(collation);
    }
    let rendered_by_table_constraint = schema
        .primary_key_constraint
        .as_ref()
        .is_some_and(|constraint| constraint.columns.iter().any(|name| name == &column.name));
    if column.primary_key && !rendered_by_table_constraint {
        if let Some(constraint_name) = &column.primary_key_constraint_name {
            rendered.push_str(" CONSTRAINT ");
            rendered.push_str(constraint_name);
        }
        rendered.push_str(" PRIMARY KEY");
        if let Some(conflict_clause) = &column.primary_key_conflict_clause {
            rendered.push_str(" ON CONFLICT ");
            rendered.push_str(conflict_clause);
        }
        if let Some(sort_order) = column.primary_key_sort_order {
            match sort_order {
                SortOrder::Asc => rendered.push_str(" ASC"),
                SortOrder::Desc => rendered.push_str(" DESC"),
            }
        }
        if column.autoincrement {
            rendered.push_str(" AUTOINCREMENT");
        }
    }
    if column.unique {
        if let Some(constraint_name) = &column.unique_constraint_name {
            rendered.push_str(" CONSTRAINT ");
            rendered.push_str(constraint_name);
        }
        rendered.push_str(" UNIQUE");
        if let Some(conflict_clause) = &column.unique_conflict_clause {
            rendered.push_str(" ON CONFLICT ");
            rendered.push_str(conflict_clause);
        }
    }
    if !column.nullable && !column.primary_key {
        if let Some(constraint_name) = &column.not_null_constraint_name {
            rendered.push_str(" CONSTRAINT ");
            rendered.push_str(constraint_name);
        }
        rendered.push_str(" NOT NULL");
        if let Some(conflict_clause) = &column.not_null_conflict_clause {
            rendered.push_str(" ON CONFLICT ");
            rendered.push_str(conflict_clause);
        }
    }
    if let Some(default_value) = &column.default_value {
        rendered.push_str(" DEFAULT ");
        rendered.push_str(&render_column_default(default_value));
    }
    if let Some(expr) = &column.generated_expr {
        if column.generated_always_explicit {
            rendered.push_str(" GENERATED ALWAYS AS (");
        } else {
            rendered.push_str(" AS (");
        }
        rendered.push_str(expr);
        rendered.push(')');
        if column.generated_stored {
            rendered.push_str(" STORED");
        } else if column.generated_storage_explicit {
            rendered.push_str(" VIRTUAL");
        }
    }
    for check in &column.checks {
        rendered.push(' ');
        rendered.push_str(&render_check_constraint(check));
    }
    if let Some(foreign_key) = &column.foreign_key {
        rendered.push(' ');
        rendered.push_str(&render_inline_foreign_key(foreign_key));
    }
    rendered
}

fn render_foreign_key(foreign_key: &ForeignKey) -> String {
    let mut rendered = String::new();
    if let Some(constraint_name) = &foreign_key.constraint_name {
        rendered.push_str("CONSTRAINT ");
        rendered.push_str(constraint_name);
        rendered.push(' ');
    }
    rendered.push_str("FOREIGN KEY (");
    rendered.push_str(&foreign_key.rendered_child_columns());
    rendered.push_str(") REFERENCES ");
    rendered.push_str(&foreign_key.ref_table);
    if let Some(ref_columns) = foreign_key.rendered_referenced_columns() {
        rendered.push('(');
        rendered.push_str(&ref_columns);
        rendered.push(')');
    }
    append_foreign_key_clauses(&mut rendered, foreign_key);
    rendered
}

fn render_inline_foreign_key(foreign_key: &ForeignKey) -> String {
    let mut rendered = String::new();
    if let Some(constraint_name) = &foreign_key.constraint_name {
        rendered.push_str("CONSTRAINT ");
        rendered.push_str(constraint_name);
        rendered.push(' ');
    }
    rendered.push_str("REFERENCES ");
    rendered.push_str(&foreign_key.ref_table);
    if let Some(ref_columns) = foreign_key.rendered_referenced_columns() {
        rendered.push('(');
        rendered.push_str(&ref_columns);
        rendered.push(')');
    }
    append_foreign_key_clauses(&mut rendered, foreign_key);
    rendered
}

fn append_foreign_key_clauses(rendered: &mut String, foreign_key: &ForeignKey) {
    if let Some(match_clause) = &foreign_key.match_clause {
        rendered.push_str(" MATCH ");
        rendered.push_str(match_clause);
    }
    if let Some(on_delete) = &foreign_key.on_delete {
        rendered.push_str(" ON DELETE ");
        rendered.push_str(on_delete);
    }
    if let Some(on_update) = &foreign_key.on_update {
        rendered.push_str(" ON UPDATE ");
        rendered.push_str(on_update);
    }
    if let Some(deferrable) = foreign_key.deferrable {
        if deferrable {
            rendered.push_str(" DEFERRABLE");
        } else {
            rendered.push_str(" NOT DEFERRABLE");
        }
    }
    if let Some(initially_deferred) = foreign_key.initially_deferred {
        if initially_deferred {
            rendered.push_str(" INITIALLY DEFERRED");
        } else {
            rendered.push_str(" INITIALLY IMMEDIATE");
        }
    }
}

fn render_check_constraint(check: &CheckConstraint) -> String {
    if check.explicit_name {
        format!(
            "CONSTRAINT {} CHECK ({})",
            check.name,
            render_check_expr(&check.expr)
        )
    } else {
        format!("CHECK ({})", render_check_expr(&check.expr))
    }
}

fn render_primary_key_constraint(primary_key: &PrimaryKeyConstraint) -> String {
    let mut rendered = String::new();
    if let Some(constraint_name) = &primary_key.constraint_name {
        rendered.push_str("CONSTRAINT ");
        rendered.push_str(constraint_name);
        rendered.push(' ');
    }
    rendered.push_str("PRIMARY KEY(");
    rendered.push_str(&primary_key.rendered_columns().join(", "));
    rendered.push(')');
    if let Some(conflict_clause) = &primary_key.conflict_clause {
        rendered.push_str(" ON CONFLICT ");
        rendered.push_str(conflict_clause);
    }
    rendered
}

fn render_unique_constraint(unique: &crate::common::types::UniqueConstraint) -> String {
    let mut rendered = String::new();
    if let Some(constraint_name) = &unique.constraint_name {
        rendered.push_str("CONSTRAINT ");
        rendered.push_str(constraint_name);
        rendered.push(' ');
    }
    rendered.push_str("UNIQUE(");
    rendered.push_str(&unique.rendered_columns().join(", "));
    rendered.push(')');
    if let Some(conflict_clause) = &unique.conflict_clause {
        rendered.push_str(" ON CONFLICT ");
        rendered.push_str(conflict_clause);
    }
    rendered
}

fn render_check_expr(expr: &CheckExpr) -> String {
    match expr {
        CheckExpr::Compare { column, op, value } => {
            format!(
                "{} {} {}",
                column,
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::IsNull { column, negated } => {
            if *negated {
                format!("{column} IS NOT NULL")
            } else {
                format!("{column} IS NULL")
            }
        }
        CheckExpr::Glob {
            column,
            pattern,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!(
                "{column} {not}GLOB {}",
                render_literal(&Value::from(pattern.as_str()))
            )
        }
        CheckExpr::Like {
            column,
            pattern,
            escape,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            let escape = escape
                .as_ref()
                .map(|escape| format!(" ESCAPE {}", render_literal(&Value::from(escape.as_str()))))
                .unwrap_or_default();
            format!(
                "{column} {not}LIKE {}{escape}",
                render_literal(&Value::from(pattern.as_str()))
            )
        }
        CheckExpr::InList {
            column,
            values,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            let values = values
                .iter()
                .map(render_literal)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{column} {not}IN ({values})")
        }
        CheckExpr::Between {
            column,
            low,
            high,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!(
                "{column} {not}BETWEEN {} AND {}",
                render_literal(low),
                render_literal(high)
            )
        }
        CheckExpr::IsBool {
            column,
            value,
            negated,
        } => {
            format!(
                "{column} IS {}{}",
                if *negated { "NOT " } else { "" },
                if *value { "TRUE" } else { "FALSE" }
            )
        }
        CheckExpr::Truthy { column } => column.clone(),
        CheckExpr::IsDistinct {
            column,
            value,
            negated,
        } => {
            let not = if *negated { "" } else { "NOT " };
            format!("{column} IS {not}DISTINCT FROM {}", render_literal(value))
        }
        CheckExpr::And(left, right) => {
            format!(
                "({}) AND ({})",
                render_check_expr(left),
                render_check_expr(right)
            )
        }
        CheckExpr::Or(left, right) => {
            format!(
                "({}) OR ({})",
                render_check_expr(left),
                render_check_expr(right)
            )
        }
        CheckExpr::Not(expr) => format!("NOT ({})", render_check_expr(expr)),
    }
}

fn render_check_op(op: CheckOp) -> &'static str {
    match op {
        CheckOp::Eq => "=",
        CheckOp::Ne => "!=",
        CheckOp::Gt => ">",
        CheckOp::Gte => ">=",
        CheckOp::Lt => "<",
        CheckOp::Lte => "<=",
    }
}

fn render_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(true) => "true".to_string(),
        Value::Boolean(false) => "false".to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Real(value) => value.to_string(),
        Value::Blob(value) => format!(
            "X'{}'",
            value
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
        Value::Text(value) => format!("'{}'", value.replace('\'', "''")),
    }
}

fn render_column_default(default_value: &ColumnDefault) -> String {
    match default_value {
        ColumnDefault::Literal(value) => render_literal(value),
        ColumnDefault::CurrentTimestamp => "CURRENT_TIMESTAMP".to_string(),
        ColumnDefault::CurrentDate => "CURRENT_DATE".to_string(),
        ColumnDefault::CurrentTime => "CURRENT_TIME".to_string(),
    }
}

fn render_create_index(table: &str, index: &IndexMeta) -> String {
    let unique = if index.unique { " UNIQUE" } else { "" };
    let predicate = index
        .predicate
        .as_ref()
        .map(|predicate| format!(" WHERE {predicate}"))
        .unwrap_or_default();
    format!(
        "CREATE{} INDEX {} ON {} ({}){};",
        unique,
        index.name,
        table,
        index.rendered_columns().join(", "),
        predicate
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
            SelectItem::Expr { alias, .. } => alias.clone().unwrap_or_else(|| "expr".to_string()),
            SelectItem::Aggregate {
                func, arg, alias, ..
            } => alias.clone().unwrap_or_else(|| {
                format!(
                    "{}({})",
                    match func {
                        crate::sql::ast::AggregateFunc::Count => "COUNT",
                        crate::sql::ast::AggregateFunc::Sum => "SUM",
                        crate::sql::ast::AggregateFunc::Avg => "AVG",
                        crate::sql::ast::AggregateFunc::Total => "TOTAL",
                        crate::sql::ast::AggregateFunc::Median => "MEDIAN",
                        crate::sql::ast::AggregateFunc::Percentile => "PERCENTILE",
                        crate::sql::ast::AggregateFunc::PercentileCont => "PERCENTILE_CONT",
                        crate::sql::ast::AggregateFunc::PercentileDisc => "PERCENTILE_DISC",
                        crate::sql::ast::AggregateFunc::GroupConcat => "GROUP_CONCAT",
                        crate::sql::ast::AggregateFunc::JsonGroupArray => "JSON_GROUP_ARRAY",
                        crate::sql::ast::AggregateFunc::JsonGroupObject => "JSON_GROUP_OBJECT",
                        crate::sql::ast::AggregateFunc::Min => "MIN",
                        crate::sql::ast::AggregateFunc::Max => "MAX",
                    },
                    match arg {
                        crate::sql::ast::AggregateArg::Wildcard => "*".to_string(),
                        crate::sql::ast::AggregateArg::Expr { expr, distinct, .. } => {
                            if *distinct {
                                format!("DISTINCT {}", scalar_expr_label(expr))
                            } else {
                                scalar_expr_label(expr)
                            }
                        }
                        crate::sql::ast::AggregateArg::GroupConcat {
                            expr,
                            separator,
                            distinct,
                            ..
                        } => {
                            let expr = if *distinct {
                                format!("DISTINCT {}", scalar_expr_label(expr))
                            } else {
                                scalar_expr_label(expr)
                            };
                            if let Some(separator) = separator {
                                format!("{expr}, {}", scalar_expr_label(separator))
                            } else {
                                expr
                            }
                        }
                        crate::sql::ast::AggregateArg::JsonGroupObject { key, value, .. } => {
                            format!("{}, {}", scalar_expr_label(key), scalar_expr_label(value))
                        }
                        crate::sql::ast::AggregateArg::Percentile { expr, fraction, .. } => {
                            format!(
                                "{}, {}",
                                scalar_expr_label(expr),
                                scalar_expr_label(fraction)
                            )
                        }
                    }
                )
            }),
        })
        .collect()
}

fn scalar_expr_label(expr: &crate::sql::ast::ScalarExpr) -> String {
    match expr {
        crate::sql::ast::ScalarExpr::Literal(value) => value.to_string(),
        crate::sql::ast::ScalarExpr::Column(name) => name.clone(),
        crate::sql::ast::ScalarExpr::UnaryMinus(expr) => format!("-{}", scalar_expr_label(expr)),
        crate::sql::ast::ScalarExpr::BitNot(expr) => format!("~{}", scalar_expr_label(expr)),
        crate::sql::ast::ScalarExpr::Not(expr) => format!("NOT {}", scalar_expr_label(expr)),
        crate::sql::ast::ScalarExpr::Cast { expr, ty } => {
            format!("CAST({} AS {})", scalar_expr_label(expr), ty.name())
        }
        crate::sql::ast::ScalarExpr::Collate { expr, collation } => {
            format!("{} COLLATE {}", scalar_expr_label(expr), collation)
        }
        crate::sql::ast::ScalarExpr::Is {
            left,
            right,
            negated,
        } => format!(
            "{} IS {}{}",
            scalar_expr_label(left),
            if *negated { "NOT " } else { "" },
            scalar_expr_label(right)
        ),
        crate::sql::ast::ScalarExpr::IsBool {
            expr,
            value,
            negated,
        } => format!(
            "{} IS {}{}",
            scalar_expr_label(expr),
            if *negated { "NOT " } else { "" },
            if *value { "TRUE" } else { "FALSE" }
        ),
        crate::sql::ast::ScalarExpr::InList {
            expr,
            values,
            negated,
        } => format!(
            "{} {}IN ({})",
            scalar_expr_label(expr),
            if *negated { "NOT " } else { "" },
            values
                .iter()
                .map(scalar_expr_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        crate::sql::ast::ScalarExpr::InSubquery {
            expr,
            query: _,
            negated,
        } => format!(
            "{} {}IN (SELECT ...)",
            scalar_expr_label(expr),
            if *negated { "NOT " } else { "" }
        ),
        crate::sql::ast::ScalarExpr::Subquery { .. } => "(SELECT ...)".to_string(),
        crate::sql::ast::ScalarExpr::Like {
            expr,
            pattern,
            escape,
            negated,
        } => format!(
            "{} {}LIKE '{}'{}",
            scalar_expr_label(expr),
            if *negated { "NOT " } else { "" },
            pattern,
            escape
                .as_ref()
                .map(|escape| format!(" ESCAPE '{}'", escape.replace('\'', "''")))
                .unwrap_or_default()
        ),
        crate::sql::ast::ScalarExpr::Glob {
            expr,
            pattern,
            negated,
        } => format!(
            "{} {}GLOB '{}'",
            scalar_expr_label(expr),
            if *negated { "NOT " } else { "" },
            pattern
        ),
        crate::sql::ast::ScalarExpr::Between {
            expr,
            low,
            high,
            negated,
        } => format!(
            "{} {}BETWEEN {} AND {}",
            scalar_expr_label(expr),
            if *negated { "NOT " } else { "" },
            scalar_expr_label(low),
            scalar_expr_label(high)
        ),
        crate::sql::ast::ScalarExpr::Compare { left, op, right } => format!(
            "{} {} {}",
            scalar_expr_label(left),
            match op {
                crate::sql::ast::CompareOp::Eq => "=",
                crate::sql::ast::CompareOp::Ne => "!=",
                crate::sql::ast::CompareOp::Gt => ">",
                crate::sql::ast::CompareOp::Gte => ">=",
                crate::sql::ast::CompareOp::Lt => "<",
                crate::sql::ast::CompareOp::Lte => "<=",
            },
            scalar_expr_label(right)
        ),
        crate::sql::ast::ScalarExpr::CompareSubquery { left, op, query: _ } => format!(
            "{} {} (SELECT ...)",
            scalar_expr_label(left),
            match op {
                crate::sql::ast::CompareOp::Eq => "=",
                crate::sql::ast::CompareOp::Ne => "!=",
                crate::sql::ast::CompareOp::Gt => ">",
                crate::sql::ast::CompareOp::Gte => ">=",
                crate::sql::ast::CompareOp::Lt => "<",
                crate::sql::ast::CompareOp::Lte => "<=",
            }
        ),
        crate::sql::ast::ScalarExpr::Case {
            base,
            when_then_clauses,
            else_expr,
        } => {
            let mut parts = vec!["CASE".to_string()];
            if let Some(base) = base {
                parts.push(scalar_expr_label(base));
            }
            for (when_expr, then_expr) in when_then_clauses {
                parts.push(format!(
                    "WHEN {} THEN {}",
                    scalar_expr_label(when_expr),
                    scalar_expr_label(then_expr)
                ));
            }
            if let Some(else_expr) = else_expr {
                parts.push(format!("ELSE {}", scalar_expr_label(else_expr)));
            }
            parts.push("END".to_string());
            parts.join(" ")
        }
        crate::sql::ast::ScalarExpr::Binary { left, op, right } => format!(
            "{} {} {}",
            scalar_expr_label(left),
            match op {
                crate::sql::ast::ScalarBinaryOp::Add => "+",
                crate::sql::ast::ScalarBinaryOp::Subtract => "-",
                crate::sql::ast::ScalarBinaryOp::Multiply => "*",
                crate::sql::ast::ScalarBinaryOp::Divide => "/",
                crate::sql::ast::ScalarBinaryOp::Modulo => "%",
                crate::sql::ast::ScalarBinaryOp::BitAnd => "&",
                crate::sql::ast::ScalarBinaryOp::BitOr => "|",
                crate::sql::ast::ScalarBinaryOp::ShiftLeft => "<<",
                crate::sql::ast::ScalarBinaryOp::ShiftRight => ">>",
                crate::sql::ast::ScalarBinaryOp::Concat => "||",
                crate::sql::ast::ScalarBinaryOp::JsonExtract => "->",
                crate::sql::ast::ScalarBinaryOp::JsonExtractText => "->>",
            },
            scalar_expr_label(right)
        ),
        crate::sql::ast::ScalarExpr::Function { func, args } => format!(
            "{}({})",
            match func {
                crate::sql::ast::ScalarFunc::Length => "LENGTH",
                crate::sql::ast::ScalarFunc::OctetLength => "OCTET_LENGTH",
                crate::sql::ast::ScalarFunc::MinScalar => "MIN",
                crate::sql::ast::ScalarFunc::MaxScalar => "MAX",
                crate::sql::ast::ScalarFunc::Date => "DATE",
                crate::sql::ast::ScalarFunc::Time => "TIME",
                crate::sql::ast::ScalarFunc::DateTime => "DATETIME",
                crate::sql::ast::ScalarFunc::TimeDiff => "TIMEDIFF",
                crate::sql::ast::ScalarFunc::Strftime => "STRFTIME",
                crate::sql::ast::ScalarFunc::JulianDay => "JULIANDAY",
                crate::sql::ast::ScalarFunc::UnixEpoch => "UNIXEPOCH",
                crate::sql::ast::ScalarFunc::Changes => "CHANGES",
                crate::sql::ast::ScalarFunc::TotalChanges => "TOTAL_CHANGES",
                crate::sql::ast::ScalarFunc::Printf => "PRINTF",
                crate::sql::ast::ScalarFunc::IIf => "IIF",
                crate::sql::ast::ScalarFunc::If => "IF",
                crate::sql::ast::ScalarFunc::Concat => "CONCAT",
                crate::sql::ast::ScalarFunc::ConcatWs => "CONCAT_WS",
                crate::sql::ast::ScalarFunc::SqliteSourceId => "SQLITE_SOURCE_ID",
                crate::sql::ast::ScalarFunc::Sign => "SIGN",
                crate::sql::ast::ScalarFunc::RandomBlob => "RANDOMBLOB",
                crate::sql::ast::ScalarFunc::Random => "RANDOM",
                crate::sql::ast::ScalarFunc::Unhex => "UNHEX",
                crate::sql::ast::ScalarFunc::Unistr => "UNISTR",
                crate::sql::ast::ScalarFunc::UnistrQuote => "UNISTR_QUOTE",
                crate::sql::ast::ScalarFunc::SqliteVersion => "SQLITE_VERSION",
                crate::sql::ast::ScalarFunc::SqliteCompileOptionUsed => {
                    "SQLITE_COMPILEOPTION_USED"
                }
                crate::sql::ast::ScalarFunc::SqliteCompileOptionGet => "SQLITE_COMPILEOPTION_GET",
                crate::sql::ast::ScalarFunc::Likely => "LIKELY",
                crate::sql::ast::ScalarFunc::Unlikely => "UNLIKELY",
                crate::sql::ast::ScalarFunc::Likelihood => "LIKELIHOOD",
                crate::sql::ast::ScalarFunc::Mod => "MOD",
                crate::sql::ast::ScalarFunc::Ceil => "CEIL",
                crate::sql::ast::ScalarFunc::Ceiling => "CEILING",
                crate::sql::ast::ScalarFunc::Floor => "FLOOR",
                crate::sql::ast::ScalarFunc::Trunc => "TRUNC",
                crate::sql::ast::ScalarFunc::Pi => "PI",
                crate::sql::ast::ScalarFunc::Sqrt => "SQRT",
                crate::sql::ast::ScalarFunc::Power => "POWER",
                crate::sql::ast::ScalarFunc::Exp => "EXP",
                crate::sql::ast::ScalarFunc::Sin => "SIN",
                crate::sql::ast::ScalarFunc::Cos => "COS",
                crate::sql::ast::ScalarFunc::Tan => "TAN",
                crate::sql::ast::ScalarFunc::Sinh => "SINH",
                crate::sql::ast::ScalarFunc::Cosh => "COSH",
                crate::sql::ast::ScalarFunc::Tanh => "TANH",
                crate::sql::ast::ScalarFunc::Acos => "ACOS",
                crate::sql::ast::ScalarFunc::Asin => "ASIN",
                crate::sql::ast::ScalarFunc::Atan => "ATAN",
                crate::sql::ast::ScalarFunc::Atan2 => "ATAN2",
                crate::sql::ast::ScalarFunc::Acosh => "ACOSH",
                crate::sql::ast::ScalarFunc::Asinh => "ASINH",
                crate::sql::ast::ScalarFunc::Atanh => "ATANH",
                crate::sql::ast::ScalarFunc::Ln => "LN",
                crate::sql::ast::ScalarFunc::Log10 => "LOG10",
                crate::sql::ast::ScalarFunc::Log2 => "LOG2",
                crate::sql::ast::ScalarFunc::Log => "LOG",
                crate::sql::ast::ScalarFunc::Degrees => "DEGREES",
                crate::sql::ast::ScalarFunc::Radians => "RADIANS",
                crate::sql::ast::ScalarFunc::Char => "CHAR",
                crate::sql::ast::ScalarFunc::ZeroBlob => "ZEROBLOB",
                crate::sql::ast::ScalarFunc::TypeOf => "TYPEOF",
                crate::sql::ast::ScalarFunc::Subtype => "SUBTYPE",
                crate::sql::ast::ScalarFunc::Hex => "HEX",
                crate::sql::ast::ScalarFunc::Substr => "SUBSTR",
                crate::sql::ast::ScalarFunc::Instr => "INSTR",
                crate::sql::ast::ScalarFunc::Replace => "REPLACE",
                crate::sql::ast::ScalarFunc::LikeFunc => "LIKE",
                crate::sql::ast::ScalarFunc::GlobFunc => "GLOB",
                crate::sql::ast::ScalarFunc::Quote => "QUOTE",
                crate::sql::ast::ScalarFunc::Unicode => "UNICODE",
                crate::sql::ast::ScalarFunc::Trim => "TRIM",
                crate::sql::ast::ScalarFunc::LTrim => "LTRIM",
                crate::sql::ast::ScalarFunc::RTrim => "RTRIM",
                crate::sql::ast::ScalarFunc::Lower => "LOWER",
                crate::sql::ast::ScalarFunc::Upper => "UPPER",
                crate::sql::ast::ScalarFunc::Abs => "ABS",
                crate::sql::ast::ScalarFunc::Round => "ROUND",
                crate::sql::ast::ScalarFunc::LastInsertRowId => "LAST_INSERT_ROWID",
                crate::sql::ast::ScalarFunc::Coalesce => "COALESCE",
                crate::sql::ast::ScalarFunc::IfNull => "IFNULL",
                crate::sql::ast::ScalarFunc::NullIf => "NULLIF",
                crate::sql::ast::ScalarFunc::Json => "JSON",
                crate::sql::ast::ScalarFunc::JsonValid => "JSON_VALID",
                crate::sql::ast::ScalarFunc::JsonErrorPosition => "JSON_ERROR_POSITION",
                crate::sql::ast::ScalarFunc::JsonPretty => "JSON_PRETTY",
                crate::sql::ast::ScalarFunc::JsonQuote => "JSON_QUOTE",
                crate::sql::ast::ScalarFunc::JsonExtract => "JSON_EXTRACT",
                crate::sql::ast::ScalarFunc::JsonType => "JSON_TYPE",
                crate::sql::ast::ScalarFunc::JsonArray => "JSON_ARRAY",
                crate::sql::ast::ScalarFunc::JsonObject => "JSON_OBJECT",
                crate::sql::ast::ScalarFunc::JsonArrayLength => "JSON_ARRAY_LENGTH",
                crate::sql::ast::ScalarFunc::JsonRemove => "JSON_REMOVE",
                crate::sql::ast::ScalarFunc::JsonSet => "JSON_SET",
                crate::sql::ast::ScalarFunc::JsonInsert => "JSON_INSERT",
                crate::sql::ast::ScalarFunc::JsonReplace => "JSON_REPLACE",
                crate::sql::ast::ScalarFunc::JsonPatch => "JSON_PATCH",
            },
            args.iter()
                .map(scalar_expr_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        crate::sql::ast::ScalarExpr::Aggregate { func, arg, .. } => {
            aggregate_expr_label(*func, arg)
        }
        crate::sql::ast::ScalarExpr::Tuple(values) => format!(
            "({})",
            values
                .iter()
                .map(scalar_expr_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn aggregate_expr_label(
    func: crate::sql::ast::AggregateFunc,
    arg: &crate::sql::ast::AggregateArg,
) -> String {
    format!(
        "{}({})",
        match func {
            crate::sql::ast::AggregateFunc::Count => "COUNT",
            crate::sql::ast::AggregateFunc::Sum => "SUM",
            crate::sql::ast::AggregateFunc::Avg => "AVG",
            crate::sql::ast::AggregateFunc::Total => "TOTAL",
            crate::sql::ast::AggregateFunc::Median => "MEDIAN",
            crate::sql::ast::AggregateFunc::Percentile => "PERCENTILE",
            crate::sql::ast::AggregateFunc::PercentileCont => "PERCENTILE_CONT",
            crate::sql::ast::AggregateFunc::PercentileDisc => "PERCENTILE_DISC",
            crate::sql::ast::AggregateFunc::GroupConcat => "GROUP_CONCAT",
            crate::sql::ast::AggregateFunc::JsonGroupArray => "JSON_GROUP_ARRAY",
            crate::sql::ast::AggregateFunc::JsonGroupObject => "JSON_GROUP_OBJECT",
            crate::sql::ast::AggregateFunc::Min => "MIN",
            crate::sql::ast::AggregateFunc::Max => "MAX",
        },
        match arg {
            crate::sql::ast::AggregateArg::Wildcard => "*".to_string(),
            crate::sql::ast::AggregateArg::Expr { expr, distinct, .. } => {
                if *distinct {
                    format!("DISTINCT {}", scalar_expr_label(expr))
                } else {
                    scalar_expr_label(expr)
                }
            }
            crate::sql::ast::AggregateArg::GroupConcat {
                expr,
                separator,
                distinct,
                ..
            } => {
                let expr = if *distinct {
                    format!("DISTINCT {}", scalar_expr_label(expr))
                } else {
                    scalar_expr_label(expr)
                };
                if let Some(separator) = separator {
                    format!("{expr}, {}", scalar_expr_label(separator))
                } else {
                    expr
                }
            }
            crate::sql::ast::AggregateArg::JsonGroupObject { key, value, .. } => {
                format!("{}, {}", scalar_expr_label(key), scalar_expr_label(value))
            }
            crate::sql::ast::AggregateArg::Percentile { expr, fraction, .. } => {
                format!(
                    "{}, {}",
                    scalar_expr_label(expr),
                    scalar_expr_label(fraction)
                )
            }
        }
    )
}

fn generic_headers(width: usize) -> Vec<String> {
    (1..=width).map(|index| format!("col{index}")).collect()
}
