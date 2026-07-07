use crate::common::error::{DbError, Result};
use crate::common::types::{
    CheckConstraint, CheckExpr, CheckOp, ColumnDef, ColumnDefault, ColumnType, ForeignKey,
    PrimaryKeyConstraint, SortOrder, UniqueConstraint, Value,
};
use crate::sql::ast::{
    AggregateArg, AggregateFunc, AlterTableAction, Assignment, CommonTableExpr, CompareOp,
    CompoundOperator, CompoundSelect, CteBody, Expr, FromItem, IsolationLevel, JoinClause,
    JoinKind, NullOrder, OrderBy, OrderByExpr, SINGLE_ROW_SOURCE_TABLE, ScalarBinaryOp, ScalarExpr,
    ScalarFunc, SelectItem, SelectStatement, Statement, TableConstraint, UpsertClause, WithClause,
};
use crate::sql::lexer::{Token, TokenKind, lex};

#[derive(Debug, Clone)]
enum InsertConflictSuffix {
    Legacy(Option<String>),
    DoNothing { target: Option<Vec<String>> },
    DoUpdate { upsert: UpsertClause },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedTableIndexHint {
    IndexedBy(String),
    NotIndexed,
}

pub fn parse_sql(input: &str) -> Result<Vec<Statement>> {
    let tokens = lex(input)?;
    let mut parser = Parser::new(tokens);

    if matches!(parser.peek_kind(), TokenKind::Eof) {
        return Err(DbError::sql("empty SQL input"));
    }

    let mut statements = Vec::new();
    loop {
        statements.push(parser.parse_statement()?);

        if !parser.matches(&TokenKind::Semicolon) {
            break;
        }

        while parser.matches(&TokenKind::Semicolon) {}
        if matches!(parser.peek_kind(), TokenKind::Eof) {
            break;
        }
    }

    parser.expect_eof()?;

    Ok(statements)
}

pub fn parse_scalar_sql_expression(input: &str) -> Result<ScalarExpr> {
    let tokens = lex(input)?;
    let mut parser = Parser::new(tokens);

    if matches!(parser.peek_kind(), TokenKind::Eof) {
        return Err(DbError::sql("empty SQL input"));
    }

    let expr = parser.parse_scalar_expr()?;
    parser.expect_eof()?;
    Ok(expr)
}

pub fn parse_check_constraint_expression(input: &str) -> Result<CheckExpr> {
    let tokens = lex(input)?;
    let mut parser = Parser::new(tokens);

    if matches!(parser.peek_kind(), TokenKind::Eof) {
        return Err(DbError::sql("empty SQL input"));
    }

    let expr = parser.parse_where_expr()?;
    parser.expect_eof()?;
    Parser::check_expr_from_expr(expr)
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, index: 0 }
    }

    fn parse_statement(&mut self) -> Result<Statement> {
        match self.peek_kind() {
            TokenKind::Create => self.parse_create(),
            TokenKind::Alter => self.parse_alter(),
            TokenKind::Replace => self.parse_replace(),
            TokenKind::Drop => self.parse_drop(),
            TokenKind::Insert => self.parse_insert(),
            TokenKind::With => self.parse_with_statement(),
            TokenKind::Select => Ok(Statement::Select(self.parse_select_statement()?)),
            TokenKind::Values => self.parse_values_statement(),
            TokenKind::Explain => self.parse_explain(),
            TokenKind::Pragma => self.parse_pragma(),
            TokenKind::Analyze => {
                self.advance();
                self.parse_optional_maintenance_target()?;
                Ok(Statement::Analyze)
            }
            TokenKind::Reindex => {
                self.advance();
                self.parse_optional_maintenance_target()?;
                Ok(Statement::Reindex)
            }
            TokenKind::Vacuum => {
                self.advance();
                self.parse_optional_maintenance_target()?;
                Ok(Statement::Vacuum)
            }
            TokenKind::Delete => self.parse_delete(),
            TokenKind::Update => self.parse_update(),
            TokenKind::Begin | TokenKind::Start => self.parse_begin_or_start_transaction(),
            TokenKind::Commit => {
                self.advance();
                Ok(Statement::Commit)
            }
            TokenKind::Rollback => {
                self.advance();
                Ok(Statement::Rollback)
            }
            TokenKind::Eof => Err(DbError::sql("empty SQL input")),
            token => {
                Err(self.error_expected(&format!("statement, found {}", display_token(token))))
            }
        }
    }

    fn parse_optional_maintenance_target(&mut self) -> Result<()> {
        if !is_identifier_token(self.peek_kind()) {
            return Ok(());
        }
        let _ = self.parse_simple_identifier()?;
        if self.matches(&TokenKind::Dot) {
            let _ = self.parse_simple_identifier()?;
        }
        Ok(())
    }

    fn parse_alter(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Alter)?;
        self.expect_keyword(TokenKind::Table)?;
        let table = self.parse_simple_identifier()?;

        if self.matches(&TokenKind::Add) {
            let _ = self.matches(&TokenKind::Column);
            let column = self.parse_column_def(Some(&table))?;
            return Ok(Statement::AlterTable {
                table,
                action: AlterTableAction::AddColumn(column),
            });
        }

        if self.matches(&TokenKind::Drop) {
            self.expect_keyword(TokenKind::Column)?;
            let old_name = self.parse_simple_identifier()?;
            return Ok(Statement::AlterTable {
                table,
                action: AlterTableAction::DropColumn { old_name },
            });
        }

        self.expect_keyword(TokenKind::Rename)?;
        if self.matches(&TokenKind::Column) {
            let old_name = self.parse_simple_identifier()?;
            self.expect_keyword(TokenKind::To)?;
            let new_name = self.parse_simple_identifier()?;
            return Ok(Statement::AlterTable {
                table,
                action: AlterTableAction::RenameColumn { old_name, new_name },
            });
        }

        if self.matches(&TokenKind::To) {
            let new_name = self.parse_simple_identifier()?;
            return Ok(Statement::AlterTable {
                table,
                action: AlterTableAction::RenameTable { new_name },
            });
        }

        Err(self.error_expected(&format!(
            "COLUMN or TO after RENAME, found {}",
            display_token(self.peek_kind())
        )))
    }

    fn parse_explain(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Explain)?;
        self.expect_keyword(TokenKind::Query)?;
        self.expect_keyword(TokenKind::Plan)?;
        match self.peek_kind() {
            TokenKind::Select => Ok(Statement::ExplainQueryPlan(Box::new(Statement::Select(
                self.parse_select_statement()?,
            )))),
            token => Err(self.error_expected(&format!(
                "SELECT after EXPLAIN QUERY PLAN, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_pragma(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Pragma)?;
        let mut name = self.parse_simple_identifier()?;
        let schema = if self.matches(&TokenKind::Dot) {
            let schema = name;
            if !schema.eq_ignore_ascii_case("main") && !schema.eq_ignore_ascii_case("temp") {
                return Err(DbError::sql(format!("unknown database {schema}")));
            }
            name = self.parse_simple_identifier()?;
            Some(schema)
        } else {
            None
        };
        if name.eq_ignore_ascii_case("database_list") {
            return Ok(Statement::PragmaDatabaseList);
        }
        if name.eq_ignore_ascii_case("page_size") {
            return Ok(Statement::PragmaPageSize);
        }
        if name.eq_ignore_ascii_case("page_count") {
            return Ok(Statement::PragmaPageCount);
        }
        if name.eq_ignore_ascii_case("freelist_count") {
            return Ok(Statement::PragmaFreelistCount);
        }
        if name.eq_ignore_ascii_case("user_version") {
            if self.matches(&TokenKind::Eq) {
                let value = match self.peek_kind() {
                    TokenKind::Integer(value) if *value >= 0 => {
                        let value = u32::try_from(*value)
                            .map_err(|_| DbError::sql("PRAGMA user_version value is too large"))?;
                        self.advance();
                        value
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "non-negative integer user_version, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaUserVersion { value });
            }
            return Ok(Statement::PragmaUserVersion);
        }
        if name.eq_ignore_ascii_case("application_id") {
            if self.matches(&TokenKind::Eq) {
                let value = match self.peek_kind() {
                    TokenKind::Integer(value) if *value >= 0 => {
                        let value = u32::try_from(*value).map_err(|_| {
                            DbError::sql("PRAGMA application_id value is too large")
                        })?;
                        self.advance();
                        value
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "non-negative integer application_id, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaApplicationId { value });
            }
            return Ok(Statement::PragmaApplicationId);
        }
        if name.eq_ignore_ascii_case("schema_version") {
            if self.matches(&TokenKind::Eq) {
                let value = match self.peek_kind() {
                    TokenKind::Integer(value) if *value >= 0 => {
                        let value = u32::try_from(*value).map_err(|_| {
                            DbError::sql("PRAGMA schema_version value is too large")
                        })?;
                        self.advance();
                        value
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "non-negative integer schema_version, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaSchemaVersion { value });
            }
            return Ok(Statement::PragmaSchemaVersion);
        }
        if name.eq_ignore_ascii_case("foreign_keys") {
            if self.matches(&TokenKind::Eq) {
                let enabled = match self.peek_kind() {
                    TokenKind::On | TokenKind::True => {
                        self.advance();
                        true
                    }
                    TokenKind::Off | TokenKind::False => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(0) => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(1) => {
                        self.advance();
                        true
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "ON, OFF, 1, or 0 for foreign_keys, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaForeignKeys { enabled });
            }
            return Ok(Statement::PragmaForeignKeys);
        }
        if name.eq_ignore_ascii_case("read_uncommitted") {
            if self.matches(&TokenKind::Eq) {
                let enabled = match self.peek_kind() {
                    TokenKind::On | TokenKind::True => {
                        self.advance();
                        true
                    }
                    TokenKind::Off | TokenKind::False => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(0) => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(1) => {
                        self.advance();
                        true
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "ON, OFF, 1, or 0 for read_uncommitted, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaReadUncommitted { enabled });
            }
            return Ok(Statement::PragmaReadUncommitted);
        }
        if name.eq_ignore_ascii_case("query_only") {
            if self.matches(&TokenKind::Eq) {
                let enabled = match self.peek_kind() {
                    TokenKind::On | TokenKind::True => {
                        self.advance();
                        true
                    }
                    TokenKind::Off | TokenKind::False => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(0) => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(1) => {
                        self.advance();
                        true
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "ON, OFF, 1, or 0 for query_only, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaQueryOnly { enabled });
            }
            return Ok(Statement::PragmaQueryOnly);
        }
        if name.eq_ignore_ascii_case("recursive_triggers") {
            if self.matches(&TokenKind::Eq) {
                let enabled = match self.peek_kind() {
                    TokenKind::On | TokenKind::True => {
                        self.advance();
                        true
                    }
                    TokenKind::Off | TokenKind::False => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(0) => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(1) => {
                        self.advance();
                        true
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "ON, OFF, 1, or 0 for recursive_triggers, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaRecursiveTriggers { enabled });
            }
            return Ok(Statement::PragmaRecursiveTriggers);
        }
        if name.eq_ignore_ascii_case("trusted_schema") {
            if self.matches(&TokenKind::Eq) {
                let enabled = match self.peek_kind() {
                    TokenKind::On | TokenKind::True => {
                        self.advance();
                        true
                    }
                    TokenKind::Off | TokenKind::False => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(0) => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(1) => {
                        self.advance();
                        true
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "ON, OFF, 1, or 0 for trusted_schema, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaTrustedSchema { enabled });
            }
            return Ok(Statement::PragmaTrustedSchema);
        }
        if name.eq_ignore_ascii_case("ignore_check_constraints") {
            if self.matches(&TokenKind::Eq) {
                let enabled = match self.peek_kind() {
                    TokenKind::On | TokenKind::True => {
                        self.advance();
                        true
                    }
                    TokenKind::Off | TokenKind::False => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(0) => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(1) => {
                        self.advance();
                        true
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "ON, OFF, 1, or 0 for ignore_check_constraints, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaIgnoreCheckConstraints { enabled });
            }
            return Ok(Statement::PragmaIgnoreCheckConstraints);
        }
        if name.eq_ignore_ascii_case("encoding") {
            return Ok(Statement::PragmaEncoding);
        }
        if name.eq_ignore_ascii_case("collation_list") {
            return Ok(Statement::PragmaCollationList);
        }
        if name.eq_ignore_ascii_case("data_version") {
            return Ok(Statement::PragmaDataVersion);
        }
        if name.eq_ignore_ascii_case("quick_check") {
            self.parse_optional_pragma_scalar_argument()?;
            return Ok(Statement::PragmaQuickCheck);
        }
        if name.eq_ignore_ascii_case("integrity_check") {
            self.parse_optional_pragma_scalar_argument()?;
            return Ok(Statement::PragmaIntegrityCheck);
        }
        if name.eq_ignore_ascii_case("function_list") {
            return Ok(Statement::PragmaFunctionList);
        }
        if name.eq_ignore_ascii_case("compile_options") {
            return Ok(Statement::PragmaCompileOptions);
        }
        if name.eq_ignore_ascii_case("journal_mode") {
            return Ok(Statement::PragmaJournalMode);
        }
        if name.eq_ignore_ascii_case("synchronous") {
            return Ok(Statement::PragmaSynchronous);
        }
        if name.eq_ignore_ascii_case("cache_size") {
            if self.matches(&TokenKind::Eq) {
                let value = self.parse_signed_pragma_integer("cache_size")?;
                return Ok(Statement::SetPragmaCacheSize { value });
            }
            return Ok(Statement::PragmaCacheSize);
        }
        if name.eq_ignore_ascii_case("temp_store") {
            return Ok(Statement::PragmaTempStore);
        }
        if name.eq_ignore_ascii_case("locking_mode") {
            return Ok(Statement::PragmaLockingMode);
        }
        if name.eq_ignore_ascii_case("busy_timeout") {
            if self.matches(&TokenKind::Eq) {
                let value = self.parse_signed_pragma_integer("busy_timeout")?;
                return Ok(Statement::SetPragmaBusyTimeout { value });
            }
            return Ok(Statement::PragmaBusyTimeout);
        }
        if name.eq_ignore_ascii_case("threads") {
            if self.matches(&TokenKind::Eq) {
                let value = match self.peek_kind() {
                    TokenKind::Integer(value) if *value >= 0 => {
                        let value = u32::try_from(*value)
                            .map_err(|_| DbError::sql("PRAGMA threads value is too large"))?;
                        self.advance();
                        value
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "non-negative integer threads value, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaThreads { value });
            }
            return Ok(Statement::PragmaThreads);
        }
        if name.eq_ignore_ascii_case("case_sensitive_like") {
            if self.matches(&TokenKind::Eq) {
                let enabled = match self.peek_kind() {
                    TokenKind::On | TokenKind::True => {
                        self.advance();
                        true
                    }
                    TokenKind::Off | TokenKind::False => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(0) => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(1) => {
                        self.advance();
                        true
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "ON, OFF, 1, or 0 for case_sensitive_like, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaCaseSensitiveLike { enabled });
            }
            return Ok(Statement::PragmaCaseSensitiveLike);
        }
        if name.eq_ignore_ascii_case("reverse_unordered_selects") {
            if self.matches(&TokenKind::Eq) {
                let enabled = match self.peek_kind() {
                    TokenKind::On | TokenKind::True => {
                        self.advance();
                        true
                    }
                    TokenKind::Off | TokenKind::False => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(0) => {
                        self.advance();
                        false
                    }
                    TokenKind::Integer(1) => {
                        self.advance();
                        true
                    }
                    token => {
                        return Err(self.error_expected(&format!(
                            "ON, OFF, 1, or 0 for reverse_unordered_selects, found {}",
                            display_token(token)
                        )));
                    }
                };
                return Ok(Statement::SetPragmaReverseUnorderedSelects { enabled });
            }
            return Ok(Statement::PragmaReverseUnorderedSelects);
        }
        if name.eq_ignore_ascii_case("optimize") {
            if self.matches(&TokenKind::Eq) {
                if is_scalar_expr_start(self.peek_kind()) {
                    let _ = self.parse_scalar_expr()?;
                } else {
                    return Err(self.error_expected(&format!(
                        "PRAGMA optimize argument, found {}",
                        display_token(self.peek_kind())
                    )));
                }
            }
            return Ok(Statement::PragmaOptimize);
        }
        if name.eq_ignore_ascii_case("table_list") {
            let table = if self.matches(&TokenKind::LParen) {
                let table = self.parse_pragma_name_argument()?;
                self.expect_symbol(TokenKind::RParen)?;
                Some(table)
            } else {
                None
            };
            return Ok(Statement::PragmaTableList { table, schema });
        }
        if name.eq_ignore_ascii_case("foreign_key_check") {
            let table = if self.matches(&TokenKind::LParen) {
                let table = self.parse_pragma_name_argument()?;
                self.expect_symbol(TokenKind::RParen)?;
                Some(table)
            } else {
                None
            };
            return Ok(Statement::PragmaForeignKeyCheck { table });
        }
        let table = self.parse_pragma_name_argument_in_parens_or_equals()?;
        if name.eq_ignore_ascii_case("table_info") {
            Ok(Statement::PragmaTableInfo { table })
        } else if name.eq_ignore_ascii_case("table_xinfo") {
            Ok(Statement::PragmaTableXInfo { table })
        } else if name.eq_ignore_ascii_case("index_list") {
            Ok(Statement::PragmaIndexList { table })
        } else if name.eq_ignore_ascii_case("index_info") {
            Ok(Statement::PragmaIndexInfo { index: table })
        } else if name.eq_ignore_ascii_case("index_xinfo") {
            Ok(Statement::PragmaIndexXInfo { index: table })
        } else if name.eq_ignore_ascii_case("foreign_key_list") {
            Ok(Statement::PragmaForeignKeyList { table })
        } else {
            Err(DbError::sql(format!("unsupported PRAGMA: {name}")))
        }
    }

    fn parse_pragma_name_argument(&mut self) -> Result<String> {
        match self.peek_kind() {
            TokenKind::String(value) => {
                let value = value.clone();
                self.advance();
                Ok(value)
            }
            _ => self.parse_simple_identifier(),
        }
    }

    fn parse_pragma_name_argument_in_parens_or_equals(&mut self) -> Result<String> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_name_argument();
        }
        self.expect_symbol(TokenKind::LParen)?;
        let value = self.parse_pragma_name_argument()?;
        self.expect_symbol(TokenKind::RParen)?;
        Ok(value)
    }

    fn parse_optional_pragma_scalar_argument(&mut self) -> Result<()> {
        if self.matches(&TokenKind::LParen) {
            let _ = self.parse_scalar_expr()?;
            self.expect_symbol(TokenKind::RParen)?;
        }
        Ok(())
    }

    fn parse_signed_pragma_integer(&mut self, pragma_name: &str) -> Result<i64> {
        match self.peek_kind() {
            TokenKind::Integer(value) => {
                let value = *value;
                self.advance();
                Ok(value)
            }
            TokenKind::Minus => {
                self.advance();
                match self.peek_kind() {
                    TokenKind::Integer(value) => {
                        let value = value.checked_neg().ok_or_else(|| {
                            DbError::sql(format!("PRAGMA {pragma_name} value is too small"))
                        })?;
                        self.advance();
                        Ok(value)
                    }
                    token => Err(self.error_expected(&format!(
                        "integer {pragma_name} value, found {}",
                        display_token(token)
                    ))),
                }
            }
            token => Err(self.error_expected(&format!(
                "integer {pragma_name} value, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Create)?;
        if self.parse_optional_identifier_keyword_if("TEMP")
            || self.parse_optional_identifier_keyword_if("TEMPORARY")
        {
            return self.parse_create_table();
        }
        match self.peek_kind() {
            TokenKind::Table => self.parse_create_table(),
            TokenKind::Index => self.parse_create_index(false),
            TokenKind::Unique => {
                self.advance();
                self.parse_create_index(true)
            }
            token => Err(self.error_expected(&format!(
                "TABLE, INDEX, or UNIQUE INDEX, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_create_table(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Table)?;
        let if_not_exists = self.parse_if_not_exists()?;
        let name = self.parse_identifier()?;
        if self.matches(&TokenKind::As) {
            return Ok(Statement::CreateTableAs {
                name,
                if_not_exists,
                select: self.parse_select_statement()?,
            });
        }
        self.expect_symbol(TokenKind::LParen)?;

        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            match self.peek_kind() {
                TokenKind::Check
                | TokenKind::Foreign
                | TokenKind::Constraint
                | TokenKind::Primary
                | TokenKind::Unique => constraints.push(self.parse_table_constraint(&name)?),
                _ => columns.push(self.parse_column_def(Some(&name))?),
            }
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }

        self.expect_symbol(TokenKind::RParen)?;
        let (strict, without_rowid) = self.parse_table_options()?;
        Ok(Statement::CreateTable {
            name,
            columns,
            constraints,
            strict,
            without_rowid,
            if_not_exists,
        })
    }

    fn parse_create_index(&mut self, unique: bool) -> Result<Statement> {
        self.expect_keyword(TokenKind::Index)?;
        let if_not_exists = self.parse_if_not_exists()?;
        let name = self.parse_identifier()?;
        self.expect_keyword(TokenKind::On)?;
        let table = self.parse_identifier()?;
        let (columns, decorated_columns) = self.parse_parenthesized_index_column_list()?;
        let predicate = if self.matches(&TokenKind::Where) {
            Some(self.parse_index_predicate_sql()?)
        } else {
            None
        };

        Ok(Statement::CreateIndex {
            name,
            table,
            columns,
            decorated_columns: Some(decorated_columns),
            unique,
            predicate,
            if_not_exists,
        })
    }

    fn parse_parenthesized_index_column_list(&mut self) -> Result<(Vec<String>, Vec<String>)> {
        self.expect_symbol(TokenKind::LParen)?;
        if matches!(self.peek_kind(), TokenKind::RParen) {
            return Err(self.error_expected("identifier"));
        }
        let mut columns = vec![self.parse_indexed_column()?];
        while self.matches(&TokenKind::Comma) {
            columns.push(self.parse_indexed_column()?);
        }
        self.expect_symbol(TokenKind::RParen)?;
        let bare_columns = columns
            .iter()
            .map(|(column, _)| column.clone())
            .collect::<Vec<_>>();
        let decorated_columns = columns
            .into_iter()
            .map(|(_, decorated)| decorated)
            .collect::<Vec<_>>();
        Ok((bare_columns, decorated_columns))
    }

    fn parse_indexed_column(&mut self) -> Result<(String, String)> {
        let start = self.index;
        let expr = self.parse_scalar_expr()?;
        let parsed = join_sql_fragments(
            &self.tokens[start..self.index]
                .iter()
                .map(token_sql_fragment)
                .collect::<Vec<_>>(),
        );
        let column = match &expr {
            ScalarExpr::Column(name) => name.clone(),
            ScalarExpr::Collate { expr, .. } => match expr.as_ref() {
                ScalarExpr::Column(name) => name.clone(),
                _ => parsed.clone(),
            },
            _ => parsed.clone(),
        };
        let mut decorated = parsed;
        if let Some(collation) = self.parse_optional_collation_name()? {
            decorated.push_str(" COLLATE ");
            decorated.push_str(&collation);
        }
        if self.matches(&TokenKind::Asc) {
            decorated.push_str(" ASC");
        } else if self.matches(&TokenKind::Desc) {
            decorated.push_str(" DESC");
        }
        Ok((column, decorated))
    }

    fn parse_drop(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Drop)?;
        match self.peek_kind() {
            TokenKind::Table => {
                self.advance();
                let if_exists = self.parse_if_exists()?;
                Ok(Statement::DropTable {
                    name: self.parse_simple_identifier()?,
                    if_exists,
                })
            }
            TokenKind::Index => {
                self.advance();
                let if_exists = self.parse_if_exists()?;
                Ok(Statement::DropIndex {
                    name: self.parse_simple_identifier()?,
                    if_exists,
                })
            }
            token => {
                Err(self.error_expected(&format!("TABLE or INDEX, found {}", display_token(token))))
            }
        }
    }

    fn parse_if_not_exists(&mut self) -> Result<bool> {
        if !self.matches(&TokenKind::If) {
            return Ok(false);
        }
        self.expect_keyword(TokenKind::Not)?;
        self.expect_keyword(TokenKind::Exists)?;
        Ok(true)
    }

    fn parse_if_exists(&mut self) -> Result<bool> {
        if !self.matches(&TokenKind::If) {
            return Ok(false);
        }
        self.expect_keyword(TokenKind::Exists)?;
        Ok(true)
    }

    fn parse_table_options(&mut self) -> Result<(bool, bool)> {
        let mut strict = false;
        let mut without_rowid = false;

        loop {
            if !strict && self.matches(&TokenKind::Strict) {
                strict = true;
            } else if !without_rowid && self.parse_optional_identifier_keyword_if("WITHOUT") {
                if !self.parse_optional_identifier_keyword_if("ROWID") {
                    return Err(self.error_expected(&format!(
                        "ROWID after WITHOUT, found {}",
                        display_token(self.peek_kind())
                    )));
                }
                without_rowid = true;
            } else {
                break;
            }

            let _ = self.matches(&TokenKind::Comma);
        }

        Ok((strict, without_rowid))
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Insert)?;
        let or_conflict = if self.matches(&TokenKind::Or) {
            Some(self.parse_insert_or_conflict_resolution()?)
        } else {
            None
        };
        self.expect_keyword(TokenKind::Into)?;
        let table = self.parse_simple_identifier()?;
        let columns = if matches!(self.peek_kind(), TokenKind::LParen) {
            Some(self.parse_parenthesized_identifier_list()?)
        } else {
            None
        };
        if self.matches(&TokenKind::Default) {
            self.expect_keyword(TokenKind::Values)?;
            let conflict_suffix = self.parse_insert_conflict_suffix(or_conflict)?;
            let returning = self.parse_optional_returning_clause()?;
            return match conflict_suffix {
                InsertConflictSuffix::Legacy(or_conflict) => {
                    if let Some(returning) = returning {
                        Ok(Statement::InsertReturning {
                            table,
                            columns,
                            or_conflict,
                            values: Vec::new(),
                            returning,
                        })
                    } else {
                        Ok(Statement::Insert {
                            table,
                            columns,
                            or_conflict,
                            values: Vec::new(),
                        })
                    }
                }
                InsertConflictSuffix::DoNothing { target } => Ok(Statement::InsertDoNothing {
                    table,
                    columns,
                    target,
                    values: Vec::new(),
                }),
                InsertConflictSuffix::DoUpdate { .. } => Err(DbError::sql(
                    "ON CONFLICT DO UPDATE with DEFAULT VALUES is not supported yet",
                )),
            };
        }

        if matches!(self.peek_kind(), TokenKind::Select | TokenKind::With) {
            let select = if matches!(self.peek_kind(), TokenKind::With) {
                self.parse_with_select_statement()?
            } else {
                self.parse_select_statement()?
            };
            let conflict_suffix = self.parse_insert_conflict_suffix(or_conflict)?;
            let returning = self.parse_optional_returning_clause()?;
            return match conflict_suffix {
                InsertConflictSuffix::Legacy(or_conflict) => {
                    if let Some(returning) = returning {
                        Ok(Statement::InsertSelectReturning {
                            table,
                            columns,
                            or_conflict,
                            select: Box::new(select),
                            returning,
                        })
                    } else {
                        Ok(Statement::InsertSelect {
                            table,
                            columns,
                            or_conflict,
                            select: Box::new(select),
                        })
                    }
                }
                InsertConflictSuffix::DoNothing { target } => {
                    if let Some(returning) = returning {
                        Ok(Statement::InsertSelectDoNothingReturning {
                            table,
                            columns,
                            target,
                            select: Box::new(select),
                            returning,
                        })
                    } else {
                        Ok(Statement::InsertSelectDoNothing {
                            table,
                            columns,
                            target,
                            select: Box::new(select),
                        })
                    }
                }
                InsertConflictSuffix::DoUpdate { upsert } => {
                    if let Some(returning) = returning {
                        Ok(Statement::InsertSelectUpsertReturning {
                            table,
                            columns,
                            select: Box::new(select),
                            upsert,
                            returning,
                        })
                    } else {
                        Ok(Statement::InsertSelectUpsert {
                            table,
                            columns,
                            select: Box::new(select),
                            upsert,
                        })
                    }
                }
            };
        }

        self.expect_keyword(TokenKind::Values)?;
        let mut rows = vec![self.parse_parenthesized_scalar_exprs()?];
        while self.matches(&TokenKind::Comma) {
            rows.push(self.parse_parenthesized_scalar_exprs()?);
        }
        let conflict_suffix = self.parse_insert_conflict_suffix(or_conflict)?;
        let returning = self.parse_optional_returning_clause()?;

        if rows.len() == 1 {
            let values = rows.pop().unwrap_or_default();
            match conflict_suffix {
                InsertConflictSuffix::Legacy(or_conflict) => {
                    if let Some(literal_values) = scalar_expr_row_literal_values(&values) {
                        if let Some(returning) = returning {
                            Ok(Statement::InsertReturning {
                                table,
                                columns,
                                or_conflict,
                                values: literal_values,
                                returning,
                            })
                        } else {
                            Ok(Statement::Insert {
                                table,
                                columns,
                                or_conflict,
                                values: literal_values,
                            })
                        }
                    } else {
                        if let Some(returning) = returning {
                            Ok(Statement::InsertExprReturning {
                                table,
                                columns,
                                or_conflict,
                                values,
                                returning,
                            })
                        } else {
                            Ok(Statement::InsertExpr {
                                table,
                                columns,
                                or_conflict,
                                values,
                            })
                        }
                    }
                }
                InsertConflictSuffix::DoNothing { target } => {
                    if let Some(literal_values) = scalar_expr_row_literal_values(&values) {
                        if let Some(returning) = returning {
                            Ok(Statement::InsertDoNothingReturning {
                                table,
                                columns,
                                target,
                                values: literal_values,
                                returning,
                            })
                        } else {
                            Ok(Statement::InsertDoNothing {
                                table,
                                columns,
                                target,
                                values: literal_values,
                            })
                        }
                    } else {
                        if let Some(returning) = returning {
                            Ok(Statement::InsertExprDoNothingReturning {
                                table,
                                columns,
                                target,
                                values,
                                returning,
                            })
                        } else {
                            Ok(Statement::InsertExprDoNothing {
                                table,
                                columns,
                                target,
                                values,
                            })
                        }
                    }
                }
                InsertConflictSuffix::DoUpdate { upsert } => {
                    if let Some(literal_values) = scalar_expr_row_literal_values(&values) {
                        if let Some(returning) = returning {
                            Ok(Statement::InsertUpsertReturning {
                                table,
                                columns,
                                values: literal_values,
                                upsert,
                                returning,
                            })
                        } else {
                            Ok(Statement::InsertUpsert {
                                table,
                                columns,
                                values: literal_values,
                                upsert,
                            })
                        }
                    } else if let Some(returning) = returning {
                        Ok(Statement::InsertExprUpsertReturning {
                            table,
                            columns,
                            values,
                            upsert,
                            returning,
                        })
                    } else {
                        Ok(Statement::InsertExprUpsert {
                            table,
                            columns,
                            values,
                            upsert,
                        })
                    }
                }
            }
        } else {
            match conflict_suffix {
                InsertConflictSuffix::Legacy(or_conflict) => {
                    if let Some(literal_rows) = scalar_expr_rows_literal_values(&rows) {
                        if let Some(returning) = returning {
                            Ok(Statement::InsertManyReturning {
                                table,
                                columns,
                                or_conflict,
                                rows: literal_rows,
                                returning,
                            })
                        } else {
                            Ok(Statement::InsertMany {
                                table,
                                columns,
                                or_conflict,
                                rows: literal_rows,
                            })
                        }
                    } else {
                        if let Some(returning) = returning {
                            Ok(Statement::InsertManyExprReturning {
                                table,
                                columns,
                                or_conflict,
                                rows,
                                returning,
                            })
                        } else {
                            Ok(Statement::InsertManyExpr {
                                table,
                                columns,
                                or_conflict,
                                rows,
                            })
                        }
                    }
                }
                InsertConflictSuffix::DoUpdate { upsert } => {
                    if let Some(literal_rows) = scalar_expr_rows_literal_values(&rows) {
                        if let Some(returning) = returning {
                            Ok(Statement::InsertManyUpsertReturning {
                                table,
                                columns,
                                rows: literal_rows,
                                upsert,
                                returning,
                            })
                        } else {
                            Ok(Statement::InsertManyUpsert {
                                table,
                                columns,
                                rows: literal_rows,
                                upsert,
                            })
                        }
                    } else if let Some(returning) = returning {
                        Ok(Statement::InsertManyExprUpsertReturning {
                            table,
                            columns,
                            rows,
                            upsert,
                            returning,
                        })
                    } else {
                        Ok(Statement::InsertManyExprUpsert {
                            table,
                            columns,
                            rows,
                            upsert,
                        })
                    }
                }
                InsertConflictSuffix::DoNothing { target } => {
                    if let Some(literal_rows) = scalar_expr_rows_literal_values(&rows) {
                        if let Some(returning) = returning {
                            Ok(Statement::InsertManyDoNothingReturning {
                                table,
                                columns,
                                target,
                                rows: literal_rows,
                                returning,
                            })
                        } else {
                            Ok(Statement::InsertManyDoNothing {
                                table,
                                columns,
                                target,
                                rows: literal_rows,
                            })
                        }
                    } else {
                        if let Some(returning) = returning {
                            Ok(Statement::InsertManyExprDoNothingReturning {
                                table,
                                columns,
                                target,
                                rows,
                                returning,
                            })
                        } else {
                            Ok(Statement::InsertManyExprDoNothing {
                                table,
                                columns,
                                target,
                                rows,
                            })
                        }
                    }
                }
            }
        }
    }

    fn parse_insert_with_clause(&mut self, with: WithClause) -> Result<Statement> {
        let statement = self.parse_insert()?;
        match statement {
            Statement::InsertSelect {
                table,
                columns,
                or_conflict,
                mut select,
            } => {
                select.with = Some(with);
                Ok(Statement::InsertSelect {
                    table,
                    columns,
                    or_conflict,
                    select,
                })
            }
            Statement::InsertSelectReturning {
                table,
                columns,
                or_conflict,
                mut select,
                returning,
            } => {
                select.with = Some(with);
                Ok(Statement::InsertSelectReturning {
                    table,
                    columns,
                    or_conflict,
                    select,
                    returning,
                })
            }
            Statement::InsertSelectDoNothing {
                table,
                columns,
                target,
                mut select,
            } => {
                select.with = Some(with);
                Ok(Statement::InsertSelectDoNothing {
                    table,
                    columns,
                    target,
                    select,
                })
            }
            Statement::InsertSelectDoNothingReturning {
                table,
                columns,
                target,
                mut select,
                returning,
            } => {
                select.with = Some(with);
                Ok(Statement::InsertSelectDoNothingReturning {
                    table,
                    columns,
                    target,
                    select,
                    returning,
                })
            }
            Statement::InsertSelectUpsert {
                table,
                columns,
                mut select,
                upsert,
            } => {
                select.with = Some(with);
                Ok(Statement::InsertSelectUpsert {
                    table,
                    columns,
                    select,
                    upsert,
                })
            }
            Statement::InsertSelectUpsertReturning {
                table,
                columns,
                mut select,
                upsert,
                returning,
            } => {
                select.with = Some(with);
                Ok(Statement::InsertSelectUpsertReturning {
                    table,
                    columns,
                    select,
                    upsert,
                    returning,
                })
            }
            statement => Ok(Statement::WithDml {
                with,
                statement: Box::new(statement),
            }),
        }
    }

    fn parse_optional_returning_clause(&mut self) -> Result<Option<Vec<SelectItem>>> {
        if !self.matches(&TokenKind::Returning) {
            return Ok(None);
        }
        self.parse_select_list().map(Some)
    }

    fn parse_values_statement(&mut self) -> Result<Statement> {
        Ok(Statement::Values(self.parse_values_rows()?))
    }

    fn parse_replace(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Replace)?;
        self.expect_keyword(TokenKind::Into)?;
        let table = self.parse_simple_identifier()?;
        let columns = if matches!(self.peek_kind(), TokenKind::LParen) {
            Some(self.parse_parenthesized_identifier_list()?)
        } else {
            None
        };

        if self.matches(&TokenKind::Default) {
            self.expect_keyword(TokenKind::Values)?;
            return Ok(Statement::Insert {
                table,
                columns,
                or_conflict: Some("REPLACE".to_string()),
                values: Vec::new(),
            });
        }

        if matches!(self.peek_kind(), TokenKind::Select | TokenKind::With) {
            let select = if matches!(self.peek_kind(), TokenKind::With) {
                self.parse_with_select_statement()?
            } else {
                self.parse_select_statement()?
            };
            return Ok(Statement::InsertSelect {
                table,
                columns,
                or_conflict: Some("REPLACE".to_string()),
                select: Box::new(select),
            });
        }

        self.expect_keyword(TokenKind::Values)?;
        let mut rows = vec![self.parse_parenthesized_scalar_exprs()?];
        while self.matches(&TokenKind::Comma) {
            rows.push(self.parse_parenthesized_scalar_exprs()?);
        }

        if rows.len() == 1 {
            let values = rows.pop().unwrap_or_default();
            if let Some(literal_values) = scalar_expr_row_literal_values(&values) {
                Ok(Statement::Insert {
                    table,
                    columns,
                    or_conflict: Some("REPLACE".to_string()),
                    values: literal_values,
                })
            } else {
                Ok(Statement::InsertExpr {
                    table,
                    columns,
                    or_conflict: Some("REPLACE".to_string()),
                    values,
                })
            }
        } else {
            if let Some(literal_rows) = scalar_expr_rows_literal_values(&rows) {
                Ok(Statement::InsertMany {
                    table,
                    columns,
                    or_conflict: Some("REPLACE".to_string()),
                    rows: literal_rows,
                })
            } else {
                Ok(Statement::InsertManyExpr {
                    table,
                    columns,
                    or_conflict: Some("REPLACE".to_string()),
                    rows,
                })
            }
        }
    }

    fn parse_delete(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Delete)?;
        self.expect_keyword(TokenKind::From)?;
        let table = self.parse_simple_identifier()?;
        let table_alias = self.parse_optional_table_alias()?;
        let filter = if self.matches(&TokenKind::Where) {
            Some(self.parse_where_expr()?)
        } else {
            None
        };
        let order_by = if self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items()?
        } else {
            Vec::new()
        };
        let (limit, offset) = self.parse_optional_limit_offset()?;
        let returning = self.parse_optional_returning_clause()?;
        let returning_order_by = if returning.is_some() && self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items()?
        } else {
            Vec::new()
        };
        let (returning_limit, returning_offset) = if returning.is_some() {
            self.parse_optional_limit_offset()?
        } else {
            (None, None)
        };

        if let Some(returning) = returning {
            if !order_by.is_empty() || limit.is_some() || offset.is_some() {
                return Err(DbError::sql(
                    "DELETE ORDER BY or LIMIT must appear after RETURNING",
                ));
            }
            if !returning_order_by.is_empty()
                || returning_limit.is_some()
                || returning_offset.is_some()
            {
                Ok(Statement::DeleteReturningLimited {
                    table,
                    table_alias,
                    filter,
                    returning,
                    order_by: returning_order_by,
                    limit: returning_limit,
                    offset: returning_offset,
                })
            } else {
                Ok(Statement::DeleteReturning {
                    table,
                    table_alias,
                    filter,
                    returning,
                })
            }
        } else if !order_by.is_empty() || limit.is_some() || offset.is_some() {
            Ok(Statement::DeleteLimited {
                table,
                table_alias,
                filter,
                order_by,
                limit,
                offset,
            })
        } else {
            Ok(Statement::Delete {
                table,
                table_alias,
                filter,
            })
        }
    }

    fn parse_optional_limit_offset(&mut self) -> Result<(Option<usize>, Option<usize>)> {
        if !self.matches(&TokenKind::Limit) {
            return Ok((None, None));
        }
        let first = self.parse_limit_value()?;
        if self.matches(&TokenKind::Comma) {
            return Ok((
                sqlite_limit_value(self.parse_limit_value()?),
                Some(sqlite_offset_value(first)),
            ));
        }
        let limit = sqlite_limit_value(first);
        let offset = if self.matches(&TokenKind::Offset) {
            Some(sqlite_offset_value(self.parse_limit_value()?))
        } else {
            None
        };
        Ok((limit, offset))
    }

    fn parse_update(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Update)?;
        let table = self.parse_simple_identifier()?;
        let table_alias = self.parse_optional_table_alias()?;
        self.expect_keyword(TokenKind::Set)?;
        let assignments = self.parse_assignments()?;
        let filter = if self.matches(&TokenKind::Where) {
            Some(self.parse_where_expr()?)
        } else {
            None
        };
        let order_by = if self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items()?
        } else {
            Vec::new()
        };
        let (limit, offset) = self.parse_optional_limit_offset()?;
        let returning = self.parse_optional_returning_clause()?;
        let returning_order_by = if returning.is_some() && self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items()?
        } else {
            Vec::new()
        };
        let (returning_limit, returning_offset) = if returning.is_some() {
            self.parse_optional_limit_offset()?
        } else {
            (None, None)
        };

        if let Some(returning) = returning {
            if !order_by.is_empty() || limit.is_some() || offset.is_some() {
                return Err(DbError::sql(
                    "UPDATE ORDER BY or LIMIT must appear after RETURNING",
                ));
            }
            if !returning_order_by.is_empty()
                || returning_limit.is_some()
                || returning_offset.is_some()
            {
                Ok(Statement::UpdateReturningLimited {
                    table,
                    table_alias,
                    assignments,
                    filter,
                    returning,
                    order_by: returning_order_by,
                    limit: returning_limit,
                    offset: returning_offset,
                })
            } else {
                Ok(Statement::UpdateReturning {
                    table,
                    table_alias,
                    assignments,
                    filter,
                    returning,
                })
            }
        } else if !order_by.is_empty() || limit.is_some() || offset.is_some() {
            Ok(Statement::UpdateLimited {
                table,
                table_alias,
                assignments,
                filter,
                order_by,
                limit,
                offset,
            })
        } else {
            Ok(Statement::Update {
                table,
                table_alias,
                assignments,
                filter,
            })
        }
    }

    fn parse_begin_or_start_transaction(&mut self) -> Result<Statement> {
        if self.matches(&TokenKind::Begin) {
            return Ok(Statement::Begin {
                isolation_level: self.parse_optional_isolation_level()?,
            });
        }

        self.expect_keyword(TokenKind::Start)?;
        self.expect_keyword(TokenKind::Transaction)?;
        Ok(Statement::Begin {
            isolation_level: self.parse_optional_isolation_level()?,
        })
    }

    fn parse_optional_isolation_level(&mut self) -> Result<Option<IsolationLevel>> {
        if !self.matches(&TokenKind::Isolation) {
            return Ok(None);
        }
        self.expect_keyword(TokenKind::Level)?;

        if self.matches(&TokenKind::Read) {
            if self.matches(&TokenKind::Committed) {
                return Ok(Some(IsolationLevel::ReadCommitted));
            }

            self.expect_keyword(TokenKind::Repeatable)?;
            self.expect_keyword(TokenKind::Read)?;
            return Ok(Some(IsolationLevel::RepeatableRead));
        }

        self.expect_keyword(TokenKind::Serializable)?;
        Ok(Some(IsolationLevel::Serializable))
    }

    fn parse_select_statement(&mut self) -> Result<SelectStatement> {
        self.parse_compound_select_statement(None)
    }

    fn parse_with_statement(&mut self) -> Result<Statement> {
        let with = self.parse_with_clause()?;
        match self.peek_kind() {
            TokenKind::Select => Ok(Statement::Select(
                self.parse_compound_select_statement(Some(with))?,
            )),
            TokenKind::Insert => self.parse_insert_with_clause(with),
            TokenKind::Update => Ok(Statement::WithDml {
                with,
                statement: Box::new(self.parse_update()?),
            }),
            TokenKind::Delete => Ok(Statement::WithDml {
                with,
                statement: Box::new(self.parse_delete()?),
            }),
            TokenKind::Values => Ok(Statement::ValuesWith {
                with,
                rows: self.parse_values_rows()?,
            }),
            token => Err(self.error_expected(&format!(
                "SELECT, INSERT, UPDATE, DELETE, or VALUES after WITH, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_with_select_statement(&mut self) -> Result<SelectStatement> {
        let with = self.parse_with_clause()?;
        self.parse_compound_select_statement(Some(with))
    }

    fn parse_with_clause(&mut self) -> Result<WithClause> {
        self.expect_keyword(TokenKind::With)?;
        let recursive = self.matches(&TokenKind::Recursive);

        let mut ctes = Vec::new();
        loop {
            ctes.push(self.parse_common_table_expr()?);
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }

        Ok(WithClause { recursive, ctes })
    }

    fn parse_common_table_expr(&mut self) -> Result<CommonTableExpr> {
        let name = self.parse_simple_identifier()?;
        let columns = if matches!(self.peek_kind(), TokenKind::LParen) {
            Some(self.parse_parenthesized_identifier_list()?)
        } else {
            None
        };
        self.expect_keyword(TokenKind::As)?;
        self.parse_optional_cte_materialization_hint()?;
        self.expect_symbol(TokenKind::LParen)?;
        let query = if matches!(self.peek_kind(), TokenKind::Values) {
            CteBody::Values(self.parse_values_rows()?)
        } else {
            CteBody::Select(Box::new(self.parse_select_statement()?))
        };
        self.expect_symbol(TokenKind::RParen)?;
        Ok(CommonTableExpr {
            name,
            columns,
            query,
        })
    }

    fn parse_optional_cte_materialization_hint(&mut self) -> Result<()> {
        if self.parse_optional_identifier_keyword_if("MATERIALIZED") {
            return Ok(());
        }
        if self.matches(&TokenKind::Not) {
            if self.parse_optional_identifier_keyword_if("MATERIALIZED") {
                return Ok(());
            }
            self.index = self.index.saturating_sub(1);
        }
        Ok(())
    }

    fn parse_compound_select_statement(
        &mut self,
        with: Option<WithClause>,
    ) -> Result<SelectStatement> {
        let mut select = self.parse_select_core(with)?;
        while matches!(
            self.peek_kind(),
            TokenKind::Union | TokenKind::Intersect | TokenKind::Except
        ) {
            let operator = if self.matches(&TokenKind::Union) {
                if self.matches(&TokenKind::All) {
                    CompoundOperator::UnionAll
                } else {
                    CompoundOperator::Union
                }
            } else if self.matches(&TokenKind::Intersect) {
                CompoundOperator::Intersect
            } else {
                self.expect_keyword(TokenKind::Except)?;
                CompoundOperator::Except
            };
            select.compounds.push(CompoundSelect {
                operator,
                select: Box::new(self.parse_select_core(None)?),
            });
        }

        select.order_by = if self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items()?
        } else {
            Vec::new()
        };
        if self.matches(&TokenKind::Limit) {
            let first = self.parse_limit_value()?;
            if self.matches(&TokenKind::Comma) {
                select.offset = Some(sqlite_offset_value(first));
                select.limit = sqlite_limit_value(self.parse_limit_value()?);
            } else {
                select.limit = sqlite_limit_value(first);
                select.offset = if self.matches(&TokenKind::Offset) {
                    Some(sqlite_offset_value(self.parse_limit_value()?))
                } else {
                    None
                };
            }
        }

        Ok(select)
    }

    fn parse_select_core(&mut self, with: Option<WithClause>) -> Result<SelectStatement> {
        self.expect_keyword(TokenKind::Select)?;
        let distinct = if self.matches(&TokenKind::Distinct) {
            true
        } else {
            self.matches(&TokenKind::All);
            false
        };
        let columns = self.parse_select_list()?;
        let from = if self.matches(&TokenKind::From) {
            self.parse_from_item()?
        } else {
            if columns.iter().any(|item| {
                matches!(
                    item,
                    SelectItem::Column(_) | SelectItem::AliasedColumn { .. }
                )
            }) {
                return Err(self
                    .error_expected(&format!("FROM, found {}", display_token(self.peek_kind()))));
            }
            FromItem::Table {
                name: SINGLE_ROW_SOURCE_TABLE.to_string(),
                alias: None,
            }
        };
        let joins = self.parse_join_clauses(&from)?;
        let filter = if self.matches(&TokenKind::Where) {
            Some(self.parse_where_expr()?)
        } else {
            None
        };
        let group_by = if self.matches(&TokenKind::Group) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_group_by_items()?
        } else {
            Vec::new()
        };
        let having = if self.matches(&TokenKind::Having) {
            Some(self.parse_where_expr()?)
        } else {
            None
        };
        Ok(SelectStatement {
            with,
            distinct,
            columns,
            from,
            joins,
            filter,
            group_by,
            having,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })
    }

    fn parse_from_item(&mut self) -> Result<FromItem> {
        if self.matches(&TokenKind::LParen) {
            if matches!(self.peek_kind(), TokenKind::Values) {
                let rows = self.parse_values_rows()?;
                self.expect_symbol(TokenKind::RParen)?;
                return Ok(FromItem::Values {
                    rows,
                    alias: self.parse_optional_table_alias()?,
                    columns: None,
                });
            }

            let query = self.parse_select_statement()?;
            self.expect_symbol(TokenKind::RParen)?;
            let alias = self.parse_optional_table_alias()?.unwrap_or_default();
            return Ok(FromItem::Subquery {
                query: Box::new(query),
                alias,
                columns: None,
            });
        }

        let name = self.parse_simple_identifier()?;
        let alias = self.parse_optional_table_alias()?;
        match self.parse_optional_table_index_hint()? {
            Some(ParsedTableIndexHint::IndexedBy(index)) => {
                Ok(FromItem::TableIndexed { name, alias, index })
            }
            Some(ParsedTableIndexHint::NotIndexed) => Ok(FromItem::TableNotIndexed { name, alias }),
            None => Ok(FromItem::Table { name, alias }),
        }
    }

    fn parse_optional_table_index_hint(&mut self) -> Result<Option<ParsedTableIndexHint>> {
        if self.matches(&TokenKind::Not) {
            if self.parse_optional_identifier_keyword_if("INDEXED") {
                return Ok(Some(ParsedTableIndexHint::NotIndexed));
            }
            self.index = self.index.saturating_sub(1);
            return Ok(None);
        }
        if self.parse_optional_identifier_keyword_if("INDEXED") {
            self.expect_keyword(TokenKind::By)?;
            return Ok(Some(ParsedTableIndexHint::IndexedBy(
                self.parse_simple_identifier()?,
            )));
        }
        Ok(None)
    }

    fn parse_values_rows(&mut self) -> Result<Vec<Vec<ScalarExpr>>> {
        self.expect_keyword(TokenKind::Values)?;
        let mut rows = vec![self.parse_parenthesized_scalar_exprs()?];
        while self.matches(&TokenKind::Comma) {
            rows.push(self.parse_parenthesized_scalar_exprs()?);
        }
        Ok(rows)
    }

    fn parse_select_list(&mut self) -> Result<Vec<SelectItem>> {
        let mut columns = Vec::new();
        loop {
            columns.push(self.parse_select_item()?);
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        Ok(columns)
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        if self.matches(&TokenKind::Star) {
            return Ok(SelectItem::Wildcard);
        }

        let mut item = if let TokenKind::Identifier(name) = self.peek_kind()
            && matches!(
                self.tokens.get(self.index + 1).map(|token| &token.kind),
                Some(TokenKind::LParen)
            )
            && is_aggregate_function_name(&name.to_ascii_uppercase())
        {
            let name = name.clone();
            let looks_like_scalar_min_max =
                matches!(name.to_ascii_uppercase().as_str(), "MIN" | "MAX")
                    && self.function_call_has_multiple_arguments(self.index + 1)?;
            if looks_like_scalar_min_max {
                let expr = self.parse_scalar_expr()?;
                match expr {
                    ScalarExpr::Column(name) => SelectItem::Column(name),
                    expr => SelectItem::Expr { expr, alias: None },
                }
            } else {
                self.advance();
                self.parse_aggregate_item(name)?
            }
        } else {
            let expr = self.parse_scalar_expr()?;
            match expr {
                ScalarExpr::Column(name) => SelectItem::Column(name),
                expr => SelectItem::Expr { expr, alias: None },
            }
        };
        let alias = self.parse_optional_select_alias()?;

        if let Some(alias) = alias {
            item = match item {
                SelectItem::Column(name) => SelectItem::AliasedColumn { name, alias },
                SelectItem::AliasedColumn { name, .. } => SelectItem::AliasedColumn { name, alias },
                SelectItem::Expr { expr, .. } => SelectItem::Expr {
                    expr,
                    alias: Some(alias),
                },
                SelectItem::Aggregate {
                    func, arg, filter, ..
                } => SelectItem::Aggregate {
                    func,
                    arg,
                    filter,
                    alias: Some(alias),
                },
                SelectItem::Wildcard => SelectItem::Wildcard,
            };
        }

        Ok(item)
    }

    fn parse_optional_select_alias(&mut self) -> Result<Option<String>> {
        if self.matches(&TokenKind::As) {
            return self.parse_select_alias().map(Some);
        }
        if is_select_alias_token(self.peek_kind()) {
            return self.parse_select_alias().map(Some);
        }
        Ok(None)
    }

    fn parse_select_alias(&mut self) -> Result<String> {
        match self.peek_kind() {
            TokenKind::String(value) => {
                let value = value.clone();
                self.advance();
                Ok(value)
            }
            TokenKind::True => {
                self.advance();
                Ok("true".to_string())
            }
            TokenKind::False => {
                self.advance();
                Ok("false".to_string())
            }
            TokenKind::Begin => {
                self.advance();
                Ok("begin".to_string())
            }
            TokenKind::Rollback => {
                self.advance();
                Ok("rollback".to_string())
            }
            _ => self.parse_simple_identifier(),
        }
    }

    fn parse_scalar_expr(&mut self) -> Result<ScalarExpr> {
        let expr = self.parse_collate_expr()?;
        let expr = self.parse_scalar_is_suffix(expr)?;
        let expr = self.parse_scalar_in_suffix(expr)?;
        let expr = self.parse_scalar_pattern_suffix(expr)?;
        self.parse_scalar_compare_suffix(expr)
    }

    fn parse_collate_expr(&mut self) -> Result<ScalarExpr> {
        let mut expr = self.parse_concat_expr()?;
        while self.matches(&TokenKind::Collate) {
            expr = ScalarExpr::Collate {
                expr: Box::new(expr),
                collation: self.parse_simple_identifier()?,
            };
        }
        Ok(expr)
    }

    fn parse_scalar_is_suffix(&mut self, expr: ScalarExpr) -> Result<ScalarExpr> {
        if self.matches(&TokenKind::IsNull) {
            return Ok(ScalarExpr::Is {
                left: Box::new(expr),
                right: Box::new(ScalarExpr::Literal(Value::Null)),
                negated: false,
            });
        }
        if self.matches(&TokenKind::NotNull) {
            return Ok(ScalarExpr::Is {
                left: Box::new(expr),
                right: Box::new(ScalarExpr::Literal(Value::Null)),
                negated: true,
            });
        }
        if self.matches(&TokenKind::Not) {
            if self.matches(&TokenKind::Null) {
                return Ok(ScalarExpr::Is {
                    left: Box::new(expr),
                    right: Box::new(ScalarExpr::Literal(Value::Null)),
                    negated: true,
                });
            }
            self.index = self.index.saturating_sub(1);
        }
        if !self.matches(&TokenKind::Is) {
            return Ok(expr);
        }

        let negated = self.matches(&TokenKind::Not);
        if self.matches(&TokenKind::Distinct) {
            self.expect_keyword(TokenKind::From)?;
            let right = self.parse_collate_expr()?;
            return Ok(ScalarExpr::Is {
                left: Box::new(expr),
                right: Box::new(right),
                negated: !negated,
            });
        }
        if self.matches(&TokenKind::True) {
            return Ok(ScalarExpr::IsBool {
                expr: Box::new(expr),
                value: true,
                negated,
            });
        }
        if self.matches(&TokenKind::False) {
            return Ok(ScalarExpr::IsBool {
                expr: Box::new(expr),
                value: false,
                negated,
            });
        }
        if self.matches(&TokenKind::Null) {
            return Ok(ScalarExpr::Is {
                left: Box::new(expr),
                right: Box::new(ScalarExpr::Literal(Value::Null)),
                negated,
            });
        }

        let right = self.parse_collate_expr()?;
        Ok(ScalarExpr::Is {
            left: Box::new(expr),
            right: Box::new(right),
            negated,
        })
    }

    fn parse_scalar_in_suffix(&mut self, expr: ScalarExpr) -> Result<ScalarExpr> {
        if self.matches(&TokenKind::Not) {
            if self.matches(&TokenKind::In) {
                if self.is_subquery_start() {
                    let query = self.parse_subquery()?;
                    return Ok(ScalarExpr::InSubquery {
                        expr: Box::new(expr),
                        query: Box::new(query),
                        negated: true,
                    });
                }
                let values = self.parse_scalar_in_value_list()?;
                return Ok(ScalarExpr::InList {
                    expr: Box::new(expr),
                    values,
                    negated: true,
                });
            }
            self.index = self.index.saturating_sub(1);
        }
        if self.matches(&TokenKind::In) {
            if self.is_subquery_start() {
                let query = self.parse_subquery()?;
                return Ok(ScalarExpr::InSubquery {
                    expr: Box::new(expr),
                    query: Box::new(query),
                    negated: false,
                });
            }
            let values = self.parse_scalar_in_value_list()?;
            return Ok(ScalarExpr::InList {
                expr: Box::new(expr),
                values,
                negated: false,
            });
        }
        Ok(expr)
    }

    fn parse_scalar_in_value_list(&mut self) -> Result<Vec<ScalarExpr>> {
        self.expect_symbol(TokenKind::LParen)?;
        if matches!(self.peek_kind(), TokenKind::Values) {
            let values = parse_values_rows_as_in_candidates(self.parse_values_rows()?);
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(values);
        }
        let mut values = Vec::new();
        if self.matches(&TokenKind::RParen) {
            return Ok(values);
        }
        loop {
            values.push(self.parse_scalar_expr()?);
            if self.matches(&TokenKind::Comma) {
                continue;
            }
            break;
        }
        self.expect_symbol(TokenKind::RParen)?;
        Ok(values)
    }

    fn parse_scalar_pattern_suffix(&mut self, expr: ScalarExpr) -> Result<ScalarExpr> {
        if self.matches(&TokenKind::Not) {
            if self.matches(&TokenKind::Like) {
                let pattern = self.parse_string_literal()?;
                let escape = self.parse_optional_escape_clause()?;
                return Ok(ScalarExpr::Like {
                    expr: Box::new(expr),
                    pattern,
                    escape,
                    negated: true,
                });
            }
            if self.matches(&TokenKind::Glob) {
                return Ok(ScalarExpr::Glob {
                    expr: Box::new(expr),
                    pattern: self.parse_string_literal()?,
                    negated: true,
                });
            }
            if self.matches(&TokenKind::Between) {
                let low = self.parse_concat_expr()?;
                self.expect_keyword(TokenKind::And)?;
                let high = self.parse_concat_expr()?;
                return Ok(ScalarExpr::Between {
                    expr: Box::new(expr),
                    low: Box::new(low),
                    high: Box::new(high),
                    negated: true,
                });
            }
            self.index = self.index.saturating_sub(1);
        }
        if self.matches(&TokenKind::Like) {
            let pattern = self.parse_string_literal()?;
            let escape = self.parse_optional_escape_clause()?;
            return Ok(ScalarExpr::Like {
                expr: Box::new(expr),
                pattern,
                escape,
                negated: false,
            });
        }
        if self.matches(&TokenKind::Glob) {
            return Ok(ScalarExpr::Glob {
                expr: Box::new(expr),
                pattern: self.parse_string_literal()?,
                negated: false,
            });
        }
        if self.matches(&TokenKind::Between) {
            let low = self.parse_concat_expr()?;
            self.expect_keyword(TokenKind::And)?;
            let high = self.parse_concat_expr()?;
            return Ok(ScalarExpr::Between {
                expr: Box::new(expr),
                low: Box::new(low),
                high: Box::new(high),
                negated: false,
            });
        }
        Ok(expr)
    }

    fn parse_optional_escape_clause(&mut self) -> Result<Option<String>> {
        if !self.matches(&TokenKind::Escape) {
            return Ok(None);
        }
        Ok(Some(self.parse_string_literal()?))
    }

    fn parse_scalar_compare_suffix(&mut self, expr: ScalarExpr) -> Result<ScalarExpr> {
        let op = match self.peek_kind() {
            TokenKind::Eq => {
                self.advance();
                CompareOp::Eq
            }
            TokenKind::Ne => {
                self.advance();
                CompareOp::Ne
            }
            TokenKind::Gt => {
                self.advance();
                CompareOp::Gt
            }
            TokenKind::Gte => {
                self.advance();
                CompareOp::Gte
            }
            TokenKind::Lt => {
                self.advance();
                CompareOp::Lt
            }
            TokenKind::Lte => {
                self.advance();
                CompareOp::Lte
            }
            _ => return Ok(expr),
        };
        if self.is_subquery_start() {
            let query = self.parse_subquery()?;
            return Ok(ScalarExpr::CompareSubquery {
                left: Box::new(expr),
                op,
                query: Box::new(query),
            });
        }
        let right = self.parse_collate_expr()?;
        Ok(ScalarExpr::Compare {
            left: Box::new(expr),
            op,
            right: Box::new(right),
        })
    }

    fn parse_concat_expr(&mut self) -> Result<ScalarExpr> {
        let mut expr = self.parse_bitwise_expr()?;
        loop {
            let op = if self.matches(&TokenKind::PipePipe) {
                ScalarBinaryOp::Concat
            } else if self.matches(&TokenKind::Arrow) {
                ScalarBinaryOp::JsonExtract
            } else if self.matches(&TokenKind::ArrowText) {
                ScalarBinaryOp::JsonExtractText
            } else {
                break;
            };
            expr = ScalarExpr::Binary {
                left: Box::new(expr),
                op,
                right: Box::new(self.parse_bitwise_expr()?),
            };
        }
        Ok(expr)
    }

    fn parse_bitwise_expr(&mut self) -> Result<ScalarExpr> {
        let mut expr = self.parse_additive_expr()?;
        loop {
            let op = if self.matches(&TokenKind::Ampersand) {
                ScalarBinaryOp::BitAnd
            } else if self.matches(&TokenKind::Pipe) {
                ScalarBinaryOp::BitOr
            } else if self.matches(&TokenKind::ShiftLeft) {
                ScalarBinaryOp::ShiftLeft
            } else if self.matches(&TokenKind::ShiftRight) {
                ScalarBinaryOp::ShiftRight
            } else {
                break;
            };
            expr = ScalarExpr::Binary {
                left: Box::new(expr),
                op,
                right: Box::new(self.parse_additive_expr()?),
            };
        }
        Ok(expr)
    }

    fn parse_additive_expr(&mut self) -> Result<ScalarExpr> {
        let mut expr = self.parse_multiplicative_expr()?;
        loop {
            let op = if self.matches(&TokenKind::Plus) {
                ScalarBinaryOp::Add
            } else if self.matches(&TokenKind::Minus) {
                ScalarBinaryOp::Subtract
            } else {
                break;
            };
            expr = ScalarExpr::Binary {
                left: Box::new(expr),
                op,
                right: Box::new(self.parse_multiplicative_expr()?),
            };
        }
        Ok(expr)
    }

    fn parse_multiplicative_expr(&mut self) -> Result<ScalarExpr> {
        let mut expr = self.parse_unary_scalar_expr()?;
        loop {
            let op = if self.matches(&TokenKind::Star) {
                ScalarBinaryOp::Multiply
            } else if self.matches(&TokenKind::Slash) {
                ScalarBinaryOp::Divide
            } else if self.matches(&TokenKind::Percent) {
                ScalarBinaryOp::Modulo
            } else {
                break;
            };
            expr = ScalarExpr::Binary {
                left: Box::new(expr),
                op,
                right: Box::new(self.parse_unary_scalar_expr()?),
            };
        }
        Ok(expr)
    }

    fn parse_unary_scalar_expr(&mut self) -> Result<ScalarExpr> {
        if self.matches(&TokenKind::Plus) {
            if !is_scalar_expr_start(self.peek_kind()) {
                return Err(self.error_expected("numeric literal after +"));
            }
            return self.parse_unary_scalar_expr();
        }
        if self.matches(&TokenKind::Minus) {
            if !is_scalar_expr_start(self.peek_kind()) {
                return Err(self.error_expected("numeric literal after -"));
            }
            return Ok(ScalarExpr::UnaryMinus(Box::new(
                self.parse_unary_scalar_expr()?,
            )));
        }
        if self.matches(&TokenKind::Tilde) {
            if !is_scalar_expr_start(self.peek_kind()) {
                return Err(self.error_expected("scalar expression after ~"));
            }
            return Ok(ScalarExpr::BitNot(Box::new(
                self.parse_unary_scalar_expr()?,
            )));
        }
        if self.matches(&TokenKind::Not) {
            if !is_scalar_expr_start(self.peek_kind()) {
                return Err(self.error_expected("scalar expression after NOT"));
            }
            return Ok(ScalarExpr::Not(Box::new(self.parse_unary_scalar_expr()?)));
        }
        self.parse_primary_scalar_expr()
    }

    fn parse_primary_scalar_expr(&mut self) -> Result<ScalarExpr> {
        if self.is_subquery_start() {
            return Ok(ScalarExpr::Subquery {
                query: Box::new(self.parse_subquery()?),
            });
        }
        if self.matches(&TokenKind::LParen) {
            let expr = self.parse_scalar_expr()?;
            if self.matches(&TokenKind::Comma) {
                let mut values = vec![expr];
                loop {
                    values.push(self.parse_scalar_expr()?);
                    if self.matches(&TokenKind::Comma) {
                        continue;
                    }
                    break;
                }
                self.expect_symbol(TokenKind::RParen)?;
                return Ok(ScalarExpr::Tuple(values));
            }
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(expr);
        }
        if self.matches(&TokenKind::Case) {
            return self.parse_case_scalar_expr();
        }
        if let TokenKind::Identifier(name) = self.peek_kind() {
            if let Some(expr) = sqlite_current_time_literal_expr(name) {
                self.advance();
                return Ok(expr);
            }
            if name.eq_ignore_ascii_case("CAST") {
                self.advance();
                return self.parse_cast_scalar_expr();
            }
        }
        match self.peek_kind() {
            TokenKind::If => {
                self.advance();
                if self.matches(&TokenKind::LParen) {
                    let lparen_index = self.index - 1;
                    self.parse_scalar_function("IF".to_string(), lparen_index)
                } else {
                    Err(self.error_expected("scalar expression, found IF"))
                }
            }
            TokenKind::Like => {
                self.advance();
                if self.matches(&TokenKind::LParen) {
                    let lparen_index = self.index - 1;
                    self.parse_scalar_function("LIKE".to_string(), lparen_index)
                } else {
                    Err(self.error_expected("scalar expression, found LIKE"))
                }
            }
            TokenKind::Glob => {
                self.advance();
                if self.matches(&TokenKind::LParen) {
                    let lparen_index = self.index - 1;
                    self.parse_scalar_function("GLOB".to_string(), lparen_index)
                } else {
                    Err(self.error_expected("scalar expression, found GLOB"))
                }
            }
            token if is_identifier_token(token) || matches!(token, TokenKind::Replace) => {
                let name = self.parse_identifier()?;
                if self.matches(&TokenKind::LParen) {
                    let lparen_index = self.index - 1;
                    self.parse_scalar_function(name, lparen_index)
                } else {
                    Ok(ScalarExpr::Column(name))
                }
            }
            TokenKind::Integer(_)
            | TokenKind::Real(_)
            | TokenKind::BlobLiteral(_)
            | TokenKind::String(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Null => Ok(ScalarExpr::Literal(self.parse_literal()?)),
            token => Err(self.error_expected(&format!(
                "scalar expression, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_cast_scalar_expr(&mut self) -> Result<ScalarExpr> {
        self.expect_symbol(TokenKind::LParen)?;
        let expr = self.parse_scalar_expr()?;
        self.expect_keyword(TokenKind::As)?;
        let ty = self
            .parse_optional_cast_type()
            .ok_or_else(|| self.error_expected("type name in CAST expression"))?;
        self.expect_symbol(TokenKind::RParen)?;
        Ok(ScalarExpr::Cast {
            expr: Box::new(expr),
            ty,
        })
    }

    fn parse_case_scalar_expr(&mut self) -> Result<ScalarExpr> {
        let base = if matches!(self.peek_kind(), TokenKind::When) {
            None
        } else {
            Some(Box::new(self.parse_scalar_expr()?))
        };

        let mut when_then_clauses = Vec::new();
        while self.matches(&TokenKind::When) {
            let when_expr = self.parse_scalar_expr()?;
            self.expect_keyword(TokenKind::Then)?;
            let then_expr = self.parse_scalar_expr()?;
            when_then_clauses.push((when_expr, then_expr));
        }

        if when_then_clauses.is_empty() {
            return Err(self.error_expected("WHEN in CASE expression"));
        }

        let else_expr = if self.matches(&TokenKind::Else) {
            Some(Box::new(self.parse_scalar_expr()?))
        } else {
            None
        };

        self.expect_keyword(TokenKind::End)?;
        Ok(ScalarExpr::Case {
            base,
            when_then_clauses,
            else_expr,
        })
    }

    fn parse_scalar_function(
        &mut self,
        function_name: String,
        lparen_index: usize,
    ) -> Result<ScalarExpr> {
        let function_name_upper = function_name.to_ascii_uppercase();
        let looks_like_scalar_min_max = matches!(function_name_upper.as_str(), "MIN" | "MAX")
            && self.function_call_has_multiple_arguments(lparen_index)?;
        if is_aggregate_function_name(&function_name_upper) && !looks_like_scalar_min_max {
            let (func, arg, filter) =
                self.parse_aggregate_call_after_lparen(&function_name_upper)?;
            return Ok(ScalarExpr::Aggregate {
                func,
                arg: Box::new(arg),
                filter: filter.map(Box::new),
            });
        }

        let func = match function_name_upper.as_str() {
            "LENGTH" => ScalarFunc::Length,
            "OCTET_LENGTH" => ScalarFunc::OctetLength,
            "MIN" => ScalarFunc::MinScalar,
            "MAX" => ScalarFunc::MaxScalar,
            "DATE" => ScalarFunc::Date,
            "TIME" => ScalarFunc::Time,
            "DATETIME" => ScalarFunc::DateTime,
            "TIMEDIFF" => ScalarFunc::TimeDiff,
            "STRFTIME" => ScalarFunc::Strftime,
            "JULIANDAY" => ScalarFunc::JulianDay,
            "UNIXEPOCH" => ScalarFunc::UnixEpoch,
            "CHANGES" => ScalarFunc::Changes,
            "TOTAL_CHANGES" => ScalarFunc::TotalChanges,
            "PRINTF" | "FORMAT" => ScalarFunc::Printf,
            "IIF" => ScalarFunc::IIf,
            "IF" => ScalarFunc::If,
            "CONCAT" => ScalarFunc::Concat,
            "CONCAT_WS" => ScalarFunc::ConcatWs,
            "LIKELY" => ScalarFunc::Likely,
            "UNLIKELY" => ScalarFunc::Unlikely,
            "SQLITE_VERSION" => ScalarFunc::SqliteVersion,
            "SQLITE_SOURCE_ID" => ScalarFunc::SqliteSourceId,
            "SQLITE_COMPILEOPTION_USED" => ScalarFunc::SqliteCompileOptionUsed,
            "SQLITE_COMPILEOPTION_GET" => ScalarFunc::SqliteCompileOptionGet,
            "SIGN" => ScalarFunc::Sign,
            "RANDOMBLOB" => ScalarFunc::RandomBlob,
            "RANDOM" => ScalarFunc::Random,
            "UNHEX" => ScalarFunc::Unhex,
            "UNISTR" => ScalarFunc::Unistr,
            "UNISTR_QUOTE" => ScalarFunc::UnistrQuote,
            "LIKELIHOOD" => ScalarFunc::Likelihood,
            "MOD" => ScalarFunc::Mod,
            "CEIL" => ScalarFunc::Ceil,
            "CEILING" => ScalarFunc::Ceiling,
            "FLOOR" => ScalarFunc::Floor,
            "TRUNC" => ScalarFunc::Trunc,
            "PI" => ScalarFunc::Pi,
            "SQRT" => ScalarFunc::Sqrt,
            "POWER" | "POW" => ScalarFunc::Power,
            "EXP" => ScalarFunc::Exp,
            "SIN" => ScalarFunc::Sin,
            "COS" => ScalarFunc::Cos,
            "TAN" => ScalarFunc::Tan,
            "SINH" => ScalarFunc::Sinh,
            "COSH" => ScalarFunc::Cosh,
            "TANH" => ScalarFunc::Tanh,
            "ACOS" => ScalarFunc::Acos,
            "ASIN" => ScalarFunc::Asin,
            "ATAN" => ScalarFunc::Atan,
            "ATAN2" => ScalarFunc::Atan2,
            "ACOSH" => ScalarFunc::Acosh,
            "ASINH" => ScalarFunc::Asinh,
            "ATANH" => ScalarFunc::Atanh,
            "LN" => ScalarFunc::Ln,
            "LOG10" => ScalarFunc::Log10,
            "LOG2" => ScalarFunc::Log2,
            "LOG" => ScalarFunc::Log,
            "DEGREES" => ScalarFunc::Degrees,
            "RADIANS" => ScalarFunc::Radians,
            "TYPEOF" => ScalarFunc::TypeOf,
            "SUBTYPE" => ScalarFunc::Subtype,
            "HEX" => ScalarFunc::Hex,
            "SUBSTR" | "SUBSTRING" => ScalarFunc::Substr,
            "INSTR" => ScalarFunc::Instr,
            "REPLACE" => ScalarFunc::Replace,
            "LIKE" => ScalarFunc::LikeFunc,
            "GLOB" => ScalarFunc::GlobFunc,
            "QUOTE" => ScalarFunc::Quote,
            "UNICODE" => ScalarFunc::Unicode,
            "CHAR" => ScalarFunc::Char,
            "ZEROBLOB" => ScalarFunc::ZeroBlob,
            "TRIM" => ScalarFunc::Trim,
            "LTRIM" => ScalarFunc::LTrim,
            "RTRIM" => ScalarFunc::RTrim,
            "LOWER" => ScalarFunc::Lower,
            "UPPER" => ScalarFunc::Upper,
            "ABS" => ScalarFunc::Abs,
            "ROUND" => ScalarFunc::Round,
            "COALESCE" => ScalarFunc::Coalesce,
            "IFNULL" => ScalarFunc::IfNull,
            "NULLIF" => ScalarFunc::NullIf,
            "JSON" => ScalarFunc::Json,
            "JSON_VALID" => ScalarFunc::JsonValid,
            "JSON_ERROR_POSITION" => ScalarFunc::JsonErrorPosition,
            "JSON_PRETTY" => ScalarFunc::JsonPretty,
            "JSON_QUOTE" => ScalarFunc::JsonQuote,
            "JSON_EXTRACT" => ScalarFunc::JsonExtract,
            "JSON_TYPE" => ScalarFunc::JsonType,
            "JSON_ARRAY" => ScalarFunc::JsonArray,
            "JSON_OBJECT" => ScalarFunc::JsonObject,
            "JSON_ARRAY_LENGTH" => ScalarFunc::JsonArrayLength,
            "JSON_REMOVE" => ScalarFunc::JsonRemove,
            "JSON_SET" => ScalarFunc::JsonSet,
            "JSON_INSERT" => ScalarFunc::JsonInsert,
            "JSON_REPLACE" => ScalarFunc::JsonReplace,
            "JSON_PATCH" => ScalarFunc::JsonPatch,
            "LAST_INSERT_ROWID" => ScalarFunc::LastInsertRowId,
            _ => {
                return Err(DbError::sql(format!(
                    "unsupported scalar function: {function_name}"
                )));
            }
        };

        let mut args = Vec::new();
        if !self.matches(&TokenKind::RParen) {
            loop {
                args.push(self.parse_scalar_expr()?);
                if self.matches(&TokenKind::Comma) {
                    continue;
                }
                self.expect_symbol(TokenKind::RParen)?;
                break;
            }
        }

        Ok(ScalarExpr::Function { func, args })
    }

    fn function_call_has_multiple_arguments(&self, lparen_index: usize) -> Result<bool> {
        let mut depth = 0usize;
        let mut index = lparen_index;
        while let Some(token) = self.tokens.get(index) {
            match token.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Ok(false);
                    }
                }
                TokenKind::Comma if depth == 1 => return Ok(true),
                _ => {}
            }
            index += 1;
        }
        Err(DbError::sql("unterminated function call"))
    }

    fn parse_aggregate_item(&mut self, function_name: String) -> Result<SelectItem> {
        self.expect_symbol(TokenKind::LParen)?;
        let (func, arg, filter) =
            self.parse_aggregate_call_after_lparen(&function_name.to_ascii_uppercase())?;
        Ok(SelectItem::Aggregate {
            func,
            arg,
            filter,
            alias: None,
        })
    }

    fn parse_aggregate_call_after_lparen(
        &mut self,
        function_name_upper: &str,
    ) -> Result<(AggregateFunc, AggregateArg, Option<Expr>)> {
        let func = match function_name_upper {
            "COUNT" => AggregateFunc::Count,
            "SUM" => AggregateFunc::Sum,
            "AVG" => AggregateFunc::Avg,
            "TOTAL" => AggregateFunc::Total,
            "MEDIAN" => AggregateFunc::Median,
            "PERCENTILE" => AggregateFunc::Percentile,
            "PERCENTILE_CONT" => AggregateFunc::PercentileCont,
            "PERCENTILE_DISC" => AggregateFunc::PercentileDisc,
            "GROUP_CONCAT" | "STRING_AGG" => AggregateFunc::GroupConcat,
            "JSON_GROUP_ARRAY" => AggregateFunc::JsonGroupArray,
            "JSON_GROUP_OBJECT" => AggregateFunc::JsonGroupObject,
            "MIN" => AggregateFunc::Min,
            "MAX" => AggregateFunc::Max,
            _ => {
                return Err(DbError::sql(format!(
                    "unsupported select function: {function_name_upper}"
                )));
            }
        };
        let arg = if matches!(func, AggregateFunc::Count)
            && matches!(self.peek_kind(), TokenKind::RParen)
        {
            AggregateArg::Wildcard
        } else if self.matches(&TokenKind::Star) {
            AggregateArg::Wildcard
        } else if matches!(func, AggregateFunc::GroupConcat) {
            let distinct = self.matches(&TokenKind::Distinct);
            if !distinct {
                self.matches(&TokenKind::All);
            }
            let expr = self.parse_scalar_expr()?;
            let separator = if self.matches(&TokenKind::Comma) {
                if distinct {
                    return Err(DbError::sql(
                        "DISTINCT aggregates must have exactly one argument",
                    ));
                }
                Some(self.parse_scalar_expr()?)
            } else {
                None
            };
            let order_by = if self.matches(&TokenKind::Order) {
                self.expect_keyword(TokenKind::By)?;
                self.parse_order_by_items()?
            } else {
                Vec::new()
            };
            AggregateArg::GroupConcat {
                expr,
                separator,
                distinct,
                order_by,
            }
        } else if matches!(
            func,
            AggregateFunc::Percentile
                | AggregateFunc::PercentileCont
                | AggregateFunc::PercentileDisc
        ) {
            let expr = self.parse_scalar_expr()?;
            self.expect_symbol(TokenKind::Comma)?;
            let fraction = self.parse_scalar_expr()?;
            let order_by = if self.matches(&TokenKind::Order) {
                self.expect_keyword(TokenKind::By)?;
                self.parse_order_by_items()?
            } else {
                Vec::new()
            };
            AggregateArg::Percentile {
                expr,
                fraction,
                order_by,
            }
        } else if matches!(func, AggregateFunc::JsonGroupObject) {
            let key = self.parse_scalar_expr()?;
            self.expect_symbol(TokenKind::Comma)?;
            let value = self.parse_scalar_expr()?;
            let order_by = if self.matches(&TokenKind::Order) {
                self.expect_keyword(TokenKind::By)?;
                self.parse_order_by_items()?
            } else {
                Vec::new()
            };
            AggregateArg::JsonGroupObject {
                key,
                value,
                order_by,
            }
        } else {
            let distinct = self.matches(&TokenKind::Distinct);
            if !distinct {
                self.matches(&TokenKind::All);
            }
            let expr = self.parse_scalar_expr()?;
            let order_by = if self.matches(&TokenKind::Order) {
                self.expect_keyword(TokenKind::By)?;
                self.parse_order_by_items()?
            } else {
                Vec::new()
            };
            AggregateArg::Expr {
                expr,
                distinct,
                order_by,
            }
        };
        if let Some(name) = aggregate_arg_nested_function_name(&arg) {
            return Err(DbError::sql(format!("misuse of aggregate function {name}")));
        }
        self.expect_symbol(TokenKind::RParen)?;
        let filter = if let TokenKind::Identifier(name) = self.peek_kind()
            && name.eq_ignore_ascii_case("FILTER")
        {
            self.advance();
            self.expect_symbol(TokenKind::LParen)?;
            self.expect_keyword(TokenKind::Where)?;
            let filter = self.parse_where_expr()?;
            self.expect_symbol(TokenKind::RParen)?;
            Some(filter)
        } else {
            None
        };
        Ok((func, arg, filter))
    }

    fn parse_where_expr(&mut self) -> Result<Expr> {
        self.parse_or_expr()
    }

    fn parse_or_expr(&mut self) -> Result<Expr> {
        let mut expr = self.parse_and_expr()?;
        while self.matches(&TokenKind::Or) {
            let right = self.parse_and_expr()?;
            expr = Expr::Or(Box::new(expr), Box::new(right));
        }
        Ok(expr)
    }

    fn parse_and_expr(&mut self) -> Result<Expr> {
        let mut expr = self.parse_not_expr()?;
        while self.matches(&TokenKind::And) {
            let right = self.parse_not_expr()?;
            expr = Expr::And(Box::new(expr), Box::new(right));
        }
        Ok(expr)
    }

    fn parse_not_expr(&mut self) -> Result<Expr> {
        if self.matches(&TokenKind::Not) {
            if self.matches(&TokenKind::Exists) {
                let query = self.parse_subquery_after_exists()?;
                return Ok(Expr::ExistsSubquery {
                    query: Box::new(query),
                    negated: true,
                });
            }
            let scalar_start = self.index;
            match self.parse_not_expr() {
                Ok(expr) => return Ok(Expr::Not(Box::new(expr))),
                Err(_) => {
                    self.index = scalar_start;
                    let expr = self.parse_scalar_expr()?;
                    return Ok(Expr::Not(Box::new(Expr::IsBool {
                        expr,
                        value: true,
                        negated: false,
                        explicit: false,
                    })));
                }
            }
        }

        self.parse_primary_expr()
    }

    fn parse_primary_expr(&mut self) -> Result<Expr> {
        if self.matches(&TokenKind::Exists) {
            let query = self.parse_subquery_after_exists()?;
            return Ok(Expr::ExistsSubquery {
                query: Box::new(query),
                negated: false,
            });
        }
        if self.matches(&TokenKind::LParen) {
            let group_start = self.index - 1;
            match self.parse_or_expr().and_then(|expr| {
                self.expect_symbol(TokenKind::RParen)?;
                Ok(expr)
            }) {
                Ok(expr) => {
                    if is_scalar_suffix_after_group(self.peek_kind()) {
                        self.index = group_start;
                    } else {
                        return Ok(expr);
                    }
                }
                Err(_) => self.index = group_start,
            }
        }

        self.parse_comparison_expr()
    }

    fn parse_comparison_expr(&mut self) -> Result<Expr> {
        if !is_scalar_expr_start(self.peek_kind()) {
            return Err(self.error_expected(&format!(
                "identifier or scalar expression, found {}",
                display_token(self.peek_kind())
            )));
        }
        let left_expr = self.parse_scalar_expr()?;
        if let ScalarExpr::IsBool {
            expr,
            value,
            negated,
        } = left_expr.clone()
        {
            return Ok(Expr::IsBool {
                expr: *expr,
                value,
                negated,
                explicit: true,
            });
        }
        if let ScalarExpr::Is {
            left,
            right,
            negated,
        } = left_expr.clone()
        {
            if matches!(right.as_ref(), ScalarExpr::Literal(Value::Null)) {
                return match *left {
                    ScalarExpr::Column(column) => Ok(Expr::IsNull { column, negated }),
                    expr => Ok(Expr::IsNullScalar { expr, negated }),
                };
            }
            return Ok(Expr::Is {
                left: *left,
                right: *right,
                negated,
            });
        }
        if let ScalarExpr::InList {
            expr,
            values,
            negated,
        } = left_expr.clone()
        {
            return match *expr {
                ScalarExpr::Column(column) => match scalar_expr_list_literal_values(&values) {
                    Some(values) => Ok(Expr::InList {
                        column,
                        values,
                        negated,
                    }),
                    None => Ok(Expr::InListScalar {
                        expr: ScalarExpr::Column(column),
                        values,
                        negated,
                    }),
                },
                expr => Ok(Expr::InListScalar {
                    expr,
                    values,
                    negated,
                }),
            };
        }
        if let ScalarExpr::InSubquery {
            expr,
            query,
            negated,
        } = left_expr.clone()
        {
            return match *expr {
                ScalarExpr::Column(column) => Ok(Expr::InSubquery {
                    column,
                    query,
                    negated,
                }),
                expr => Ok(Expr::InSubqueryScalar {
                    expr,
                    query,
                    negated,
                }),
            };
        }
        if let ScalarExpr::CompareSubquery { left, op, query } = left_expr.clone() {
            return match *left {
                ScalarExpr::Column(column) if !column.contains('.') => {
                    Ok(Expr::CompareSubquery { column, op, query })
                }
                ScalarExpr::Column(column) => Ok(Expr::CompareSubqueryScalar {
                    left: ScalarExpr::Column(column),
                    op,
                    query,
                }),
                left => Ok(Expr::CompareSubqueryScalar { left, op, query }),
            };
        }
        if let ScalarExpr::Like {
            expr,
            pattern,
            escape,
            negated,
        } = left_expr.clone()
        {
            return match *expr {
                ScalarExpr::Column(column) => Ok(Expr::Like {
                    column,
                    pattern,
                    escape,
                    negated,
                }),
                expr => Ok(Expr::LikeScalar {
                    expr,
                    pattern,
                    escape,
                    negated,
                }),
            };
        }
        if let ScalarExpr::Glob {
            expr,
            pattern,
            negated,
        } = left_expr.clone()
        {
            return match *expr {
                ScalarExpr::Column(column) => Ok(Expr::Glob {
                    column,
                    pattern,
                    negated,
                }),
                expr => Ok(Expr::GlobScalar {
                    expr,
                    pattern,
                    negated,
                }),
            };
        }
        if let ScalarExpr::Between {
            expr,
            low,
            high,
            negated,
        } = left_expr.clone()
        {
            return match (
                *expr,
                scalar_expr_literal_value(&low),
                scalar_expr_literal_value(&high),
            ) {
                (ScalarExpr::Column(column), Some(low), Some(high)) => Ok(Expr::Between {
                    column,
                    low,
                    high,
                    negated,
                }),
                (expr, _, _) => Ok(Expr::BetweenScalar {
                    expr,
                    low: *low,
                    high: *high,
                    negated,
                }),
            };
        }
        if let ScalarExpr::Compare { left, op, right } = left_expr.clone() {
            return match (&*left, &*right) {
                (ScalarExpr::Column(column), _) => {
                    if let Some(value) = scalar_expr_literal_value(&right) {
                        Ok(Expr::Compare {
                            column: column.clone(),
                            op,
                            value,
                        })
                    } else {
                        Ok(Expr::CompareScalar {
                            left: *left,
                            op,
                            right: *right,
                        })
                    }
                }
                _ => Ok(Expr::CompareScalar {
                    left: *left,
                    op,
                    right: *right,
                }),
            };
        }
        let column = if let ScalarExpr::Column(column) = &left_expr {
            Some(column.clone())
        } else {
            None
        };
        if self.matches(&TokenKind::IsNull) {
            return match column {
                Some(column) => Ok(Expr::IsNull {
                    column,
                    negated: false,
                }),
                None => Ok(Expr::IsNullScalar {
                    expr: left_expr,
                    negated: false,
                }),
            };
        }
        if self.matches(&TokenKind::NotNull) {
            return match column {
                Some(column) => Ok(Expr::IsNull {
                    column,
                    negated: true,
                }),
                None => Ok(Expr::IsNullScalar {
                    expr: left_expr,
                    negated: true,
                }),
            };
        }
        if self.matches(&TokenKind::Not) {
            if self.matches(&TokenKind::Null) {
                return match column {
                    Some(column) => Ok(Expr::IsNull {
                        column,
                        negated: true,
                    }),
                    None => Ok(Expr::IsNullScalar {
                        expr: left_expr,
                        negated: true,
                    }),
                };
            }
            self.index = self.index.saturating_sub(1);
        }
        if self.matches(&TokenKind::Is) {
            let negated = self.matches(&TokenKind::Not);
            if self.matches(&TokenKind::Null) {
                return match column {
                    Some(column) => Ok(Expr::IsNull { column, negated }),
                    None => Ok(Expr::IsNullScalar {
                        expr: left_expr,
                        negated,
                    }),
                };
            }
            if self.matches(&TokenKind::True) {
                return Ok(Expr::IsBool {
                    expr: left_expr,
                    value: true,
                    negated,
                    explicit: true,
                });
            }
            if self.matches(&TokenKind::False) {
                return Ok(Expr::IsBool {
                    expr: left_expr,
                    value: false,
                    negated,
                    explicit: true,
                });
            }
            return Err(self.error_expected(&format!(
                "NULL, TRUE, FALSE, IS NOT NULL, IS NOT TRUE, or IS NOT FALSE, found {}",
                display_token(self.peek_kind())
            )));
        }
        if self.matches(&TokenKind::Not) {
            if self.matches(&TokenKind::Like) {
                let pattern = self.parse_string_literal()?;
                let escape = self.parse_optional_escape_clause()?;
                return match column {
                    Some(column) => Ok(Expr::Like {
                        column,
                        pattern,
                        escape,
                        negated: true,
                    }),
                    None => Ok(Expr::LikeScalar {
                        expr: left_expr,
                        pattern,
                        escape,
                        negated: true,
                    }),
                };
            }
            if self.matches(&TokenKind::Glob) {
                let pattern = self.parse_string_literal()?;
                return match column {
                    Some(column) => Ok(Expr::Glob {
                        column,
                        pattern,
                        negated: true,
                    }),
                    None => Ok(Expr::GlobScalar {
                        expr: left_expr,
                        pattern,
                        negated: true,
                    }),
                };
            }
            if self.matches(&TokenKind::Between) {
                let low = self.parse_scalar_expr()?;
                self.expect_keyword(TokenKind::And)?;
                let high = self.parse_scalar_expr()?;
                return match (
                    column,
                    scalar_expr_literal_value(&low),
                    scalar_expr_literal_value(&high),
                ) {
                    (Some(column), Some(low), Some(high)) => Ok(Expr::Between {
                        column,
                        low,
                        high,
                        negated: true,
                    }),
                    _ => Ok(Expr::BetweenScalar {
                        expr: left_expr,
                        low,
                        high,
                        negated: true,
                    }),
                };
            }
            self.expect_keyword(TokenKind::In)?;
            return self.parse_in_rhs(left_expr, column, true);
        }
        if self.matches(&TokenKind::In) {
            return self.parse_in_rhs(left_expr, column, false);
        }
        if self.matches(&TokenKind::Like) {
            let pattern = self.parse_string_literal()?;
            let escape = self.parse_optional_escape_clause()?;
            return match column {
                Some(column) => Ok(Expr::Like {
                    column,
                    pattern,
                    escape,
                    negated: false,
                }),
                None => Ok(Expr::LikeScalar {
                    expr: left_expr,
                    pattern,
                    escape,
                    negated: false,
                }),
            };
        }
        if self.matches(&TokenKind::Glob) {
            let pattern = self.parse_string_literal()?;
            return match column {
                Some(column) => Ok(Expr::Glob {
                    column,
                    pattern,
                    negated: false,
                }),
                None => Ok(Expr::GlobScalar {
                    expr: left_expr,
                    pattern,
                    negated: false,
                }),
            };
        }
        if self.matches(&TokenKind::Between) {
            let low = self.parse_scalar_expr()?;
            self.expect_keyword(TokenKind::And)?;
            let high = self.parse_scalar_expr()?;
            return match (
                column,
                scalar_expr_literal_value(&low),
                scalar_expr_literal_value(&high),
            ) {
                (Some(column), Some(low), Some(high)) => Ok(Expr::Between {
                    column,
                    low,
                    high,
                    negated: false,
                }),
                _ => Ok(Expr::BetweenScalar {
                    expr: left_expr,
                    low,
                    high,
                    negated: false,
                }),
            };
        }

        let op = match self.peek_kind() {
            TokenKind::Eq => {
                self.advance();
                CompareOp::Eq
            }
            TokenKind::Ne => {
                self.advance();
                CompareOp::Ne
            }
            TokenKind::Gt => {
                self.advance();
                CompareOp::Gt
            }
            TokenKind::Gte => {
                self.advance();
                CompareOp::Gte
            }
            TokenKind::Lt => {
                self.advance();
                CompareOp::Lt
            }
            TokenKind::Lte => {
                self.advance();
                CompareOp::Lte
            }
            token => {
                if is_filter_expr_terminator(token) {
                    return Ok(Expr::IsBool {
                        expr: left_expr,
                        value: true,
                        negated: false,
                        explicit: false,
                    });
                }
                return Err(self.error_expected(&format!(
                    "comparison operator (=, !=, <>, >, >=, <, <=) or IS NULL, found {}",
                    display_token(token)
                )));
            }
        };
        if self.is_subquery_start() {
            let query = self.parse_subquery()?;
            return Ok(Expr::CompareSubqueryScalar {
                left: left_expr,
                op,
                query: Box::new(query),
            });
        }
        let right_expr = self.parse_scalar_expr()?;

        match (&left_expr, &right_expr) {
            (ScalarExpr::Column(column), _) => {
                if let Some(value) = scalar_expr_literal_value(&right_expr) {
                    Ok(Expr::Compare {
                        column: column.clone(),
                        op,
                        value,
                    })
                } else {
                    Ok(Expr::CompareScalar {
                        left: left_expr,
                        op,
                        right: right_expr,
                    })
                }
            }
            _ => Ok(Expr::CompareScalar {
                left: left_expr,
                op,
                right: right_expr,
            }),
        }
    }

    fn parse_in_rhs(
        &mut self,
        left_expr: ScalarExpr,
        column: Option<String>,
        negated: bool,
    ) -> Result<Expr> {
        self.expect_symbol(TokenKind::LParen)?;
        if matches!(self.peek_kind(), TokenKind::Select) {
            let query = self.parse_select_statement()?;
            self.expect_symbol(TokenKind::RParen)?;
            return match column {
                Some(column) => Ok(Expr::InSubquery {
                    column,
                    query: Box::new(query),
                    negated,
                }),
                None => Ok(Expr::InSubqueryScalar {
                    expr: left_expr,
                    query: Box::new(query),
                    negated,
                }),
            };
        }

        let values = if matches!(self.peek_kind(), TokenKind::Values) {
            let values = parse_values_rows_as_in_candidates(self.parse_values_rows()?);
            self.expect_symbol(TokenKind::RParen)?;
            values
        } else {
            let mut values = Vec::new();
            if self.matches(&TokenKind::RParen) {
                values
            } else {
                loop {
                    values.push(self.parse_scalar_expr()?);
                    if self.matches(&TokenKind::Comma) {
                        continue;
                    }
                    break;
                }
                self.expect_symbol(TokenKind::RParen)?;
                values
            }
        };

        match column {
            Some(column) => match scalar_expr_list_literal_values(&values) {
                Some(values) => Ok(Expr::InList {
                    column,
                    values,
                    negated,
                }),
                None => Ok(Expr::InListScalar {
                    expr: ScalarExpr::Column(column),
                    values,
                    negated,
                }),
            },
            None => Ok(Expr::InListScalar {
                expr: left_expr,
                values,
                negated,
            }),
        }
    }

    fn parse_column_def(&mut self, table_name: Option<&str>) -> Result<ColumnDef> {
        let name = self.parse_simple_identifier()?;
        let column_type = self.parse_optional_column_type().unwrap_or(ColumnType::Any);
        let mut column = ColumnDef::new(name, column_type);
        let mut pending_constraint_name = None;

        loop {
            if self.matches(&TokenKind::Constraint) {
                pending_constraint_name = Some(self.parse_simple_identifier()?);
                continue;
            }

            if self.matches(&TokenKind::Primary) {
                self.expect_keyword(TokenKind::Key)?;
                column.primary_key = true;
                column.nullable = false;
                if let Some(constraint_name) = pending_constraint_name.take() {
                    column = column.with_primary_key_name(constraint_name);
                }
                if self.matches(&TokenKind::Desc) {
                    column = column.primary_key_sort_order(SortOrder::Desc);
                } else if self.matches(&TokenKind::Asc) {
                    column = column.primary_key_sort_order(SortOrder::Asc);
                }
                if let Some(conflict_clause) = self.parse_optional_on_conflict_clause()? {
                    column = column.with_primary_key_conflict_clause(conflict_clause);
                }
                if self.matches(&TokenKind::Autoincrement) {
                    column.autoincrement = true;
                }
                continue;
            }

            if self.matches(&TokenKind::Not) {
                self.expect_keyword(TokenKind::Null)?;
                column.nullable = false;
                if let Some(constraint_name) = pending_constraint_name.take() {
                    column = column.with_not_null_name(constraint_name);
                }
                if let Some(conflict_clause) = self.parse_optional_on_conflict_clause()? {
                    column = column.with_not_null_conflict_clause(conflict_clause);
                }
                continue;
            }

            if self.matches(&TokenKind::Collate) {
                let collation = self.parse_simple_identifier()?;
                column = column.collation(collation);
                pending_constraint_name = None;
                continue;
            }

            if self.matches(&TokenKind::Unique) {
                column.unique = true;
                if let Some(constraint_name) = pending_constraint_name.take() {
                    column = column.with_unique_name(constraint_name);
                }
                if let Some(conflict_clause) = self.parse_optional_on_conflict_clause()? {
                    column = column.with_unique_conflict_clause(conflict_clause);
                }
                continue;
            }

            if self.matches(&TokenKind::Default) {
                column = column.default_value(self.parse_column_default_value()?);
                pending_constraint_name = None;
                continue;
            }

            if self.matches(&TokenKind::Generated) {
                self.expect_keyword(TokenKind::Always)?;
                column = self.parse_generated_column(column, true)?;
                pending_constraint_name = None;
                continue;
            }

            if self.matches(&TokenKind::As) {
                column = self.parse_generated_column(column, false)?;
                pending_constraint_name = None;
                continue;
            }

            if self.matches(&TokenKind::Check) {
                let explicit_name = pending_constraint_name.is_some();
                let check_name = pending_constraint_name.take().unwrap_or_else(|| {
                    format!("{}_{}_check", table_name.unwrap_or("column"), column.name)
                });
                column = column.check(self.parse_check_constraint(check_name, explicit_name)?);
                continue;
            }

            if self.matches(&TokenKind::References) {
                let ref_table = self.parse_simple_identifier()?;
                let mut column_foreign_key = if self.matches(&TokenKind::LParen) {
                    let ref_column = self.parse_simple_identifier()?;
                    self.expect_symbol(TokenKind::RParen)?;
                    column.references(ref_table, ref_column)
                } else {
                    column.references_parent_primary_key(ref_table)
                };
                if let Some(constraint_name) = pending_constraint_name.take() {
                    column_foreign_key.foreign_key = column_foreign_key
                        .foreign_key
                        .take()
                        .map(|foreign_key| foreign_key.named(constraint_name));
                }
                if let Some(foreign_key) = column_foreign_key.foreign_key.take() {
                    column_foreign_key.foreign_key =
                        Some(self.parse_optional_foreign_key_actions(foreign_key)?);
                }
                column = column_foreign_key;
                continue;
            }

            break;
        }

        Ok(column)
    }

    fn parse_generated_column(
        &mut self,
        column: ColumnDef,
        generated_always_explicit: bool,
    ) -> Result<ColumnDef> {
        if generated_always_explicit {
            self.expect_keyword(TokenKind::As)?;
        }
        self.expect_symbol(TokenKind::LParen)?;
        let expr = self.parse_parenthesized_sql_expression()?;
        if self.matches(&TokenKind::Stored) {
            return Ok(if generated_always_explicit {
                column.generated_stored(expr)
            } else {
                column.generated_as_stored(expr)
            });
        }
        if self.matches(&TokenKind::Virtual) {
            return Ok(if generated_always_explicit {
                column.generated_virtual(expr)
            } else {
                column.generated_as_virtual(expr)
            });
        }
        Ok(if generated_always_explicit {
            column.generated_virtual_implicit(expr)
        } else {
            column.generated_as(expr)
        })
    }

    fn parse_parenthesized_sql_expression(&mut self) -> Result<String> {
        let start = self.index;
        let mut depth = 1_usize;
        while depth > 0 {
            let token = self
                .tokens
                .get(self.index)
                .ok_or_else(|| DbError::sql("unterminated parenthesized SQL expression"))?;
            match token.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth = depth.saturating_sub(1),
                TokenKind::Eof => {
                    return Err(DbError::sql("unterminated parenthesized SQL expression"));
                }
                _ => {}
            }
            self.index += 1;
        }

        let end = self.index - 1;
        let sql = self.tokens[start..end]
            .iter()
            .map(token_sql_fragment)
            .collect::<Vec<_>>()
            .join(" ");
        Ok(sql)
    }

    fn parse_table_constraint(&mut self, table_name: &str) -> Result<TableConstraint> {
        let name = if self.matches(&TokenKind::Constraint) {
            Some(self.parse_simple_identifier()?)
        } else {
            None
        };

        if self.matches(&TokenKind::Check) {
            let explicit_name = name.is_some();
            let name = name.unwrap_or_else(|| format!("{table_name}_check"));
            return Ok(TableConstraint::Check(
                self.parse_check_constraint(name, explicit_name)?,
            ));
        }

        if self.matches(&TokenKind::Foreign) {
            self.expect_keyword(TokenKind::Key)?;
            return Ok(TableConstraint::ForeignKey(
                self.parse_foreign_key_constraint(name)?,
            ));
        }

        if self.matches(&TokenKind::Primary) {
            self.expect_keyword(TokenKind::Key)?;
            let (columns, decorated_columns) =
                self.parse_parenthesized_constraint_indexed_columns()?;
            let conflict_clause = self.parse_optional_on_conflict_clause()?;
            let primary_key = if let Some(name) = name {
                PrimaryKeyConstraint::new(columns).named(name)
            } else {
                PrimaryKeyConstraint::new(columns)
            };
            let primary_key = if let Some(conflict_clause) = conflict_clause {
                primary_key.with_conflict_clause(conflict_clause)
            } else {
                primary_key
            };
            return Ok(TableConstraint::PrimaryKey(
                primary_key.with_decorated_columns(decorated_columns),
            ));
        }

        if self.matches(&TokenKind::Unique) {
            let (columns, decorated_columns) =
                self.parse_parenthesized_constraint_indexed_columns()?;
            let conflict_clause = self.parse_optional_on_conflict_clause()?;
            let unique = if let Some(name) = name {
                UniqueConstraint::new(columns).named(name)
            } else {
                UniqueConstraint::new(columns)
            };
            let unique = if let Some(conflict_clause) = conflict_clause {
                unique.with_conflict_clause(conflict_clause)
            } else {
                unique
            };
            return Ok(TableConstraint::Unique(
                unique.with_decorated_columns(decorated_columns),
            ));
        }

        Err(self.error_expected(&format!(
            "CHECK, FOREIGN KEY, PRIMARY KEY, or UNIQUE constraint, found {}",
            display_token(self.peek_kind())
        )))
    }

    fn parse_check_constraint(
        &mut self,
        name: String,
        explicit_name: bool,
    ) -> Result<CheckConstraint> {
        self.expect_symbol(TokenKind::LParen)?;
        let expr = self.parse_where_expr()?;
        self.expect_symbol(TokenKind::RParen)?;
        Ok(CheckConstraint {
            name,
            explicit_name,
            expr: Self::check_expr_from_expr(expr)?,
        })
    }

    fn parse_foreign_key_constraint(
        &mut self,
        constraint_name: Option<String>,
    ) -> Result<ForeignKey> {
        self.expect_symbol(TokenKind::LParen)?;
        let mut columns = vec![self.parse_simple_identifier()?];
        while self.matches(&TokenKind::Comma) {
            columns.push(self.parse_simple_identifier()?);
        }
        self.expect_symbol(TokenKind::RParen)?;
        self.expect_keyword(TokenKind::References)?;
        let ref_table = self.parse_simple_identifier()?;
        let mut foreign_key = if self.matches(&TokenKind::LParen) {
            let mut ref_columns = vec![self.parse_simple_identifier()?];
            while self.matches(&TokenKind::Comma) {
                ref_columns.push(self.parse_simple_identifier()?);
            }
            self.expect_symbol(TokenKind::RParen)?;
            ForeignKey::multi_column(columns, ref_table, ref_columns)
        } else {
            ForeignKey::multi_column_to_parent_primary_key(columns, ref_table)
        };
        if let Some(constraint_name) = constraint_name {
            foreign_key = foreign_key.named(constraint_name);
        }
        self.parse_optional_foreign_key_actions(foreign_key)
    }

    fn parse_optional_on_conflict_clause(&mut self) -> Result<Option<String>> {
        if !self.matches(&TokenKind::On) {
            return Ok(None);
        }

        let conflict = self.parse_conflict_clause_word()?;
        if !conflict.eq_ignore_ascii_case("CONFLICT") {
            return Err(DbError::sql(format!(
                "expected CONFLICT after ON at position {}",
                self.tokens[self.index.saturating_sub(1)].position
            )));
        }

        let resolution = self.parse_conflict_clause_word()?;
        if !matches!(
            resolution.to_ascii_uppercase().as_str(),
            "ROLLBACK" | "ABORT" | "FAIL" | "IGNORE" | "REPLACE"
        ) {
            return Err(DbError::sql(format!(
                "unsupported conflict resolution '{}' at position {}",
                resolution,
                self.tokens[self.index.saturating_sub(1)].position
            )));
        }

        Ok(Some(resolution))
    }

    fn parse_insert_or_conflict_resolution(&mut self) -> Result<String> {
        let resolution = self.parse_conflict_clause_word()?;
        if !matches!(
            resolution.to_ascii_uppercase().as_str(),
            "ROLLBACK" | "ABORT" | "FAIL" | "IGNORE" | "REPLACE"
        ) {
            return Err(DbError::sql(format!(
                "unsupported conflict resolution '{}' at position {}",
                resolution,
                self.tokens[self.index.saturating_sub(1)].position
            )));
        }
        Ok(resolution)
    }

    fn parse_insert_conflict_suffix(
        &mut self,
        existing_or_conflict: Option<String>,
    ) -> Result<InsertConflictSuffix> {
        if !self.matches(&TokenKind::On) {
            return Ok(InsertConflictSuffix::Legacy(existing_or_conflict));
        }
        self.expect_keyword(TokenKind::Conflict)?;
        let target = if self.matches(&TokenKind::LParen) {
            self.index = self.index.saturating_sub(1);
            Some(self.parse_parenthesized_identifier_list()?)
        } else {
            None
        };
        self.expect_keyword(TokenKind::Do)?;
        if self.matches(&TokenKind::Update) {
            self.expect_keyword(TokenKind::Set)?;
            let assignments = self.parse_assignments()?;
            let filter = if self.matches(&TokenKind::Where) {
                Some(self.parse_where_expr()?)
            } else {
                None
            };
            if existing_or_conflict.is_some() {
                return Err(DbError::sql(
                    "cannot combine INSERT OR <conflict-algorithm> with ON CONFLICT DO UPDATE",
                ));
            }
            return Ok(InsertConflictSuffix::DoUpdate {
                upsert: UpsertClause {
                    target,
                    assignments,
                    filter,
                },
            });
        }

        self.expect_keyword(TokenKind::Nothing)?;
        if existing_or_conflict.is_some() {
            return Err(DbError::sql(
                "cannot combine INSERT OR <conflict-algorithm> with ON CONFLICT DO NOTHING",
            ));
        }
        Ok(InsertConflictSuffix::DoNothing { target })
    }

    fn parse_optional_foreign_key_actions(
        &mut self,
        mut foreign_key: ForeignKey,
    ) -> Result<ForeignKey> {
        loop {
            if self.parse_optional_identifier_keyword_if("MATCH") {
                let match_clause = self.parse_conflict_clause_word()?;
                foreign_key = foreign_key.with_match(match_clause);
                continue;
            }
            if self.matches(&TokenKind::On) {
                if self.matches(&TokenKind::Delete) {
                    foreign_key = foreign_key.with_on_delete(self.parse_foreign_key_action()?);
                    continue;
                }
                if self.matches(&TokenKind::Update) {
                    foreign_key = foreign_key.with_on_update(self.parse_foreign_key_action()?);
                    continue;
                }
                return Err(DbError::sql(format!(
                    "expected DELETE or UPDATE after ON at position {}",
                    self.tokens[self.index.saturating_sub(1)].position
                )));
            }
            if let Some((deferrable, initially_deferred)) =
                self.parse_optional_deferrable_clause()?
            {
                if let Some(deferrable) = deferrable {
                    foreign_key = foreign_key.deferrable(deferrable);
                }
                if let Some(initially_deferred) = initially_deferred {
                    foreign_key = foreign_key.initially_deferred(initially_deferred);
                }
                continue;
            }
            break;
        }
        Ok(foreign_key)
    }

    fn parse_foreign_key_action(&mut self) -> Result<String> {
        if let Some(action) = self.parse_optional_identifier_keyword() {
            if matches!(action.to_ascii_uppercase().as_str(), "CASCADE" | "RESTRICT") {
                return Ok(action.to_ascii_uppercase());
            }
            if action.eq_ignore_ascii_case("NO") {
                let target = self.parse_conflict_clause_word()?;
                if target.eq_ignore_ascii_case("ACTION") {
                    return Ok("NO ACTION".to_string());
                }
                return Err(DbError::sql(format!(
                    "unsupported foreign key NO action '{}' at position {}",
                    target,
                    self.tokens[self.index.saturating_sub(1)].position
                )));
            }
        }
        if self.matches(&TokenKind::Set) {
            let target = self.parse_conflict_clause_word()?;
            if matches!(target.to_ascii_uppercase().as_str(), "NULL" | "DEFAULT") {
                return Ok(format!("SET {}", target.to_ascii_uppercase()));
            }
            return Err(DbError::sql(format!(
                "unsupported foreign key SET action '{}' at position {}",
                target,
                self.tokens[self.index.saturating_sub(1)].position
            )));
        }

        Err(self.error_expected(&format!(
            "foreign key action, found {}",
            display_token(self.peek_kind())
        )))
    }

    fn parse_optional_deferrable_clause(&mut self) -> Result<Option<(Option<bool>, Option<bool>)>> {
        let saw_not = self.matches(&TokenKind::Not);
        if self.parse_optional_identifier_keyword_if("DEFERRABLE") {
            let initially_deferred = self.parse_optional_initially_clause()?;
            return Ok(Some((Some(!saw_not), initially_deferred)));
        }
        if saw_not {
            return Err(self.error_expected(&format!(
                "DEFERRABLE after NOT, found {}",
                display_token(self.peek_kind())
            )));
        }
        if self.parse_optional_identifier_keyword_if("INITIALLY") {
            return Ok(Some((None, Some(self.parse_deferrable_initial_mode()?))));
        }
        Ok(None)
    }

    fn parse_optional_initially_clause(&mut self) -> Result<Option<bool>> {
        if !self.parse_optional_identifier_keyword_if("INITIALLY") {
            return Ok(None);
        }
        Ok(Some(self.parse_deferrable_initial_mode()?))
    }

    fn parse_deferrable_initial_mode(&mut self) -> Result<bool> {
        if self.parse_optional_identifier_keyword_if("DEFERRED") {
            return Ok(true);
        }
        if self.parse_optional_identifier_keyword_if("IMMEDIATE") {
            return Ok(false);
        }
        Err(self.error_expected(&format!(
            "DEFERRED or IMMEDIATE, found {}",
            display_token(self.peek_kind())
        )))
    }

    fn parse_conflict_clause_word(&mut self) -> Result<String> {
        match self.peek_kind() {
            TokenKind::Identifier(word) => {
                let word = word.clone();
                self.advance();
                Ok(word)
            }
            TokenKind::Conflict => {
                self.advance();
                Ok("CONFLICT".to_string())
            }
            TokenKind::Replace => {
                self.advance();
                Ok("REPLACE".to_string())
            }
            TokenKind::Rollback => {
                self.advance();
                Ok("ROLLBACK".to_string())
            }
            TokenKind::Null => {
                self.advance();
                Ok("NULL".to_string())
            }
            TokenKind::Default => {
                self.advance();
                Ok("DEFAULT".to_string())
            }
            TokenKind::Full => {
                self.advance();
                Ok("FULL".to_string())
            }
            token => Err(self.error_expected(&format!(
                "conflict clause keyword, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_optional_identifier_keyword(&mut self) -> Option<String> {
        let TokenKind::Identifier(word) = self.peek_kind() else {
            return None;
        };
        let word = word.clone();
        self.advance();
        Some(word)
    }

    fn parse_optional_identifier_keyword_if(&mut self, expected: &str) -> bool {
        let Some(word) = self.parse_optional_identifier_keyword() else {
            return false;
        };
        if word.eq_ignore_ascii_case(expected) {
            return true;
        }
        self.index = self.index.saturating_sub(1);
        false
    }

    fn check_expr_from_expr(expr: Expr) -> Result<CheckExpr> {
        Ok(match expr {
            Expr::Compare { column, op, value } => CheckExpr::Compare {
                column,
                op: Self::check_op_from_compare_op(op),
                value,
            },
            Expr::IsNull { column, negated } => CheckExpr::IsNull { column, negated },
            Expr::Glob {
                column,
                pattern,
                negated,
            } => CheckExpr::Glob {
                column,
                pattern,
                negated,
            },
            Expr::Like {
                column,
                pattern,
                escape,
                negated,
            } => CheckExpr::Like {
                column,
                pattern,
                escape,
                negated,
            },
            Expr::InList {
                column,
                values,
                negated,
            } => CheckExpr::InList {
                column,
                values,
                negated,
            },
            Expr::Between {
                column,
                low,
                high,
                negated,
            } => CheckExpr::Between {
                column,
                low,
                high,
                negated,
            },
            Expr::IsBool {
                expr,
                value,
                negated,
                explicit,
            } => match expr {
                ScalarExpr::Column(column) => {
                    if explicit {
                        CheckExpr::IsBool {
                            column,
                            value,
                            negated,
                        }
                    } else if value && !negated {
                        CheckExpr::Truthy { column }
                    } else {
                        CheckExpr::Not(Box::new(CheckExpr::Truthy { column }))
                    }
                }
                _ => return Err(DbError::sql("unsupported CHECK expression")),
            },
            Expr::Is {
                left,
                right,
                negated,
            } => match (left, right) {
                (ScalarExpr::Column(column), ScalarExpr::Literal(value)) => CheckExpr::IsDistinct {
                    column,
                    value,
                    negated,
                },
                _ => return Err(DbError::sql("unsupported CHECK expression")),
            },
            Expr::And(left, right) => CheckExpr::And(
                Box::new(Self::check_expr_from_expr(*left)?),
                Box::new(Self::check_expr_from_expr(*right)?),
            ),
            Expr::Or(left, right) => CheckExpr::Or(
                Box::new(Self::check_expr_from_expr(*left)?),
                Box::new(Self::check_expr_from_expr(*right)?),
            ),
            Expr::Not(expr) => CheckExpr::Not(Box::new(Self::check_expr_from_expr(*expr)?)),
            _ => return Err(DbError::sql("unsupported CHECK expression")),
        })
    }

    fn check_op_from_compare_op(op: CompareOp) -> CheckOp {
        match op {
            CompareOp::Eq => CheckOp::Eq,
            CompareOp::Ne => CheckOp::Ne,
            CompareOp::Gt => CheckOp::Gt,
            CompareOp::Gte => CheckOp::Gte,
            CompareOp::Lt => CheckOp::Lt,
            CompareOp::Lte => CheckOp::Lte,
        }
    }

    fn parse_optional_column_type(&mut self) -> Option<ColumnType> {
        let start = self.index;
        let mut declared_type = vec![self.parse_optional_declared_type_word()?];

        while let Some(word) = self.parse_optional_declared_type_word() {
            declared_type.push(word);
        }

        if self.parse_optional_type_modifiers().is_err() {
            self.index = start;
            return None;
        }

        let declared_type = declared_type.join(" ");
        match sqlite_declared_type_affinity(&declared_type) {
            Some(column_type) => Some(column_type),
            None => {
                self.index = start;
                None
            }
        }
    }

    fn parse_optional_cast_type(&mut self) -> Option<ColumnType> {
        let start = self.index;
        let mut declared_type = vec![self.parse_optional_declared_type_word()?];

        while let Some(word) = self.parse_optional_declared_type_word() {
            declared_type.push(word);
        }

        if self.parse_optional_type_modifiers().is_err() {
            self.index = start;
            return None;
        }

        let declared_type = declared_type.join(" ");
        if sqlite_declared_type_is_numeric(&declared_type) {
            return Some(ColumnType::Numeric);
        }
        match sqlite_declared_type_affinity(&declared_type) {
            Some(column_type) => Some(column_type),
            None => {
                self.index = start;
                None
            }
        }
    }

    fn parse_optional_declared_type_word(&mut self) -> Option<String> {
        let word = match self.peek_kind() {
            TokenKind::IntegerType => "INTEGER".to_string(),
            TokenKind::TextType => "TEXT".to_string(),
            TokenKind::BlobType => "BLOB".to_string(),
            TokenKind::BooleanType => "BOOLEAN".to_string(),
            TokenKind::Identifier(name) => name.to_ascii_uppercase(),
            _ => return None,
        };
        self.advance();
        Some(word)
    }

    fn parse_optional_type_modifiers(&mut self) -> Result<()> {
        if !self.matches(&TokenKind::LParen) {
            return Ok(());
        }

        self.expect_integer_literal_type_modifier()?;
        while self.matches(&TokenKind::Comma) {
            self.expect_integer_literal_type_modifier()?;
        }
        self.expect_symbol(TokenKind::RParen)?;
        Ok(())
    }

    fn expect_integer_literal_type_modifier(&mut self) -> Result<()> {
        match self.peek_kind() {
            TokenKind::Integer(_) => {
                self.advance();
                Ok(())
            }
            token => Err(self.error_expected(&format!(
                "integer type modifier, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_optional_collation_name(&mut self) -> Result<Option<String>> {
        if self.matches(&TokenKind::Collate) {
            return Ok(Some(self.parse_simple_identifier()?));
        }
        Ok(None)
    }

    fn parse_index_predicate_sql(&mut self) -> Result<String> {
        let start = self.index;
        while !matches!(self.peek_kind(), TokenKind::Semicolon | TokenKind::Eof) {
            self.advance();
        }
        let predicate_tokens = &self.tokens[start..self.index];
        if predicate_tokens.is_empty() {
            return Err(DbError::sql("partial index WHERE clause cannot be empty"));
        }
        Ok(predicate_tokens
            .iter()
            .map(token_sql_fragment)
            .collect::<Vec<_>>()
            .join(" "))
    }

    fn parse_parenthesized_identifier_list(&mut self) -> Result<Vec<String>> {
        self.expect_symbol(TokenKind::LParen)?;
        let mut values = vec![self.parse_simple_identifier()?];
        while self.matches(&TokenKind::Comma) {
            values.push(self.parse_simple_identifier()?);
        }
        self.expect_symbol(TokenKind::RParen)?;
        Ok(values)
    }

    fn parse_parenthesized_constraint_indexed_columns(
        &mut self,
    ) -> Result<(Vec<String>, Vec<String>)> {
        self.expect_symbol(TokenKind::LParen)?;
        let mut values = vec![self.parse_constraint_indexed_column()?];
        while self.matches(&TokenKind::Comma) {
            values.push(self.parse_constraint_indexed_column()?);
        }
        self.expect_symbol(TokenKind::RParen)?;
        let columns = values
            .iter()
            .map(|(column, _)| column.clone())
            .collect::<Vec<_>>();
        let decorated_columns = values
            .into_iter()
            .map(|(_, decorated)| decorated)
            .collect::<Vec<_>>();
        Ok((columns, decorated_columns))
    }

    fn parse_constraint_indexed_column(&mut self) -> Result<(String, String)> {
        let column = self.parse_simple_identifier()?;
        let mut decorated = column.clone();
        if let Some(collation) = self.parse_optional_collation_name()? {
            decorated.push_str(" COLLATE ");
            decorated.push_str(&collation);
        }
        if self.matches(&TokenKind::Asc) {
            decorated.push_str(" ASC");
        } else if self.matches(&TokenKind::Desc) {
            decorated.push_str(" DESC");
        }
        Ok((column, decorated))
    }

    fn parse_assignments(&mut self) -> Result<Vec<Assignment>> {
        let mut assignments = Vec::new();
        loop {
            let column = self.parse_simple_identifier()?;
            self.expect_symbol(TokenKind::Eq)?;
            let value = self.parse_scalar_expr()?;
            assignments.push(Assignment { column, value });
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        Ok(assignments)
    }

    fn parse_order_by_items(&mut self) -> Result<Vec<OrderBy>> {
        let mut items = Vec::new();
        loop {
            let expr = if let TokenKind::Integer(value) = self.peek_kind()
                && *value > 0
                && matches!(
                    self.tokens.get(self.index + 1).map(|token| &token.kind),
                    Some(
                        TokenKind::Asc
                            | TokenKind::Desc
                            | TokenKind::Nulls
                            | TokenKind::Comma
                            | TokenKind::Semicolon
                            | TokenKind::Eof
                    )
                ) {
                let position = usize::try_from(*value)
                    .map_err(|_| DbError::sql("ORDER BY position is too large"))?;
                self.advance();
                OrderByExpr::Position(position)
            } else {
                let expr = self.parse_scalar_expr()?;
                match expr {
                    ScalarExpr::Column(name) => OrderByExpr::Column(name),
                    ScalarExpr::Collate { expr, collation } => {
                        items.push(OrderBy {
                            expr: match *expr {
                                ScalarExpr::Column(name) => OrderByExpr::Column(name),
                                expr => OrderByExpr::Expr(expr),
                            },
                            collation: Some(collation),
                            descending: if self.matches(&TokenKind::Desc) {
                                true
                            } else {
                                self.matches(&TokenKind::Asc);
                                false
                            },
                            nulls: if self.matches(&TokenKind::Nulls) {
                                if self.matches(&TokenKind::First) {
                                    Some(NullOrder::First)
                                } else if self.matches(&TokenKind::Last) {
                                    Some(NullOrder::Last)
                                } else {
                                    return Err(self.error_expected("FIRST or LAST after NULLS"));
                                }
                            } else {
                                None
                            },
                        });
                        if !self.matches(&TokenKind::Comma) {
                            break;
                        }
                        continue;
                    }
                    expr => OrderByExpr::Expr(expr),
                }
            };
            let collation = self.parse_optional_collation_name()?;
            let descending = if self.matches(&TokenKind::Desc) {
                true
            } else {
                self.matches(&TokenKind::Asc);
                false
            };
            let nulls = if self.matches(&TokenKind::Nulls) {
                if self.matches(&TokenKind::First) {
                    Some(NullOrder::First)
                } else if self.matches(&TokenKind::Last) {
                    Some(NullOrder::Last)
                } else {
                    return Err(self.error_expected(&format!(
                        "FIRST or LAST after NULLS, found {}",
                        display_token(self.peek_kind())
                    )));
                }
            } else {
                None
            };
            items.push(OrderBy {
                expr,
                collation,
                descending,
                nulls,
            });
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn parse_group_by_items(&mut self) -> Result<Vec<ScalarExpr>> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_scalar_expr()?);
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn parse_join_clauses(&mut self, from: &FromItem) -> Result<Vec<JoinClause>> {
        let mut joins = Vec::new();
        let left_qualifier = from_item_qualifier(from);
        loop {
            let natural = self.matches(&TokenKind::Natural);
            let kind = if self.matches(&TokenKind::Inner) {
                self.expect_keyword(TokenKind::Join)?;
                JoinKind::Inner
            } else if self.matches(&TokenKind::Cross) {
                self.expect_keyword(TokenKind::Join)?;
                let source = self.parse_from_item()?;
                let (on, using_columns) = if natural {
                    (
                        Expr::CompareScalar {
                            left: ScalarExpr::Literal(Value::Boolean(true)),
                            op: CompareOp::Eq,
                            right: ScalarExpr::Literal(Value::Boolean(true)),
                        },
                        Vec::new(),
                    )
                } else if self.matches(&TokenKind::On) {
                    (self.parse_where_expr()?, Vec::new())
                } else if self.matches(&TokenKind::Using) {
                    self.parse_join_using_expr(left_qualifier.as_deref(), &source)?
                } else {
                    (
                        Expr::CompareScalar {
                            left: ScalarExpr::Literal(Value::Boolean(true)),
                            op: CompareOp::Eq,
                            right: ScalarExpr::Literal(Value::Boolean(true)),
                        },
                        Vec::new(),
                    )
                };
                joins.push(JoinClause {
                    kind: JoinKind::Inner,
                    source,
                    using_columns,
                    natural,
                    on,
                });
                continue;
            } else if self.matches(&TokenKind::Left) {
                let _ = self.matches(&TokenKind::Outer);
                self.expect_keyword(TokenKind::Join)?;
                JoinKind::Left
            } else if self.matches(&TokenKind::Right) {
                let _ = self.matches(&TokenKind::Outer);
                self.expect_keyword(TokenKind::Join)?;
                JoinKind::Right
            } else if self.matches(&TokenKind::Full) {
                let _ = self.matches(&TokenKind::Outer);
                self.expect_keyword(TokenKind::Join)?;
                JoinKind::Full
            } else if self.matches(&TokenKind::Join) {
                JoinKind::Inner
            } else {
                break;
            };

            let source = self.parse_from_item()?;
            let (on, using_columns) = if natural {
                (
                    Expr::CompareScalar {
                        left: ScalarExpr::Literal(Value::Boolean(true)),
                        op: CompareOp::Eq,
                        right: ScalarExpr::Literal(Value::Boolean(true)),
                    },
                    Vec::new(),
                )
            } else if self.matches(&TokenKind::On) {
                (self.parse_where_expr()?, Vec::new())
            } else if self.matches(&TokenKind::Using) {
                self.parse_join_using_expr(left_qualifier.as_deref(), &source)?
            } else {
                return Err(self.error_expected(&format!(
                    "ON or USING, found {}",
                    display_token(self.peek_kind())
                )));
            };
            joins.push(JoinClause {
                kind,
                source,
                on,
                using_columns,
                natural,
            });
        }
        Ok(joins)
    }

    fn parse_join_using_expr(
        &mut self,
        left_qualifier: Option<&str>,
        right_source: &FromItem,
    ) -> Result<(Expr, Vec<String>)> {
        self.expect_symbol(TokenKind::LParen)?;
        let right_qualifier = from_item_qualifier(right_source);
        let mut terms = Vec::new();
        let mut columns = Vec::new();
        loop {
            let column = self.parse_simple_identifier()?;
            let left = qualify_join_using_column(left_qualifier, &column);
            let right = qualify_join_using_column(right_qualifier.as_deref(), &column);
            columns.push(column);
            terms.push(Expr::CompareColumns {
                left,
                op: CompareOp::Eq,
                right,
            });
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_symbol(TokenKind::RParen)?;
        Ok((rebuild_and_expr(terms), columns))
    }

    fn is_subquery_start(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::LParen)
            && matches!(
                self.tokens.get(self.index + 1).map(|token| &token.kind),
                Some(TokenKind::Select)
            )
    }

    fn parse_subquery(&mut self) -> Result<SelectStatement> {
        self.expect_symbol(TokenKind::LParen)?;
        let query = self.parse_select_statement()?;
        self.expect_symbol(TokenKind::RParen)?;
        Ok(query)
    }

    fn parse_subquery_after_exists(&mut self) -> Result<SelectStatement> {
        self.parse_subquery()
    }

    fn parse_limit_value(&mut self) -> Result<i64> {
        let negative = self.matches(&TokenKind::Minus);
        match self.peek_kind() {
            TokenKind::Integer(value) => {
                let mut value = *value;
                self.advance();
                if negative {
                    value = value
                        .checked_neg()
                        .ok_or_else(|| DbError::sql("LIMIT/OFFSET literal is out of range"))?;
                }
                Ok(value)
            }
            TokenKind::Real(value) => {
                let mut value = *value;
                self.advance();
                if negative {
                    value = -value;
                }
                if !value.is_finite()
                    || value.fract() != 0.0
                    || value < i64::MIN as f64
                    || value > i64::MAX as f64
                {
                    return Err(DbError::sql("LIMIT/OFFSET literal is out of range"));
                }
                Ok(value as i64)
            }
            token => {
                Err(self
                    .error_expected(&format!("numeric literal, found {}", display_token(token))))
            }
        }
    }

    fn parse_parenthesized_scalar_exprs(&mut self) -> Result<Vec<ScalarExpr>> {
        self.expect_symbol(TokenKind::LParen)?;
        if self.matches(&TokenKind::RParen) {
            return Err(self.error_expected("literal or scalar expression"));
        }
        let mut values = Vec::new();
        loop {
            values.push(self.parse_scalar_expr()?);
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_symbol(TokenKind::RParen)?;
        Ok(values)
    }

    fn parse_literal(&mut self) -> Result<Value> {
        if self.matches(&TokenKind::Plus) {
            match self.peek_kind() {
                TokenKind::Integer(value) => {
                    let value = *value;
                    self.advance();
                    return Ok(Value::Integer(value));
                }
                TokenKind::Real(value) => {
                    let value = *value;
                    self.advance();
                    return Ok(Value::Real(value));
                }
                _ => return Err(self.error_expected("numeric literal after +")),
            }
        }
        if self.matches(&TokenKind::Minus) {
            match self.peek_kind() {
                TokenKind::Integer(value) => {
                    let value = *value;
                    self.advance();
                    return Ok(Value::Integer(-value));
                }
                TokenKind::Real(value) => {
                    let value = *value;
                    self.advance();
                    return Ok(Value::Real(-value));
                }
                _ => return Err(self.error_expected("numeric literal after -")),
            }
        }

        match self.peek_kind() {
            TokenKind::Integer(value) => {
                let value = *value;
                self.advance();
                Ok(Value::Integer(value))
            }
            TokenKind::Real(value) => {
                let value = *value;
                self.advance();
                Ok(Value::Real(value))
            }
            TokenKind::BlobLiteral(value) => {
                let value = value.clone();
                self.advance();
                Ok(Value::Blob(value))
            }
            TokenKind::String(value) => {
                let value = value.clone();
                self.advance();
                Ok(Value::Text(value))
            }
            TokenKind::True => {
                self.advance();
                Ok(Value::Boolean(true))
            }
            TokenKind::False => {
                self.advance();
                Ok(Value::Boolean(false))
            }
            TokenKind::Null => {
                self.advance();
                Ok(Value::Null)
            }
            token => Err(self.error_expected(&format!("literal, found {}", display_token(token)))),
        }
    }

    fn parse_column_default_value(&mut self) -> Result<ColumnDefault> {
        if self.matches(&TokenKind::LParen) {
            let default = if let TokenKind::Identifier(name) = self.peek_kind()
                && name.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
            {
                self.advance();
                ColumnDefault::CurrentTimestamp
            } else if let TokenKind::Identifier(name) = self.peek_kind()
                && name.eq_ignore_ascii_case("CURRENT_DATE")
            {
                self.advance();
                ColumnDefault::CurrentDate
            } else if let TokenKind::Identifier(name) = self.peek_kind()
                && name.eq_ignore_ascii_case("CURRENT_TIME")
            {
                self.advance();
                ColumnDefault::CurrentTime
            } else {
                ColumnDefault::Literal(self.parse_literal()?)
            };
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(default);
        }

        if let TokenKind::Identifier(name) = self.peek_kind()
            && name.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
        {
            self.advance();
            return Ok(ColumnDefault::CurrentTimestamp);
        }
        if let TokenKind::Identifier(name) = self.peek_kind()
            && name.eq_ignore_ascii_case("CURRENT_DATE")
        {
            self.advance();
            return Ok(ColumnDefault::CurrentDate);
        }
        if let TokenKind::Identifier(name) = self.peek_kind()
            && name.eq_ignore_ascii_case("CURRENT_TIME")
        {
            self.advance();
            return Ok(ColumnDefault::CurrentTime);
        }

        Ok(ColumnDefault::Literal(self.parse_literal()?))
    }

    fn parse_string_literal(&mut self) -> Result<String> {
        match self.parse_literal()? {
            Value::Text(value) => Ok(value),
            value => Err(DbError::sql(format!(
                "expected string literal, found {}",
                value.type_name()
            ))),
        }
    }

    fn parse_identifier(&mut self) -> Result<String> {
        let mut identifier = self.parse_simple_identifier()?;
        while self.matches(&TokenKind::Dot) {
            let segment = self.parse_simple_identifier()?;
            identifier.push('.');
            identifier.push_str(&segment);
        }
        Ok(identifier)
    }

    fn parse_simple_identifier(&mut self) -> Result<String> {
        match self.peek_kind() {
            TokenKind::Identifier(name) => {
                let name = name.clone();
                self.advance();
                Ok(name)
            }
            TokenKind::Nulls => {
                self.advance();
                Ok("nulls".to_string())
            }
            TokenKind::First => {
                self.advance();
                Ok("first".to_string())
            }
            TokenKind::Last => {
                self.advance();
                Ok("last".to_string())
            }
            TokenKind::Level => {
                self.advance();
                Ok("level".to_string())
            }
            TokenKind::Offset => {
                self.advance();
                Ok("offset".to_string())
            }
            TokenKind::Read => {
                self.advance();
                Ok("read".to_string())
            }
            TokenKind::Committed => {
                self.advance();
                Ok("committed".to_string())
            }
            TokenKind::Repeatable => {
                self.advance();
                Ok("repeatable".to_string())
            }
            TokenKind::Serializable => {
                self.advance();
                Ok("serializable".to_string())
            }
            TokenKind::Replace => {
                self.advance();
                Ok("replace".to_string())
            }
            token => {
                Err(self.error_expected(&format!("identifier, found {}", display_token(token))))
            }
        }
    }

    fn parse_optional_table_alias(&mut self) -> Result<Option<String>> {
        if self.matches(&TokenKind::As) {
            return Ok(Some(self.parse_simple_identifier()?));
        }
        if self.is_table_index_hint_start() {
            return Ok(None);
        }
        if is_identifier_token(self.peek_kind()) {
            return Ok(Some(self.parse_simple_identifier()?));
        }
        Ok(None)
    }

    fn is_table_index_hint_start(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Not)
            || matches!(self.peek_kind(), TokenKind::Identifier(name) if name.eq_ignore_ascii_case("INDEXED"))
    }

    fn expect_keyword(&mut self, expected: TokenKind) -> Result<()> {
        if same_variant(self.peek_kind(), &expected) {
            self.advance();
            Ok(())
        } else {
            Err(self.error_expected(&format!(
                "{}, found {}",
                display_token(&expected),
                display_token(self.peek_kind())
            )))
        }
    }

    fn expect_symbol(&mut self, expected: TokenKind) -> Result<()> {
        self.expect_keyword(expected)
    }

    fn expect_eof(&self) -> Result<()> {
        if matches!(self.peek_kind(), TokenKind::Eof) {
            Ok(())
        } else {
            Err(self.error_expected(&format!(
                "end of input, found {}",
                display_token(self.peek_kind())
            )))
        }
    }

    fn matches(&mut self, expected: &TokenKind) -> bool {
        if same_variant(self.peek_kind(), expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.index].kind
    }

    fn advance(&mut self) {
        if self.index + 1 < self.tokens.len() {
            self.index += 1;
        }
    }

    fn error_expected(&self, message: &str) -> DbError {
        let token = &self.tokens[self.index];
        DbError::sql(format!("expected {message} at position {}", token.position))
    }
}

fn same_variant(left: &TokenKind, right: &TokenKind) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}

fn is_identifier_token(token: &TokenKind) -> bool {
    matches!(
        token,
        TokenKind::Identifier(_)
            | TokenKind::Nulls
            | TokenKind::First
            | TokenKind::Last
            | TokenKind::Level
            | TokenKind::Offset
            | TokenKind::Read
            | TokenKind::Committed
            | TokenKind::Repeatable
            | TokenKind::Serializable
    )
}

fn is_select_alias_token(token: &TokenKind) -> bool {
    matches!(
        token,
        TokenKind::String(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Begin
            | TokenKind::Rollback
    ) || is_identifier_token(token)
}

fn sqlite_limit_value(value: i64) -> Option<usize> {
    (value >= 0).then_some(value as usize)
}

fn sqlite_offset_value(value: i64) -> usize {
    value.max(0) as usize
}

fn is_scalar_expr_start(token: &TokenKind) -> bool {
    is_identifier_token(token)
        || matches!(
            token,
            TokenKind::LParen
                | TokenKind::Case
                | TokenKind::Not
                | TokenKind::Minus
                | TokenKind::Replace
                | TokenKind::Integer(_)
                | TokenKind::Real(_)
                | TokenKind::BlobLiteral(_)
                | TokenKind::String(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Null
        )
}

fn is_filter_expr_terminator(token: &TokenKind) -> bool {
    matches!(
        token,
        TokenKind::And
            | TokenKind::Or
            | TokenKind::Group
            | TokenKind::Having
            | TokenKind::On
            | TokenKind::Order
            | TokenKind::Limit
            | TokenKind::Offset
            | TokenKind::Union
            | TokenKind::RParen
            | TokenKind::Semicolon
            | TokenKind::Eof
    )
}

fn is_scalar_suffix_after_group(token: &TokenKind) -> bool {
    matches!(
        token,
        TokenKind::Eq
            | TokenKind::Ne
            | TokenKind::Gt
            | TokenKind::Gte
            | TokenKind::Lt
            | TokenKind::Lte
            | TokenKind::Is
            | TokenKind::IsNull
            | TokenKind::NotNull
            | TokenKind::Like
            | TokenKind::Glob
            | TokenKind::Between
            | TokenKind::In
            | TokenKind::Not
    )
}

fn sqlite_current_time_literal_expr(name: &str) -> Option<ScalarExpr> {
    let func = match name.to_ascii_uppercase().as_str() {
        "CURRENT_DATE" => ScalarFunc::Date,
        "CURRENT_TIME" => ScalarFunc::Time,
        "CURRENT_TIMESTAMP" => ScalarFunc::DateTime,
        _ => return None,
    };
    Some(ScalarExpr::Function {
        func,
        args: vec![ScalarExpr::Literal(Value::from("now"))],
    })
}

fn sqlite_declared_type_is_numeric(declared_type: &str) -> bool {
    let normalized = declared_type.trim().to_ascii_uppercase();
    matches!(normalized.as_str(), "NUMERIC" | "DECIMAL")
        || normalized.starts_with("NUMERIC(")
        || normalized.starts_with("DECIMAL(")
}

fn scalar_expr_literal_value(expr: &ScalarExpr) -> Option<Value> {
    match expr {
        ScalarExpr::Literal(value) => Some(value.clone()),
        ScalarExpr::UnaryMinus(expr) => match expr.as_ref() {
            ScalarExpr::Literal(Value::Integer(value)) => Some(Value::Integer(-value)),
            ScalarExpr::Literal(Value::Real(value)) => Some(Value::Real(-value)),
            _ => None,
        },
        _ => None,
    }
}

fn scalar_expr_list_literal_values(values: &[ScalarExpr]) -> Option<Vec<Value>> {
    values.iter().map(scalar_expr_literal_value).collect()
}

fn scalar_expr_row_literal_values(values: &[ScalarExpr]) -> Option<Vec<Value>> {
    values.iter().map(scalar_expr_literal_value).collect()
}

fn scalar_expr_rows_literal_values(rows: &[Vec<ScalarExpr>]) -> Option<Vec<Vec<Value>>> {
    rows.iter()
        .map(|values| scalar_expr_row_literal_values(values))
        .collect()
}

fn parse_values_rows_as_in_candidates(rows: Vec<Vec<ScalarExpr>>) -> Vec<ScalarExpr> {
    rows.into_iter()
        .map(|mut row| {
            if row.len() == 1 {
                row.remove(0)
            } else {
                ScalarExpr::Tuple(row)
            }
        })
        .collect()
}

fn aggregate_arg_nested_function_name(arg: &AggregateArg) -> Option<&'static str> {
    match arg {
        AggregateArg::Wildcard => None,
        AggregateArg::Expr { expr, order_by, .. } => scalar_expr_nested_aggregate_name(expr)
            .or_else(|| order_by_nested_aggregate_name(order_by)),
        AggregateArg::GroupConcat {
            expr,
            separator,
            order_by,
            ..
        } => scalar_expr_nested_aggregate_name(expr)
            .or_else(|| {
                separator
                    .as_ref()
                    .and_then(scalar_expr_nested_aggregate_name)
            })
            .or_else(|| order_by_nested_aggregate_name(order_by)),
        AggregateArg::JsonGroupObject {
            key,
            value,
            order_by,
        } => scalar_expr_nested_aggregate_name(key)
            .or_else(|| scalar_expr_nested_aggregate_name(value))
            .or_else(|| order_by_nested_aggregate_name(order_by)),
        AggregateArg::Percentile {
            expr,
            fraction,
            order_by,
        } => scalar_expr_nested_aggregate_name(expr)
            .or_else(|| scalar_expr_nested_aggregate_name(fraction))
            .or_else(|| order_by_nested_aggregate_name(order_by)),
    }
}

fn order_by_nested_aggregate_name(order_by: &[OrderBy]) -> Option<&'static str> {
    order_by.iter().find_map(|item| match &item.expr {
        OrderByExpr::Expr(expr) => scalar_expr_nested_aggregate_name(expr),
        OrderByExpr::Column(_) | OrderByExpr::Position(_) => None,
    })
}

fn scalar_expr_nested_aggregate_name(expr: &ScalarExpr) -> Option<&'static str> {
    match expr {
        ScalarExpr::Aggregate { func, .. } => Some(aggregate_function_name(*func)),
        ScalarExpr::Tuple(values) => values.iter().find_map(scalar_expr_nested_aggregate_name),
        ScalarExpr::UnaryMinus(expr)
        | ScalarExpr::BitNot(expr)
        | ScalarExpr::Not(expr)
        | ScalarExpr::Cast { expr, .. }
        | ScalarExpr::Collate { expr, .. }
        | ScalarExpr::IsBool { expr, .. }
        | ScalarExpr::Like { expr, .. }
        | ScalarExpr::Glob { expr, .. } => scalar_expr_nested_aggregate_name(expr),
        ScalarExpr::Is { left, right, .. }
        | ScalarExpr::Compare { left, right, .. }
        | ScalarExpr::Binary { left, right, .. } => scalar_expr_nested_aggregate_name(left)
            .or_else(|| scalar_expr_nested_aggregate_name(right)),
        ScalarExpr::InList { expr, values, .. } => scalar_expr_nested_aggregate_name(expr)
            .or_else(|| values.iter().find_map(scalar_expr_nested_aggregate_name)),
        ScalarExpr::InSubquery { expr, .. } | ScalarExpr::CompareSubquery { left: expr, .. } => {
            scalar_expr_nested_aggregate_name(expr)
        }
        ScalarExpr::Subquery { .. } => None,
        ScalarExpr::Between {
            expr, low, high, ..
        } => scalar_expr_nested_aggregate_name(expr)
            .or_else(|| scalar_expr_nested_aggregate_name(low))
            .or_else(|| scalar_expr_nested_aggregate_name(high)),
        ScalarExpr::Case {
            base,
            when_then_clauses,
            else_expr,
        } => base
            .as_deref()
            .and_then(scalar_expr_nested_aggregate_name)
            .or_else(|| {
                when_then_clauses.iter().find_map(|(when_expr, then_expr)| {
                    scalar_expr_nested_aggregate_name(when_expr)
                        .or_else(|| scalar_expr_nested_aggregate_name(then_expr))
                })
            })
            .or_else(|| {
                else_expr
                    .as_deref()
                    .and_then(scalar_expr_nested_aggregate_name)
            }),
        ScalarExpr::Function { args, .. } => {
            args.iter().find_map(scalar_expr_nested_aggregate_name)
        }
        ScalarExpr::Literal(_) | ScalarExpr::Column(_) => None,
    }
}

fn aggregate_function_name(func: AggregateFunc) -> &'static str {
    match func {
        AggregateFunc::Count => "COUNT",
        AggregateFunc::Sum => "SUM",
        AggregateFunc::Avg => "AVG",
        AggregateFunc::Total => "TOTAL",
        AggregateFunc::Median => "MEDIAN",
        AggregateFunc::Percentile => "PERCENTILE",
        AggregateFunc::PercentileCont => "PERCENTILE_CONT",
        AggregateFunc::PercentileDisc => "PERCENTILE_DISC",
        AggregateFunc::GroupConcat => "GROUP_CONCAT",
        AggregateFunc::JsonGroupArray => "JSON_GROUP_ARRAY",
        AggregateFunc::JsonGroupObject => "JSON_GROUP_OBJECT",
        AggregateFunc::Min => "MIN",
        AggregateFunc::Max => "MAX",
    }
}

fn is_aggregate_function_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT"
            | "SUM"
            | "AVG"
            | "TOTAL"
            | "MEDIAN"
            | "PERCENTILE"
            | "PERCENTILE_CONT"
            | "PERCENTILE_DISC"
            | "GROUP_CONCAT"
            | "STRING_AGG"
            | "JSON_GROUP_ARRAY"
            | "JSON_GROUP_OBJECT"
            | "MIN"
            | "MAX"
    )
}

fn sqlite_declared_type_affinity(declared_type: &str) -> Option<ColumnType> {
    let normalized = declared_type.trim().to_ascii_uppercase();

    if normalized.contains("INT") {
        return Some(ColumnType::Integer);
    }
    if matches!(
        normalized.as_str(),
        "CHAR" | "CLOB" | "TEXT" | "VARCHAR" | "NCHAR" | "NVARCHAR"
    ) || normalized.contains("CHAR")
        || normalized.contains("CLOB")
        || normalized.contains("TEXT")
    {
        return Some(ColumnType::Text);
    }
    if normalized.contains("BLOB") || normalized.is_empty() {
        return Some(ColumnType::Blob);
    }
    if normalized == "BOOLEAN" {
        return Some(ColumnType::Boolean);
    }
    if matches!(
        normalized.as_str(),
        "REAL" | "DOUBLE" | "DOUBLE PRECISION" | "FLOAT" | "DATE" | "DATETIME"
    ) {
        return Some(ColumnType::Real);
    }
    if matches!(normalized.as_str(), "NUMERIC" | "DECIMAL")
        || normalized.starts_with("NUMERIC(")
        || normalized.starts_with("DECIMAL(")
    {
        return Some(ColumnType::Real);
    }

    None
}

fn from_item_qualifier(from: &FromItem) -> Option<String> {
    match from {
        FromItem::Table { name, alias }
        | FromItem::TableIndexed { name, alias, .. }
        | FromItem::TableNotIndexed { name, alias } => {
            Some(alias.clone().unwrap_or_else(|| name.clone()))
        }
        FromItem::Subquery { alias, .. } => (!alias.is_empty()).then(|| alias.clone()),
        FromItem::Values { alias, .. } => alias.clone(),
    }
}

fn qualify_join_using_column(qualifier: Option<&str>, column: &str) -> String {
    qualifier.map_or_else(
        || column.to_string(),
        |qualifier| format!("{qualifier}.{column}"),
    )
}

fn rebuild_and_expr(mut terms: Vec<Expr>) -> Expr {
    let mut expr = terms.remove(0);
    for term in terms {
        expr = Expr::And(Box::new(expr), Box::new(term));
    }
    expr
}

fn display_token(token: &TokenKind) -> String {
    match token {
        TokenKind::Create => "CREATE".to_string(),
        TokenKind::Alter => "ALTER".to_string(),
        TokenKind::Add => "ADD".to_string(),
        TokenKind::Rename => "RENAME".to_string(),
        TokenKind::Replace => "REPLACE".to_string(),
        TokenKind::Drop => "DROP".to_string(),
        TokenKind::Table => "TABLE".to_string(),
        TokenKind::Column => "COLUMN".to_string(),
        TokenKind::To => "TO".to_string(),
        TokenKind::Unique => "UNIQUE".to_string(),
        TokenKind::Index => "INDEX".to_string(),
        TokenKind::On => "ON".to_string(),
        TokenKind::Off => "OFF".to_string(),
        TokenKind::Conflict => "CONFLICT".to_string(),
        TokenKind::Do => "DO".to_string(),
        TokenKind::Nothing => "NOTHING".to_string(),
        TokenKind::Insert => "INSERT".to_string(),
        TokenKind::Into => "INTO".to_string(),
        TokenKind::Values => "VALUES".to_string(),
        TokenKind::Select => "SELECT".to_string(),
        TokenKind::Delete => "DELETE".to_string(),
        TokenKind::Update => "UPDATE".to_string(),
        TokenKind::Set => "SET".to_string(),
        TokenKind::From => "FROM".to_string(),
        TokenKind::Where => "WHERE".to_string(),
        TokenKind::Group => "GROUP".to_string(),
        TokenKind::Order => "ORDER".to_string(),
        TokenKind::By => "BY".to_string(),
        TokenKind::Limit => "LIMIT".to_string(),
        TokenKind::Offset => "OFFSET".to_string(),
        TokenKind::With => "WITH".to_string(),
        TokenKind::Recursive => "RECURSIVE".to_string(),
        TokenKind::Union => "UNION".to_string(),
        TokenKind::Intersect => "INTERSECT".to_string(),
        TokenKind::Except => "EXCEPT".to_string(),
        TokenKind::All => "ALL".to_string(),
        TokenKind::As => "AS".to_string(),
        TokenKind::Inner => "INNER".to_string(),
        TokenKind::Cross => "CROSS".to_string(),
        TokenKind::Left => "LEFT".to_string(),
        TokenKind::Right => "RIGHT".to_string(),
        TokenKind::Full => "FULL".to_string(),
        TokenKind::Outer => "OUTER".to_string(),
        TokenKind::Natural => "NATURAL".to_string(),
        TokenKind::Join => "JOIN".to_string(),
        TokenKind::Using => "USING".to_string(),
        TokenKind::Having => "HAVING".to_string(),
        TokenKind::Distinct => "DISTINCT".to_string(),
        TokenKind::Asc => "ASC".to_string(),
        TokenKind::Desc => "DESC".to_string(),
        TokenKind::And => "AND".to_string(),
        TokenKind::Or => "OR".to_string(),
        TokenKind::Like => "LIKE".to_string(),
        TokenKind::Glob => "GLOB".to_string(),
        TokenKind::Escape => "ESCAPE".to_string(),
        TokenKind::Between => "BETWEEN".to_string(),
        TokenKind::Begin => "BEGIN".to_string(),
        TokenKind::Start => "START".to_string(),
        TokenKind::Transaction => "TRANSACTION".to_string(),
        TokenKind::Isolation => "ISOLATION".to_string(),
        TokenKind::Level => "LEVEL".to_string(),
        TokenKind::Read => "READ".to_string(),
        TokenKind::Committed => "COMMITTED".to_string(),
        TokenKind::Repeatable => "REPEATABLE".to_string(),
        TokenKind::Serializable => "SERIALIZABLE".to_string(),
        TokenKind::Commit => "COMMIT".to_string(),
        TokenKind::Rollback => "ROLLBACK".to_string(),
        TokenKind::Case => "CASE".to_string(),
        TokenKind::When => "WHEN".to_string(),
        TokenKind::Then => "THEN".to_string(),
        TokenKind::Else => "ELSE".to_string(),
        TokenKind::End => "END".to_string(),
        TokenKind::If => "IF".to_string(),
        TokenKind::Not => "NOT".to_string(),
        TokenKind::In => "IN".to_string(),
        TokenKind::Is => "IS".to_string(),
        TokenKind::Exists => "EXISTS".to_string(),
        TokenKind::Explain => "EXPLAIN".to_string(),
        TokenKind::Query => "QUERY".to_string(),
        TokenKind::Plan => "PLAN".to_string(),
        TokenKind::Pragma => "PRAGMA".to_string(),
        TokenKind::Analyze => "ANALYZE".to_string(),
        TokenKind::Reindex => "REINDEX".to_string(),
        TokenKind::Vacuum => "VACUUM".to_string(),
        TokenKind::Default => "DEFAULT".to_string(),
        TokenKind::Returning => "RETURNING".to_string(),
        TokenKind::Check => "CHECK".to_string(),
        TokenKind::Collate => "COLLATE".to_string(),
        TokenKind::Constraint => "CONSTRAINT".to_string(),
        TokenKind::Foreign => "FOREIGN".to_string(),
        TokenKind::References => "REFERENCES".to_string(),
        TokenKind::Generated => "GENERATED".to_string(),
        TokenKind::Always => "ALWAYS".to_string(),
        TokenKind::Stored => "STORED".to_string(),
        TokenKind::Virtual => "VIRTUAL".to_string(),
        TokenKind::IsNull => "ISNULL".to_string(),
        TokenKind::NotNull => "NOTNULL".to_string(),
        TokenKind::Nulls => "NULLS".to_string(),
        TokenKind::First => "FIRST".to_string(),
        TokenKind::Last => "LAST".to_string(),
        TokenKind::Primary => "PRIMARY".to_string(),
        TokenKind::Key => "KEY".to_string(),
        TokenKind::Strict => "STRICT".to_string(),
        TokenKind::Autoincrement => "AUTOINCREMENT".to_string(),
        TokenKind::IntegerType => "INTEGER".to_string(),
        TokenKind::TextType => "TEXT".to_string(),
        TokenKind::BlobType => "BLOB".to_string(),
        TokenKind::BooleanType => "BOOLEAN".to_string(),
        TokenKind::True => "TRUE".to_string(),
        TokenKind::False => "FALSE".to_string(),
        TokenKind::Null => "NULL".to_string(),
        TokenKind::Identifier(name) => format!("identifier '{name}'"),
        TokenKind::Integer(value) => format!("integer literal {value}"),
        TokenKind::Real(value) => format!("real literal {value}"),
        TokenKind::BlobLiteral(value) => format!(
            "blob literal X'{}'",
            value
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
        TokenKind::String(value) => format!("string literal '{value}'"),
        TokenKind::Star => "*".to_string(),
        TokenKind::Comma => ",".to_string(),
        TokenKind::Dot => ".".to_string(),
        TokenKind::Semicolon => ";".to_string(),
        TokenKind::LParen => "(".to_string(),
        TokenKind::RParen => ")".to_string(),
        TokenKind::Eq => "=".to_string(),
        TokenKind::Ne => "!=".to_string(),
        TokenKind::Gt => ">".to_string(),
        TokenKind::Gte => ">=".to_string(),
        TokenKind::Lt => "<".to_string(),
        TokenKind::Lte => "<=".to_string(),
        TokenKind::Plus => "+".to_string(),
        TokenKind::Minus => "-".to_string(),
        TokenKind::Slash => "/".to_string(),
        TokenKind::Percent => "%".to_string(),
        TokenKind::Ampersand => "&".to_string(),
        TokenKind::Pipe => "|".to_string(),
        TokenKind::ShiftLeft => "<<".to_string(),
        TokenKind::ShiftRight => ">>".to_string(),
        TokenKind::Tilde => "~".to_string(),
        TokenKind::PipePipe => "||".to_string(),
        TokenKind::Arrow => "->".to_string(),
        TokenKind::ArrowText => "->>".to_string(),
        TokenKind::Eof => "end of input".to_string(),
    }
}

fn token_sql_fragment(token: &Token) -> String {
    match &token.kind {
        TokenKind::Identifier(name) => name.clone(),
        TokenKind::IntegerType => "INTEGER".to_string(),
        TokenKind::TextType => "TEXT".to_string(),
        TokenKind::BlobType => "BLOB".to_string(),
        TokenKind::BooleanType => "BOOLEAN".to_string(),
        TokenKind::Create => "CREATE".to_string(),
        TokenKind::Alter => "ALTER".to_string(),
        TokenKind::Add => "ADD".to_string(),
        TokenKind::Rename => "RENAME".to_string(),
        TokenKind::Replace => "REPLACE".to_string(),
        TokenKind::Drop => "DROP".to_string(),
        TokenKind::Table => "TABLE".to_string(),
        TokenKind::Column => "COLUMN".to_string(),
        TokenKind::To => "TO".to_string(),
        TokenKind::Unique => "UNIQUE".to_string(),
        TokenKind::Index => "INDEX".to_string(),
        TokenKind::On => "ON".to_string(),
        TokenKind::Off => "OFF".to_string(),
        TokenKind::Conflict => "CONFLICT".to_string(),
        TokenKind::Do => "DO".to_string(),
        TokenKind::Nothing => "NOTHING".to_string(),
        TokenKind::Insert => "INSERT".to_string(),
        TokenKind::Into => "INTO".to_string(),
        TokenKind::Values => "VALUES".to_string(),
        TokenKind::Select => "SELECT".to_string(),
        TokenKind::Delete => "DELETE".to_string(),
        TokenKind::Update => "UPDATE".to_string(),
        TokenKind::Set => "SET".to_string(),
        TokenKind::From => "FROM".to_string(),
        TokenKind::Where => "WHERE".to_string(),
        TokenKind::Group => "GROUP".to_string(),
        TokenKind::Having => "HAVING".to_string(),
        TokenKind::Order => "ORDER".to_string(),
        TokenKind::By => "BY".to_string(),
        TokenKind::Limit => "LIMIT".to_string(),
        TokenKind::Offset => "OFFSET".to_string(),
        TokenKind::With => "WITH".to_string(),
        TokenKind::Recursive => "RECURSIVE".to_string(),
        TokenKind::Union => "UNION".to_string(),
        TokenKind::Intersect => "INTERSECT".to_string(),
        TokenKind::Except => "EXCEPT".to_string(),
        TokenKind::All => "ALL".to_string(),
        TokenKind::As => "AS".to_string(),
        TokenKind::Inner => "INNER".to_string(),
        TokenKind::Cross => "CROSS".to_string(),
        TokenKind::Left => "LEFT".to_string(),
        TokenKind::Right => "RIGHT".to_string(),
        TokenKind::Full => "FULL".to_string(),
        TokenKind::Outer => "OUTER".to_string(),
        TokenKind::Natural => "NATURAL".to_string(),
        TokenKind::Join => "JOIN".to_string(),
        TokenKind::Using => "USING".to_string(),
        TokenKind::Asc => "ASC".to_string(),
        TokenKind::Desc => "DESC".to_string(),
        TokenKind::And => "AND".to_string(),
        TokenKind::Or => "OR".to_string(),
        TokenKind::Like => "LIKE".to_string(),
        TokenKind::Glob => "GLOB".to_string(),
        TokenKind::Escape => "ESCAPE".to_string(),
        TokenKind::Between => "BETWEEN".to_string(),
        TokenKind::Begin => "BEGIN".to_string(),
        TokenKind::Start => "START".to_string(),
        TokenKind::Transaction => "TRANSACTION".to_string(),
        TokenKind::Isolation => "ISOLATION".to_string(),
        TokenKind::Level => "LEVEL".to_string(),
        TokenKind::Read => "READ".to_string(),
        TokenKind::Committed => "COMMITTED".to_string(),
        TokenKind::Repeatable => "REPEATABLE".to_string(),
        TokenKind::Serializable => "SERIALIZABLE".to_string(),
        TokenKind::Commit => "COMMIT".to_string(),
        TokenKind::Rollback => "ROLLBACK".to_string(),
        TokenKind::Case => "CASE".to_string(),
        TokenKind::When => "WHEN".to_string(),
        TokenKind::Then => "THEN".to_string(),
        TokenKind::Else => "ELSE".to_string(),
        TokenKind::End => "END".to_string(),
        TokenKind::If => "IF".to_string(),
        TokenKind::Not => "NOT".to_string(),
        TokenKind::In => "IN".to_string(),
        TokenKind::Is => "IS".to_string(),
        TokenKind::Exists => "EXISTS".to_string(),
        TokenKind::Explain => "EXPLAIN".to_string(),
        TokenKind::Query => "QUERY".to_string(),
        TokenKind::Plan => "PLAN".to_string(),
        TokenKind::Pragma => "PRAGMA".to_string(),
        TokenKind::Analyze => "ANALYZE".to_string(),
        TokenKind::Reindex => "REINDEX".to_string(),
        TokenKind::Vacuum => "VACUUM".to_string(),
        TokenKind::Distinct => "DISTINCT".to_string(),
        TokenKind::Default => "DEFAULT".to_string(),
        TokenKind::Returning => "RETURNING".to_string(),
        TokenKind::Check => "CHECK".to_string(),
        TokenKind::Collate => "COLLATE".to_string(),
        TokenKind::Constraint => "CONSTRAINT".to_string(),
        TokenKind::Foreign => "FOREIGN".to_string(),
        TokenKind::References => "REFERENCES".to_string(),
        TokenKind::Generated => "GENERATED".to_string(),
        TokenKind::Always => "ALWAYS".to_string(),
        TokenKind::Stored => "STORED".to_string(),
        TokenKind::Virtual => "VIRTUAL".to_string(),
        TokenKind::IsNull => "ISNULL".to_string(),
        TokenKind::NotNull => "NOTNULL".to_string(),
        TokenKind::Nulls => "NULLS".to_string(),
        TokenKind::First => "FIRST".to_string(),
        TokenKind::Last => "LAST".to_string(),
        TokenKind::Primary => "PRIMARY".to_string(),
        TokenKind::Key => "KEY".to_string(),
        TokenKind::Strict => "STRICT".to_string(),
        TokenKind::Autoincrement => "AUTOINCREMENT".to_string(),
        TokenKind::True => "TRUE".to_string(),
        TokenKind::False => "FALSE".to_string(),
        TokenKind::Null => "NULL".to_string(),
        TokenKind::Integer(value) => value.to_string(),
        TokenKind::Real(value) => value.to_string(),
        TokenKind::String(value) => format!("'{}'", value.replace('\'', "''")),
        TokenKind::BlobLiteral(value) => format!(
            "X'{}'",
            value
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
        TokenKind::Star => "*".to_string(),
        TokenKind::Comma => ",".to_string(),
        TokenKind::Semicolon => ";".to_string(),
        TokenKind::LParen => "(".to_string(),
        TokenKind::RParen => ")".to_string(),
        TokenKind::Eq => "=".to_string(),
        TokenKind::Ne => "!=".to_string(),
        TokenKind::Gt => ">".to_string(),
        TokenKind::Gte => ">=".to_string(),
        TokenKind::Lt => "<".to_string(),
        TokenKind::Lte => "<=".to_string(),
        TokenKind::Plus => "+".to_string(),
        TokenKind::Minus => "-".to_string(),
        TokenKind::Slash => "/".to_string(),
        TokenKind::Percent => "%".to_string(),
        TokenKind::Ampersand => "&".to_string(),
        TokenKind::Pipe => "|".to_string(),
        TokenKind::ShiftLeft => "<<".to_string(),
        TokenKind::ShiftRight => ">>".to_string(),
        TokenKind::Tilde => "~".to_string(),
        TokenKind::PipePipe => "||".to_string(),
        TokenKind::Arrow => "->".to_string(),
        TokenKind::ArrowText => "->>".to_string(),
        TokenKind::Dot => ".".to_string(),
        TokenKind::Eof => String::new(),
    }
}

fn join_sql_fragments(fragments: &[String]) -> String {
    let mut sql = String::new();
    for fragment in fragments {
        let starts_with_paren = fragment.starts_with('(');
        let needs_space_before = !sql.is_empty()
            && !matches!(fragment.as_str(), ")" | "," | ";" | ".")
            && !starts_with_paren
            && !matches!(sql.chars().last(), Some('(' | '.' | ' '));
        if needs_space_before {
            sql.push(' ');
        }
        sql.push_str(fragment);
    }
    sql
}
