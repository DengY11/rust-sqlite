use crate::common::error::{DbError, Result};
use crate::common::types::{
    BinaryMathFunc, CheckConstraint, CheckExpr, CheckOp, ColumnDef, ColumnDefault, ColumnType,
    ForeignKey, PrimaryKeyConstraint, RoundingFunc, SortOrder, TrimSide, UnaryMathFunc,
    UniqueConstraint, Value,
};
use crate::sql::ast::{
    AggregateArg, AggregateFunc, AlterTableAction, Assignment, CommonTableExpr, CompareOp,
    CompoundOperator, CompoundSelect, CteBody, Expr, FromItem, IsolationLevel, JoinClause,
    JoinKind, NullOrder, OrderBy, OrderByExpr, SINGLE_ROW_SOURCE_TABLE, ScalarBinaryOp, ScalarExpr,
    ScalarFunc, SelectItem, SelectStatement, Statement, TableConstraint, TableIndexHint,
    UpsertClause, WindowExclude, WindowFrame, WindowFunc, WindowRangeOffset, WithClause,
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

#[derive(Debug, Clone)]
struct NamedWindowSpec {
    name: String,
    base_name: Option<String>,
    partition_by: Vec<ScalarExpr>,
    order_by: Vec<OrderBy>,
    frame: WindowFrame,
    exclude: WindowExclude,
}

fn table_index_hint_from_parsed(hint: ParsedTableIndexHint) -> TableIndexHint {
    match hint {
        ParsedTableIndexHint::IndexedBy(index) => TableIndexHint::IndexedBy(index),
        ParsedTableIndexHint::NotIndexed => TableIndexHint::NotIndexed,
    }
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
            TokenKind::Commit | TokenKind::End => {
                self.advance();
                let _ = self.matches(&TokenKind::Transaction);
                Ok(Statement::Commit)
            }
            TokenKind::Rollback => self.parse_rollback_statement(),
            TokenKind::Savepoint => self.parse_savepoint_statement(),
            TokenKind::Release => self.parse_release_statement(),
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
        let (table, schema) = self.parse_schema_qualified_name_with_schema()?;

        if self.matches(&TokenKind::Add) {
            let _ = self.matches(&TokenKind::Column);
            let column = self.parse_column_def(Some(&table))?;
            return Ok(Statement::AlterTable {
                table,
                schema,
                action: AlterTableAction::AddColumn(column),
            });
        }

        if self.matches(&TokenKind::Drop) {
            self.expect_keyword(TokenKind::Column)?;
            let old_name = self.parse_simple_identifier()?;
            return Ok(Statement::AlterTable {
                table,
                schema,
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
                schema,
                action: AlterTableAction::RenameColumn { old_name, new_name },
            });
        }

        if self.matches(&TokenKind::To) {
            let new_name = self.parse_simple_identifier()?;
            return Ok(Statement::AlterTable {
                table,
                schema,
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
            if let Some(value) =
                self.parse_optional_unsigned_pragma_integer_assignment("page_size")?
            {
                return Ok(Statement::SetPragmaPageSize { value, schema });
            }
            return Ok(Statement::PragmaPageSize { schema });
        }
        if name.eq_ignore_ascii_case("page_count") {
            return Ok(Statement::PragmaPageCount { schema });
        }
        if name.eq_ignore_ascii_case("max_page_count") {
            if let Some(value) = self.parse_optional_pragma_max_page_count_assignment()? {
                return Ok(Statement::SetPragmaMaxPageCount { value });
            }
            return Ok(Statement::PragmaMaxPageCount);
        }
        if name.eq_ignore_ascii_case("freelist_count") {
            return Ok(Statement::PragmaFreelistCount { schema });
        }
        if name.eq_ignore_ascii_case("user_version") {
            if let Some(value) =
                self.parse_optional_signed_pragma_i32_bits_assignment("user_version")?
            {
                return Ok(Statement::SetPragmaUserVersion { value, schema });
            }
            return Ok(Statement::PragmaUserVersion { schema });
        }
        if name.eq_ignore_ascii_case("application_id") {
            if let Some(value) =
                self.parse_optional_signed_pragma_i32_bits_assignment("application_id")?
            {
                return Ok(Statement::SetPragmaApplicationId { value, schema });
            }
            return Ok(Statement::PragmaApplicationId { schema });
        }
        if name.eq_ignore_ascii_case("schema_version") {
            if let Some(value) =
                self.parse_optional_signed_pragma_i32_bits_assignment("schema_version")?
            {
                return Ok(Statement::SetPragmaSchemaVersion { value, schema });
            }
            return Ok(Statement::PragmaSchemaVersion { schema });
        }
        if name.eq_ignore_ascii_case("foreign_keys") {
            if let Some(enabled) = self.parse_optional_pragma_boolean_assignment("foreign_keys")? {
                return Ok(Statement::SetPragmaForeignKeys { enabled });
            }
            return Ok(Statement::PragmaForeignKeys);
        }
        if name.eq_ignore_ascii_case("defer_foreign_keys") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("defer_foreign_keys")?
            {
                return Ok(Statement::SetPragmaDeferForeignKeys { enabled });
            }
            return Ok(Statement::PragmaDeferForeignKeys);
        }
        if name.eq_ignore_ascii_case("read_uncommitted") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("read_uncommitted")?
            {
                return Ok(Statement::SetPragmaReadUncommitted { enabled });
            }
            return Ok(Statement::PragmaReadUncommitted);
        }
        if name.eq_ignore_ascii_case("query_only") {
            if let Some(enabled) = self.parse_optional_pragma_boolean_assignment("query_only")? {
                return Ok(Statement::SetPragmaQueryOnly { enabled });
            }
            return Ok(Statement::PragmaQueryOnly);
        }
        if name.eq_ignore_ascii_case("count_changes") {
            if let Some(enabled) = self.parse_optional_pragma_boolean_assignment("count_changes")? {
                return Ok(Statement::SetPragmaCountChanges { enabled });
            }
            return Ok(Statement::PragmaCountChanges);
        }
        if name.eq_ignore_ascii_case("recursive_triggers") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("recursive_triggers")?
            {
                return Ok(Statement::SetPragmaRecursiveTriggers { enabled });
            }
            return Ok(Statement::PragmaRecursiveTriggers);
        }
        if name.eq_ignore_ascii_case("trusted_schema") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("trusted_schema")?
            {
                return Ok(Statement::SetPragmaTrustedSchema { enabled });
            }
            return Ok(Statement::PragmaTrustedSchema);
        }
        if name.eq_ignore_ascii_case("ignore_check_constraints") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("ignore_check_constraints")?
            {
                return Ok(Statement::SetPragmaIgnoreCheckConstraints { enabled });
            }
            return Ok(Statement::PragmaIgnoreCheckConstraints);
        }
        if name.eq_ignore_ascii_case("encoding") {
            if self.parse_optional_pragma_encoding_assignment()? {
                return Ok(Statement::SetPragmaEncoding);
            }
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
        if name.eq_ignore_ascii_case("pragma_list") {
            return Ok(Statement::PragmaPragmaList);
        }
        if name.eq_ignore_ascii_case("module_list") {
            return Ok(Statement::PragmaModuleList);
        }
        if name.eq_ignore_ascii_case("stats") {
            return Ok(Statement::PragmaStats);
        }
        if name.eq_ignore_ascii_case("journal_mode") {
            if let Some(mode) = self.parse_optional_pragma_journal_mode_assignment()? {
                return Ok(Statement::SetPragmaJournalMode { mode, schema });
            }
            return Ok(Statement::PragmaJournalMode { schema });
        }
        if name.eq_ignore_ascii_case("synchronous") {
            if let Some(value) = self.parse_optional_pragma_synchronous_assignment()? {
                return Ok(Statement::SetPragmaSynchronous { value, schema });
            }
            return Ok(Statement::PragmaSynchronous { schema });
        }
        if name.eq_ignore_ascii_case("cache_size") {
            if let Some(value) = self.parse_optional_pragma_cache_size_assignment()? {
                return Ok(Statement::SetPragmaCacheSize { value, schema });
            }
            return Ok(Statement::PragmaCacheSize { schema });
        }
        if name.eq_ignore_ascii_case("cache_spill") {
            if let Some(value) = self.parse_optional_pragma_cache_spill_assignment()? {
                return Ok(Statement::SetPragmaCacheSpill { value });
            }
            return Ok(Statement::PragmaCacheSpill);
        }
        if name.eq_ignore_ascii_case("temp_store") {
            if let Some(value) = self.parse_optional_pragma_temp_store_assignment()? {
                return Ok(Statement::SetPragmaTempStore { value });
            }
            return Ok(Statement::PragmaTempStore);
        }
        if name.eq_ignore_ascii_case("locking_mode") {
            if let Some(mode) = self.parse_optional_pragma_locking_mode_assignment()? {
                return Ok(Statement::SetPragmaLockingMode { mode, schema });
            }
            return Ok(Statement::PragmaLockingMode { schema });
        }
        if name.eq_ignore_ascii_case("secure_delete") {
            if let Some(value) = self.parse_optional_pragma_secure_delete_assignment()? {
                return Ok(Statement::SetPragmaSecureDelete { value, schema });
            }
            return Ok(Statement::PragmaSecureDelete { schema });
        }
        if name.eq_ignore_ascii_case("wal_autocheckpoint") {
            if let Some(value) = self.parse_optional_pragma_wal_autocheckpoint_assignment()? {
                return Ok(Statement::SetPragmaWalAutocheckpoint { value });
            }
            return Ok(Statement::PragmaWalAutocheckpoint);
        }
        if name.eq_ignore_ascii_case("wal_checkpoint") {
            self.parse_optional_pragma_scalar_argument()?;
            return Ok(Statement::PragmaWalCheckpoint);
        }
        if name.eq_ignore_ascii_case("mmap_size") {
            if let Some(value) =
                self.parse_optional_signed_pragma_integer_assignment("mmap_size")?
            {
                return Ok(Statement::SetPragmaMmapSize { value });
            }
            return Ok(Statement::PragmaMmapSize);
        }
        if name.eq_ignore_ascii_case("auto_vacuum") {
            if let Some(value) = self.parse_optional_pragma_auto_vacuum_assignment()? {
                return Ok(Statement::SetPragmaAutoVacuum { value });
            }
            return Ok(Statement::PragmaAutoVacuum);
        }
        if name.eq_ignore_ascii_case("busy_timeout") {
            if let Some(value) = self.parse_optional_pragma_busy_timeout_assignment()? {
                return Ok(Statement::SetPragmaBusyTimeout { value });
            }
            return Ok(Statement::PragmaBusyTimeout);
        }
        if name.eq_ignore_ascii_case("analysis_limit") {
            if let Some(value) =
                self.parse_optional_lenient_unsigned_pragma_integer_assignment("analysis_limit")?
            {
                return Ok(Statement::SetPragmaAnalysisLimit { value });
            }
            return Ok(Statement::PragmaAnalysisLimit);
        }
        if name.eq_ignore_ascii_case("journal_size_limit") {
            if let Some(value) =
                self.parse_optional_signed_pragma_integer_assignment("journal_size_limit")?
            {
                return Ok(Statement::SetPragmaJournalSizeLimit { value });
            }
            return Ok(Statement::PragmaJournalSizeLimit);
        }
        if name.eq_ignore_ascii_case("soft_heap_limit") {
            if let Some(value) =
                self.parse_optional_signed_pragma_integer_assignment("soft_heap_limit")?
            {
                return Ok(Statement::SetPragmaSoftHeapLimit { value });
            }
            return Ok(Statement::PragmaSoftHeapLimit);
        }
        if name.eq_ignore_ascii_case("hard_heap_limit") {
            if let Some(value) =
                self.parse_optional_signed_pragma_integer_assignment("hard_heap_limit")?
            {
                return Ok(Statement::SetPragmaHardHeapLimit { value });
            }
            return Ok(Statement::PragmaHardHeapLimit);
        }
        if name.eq_ignore_ascii_case("threads") {
            if let Some(value) =
                self.parse_optional_lenient_unsigned_pragma_integer_assignment("threads")?
            {
                return Ok(Statement::SetPragmaThreads { value });
            }
            return Ok(Statement::PragmaThreads);
        }
        if name.eq_ignore_ascii_case("automatic_index") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("automatic_index")?
            {
                return Ok(Statement::SetPragmaAutomaticIndex { enabled });
            }
            return Ok(Statement::PragmaAutomaticIndex);
        }
        if name.eq_ignore_ascii_case("cell_size_check") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("cell_size_check")?
            {
                return Ok(Statement::SetPragmaCellSizeCheck { enabled });
            }
            return Ok(Statement::PragmaCellSizeCheck);
        }
        if name.eq_ignore_ascii_case("full_column_names") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("full_column_names")?
            {
                return Ok(Statement::SetPragmaFullColumnNames { enabled });
            }
            return Ok(Statement::PragmaFullColumnNames);
        }
        if name.eq_ignore_ascii_case("short_column_names") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("short_column_names")?
            {
                return Ok(Statement::SetPragmaShortColumnNames { enabled });
            }
            return Ok(Statement::PragmaShortColumnNames);
        }
        if name.eq_ignore_ascii_case("fullfsync") {
            if let Some(enabled) = self.parse_optional_pragma_boolean_assignment("fullfsync")? {
                return Ok(Statement::SetPragmaFullFsync { enabled });
            }
            return Ok(Statement::PragmaFullFsync);
        }
        if name.eq_ignore_ascii_case("checkpoint_fullfsync") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("checkpoint_fullfsync")?
            {
                return Ok(Statement::SetPragmaCheckpointFullFsync { enabled });
            }
            return Ok(Statement::PragmaCheckpointFullFsync);
        }
        if name.eq_ignore_ascii_case("empty_result_callbacks") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("empty_result_callbacks")?
            {
                return Ok(Statement::SetPragmaEmptyResultCallbacks { enabled });
            }
            return Ok(Statement::PragmaEmptyResultCallbacks);
        }
        if name.eq_ignore_ascii_case("case_sensitive_like") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("case_sensitive_like")?
            {
                return Ok(Statement::SetPragmaCaseSensitiveLike { enabled });
            }
            return Ok(Statement::PragmaCaseSensitiveLike);
        }
        if name.eq_ignore_ascii_case("reverse_unordered_selects") {
            if let Some(enabled) =
                self.parse_optional_pragma_boolean_assignment("reverse_unordered_selects")?
            {
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
            } else if self.matches(&TokenKind::LParen) {
                if is_scalar_expr_start(self.peek_kind()) {
                    let _ = self.parse_scalar_expr()?;
                    self.expect_symbol(TokenKind::RParen)?;
                } else {
                    return Err(self.error_expected(&format!(
                        "PRAGMA optimize argument, found {}",
                        display_token(self.peek_kind())
                    )));
                }
            }
            return Ok(Statement::PragmaOptimize);
        }
        if name.eq_ignore_ascii_case("shrink_memory") {
            self.parse_optional_pragma_scalar_argument()?;
            return Ok(Statement::PragmaShrinkMemory);
        }
        if name.eq_ignore_ascii_case("incremental_vacuum") {
            self.parse_optional_pragma_scalar_argument()?;
            return Ok(Statement::PragmaIncrementalVacuum);
        }
        if name.eq_ignore_ascii_case("table_list") {
            let table = self.parse_optional_pragma_name_argument_in_parens_or_equals()?;
            return Ok(Statement::PragmaTableList { table, schema });
        }
        if name.eq_ignore_ascii_case("foreign_key_check") {
            let table = self.parse_optional_pragma_name_argument_in_parens_or_equals()?;
            return Ok(Statement::PragmaForeignKeyCheck { table, schema });
        }
        let table = self.parse_pragma_name_argument_in_parens_or_equals()?;
        if name.eq_ignore_ascii_case("table_info") {
            Ok(Statement::PragmaTableInfo { table, schema })
        } else if name.eq_ignore_ascii_case("table_xinfo") {
            Ok(Statement::PragmaTableXInfo { table, schema })
        } else if name.eq_ignore_ascii_case("index_list") {
            Ok(Statement::PragmaIndexList { table, schema })
        } else if name.eq_ignore_ascii_case("index_info") {
            Ok(Statement::PragmaIndexInfo {
                index: table,
                schema,
            })
        } else if name.eq_ignore_ascii_case("index_xinfo") {
            Ok(Statement::PragmaIndexXInfo {
                index: table,
                schema,
            })
        } else if name.eq_ignore_ascii_case("foreign_key_list") {
            Ok(Statement::PragmaForeignKeyList { table, schema })
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

    fn parse_optional_pragma_name_argument_in_parens_or_equals(
        &mut self,
    ) -> Result<Option<String>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_name_argument().map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_pragma_name_argument()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_optional_pragma_scalar_argument(&mut self) -> Result<()> {
        if self.matches(&TokenKind::Eq) {
            let _ = self.parse_scalar_expr()?;
        } else if self.matches(&TokenKind::LParen) {
            let _ = self.parse_scalar_expr()?;
            self.expect_symbol(TokenKind::RParen)?;
        }
        Ok(())
    }

    fn parse_optional_pragma_encoding_assignment(&mut self) -> Result<bool> {
        if self.matches(&TokenKind::Eq) {
            self.parse_pragma_encoding()?;
            return Ok(true);
        }
        if self.matches(&TokenKind::LParen) {
            self.parse_pragma_encoding()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn parse_pragma_encoding(&mut self) -> Result<()> {
        let encoding = self.parse_pragma_name_argument()?;
        let normalized = encoding.replace('-', "");
        if normalized.eq_ignore_ascii_case("utf8") {
            Ok(())
        } else {
            Err(DbError::sql(format!("unsupported encoding: {encoding}")))
        }
    }

    fn parse_optional_pragma_temp_store_assignment(&mut self) -> Result<Option<i64>> {
        if self.matches(&TokenKind::Eq) {
            return Ok(Some(self.parse_pragma_temp_store()?));
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_pragma_temp_store()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_optional_pragma_busy_timeout_assignment(&mut self) -> Result<Option<i64>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_busy_timeout().map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_pragma_busy_timeout()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_pragma_busy_timeout(&mut self) -> Result<i64> {
        if self.matches(&TokenKind::Default) {
            Ok(0)
        } else {
            self.parse_signed_pragma_integer("busy_timeout")
        }
    }

    fn parse_optional_pragma_auto_vacuum_assignment(&mut self) -> Result<Option<Option<i64>>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_auto_vacuum().map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_pragma_auto_vacuum()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_pragma_auto_vacuum(&mut self) -> Result<Option<i64>> {
        match self.peek_kind() {
            TokenKind::Integer(value @ 0..=2) => {
                let value = *value;
                self.advance();
                Ok(Some(value))
            }
            TokenKind::Default | TokenKind::Off | TokenKind::False => {
                self.advance();
                Ok(Some(0))
            }
            TokenKind::On | TokenKind::True | TokenKind::Full => {
                self.advance();
                Ok(Some(1))
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("NONE") => {
                self.advance();
                Ok(Some(0))
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("FULL") => {
                self.advance();
                Ok(Some(1))
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("INCREMENTAL") => {
                self.advance();
                Ok(Some(2))
            }
            TokenKind::String(value) => {
                let value = value.clone();
                self.advance();
                if value.eq_ignore_ascii_case("NONE") {
                    Ok(Some(0))
                } else if value.eq_ignore_ascii_case("FULL") {
                    Ok(Some(1))
                } else if value.eq_ignore_ascii_case("INCREMENTAL") {
                    Ok(Some(2))
                } else {
                    let value = sqlite_pragma_string_integer_prefix(&value);
                    Ok((0..=2).contains(&value).then_some(value))
                }
            }
            TokenKind::Integer(_) | TokenKind::Identifier(_) => {
                let _ = self.parse_pragma_name_argument()?;
                Ok(None)
            }
            token => Err(self.error_expected(&format!(
                "NONE, FULL, INCREMENTAL, 0, 1, or 2 for auto_vacuum, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_optional_pragma_cache_size_assignment(&mut self) -> Result<Option<i64>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_cache_size().map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_pragma_cache_size()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_pragma_cache_size(&mut self) -> Result<i64> {
        if self.matches(&TokenKind::Default) {
            Ok(0)
        } else {
            self.parse_signed_pragma_integer("cache_size")
        }
    }

    fn parse_optional_pragma_cache_spill_assignment(&mut self) -> Result<Option<Option<i64>>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_cache_spill().map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_pragma_cache_spill()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_pragma_cache_spill(&mut self) -> Result<Option<i64>> {
        match self.peek_kind() {
            TokenKind::On | TokenKind::True | TokenKind::Default => {
                self.advance();
                Ok(Some(2000))
            }
            TokenKind::Off | TokenKind::False => {
                self.advance();
                Ok(Some(0))
            }
            TokenKind::Integer(value) => {
                let value = *value;
                self.advance();
                Ok(Some(if value > 0 { value.max(2000) } else { 0 }))
            }
            TokenKind::String(value) => {
                let value = value.clone();
                self.advance();
                if value.eq_ignore_ascii_case("ON") {
                    Ok(Some(2000))
                } else if value.eq_ignore_ascii_case("OFF") {
                    Ok(Some(0))
                } else {
                    let value = sqlite_pragma_string_integer_prefix(&value);
                    Ok(Some(if value > 0 { value.max(2000) } else { 0 }))
                }
            }
            _ => {
                self.parse_pragma_name_argument()?;
                Ok(None)
            }
        }
    }

    fn parse_pragma_temp_store(&mut self) -> Result<i64> {
        match self.peek_kind() {
            TokenKind::Integer(value @ 0..=3) => {
                let value = *value;
                self.advance();
                Ok(value)
            }
            TokenKind::Default => {
                self.advance();
                Ok(0)
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("DEFAULT") => {
                self.advance();
                Ok(0)
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("FILE") => {
                self.advance();
                Ok(1)
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("MEMORY") => {
                self.advance();
                Ok(2)
            }
            TokenKind::String(value) => {
                let value = value.clone();
                self.advance();
                if value.eq_ignore_ascii_case("DEFAULT") {
                    Ok(0)
                } else if value.eq_ignore_ascii_case("FILE") {
                    Ok(1)
                } else if value.eq_ignore_ascii_case("MEMORY") {
                    Ok(2)
                } else {
                    let value = sqlite_pragma_string_integer_prefix(&value);
                    if (0..=3).contains(&value) {
                        Ok(value)
                    } else {
                        Err(DbError::sql(format!("unsupported temp_store: {value}")))
                    }
                }
            }
            TokenKind::Integer(value) => {
                let value = *value;
                self.advance();
                Err(DbError::sql(format!("unsupported temp_store: {value}")))
            }
            TokenKind::Identifier(_) => {
                let value = self.parse_pragma_name_argument()?;
                Err(DbError::sql(format!("unsupported temp_store: {value}")))
            }
            token => Err(self.error_expected(&format!(
                "DEFAULT, FILE, MEMORY, 0, 1, or 2 for temp_store, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_optional_pragma_synchronous_assignment(&mut self) -> Result<Option<i64>> {
        if self.matches(&TokenKind::Eq) {
            return Ok(Some(self.parse_pragma_synchronous()?));
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_pragma_synchronous()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_pragma_synchronous(&mut self) -> Result<i64> {
        match self.peek_kind() {
            TokenKind::Integer(value @ 0..=3) => {
                let value = *value;
                self.advance();
                Ok(value)
            }
            TokenKind::Full => {
                self.advance();
                Ok(2)
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("FULL") => {
                self.advance();
                Ok(2)
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("EXTRA") => {
                self.advance();
                Ok(3)
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("NORMAL") => {
                self.advance();
                Ok(1)
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("OFF") => {
                self.advance();
                Ok(0)
            }
            TokenKind::String(value) => {
                let value = value.clone();
                self.advance();
                if value.eq_ignore_ascii_case("FULL") {
                    Ok(2)
                } else if value.eq_ignore_ascii_case("EXTRA") {
                    Ok(3)
                } else if value.eq_ignore_ascii_case("NORMAL") {
                    Ok(1)
                } else if value.eq_ignore_ascii_case("OFF") {
                    Ok(0)
                } else {
                    let value = sqlite_pragma_string_integer_prefix(&value);
                    if (0..=3).contains(&value) {
                        Ok(value)
                    } else {
                        Err(DbError::sql(format!("unsupported synchronous: {value}")))
                    }
                }
            }
            TokenKind::Integer(value) => {
                let value = *value;
                self.advance();
                Err(DbError::sql(format!("unsupported synchronous: {value}")))
            }
            TokenKind::Off => {
                self.advance();
                Ok(0)
            }
            TokenKind::Identifier(_) => {
                let value = self.parse_pragma_name_argument()?;
                Err(DbError::sql(format!("unsupported synchronous: {value}")))
            }
            token => Err(self.error_expected(&format!(
                "OFF, NORMAL, FULL, EXTRA, 0, 1, 2, or 3 for synchronous, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_optional_pragma_journal_mode_assignment(&mut self) -> Result<Option<String>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_journal_mode().map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let mode = self.parse_pragma_journal_mode()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(mode));
        }
        Ok(None)
    }

    fn parse_pragma_journal_mode(&mut self) -> Result<String> {
        match self.peek_kind() {
            TokenKind::Delete => {
                self.advance();
                Ok("delete".to_string())
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("memory") => {
                self.advance();
                Ok("memory".to_string())
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("truncate") => {
                self.advance();
                Ok("truncate".to_string())
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("persist") => {
                self.advance();
                Ok("persist".to_string())
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("off") => {
                self.advance();
                Ok("off".to_string())
            }
            TokenKind::Off => {
                self.advance();
                Ok("off".to_string())
            }
            TokenKind::String(value) => {
                let value = value.clone();
                self.advance();
                if value.eq_ignore_ascii_case("delete") {
                    Ok("delete".to_string())
                } else if value.eq_ignore_ascii_case("memory") {
                    Ok("memory".to_string())
                } else if value.eq_ignore_ascii_case("truncate") {
                    Ok("truncate".to_string())
                } else if value.eq_ignore_ascii_case("persist") {
                    Ok("persist".to_string())
                } else if value.eq_ignore_ascii_case("off") {
                    Ok("off".to_string())
                } else {
                    Err(DbError::sql(format!(
                        "changing journal_mode is not supported: {value}"
                    )))
                }
            }
            TokenKind::Identifier(_) => {
                let value = self.parse_pragma_name_argument()?;
                Err(DbError::sql(format!(
                    "changing journal_mode is not supported: {value}"
                )))
            }
            token => {
                let value = display_token(token);
                self.advance();
                Err(DbError::sql(format!(
                    "changing journal_mode is not supported: {value}"
                )))
            }
        }
    }

    fn parse_optional_pragma_locking_mode_assignment(&mut self) -> Result<Option<String>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_name_argument().map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let mode = self.parse_pragma_name_argument()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(mode));
        }
        Ok(None)
    }

    fn parse_optional_pragma_secure_delete_assignment(&mut self) -> Result<Option<Option<i64>>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_secure_delete().map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_pragma_secure_delete()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_pragma_secure_delete(&mut self) -> Result<Option<i64>> {
        match self.peek_kind() {
            TokenKind::On | TokenKind::True => {
                self.advance();
                Ok(Some(1))
            }
            TokenKind::Off | TokenKind::False => {
                self.advance();
                Ok(Some(0))
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("FAST") => {
                self.advance();
                Ok(Some(2))
            }
            TokenKind::Integer(value) if (0..=2).contains(value) => {
                let value = *value;
                self.advance();
                Ok(Some(value))
            }
            TokenKind::String(value) => {
                let value = value.clone();
                self.advance();
                if value.eq_ignore_ascii_case("ON") {
                    Ok(Some(1))
                } else if value.eq_ignore_ascii_case("OFF") {
                    Ok(Some(0))
                } else if value.eq_ignore_ascii_case("FAST") {
                    Ok(Some(2))
                } else {
                    let value = sqlite_pragma_string_integer_prefix(&value);
                    Ok((0..=2).contains(&value).then_some(value))
                }
            }
            _ => {
                self.parse_pragma_name_argument()?;
                Ok(None)
            }
        }
    }

    fn parse_optional_pragma_wal_autocheckpoint_assignment(
        &mut self,
    ) -> Result<Option<Option<i64>>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_wal_autocheckpoint().map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_pragma_wal_autocheckpoint()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_pragma_wal_autocheckpoint(&mut self) -> Result<Option<i64>> {
        match self.peek_kind() {
            TokenKind::Integer(value) => {
                let value = *value;
                self.advance();
                Ok(Some(value.max(0)))
            }
            TokenKind::Minus => {
                self.advance();
                self.expect_keyword(TokenKind::Integer(1))?;
                Ok(Some(0))
            }
            _ => {
                self.parse_pragma_name_argument()?;
                Ok(None)
            }
        }
    }

    fn parse_optional_pragma_max_page_count_assignment(&mut self) -> Result<Option<Option<i64>>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_max_page_count().map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_pragma_max_page_count()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_pragma_max_page_count(&mut self) -> Result<Option<i64>> {
        match self.peek_kind() {
            TokenKind::Integer(value) => {
                let value = *value;
                self.advance();
                Ok((value > 0).then_some(value))
            }
            TokenKind::Minus => {
                self.advance();
                self.expect_keyword(TokenKind::Integer(1))?;
                Ok(None)
            }
            _ => {
                self.parse_pragma_name_argument()?;
                Ok(None)
            }
        }
    }

    fn parse_optional_pragma_boolean_assignment(
        &mut self,
        pragma_name: &str,
    ) -> Result<Option<bool>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_pragma_boolean(pragma_name).map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let enabled = self.parse_pragma_boolean(pragma_name)?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(enabled));
        }
        Ok(None)
    }

    fn parse_pragma_boolean(&mut self, pragma_name: &str) -> Result<bool> {
        match self.peek_kind() {
            TokenKind::On | TokenKind::True => {
                self.advance();
                Ok(true)
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("YES") => {
                self.advance();
                Ok(true)
            }
            TokenKind::Off | TokenKind::False => {
                self.advance();
                Ok(false)
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("NO") => {
                self.advance();
                Ok(false)
            }
            TokenKind::Default => {
                self.advance();
                Ok(false)
            }
            TokenKind::Integer(value) => {
                let enabled = *value > 0;
                self.advance();
                Ok(enabled)
            }
            TokenKind::Minus => {
                self.advance();
                self.expect_keyword(TokenKind::Integer(1))?;
                Ok(false)
            }
            TokenKind::String(value) => {
                let enabled = sqlite_pragma_boolean_string(value);
                self.advance();
                Ok(enabled)
            }
            token => Err(self.error_expected(&format!(
                "ON, OFF, 1, or 0 for {pragma_name}, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_optional_signed_pragma_integer_assignment(
        &mut self,
        pragma_name: &str,
    ) -> Result<Option<i64>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_signed_pragma_integer(pragma_name).map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_signed_pragma_integer(pragma_name)?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_optional_unsigned_pragma_integer_assignment(
        &mut self,
        pragma_name: &str,
    ) -> Result<Option<u32>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_unsigned_pragma_integer(pragma_name).map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_unsigned_pragma_integer(pragma_name)?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_optional_lenient_unsigned_pragma_integer_assignment(
        &mut self,
        pragma_name: &str,
    ) -> Result<Option<Option<u32>>> {
        if self.matches(&TokenKind::Eq) {
            return self
                .parse_lenient_unsigned_pragma_integer(pragma_name)
                .map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_lenient_unsigned_pragma_integer(pragma_name)?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_optional_signed_pragma_i32_bits_assignment(
        &mut self,
        pragma_name: &str,
    ) -> Result<Option<u32>> {
        if self.matches(&TokenKind::Eq) {
            return self.parse_signed_pragma_i32_bits(pragma_name).map(Some);
        }
        if self.matches(&TokenKind::LParen) {
            let value = self.parse_signed_pragma_i32_bits(pragma_name)?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn parse_signed_pragma_i32_bits(&mut self, pragma_name: &str) -> Result<u32> {
        let value = self.parse_signed_pragma_integer(pragma_name)?;
        let value = i32::try_from(value)
            .map_err(|_| DbError::sql(format!("PRAGMA {pragma_name} value is too large")))?;
        Ok(u32::from_ne_bytes(value.to_ne_bytes()))
    }

    fn parse_unsigned_pragma_integer(&mut self, pragma_name: &str) -> Result<u32> {
        match self.peek_kind() {
            TokenKind::Integer(value) if *value >= 0 => {
                let value = u32::try_from(*value).map_err(|_| {
                    DbError::sql(format!("PRAGMA {pragma_name} value is too large"))
                })?;
                self.advance();
                Ok(value)
            }
            TokenKind::String(value) => {
                let value = sqlite_pragma_string_integer_prefix(value).max(0);
                let value = u32::try_from(value).map_err(|_| {
                    DbError::sql(format!("PRAGMA {pragma_name} value is too large"))
                })?;
                self.advance();
                Ok(value)
            }
            token => Err(self.error_expected(&format!(
                "non-negative integer {pragma_name} value, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_lenient_unsigned_pragma_integer(&mut self, pragma_name: &str) -> Result<Option<u32>> {
        match self.peek_kind() {
            TokenKind::Integer(value) if *value >= 0 => {
                let value = u32::try_from(*value).map_err(|_| {
                    DbError::sql(format!("PRAGMA {pragma_name} value is too large"))
                })?;
                self.advance();
                Ok(Some(value))
            }
            TokenKind::String(value) => {
                let value = sqlite_pragma_string_integer_prefix(value);
                self.advance();
                if value >= 0 {
                    let value = u32::try_from(value).map_err(|_| {
                        DbError::sql(format!("PRAGMA {pragma_name} value is too large"))
                    })?;
                    Ok(Some(value))
                } else {
                    Ok(None)
                }
            }
            TokenKind::Minus => {
                self.advance();
                if matches!(self.peek_kind(), TokenKind::Integer(_)) {
                    self.advance();
                } else if is_identifier_token(self.peek_kind()) {
                    let _ = self.parse_simple_identifier()?;
                }
                Ok(None)
            }
            TokenKind::Default => {
                self.advance();
                Ok(None)
            }
            TokenKind::Identifier(_) => {
                let _ = self.parse_pragma_name_argument()?;
                Ok(None)
            }
            token => Err(self.error_expected(&format!(
                "integer {pragma_name} value, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_signed_pragma_integer(&mut self, pragma_name: &str) -> Result<i64> {
        match self.peek_kind() {
            TokenKind::Integer(value) => {
                let value = *value;
                self.advance();
                Ok(value)
            }
            TokenKind::String(value) => {
                let value = sqlite_pragma_string_integer_prefix(value);
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
            return match self.peek_kind() {
                TokenKind::Table => self.parse_create_table(true),
                TokenKind::Identifier(name) if name.eq_ignore_ascii_case("VIEW") => {
                    self.parse_create_view(true)
                }
                token => {
                    Err(self
                        .error_expected(&format!("TABLE or VIEW, found {}", display_token(token))))
                }
            };
        }
        match self.peek_kind() {
            TokenKind::Table => self.parse_create_table(false),
            TokenKind::Identifier(name) if name.eq_ignore_ascii_case("VIEW") => {
                self.parse_create_view(false)
            }
            TokenKind::Index => self.parse_create_index(false),
            TokenKind::Unique => {
                self.advance();
                self.parse_create_index(true)
            }
            TokenKind::Identifier(name) if name.eq_ignore_ascii_case("TRIGGER") => {
                self.parse_create_trigger()
            }
            token => Err(self.error_expected(&format!(
                "TABLE, VIEW, INDEX, UNIQUE INDEX, or TRIGGER, found {}",
                display_token(token)
            ))),
        }
    }

    fn parse_create_trigger(&mut self) -> Result<Statement> {
        let start = self.index.saturating_sub(1);
        self.expect_identifier_keyword("TRIGGER")?;
        let if_not_exists = self.parse_if_not_exists()?;
        let (name, schema) = self.parse_schema_qualified_name_with_schema()?;

        while !matches!(self.peek_kind(), TokenKind::On | TokenKind::Eof) {
            self.advance();
        }
        self.expect_keyword(TokenKind::On)?;
        let (table, _table_schema) = self.parse_schema_qualified_name_with_schema()?;

        while !matches!(self.peek_kind(), TokenKind::Begin | TokenKind::Eof) {
            self.advance();
        }
        self.expect_keyword(TokenKind::Begin)?;

        let mut case_depth = 0usize;
        loop {
            match self.peek_kind() {
                TokenKind::Case => {
                    case_depth += 1;
                    self.advance();
                }
                TokenKind::End if case_depth > 0 => {
                    case_depth -= 1;
                    self.advance();
                }
                TokenKind::End => {
                    self.advance();
                    break;
                }
                TokenKind::Eof => break,
                _ => self.advance(),
            }
        }

        let fragments = self.tokens[start..self.index]
            .iter()
            .map(token_sql_fragment)
            .collect::<Vec<_>>();
        let sql = join_sql_fragments(&fragments).replace("VALUES(", "VALUES (");
        Ok(Statement::CreateTrigger {
            name,
            schema,
            table,
            sql,
            if_not_exists,
        })
    }

    fn parse_create_view(&mut self, temporary: bool) -> Result<Statement> {
        self.expect_identifier_keyword("VIEW")?;
        let if_not_exists = self.parse_if_not_exists()?;
        let (name, schema) = self.parse_schema_qualified_name_with_schema()?;
        let temporary =
            temporary || schema.is_some_and(|schema| schema.eq_ignore_ascii_case("temp"));
        let columns = if matches!(self.peek_kind(), TokenKind::LParen) {
            Some(self.parse_parenthesized_identifier_list()?)
        } else {
            None
        };
        self.expect_keyword(TokenKind::As)?;
        let select = if matches!(self.peek_kind(), TokenKind::Values) {
            self.parse_values_as_select_statement()?
        } else if matches!(self.peek_kind(), TokenKind::With) {
            self.parse_with_select_statement()?
        } else {
            self.parse_select_statement()?
        };
        Ok(Statement::CreateView {
            name,
            columns,
            if_not_exists,
            select,
            temporary,
        })
    }

    fn parse_create_table(&mut self, temporary: bool) -> Result<Statement> {
        self.expect_keyword(TokenKind::Table)?;
        let if_not_exists = self.parse_if_not_exists()?;
        let (name, schema) = self.parse_schema_qualified_name_with_schema()?;
        let temporary =
            temporary || schema.is_some_and(|schema| schema.eq_ignore_ascii_case("temp"));
        if self.matches(&TokenKind::As) {
            if matches!(self.peek_kind(), TokenKind::With) {
                return match self.parse_with_statement()? {
                    Statement::Select(select) => Ok(Statement::CreateTableAs {
                        name,
                        if_not_exists,
                        select,
                        temporary,
                    }),
                    Statement::ValuesWith { with, rows } => Ok(Statement::CreateTableAsValues {
                        name,
                        if_not_exists,
                        with: Some(with),
                        rows,
                        temporary,
                    }),
                    _ => Err(DbError::sql(
                        "CREATE TABLE AS WITH only supports SELECT or VALUES",
                    )),
                };
            }
            if matches!(self.peek_kind(), TokenKind::Values) {
                return Ok(Statement::CreateTableAsValues {
                    name,
                    if_not_exists,
                    with: None,
                    rows: self.parse_values_rows()?,
                    temporary,
                });
            }
            let select = self.parse_select_statement()?;
            return Ok(Statement::CreateTableAs {
                name,
                if_not_exists,
                select,
                temporary,
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
        if strict {
            validate_strict_table_declared_types(&name, &columns)?;
        }
        Ok(Statement::CreateTable {
            name,
            columns,
            constraints,
            strict,
            without_rowid,
            if_not_exists,
            temporary,
        })
    }

    fn parse_create_index(&mut self, unique: bool) -> Result<Statement> {
        self.expect_keyword(TokenKind::Index)?;
        let if_not_exists = self.parse_if_not_exists()?;
        let (name, schema) = self.parse_schema_qualified_name_with_schema()?;
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
            schema,
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
                let (name, schema) = self.parse_schema_qualified_name_with_schema()?;
                Ok(Statement::DropTable {
                    name,
                    schema,
                    if_exists,
                })
            }
            TokenKind::Index => {
                self.advance();
                let if_exists = self.parse_if_exists()?;
                let (name, schema) = self.parse_schema_qualified_name_with_schema()?;
                Ok(Statement::DropIndex {
                    name,
                    schema,
                    if_exists,
                })
            }
            TokenKind::Identifier(name) if name.eq_ignore_ascii_case("VIEW") => {
                self.advance();
                let if_exists = self.parse_if_exists()?;
                let (name, schema) = self.parse_schema_qualified_name_with_schema()?;
                Ok(Statement::DropView {
                    name,
                    schema,
                    if_exists,
                })
            }
            TokenKind::Identifier(name) if name.eq_ignore_ascii_case("TRIGGER") => {
                self.advance();
                let if_exists = self.parse_if_exists()?;
                let (name, schema) = self.parse_schema_qualified_name_with_schema()?;
                Ok(Statement::DropTrigger {
                    name,
                    schema,
                    if_exists,
                })
            }
            token => Err(self.error_expected(&format!(
                "TABLE, VIEW, INDEX, or TRIGGER, found {}",
                display_token(token)
            ))),
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
        let table = self.parse_schema_qualified_name()?;
        let _ = self.parse_optional_insert_target_alias()?;
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
        let table = self.parse_schema_qualified_name()?;
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
        let (table, schema) = self.parse_schema_qualified_name_with_schema()?;
        let table_alias = self.parse_optional_table_alias()?;
        let index_hint = self
            .parse_optional_table_index_hint()?
            .map(table_index_hint_from_parsed);
        let filter = if self.matches(&TokenKind::Where) {
            Some(self.parse_where_expr()?)
        } else {
            None
        };
        let order_by = if self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items(false)?
        } else {
            Vec::new()
        };
        let (limit, offset) = self.parse_optional_limit_offset()?;
        let returning = self.parse_optional_returning_clause()?;
        let returning_order_by = if returning.is_some() && self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items(false)?
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
                    schema,
                    table_alias,
                    index_hint,
                    filter,
                    returning,
                    order_by: returning_order_by,
                    limit: returning_limit,
                    offset: returning_offset,
                })
            } else {
                Ok(Statement::DeleteReturning {
                    table,
                    schema,
                    table_alias,
                    index_hint,
                    filter,
                    returning,
                })
            }
        } else if !order_by.is_empty() || limit.is_some() || offset.is_some() {
            Ok(Statement::DeleteLimited {
                table,
                schema,
                table_alias,
                index_hint,
                filter,
                order_by,
                limit,
                offset,
            })
        } else {
            Ok(Statement::Delete {
                table,
                schema,
                table_alias,
                index_hint,
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
        let or_conflict = if self.matches(&TokenKind::Or) {
            Some(self.parse_insert_or_conflict_resolution()?)
        } else {
            None
        };
        let (table, schema) = self.parse_schema_qualified_name_with_schema()?;
        let table_alias = self.parse_optional_table_alias()?;
        let index_hint = self
            .parse_optional_table_index_hint()?
            .map(table_index_hint_from_parsed);
        self.expect_keyword(TokenKind::Set)?;
        let assignments = self.parse_assignments()?;
        let from = if self.matches(&TokenKind::From) {
            Some(self.parse_from_item()?)
        } else {
            None
        };
        let filter = if self.matches(&TokenKind::Where) {
            Some(self.parse_where_expr()?)
        } else {
            None
        };
        let order_by = if self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items(false)?
        } else {
            Vec::new()
        };
        let (limit, offset) = self.parse_optional_limit_offset()?;
        let returning = self.parse_optional_returning_clause()?;
        let returning_order_by = if returning.is_some() && self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items(false)?
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
                    schema,
                    table_alias,
                    index_hint,
                    or_conflict,
                    assignments,
                    from,
                    filter,
                    returning,
                    order_by: returning_order_by,
                    limit: returning_limit,
                    offset: returning_offset,
                })
            } else {
                Ok(Statement::UpdateReturning {
                    table,
                    schema,
                    table_alias,
                    index_hint,
                    or_conflict,
                    assignments,
                    from,
                    filter,
                    returning,
                })
            }
        } else if !order_by.is_empty() || limit.is_some() || offset.is_some() {
            Ok(Statement::UpdateLimited {
                table,
                schema,
                table_alias,
                index_hint,
                or_conflict,
                assignments,
                from,
                filter,
                order_by,
                limit,
                offset,
            })
        } else {
            Ok(Statement::Update {
                table,
                schema,
                table_alias,
                index_hint,
                or_conflict,
                assignments,
                from,
                filter,
            })
        }
    }

    fn parse_begin_or_start_transaction(&mut self) -> Result<Statement> {
        if self.matches(&TokenKind::Begin) {
            self.parse_optional_sqlite_begin_mode();
            let _ = self.matches(&TokenKind::Transaction);
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

    fn parse_savepoint_statement(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Savepoint)?;
        Ok(Statement::Savepoint {
            name: self.parse_simple_identifier()?,
        })
    }

    fn parse_rollback_statement(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Rollback)?;
        let _ = self.matches(&TokenKind::Transaction);
        if self.matches(&TokenKind::To) {
            let _ = self.matches(&TokenKind::Savepoint);
            return Ok(Statement::RollbackTo {
                name: self.parse_simple_identifier()?,
            });
        }
        Ok(Statement::Rollback)
    }

    fn parse_release_statement(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Release)?;
        let _ = self.matches(&TokenKind::Savepoint);
        Ok(Statement::Release {
            name: self.parse_simple_identifier()?,
        })
    }

    fn parse_optional_sqlite_begin_mode(&mut self) {
        if self.parse_optional_identifier_keyword_if("DEFERRED")
            || self.parse_optional_identifier_keyword_if("IMMEDIATE")
            || self.parse_optional_identifier_keyword_if("EXCLUSIVE")
        {
            return;
        }
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

    fn parse_values_as_select_statement(&mut self) -> Result<SelectStatement> {
        Ok(SelectStatement {
            with: None,
            distinct: false,
            columns: vec![SelectItem::Wildcard],
            from: FromItem::Values {
                rows: self.parse_values_rows()?,
                alias: None,
                columns: None,
            },
            joins: vec![],
            filter: None,
            group_by: vec![],
            having: None,
            compounds: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        })
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
            self.parse_order_by_items(true)?
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
        let mut columns = self.parse_select_list()?;
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
                schema: None,
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
        let named_windows = self.parse_optional_named_windows()?;
        columns = self.resolve_named_window_select_items(columns, &named_windows)?;
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

    fn parse_optional_named_windows(&mut self) -> Result<Vec<NamedWindowSpec>> {
        if !self.parse_optional_identifier_keyword_if("WINDOW") {
            return Ok(Vec::new());
        }
        let mut windows = Vec::new();
        loop {
            let name = self.parse_simple_identifier()?;
            self.expect_keyword(TokenKind::As)?;
            self.expect_symbol(TokenKind::LParen)?;
            let (base_name, partition_by, order_by, frame, exclude) =
                self.parse_window_definition_body()?;
            self.expect_symbol(TokenKind::RParen)?;
            windows.push(NamedWindowSpec {
                name,
                base_name,
                partition_by,
                order_by,
                frame,
                exclude,
            });
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        Ok(windows)
    }

    fn resolve_named_window_select_items(
        &self,
        items: Vec<SelectItem>,
        windows: &[NamedWindowSpec],
    ) -> Result<Vec<SelectItem>> {
        items
            .into_iter()
            .map(|item| self.resolve_named_window_select_item(item, windows))
            .collect()
    }

    fn resolve_named_window_select_item(
        &self,
        item: SelectItem,
        windows: &[NamedWindowSpec],
    ) -> Result<SelectItem> {
        match item {
            SelectItem::Expr { expr, alias } => Ok(SelectItem::Expr {
                expr: self.resolve_named_window_scalar_expr(expr, windows)?,
                alias,
            }),
            item => Ok(item),
        }
    }

    fn resolve_named_window_scalar_expr(
        &self,
        expr: ScalarExpr,
        windows: &[NamedWindowSpec],
    ) -> Result<ScalarExpr> {
        match expr {
            ScalarExpr::WindowFunction {
                func,
                args,
                partition_by,
                order_by,
                frame,
                exclude,
                window_name: Some(name),
                filter,
            } => {
                let (base_partition_by, base_order_by, base_frame, base_exclude) =
                    self.resolve_named_window_spec(&name, windows)?;
                if !partition_by.is_empty() {
                    return Err(DbError::sql(
                        "named window reference cannot override PARTITION BY",
                    ));
                }
                if !base_order_by.is_empty() && !order_by.is_empty() {
                    return Err(DbError::sql(format!(
                        "cannot override ORDER BY clause of window: {name}"
                    )));
                }
                if base_frame != WindowFrame::Default && frame != WindowFrame::Default {
                    return Err(DbError::sql(format!(
                        "cannot override frame specification of window: {name}"
                    )));
                }
                if base_exclude != WindowExclude::NoOthers && exclude != WindowExclude::NoOthers {
                    return Err(DbError::sql(format!(
                        "cannot override frame specification of window: {name}"
                    )));
                }
                Ok(ScalarExpr::WindowFunction {
                    func,
                    args,
                    partition_by: base_partition_by,
                    order_by: if order_by.is_empty() {
                        base_order_by
                    } else {
                        order_by
                    },
                    frame: if frame == WindowFrame::Default {
                        base_frame
                    } else {
                        frame
                    },
                    exclude: if exclude == WindowExclude::NoOthers {
                        base_exclude
                    } else {
                        exclude
                    },
                    window_name: None,
                    filter,
                })
            }
            ScalarExpr::WindowFunction {
                func,
                args,
                partition_by,
                order_by,
                frame,
                exclude,
                window_name: None,
                filter,
            } => Ok(ScalarExpr::WindowFunction {
                func,
                args,
                partition_by,
                order_by,
                frame,
                exclude,
                window_name: None,
                filter,
            }),
            expr => Ok(expr),
        }
    }

    fn resolve_named_window_spec(
        &self,
        name: &str,
        windows: &[NamedWindowSpec],
    ) -> Result<(Vec<ScalarExpr>, Vec<OrderBy>, WindowFrame, WindowExclude)> {
        let Some(window) = windows
            .iter()
            .find(|candidate| candidate.name.eq_ignore_ascii_case(name))
        else {
            return Err(DbError::sql(format!("unknown window name {name}")));
        };
        let (base_partition_by, base_order_by, base_frame, base_exclude) =
            if let Some(base_name) = window.base_name.as_deref() {
                self.resolve_named_window_spec(base_name, windows)?
            } else {
                (
                    Vec::new(),
                    Vec::new(),
                    WindowFrame::Default,
                    WindowExclude::NoOthers,
                )
            };
        if window.base_name.is_some() && !window.partition_by.is_empty() {
            return Err(DbError::sql(
                "named window reference cannot override PARTITION BY",
            ));
        }
        if window.base_name.is_some() && !base_order_by.is_empty() && !window.order_by.is_empty() {
            return Err(DbError::sql(format!(
                "cannot override ORDER BY clause of window: {}",
                window.base_name.as_deref().unwrap_or_default()
            )));
        }
        if window.base_name.is_some()
            && base_frame != WindowFrame::Default
            && window.frame != WindowFrame::Default
        {
            return Err(DbError::sql(format!(
                "cannot override frame specification of window: {}",
                window.base_name.as_deref().unwrap_or_default()
            )));
        }
        if window.base_name.is_some()
            && base_exclude != WindowExclude::NoOthers
            && window.exclude != WindowExclude::NoOthers
        {
            return Err(DbError::sql(format!(
                "cannot override frame specification of window: {}",
                window.base_name.as_deref().unwrap_or_default()
            )));
        }
        Ok((
            if window.partition_by.is_empty() {
                base_partition_by
            } else {
                window.partition_by.clone()
            },
            if window.order_by.is_empty() {
                base_order_by
            } else {
                window.order_by.clone()
            },
            if window.frame == WindowFrame::Default {
                base_frame
            } else {
                window.frame
            },
            if window.exclude == WindowExclude::NoOthers {
                base_exclude
            } else {
                window.exclude
            },
        ))
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

        let (name, schema) = self.parse_schema_qualified_name_with_schema()?;
        if is_pragma_table_function_name(&name)
            && (matches!(self.peek_kind(), TokenKind::LParen)
                || pragma_table_function_allows_no_argument(&name))
        {
            let argument = if self.matches(&TokenKind::LParen) {
                let argument = if matches!(self.peek_kind(), TokenKind::RParen) {
                    None
                } else {
                    Some(self.parse_pragma_name_argument()?)
                };
                self.expect_symbol(TokenKind::RParen)?;
                argument
            } else {
                None
            };
            return Ok(FromItem::PragmaTableFunction {
                name,
                argument,
                alias: self.parse_optional_table_alias()?,
            });
        }
        let alias = self.parse_optional_table_alias()?;
        match self.parse_optional_table_index_hint()? {
            Some(ParsedTableIndexHint::IndexedBy(index)) => Ok(FromItem::TableIndexed {
                name,
                schema,
                alias,
                index,
            }),
            Some(ParsedTableIndexHint::NotIndexed) => Ok(FromItem::TableNotIndexed {
                name,
                schema,
                alias,
            }),
            None => Ok(FromItem::Table {
                name,
                schema,
                alias,
            }),
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
                let pattern = self.parse_scalar_expr()?;
                let escape = self.parse_optional_escape_clause()?;
                return Ok(ScalarExpr::Like {
                    expr: Box::new(expr),
                    pattern: Box::new(pattern),
                    escape,
                    negated: true,
                });
            }
            if self.matches(&TokenKind::Glob) {
                let pattern = self.parse_scalar_expr()?;
                return Ok(ScalarExpr::Glob {
                    expr: Box::new(expr),
                    pattern: Box::new(pattern),
                    negated: true,
                });
            }
            if self.matches(&TokenKind::Regexp) {
                let pattern = self.parse_scalar_expr()?;
                return Ok(sqlite_binary_pattern_function(
                    ScalarFunc::RegexpFunc,
                    pattern,
                    expr,
                    true,
                ));
            }
            if self.matches(&TokenKind::Match) {
                let pattern = self.parse_scalar_expr()?;
                return Ok(sqlite_binary_pattern_function(
                    ScalarFunc::MatchFunc,
                    pattern,
                    expr,
                    true,
                ));
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
            let pattern = self.parse_scalar_expr()?;
            let escape = self.parse_optional_escape_clause()?;
            return Ok(ScalarExpr::Like {
                expr: Box::new(expr),
                pattern: Box::new(pattern),
                escape,
                negated: false,
            });
        }
        if self.matches(&TokenKind::Glob) {
            let pattern = self.parse_scalar_expr()?;
            return Ok(ScalarExpr::Glob {
                expr: Box::new(expr),
                pattern: Box::new(pattern),
                negated: false,
            });
        }
        if self.matches(&TokenKind::Regexp) {
            let pattern = self.parse_scalar_expr()?;
            return Ok(sqlite_binary_pattern_function(
                ScalarFunc::RegexpFunc,
                pattern,
                expr,
                false,
            ));
        }
        if self.matches(&TokenKind::Match) {
            let pattern = self.parse_scalar_expr()?;
            return Ok(sqlite_binary_pattern_function(
                ScalarFunc::MatchFunc,
                pattern,
                expr,
                false,
            ));
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

    fn parse_optional_escape_clause(&mut self) -> Result<Option<Box<ScalarExpr>>> {
        if !self.matches(&TokenKind::Escape) {
            return Ok(None);
        }
        Ok(Some(Box::new(self.parse_scalar_expr()?)))
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
            return Ok(ScalarExpr::UnaryPlus(Box::new(
                self.parse_unary_scalar_expr()?,
            )));
        }
        if self.matches(&TokenKind::Minus) {
            if !is_scalar_expr_start(self.peek_kind()) {
                return Err(self.error_expected("numeric literal after -"));
            }
            let expr = self.parse_unary_scalar_expr()?;
            return Ok(match expr {
                ScalarExpr::Literal(Value::Integer(value)) if value == i64::MIN => {
                    return Err(DbError::sql("integer overflow"));
                }
                ScalarExpr::Literal(Value::Real(value)) if value == 9_223_372_036_854_776_000.0 => {
                    ScalarExpr::Literal(Value::Integer(i64::MIN))
                }
                expr => ScalarExpr::UnaryMinus(Box::new(expr)),
            });
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
        if let Some(window_func) = match function_name_upper.as_str() {
            "ROW_NUMBER" => Some(WindowFunc::RowNumber),
            "RANK" => Some(WindowFunc::Rank),
            "DENSE_RANK" => Some(WindowFunc::DenseRank),
            "LAG" => Some(WindowFunc::Lag),
            "LEAD" => Some(WindowFunc::Lead),
            "NTILE" => Some(WindowFunc::Ntile),
            "PERCENT_RANK" => Some(WindowFunc::PercentRank),
            "CUME_DIST" => Some(WindowFunc::CumeDist),
            "FIRST_VALUE" => Some(WindowFunc::FirstValue),
            "LAST_VALUE" => Some(WindowFunc::LastValue),
            "NTH_VALUE" => Some(WindowFunc::NthValue),
            _ => None,
        } {
            return self.parse_ranking_window_function(window_func);
        }
        let looks_like_scalar_min_max = matches!(function_name_upper.as_str(), "MIN" | "MAX")
            && self.function_call_has_multiple_arguments(lparen_index)?;
        if is_aggregate_function_name(&function_name_upper) && !looks_like_scalar_min_max {
            let (func, arg, filter) =
                self.parse_aggregate_call_after_lparen(&function_name_upper)?;
            if self.parse_optional_identifier_keyword_if("OVER") {
                return self.parse_aggregate_window_function(func, arg, filter);
            }
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
            "SQLITE_LOG" => ScalarFunc::SqliteLog,
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
            "REGEXP" => ScalarFunc::RegexpFunc,
            "MATCH" => ScalarFunc::MatchFunc,
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
            "UNKNOWN" => ScalarFunc::Unknown,
            "JSON" => ScalarFunc::Json,
            "JSONB" => ScalarFunc::Jsonb,
            "JSON_VALID" => ScalarFunc::JsonValid,
            "JSON_ERROR_POSITION" => ScalarFunc::JsonErrorPosition,
            "JSON_PRETTY" => ScalarFunc::JsonPretty,
            "JSON_QUOTE" => ScalarFunc::JsonQuote,
            "JSON_EXTRACT" => ScalarFunc::JsonExtract,
            "JSONB_EXTRACT" => ScalarFunc::JsonbExtract,
            "JSON_TYPE" => ScalarFunc::JsonType,
            "JSON_ARRAY" => ScalarFunc::JsonArray,
            "JSONB_ARRAY" => ScalarFunc::JsonbArray,
            "JSON_OBJECT" => ScalarFunc::JsonObject,
            "JSONB_OBJECT" => ScalarFunc::JsonbObject,
            "JSON_ARRAY_LENGTH" => ScalarFunc::JsonArrayLength,
            "JSON_REMOVE" => ScalarFunc::JsonRemove,
            "JSONB_REMOVE" => ScalarFunc::JsonbRemove,
            "JSON_SET" => ScalarFunc::JsonSet,
            "JSONB_SET" => ScalarFunc::JsonbSet,
            "JSON_INSERT" => ScalarFunc::JsonInsert,
            "JSONB_INSERT" => ScalarFunc::JsonbInsert,
            "JSON_REPLACE" => ScalarFunc::JsonReplace,
            "JSONB_REPLACE" => ScalarFunc::JsonbReplace,
            "JSON_PATCH" => ScalarFunc::JsonPatch,
            "JSONB_PATCH" => ScalarFunc::JsonbPatch,
            "LAST_INSERT_ROWID" => ScalarFunc::LastInsertRowId,
            _ => {
                return Err(DbError::sql(format!(
                    "unsupported scalar function: {function_name}"
                )));
            }
        };

        let mut args = Vec::new();
        if !self.matches(&TokenKind::RParen) {
            if matches!(func, ScalarFunc::MinScalar | ScalarFunc::MaxScalar) {
                self.matches(&TokenKind::Distinct);
            }
            loop {
                args.push(self.parse_scalar_expr()?);
                if self.matches(&TokenKind::Comma) {
                    continue;
                }
                self.expect_symbol(TokenKind::RParen)?;
                break;
            }
        }

        if matches!(func, ScalarFunc::Likelihood) {
            validate_likelihood_probability_arg(&args)?;
        }

        Ok(ScalarExpr::Function { func, args })
    }

    fn parse_ranking_window_function(&mut self, func: WindowFunc) -> Result<ScalarExpr> {
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
        match func {
            WindowFunc::RowNumber
            | WindowFunc::Rank
            | WindowFunc::DenseRank
            | WindowFunc::PercentRank
            | WindowFunc::CumeDist => {
                if !args.is_empty() {
                    return Err(DbError::sql("ranking window function expects no arguments"));
                }
            }
            WindowFunc::Lag | WindowFunc::Lead => {
                if !(1..=3).contains(&args.len()) {
                    return Err(DbError::sql("LAG/LEAD expects 1 to 3 arguments"));
                }
            }
            WindowFunc::Ntile => {
                if args.len() != 1 {
                    return Err(DbError::sql("NTILE expects exactly 1 argument"));
                }
            }
            WindowFunc::FirstValue | WindowFunc::LastValue => {
                if args.len() != 1 {
                    return Err(DbError::sql(
                        "FIRST_VALUE/LAST_VALUE expects exactly 1 argument",
                    ));
                }
            }
            WindowFunc::NthValue => {
                if args.len() != 2 {
                    return Err(DbError::sql("NTH_VALUE expects exactly 2 arguments"));
                }
            }
            WindowFunc::Count
            | WindowFunc::Sum
            | WindowFunc::Avg
            | WindowFunc::Total
            | WindowFunc::Min
            | WindowFunc::Max
            | WindowFunc::GroupConcat
            | WindowFunc::JsonGroupArray
            | WindowFunc::JsonGroupObject => {}
        }
        if !self.parse_optional_identifier_keyword_if("OVER") {
            return Err(DbError::sql(
                "window ranking function requires an OVER clause",
            ));
        }
        self.parse_window_function_over_clause(func, args, None)
    }

    fn parse_window_function_over_clause(
        &mut self,
        func: WindowFunc,
        args: Vec<ScalarExpr>,
        filter: Option<Box<Expr>>,
    ) -> Result<ScalarExpr> {
        if !matches!(self.peek_kind(), TokenKind::LParen) {
            let window_name = self.parse_simple_identifier()?;
            return Ok(ScalarExpr::WindowFunction {
                func,
                args,
                partition_by: Vec::new(),
                order_by: Vec::new(),
                frame: WindowFrame::Default,
                exclude: WindowExclude::NoOthers,
                window_name: Some(window_name),
                filter,
            });
        }
        self.expect_symbol(TokenKind::LParen)?;
        let (base_name, partition_by, order_by, frame, exclude) =
            self.parse_window_definition_body()?;
        self.expect_symbol(TokenKind::RParen)?;
        Ok(ScalarExpr::WindowFunction {
            func,
            args,
            partition_by,
            order_by,
            frame,
            exclude,
            window_name: base_name,
            filter,
        })
    }

    fn parse_window_definition_body(
        &mut self,
    ) -> Result<(
        Option<String>,
        Vec<ScalarExpr>,
        Vec<OrderBy>,
        WindowFrame,
        WindowExclude,
    )> {
        let base_name = if let TokenKind::Identifier(name) = self.peek_kind()
            && !name.eq_ignore_ascii_case("PARTITION")
            && !name.eq_ignore_ascii_case("ORDER")
            && !name.eq_ignore_ascii_case("ROWS")
            && !name.eq_ignore_ascii_case("RANGE")
            && !name.eq_ignore_ascii_case("GROUPS")
        {
            Some(self.parse_simple_identifier()?)
        } else {
            None
        };
        let mut partition_by = Vec::new();
        if self.parse_optional_identifier_keyword_if("PARTITION") {
            self.expect_keyword(TokenKind::By)?;
            loop {
                partition_by.push(self.parse_scalar_expr()?);
                if !self.matches(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let order_by = if self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items(false)?
        } else {
            Vec::new()
        };
        let frame = self.parse_optional_window_frame()?;
        let exclude = self.parse_optional_window_exclude()?;
        Ok((base_name, partition_by, order_by, frame, exclude))
    }

    fn parse_optional_window_exclude(&mut self) -> Result<WindowExclude> {
        if !self.parse_optional_identifier_keyword_if("EXCLUDE") {
            return Ok(WindowExclude::NoOthers);
        }
        if self.parse_optional_identifier_keyword_if("CURRENT") {
            self.expect_identifier_keyword("ROW")?;
            return Ok(WindowExclude::CurrentRow);
        }
        if self.matches(&TokenKind::Group) || self.parse_optional_identifier_keyword_if("GROUP") {
            return Ok(WindowExclude::Group);
        }
        if self.parse_optional_identifier_keyword_if("TIES") {
            return Ok(WindowExclude::Ties);
        }
        self.expect_identifier_keyword("NO")?;
        self.expect_identifier_keyword("OTHERS")?;
        Ok(WindowExclude::NoOthers)
    }

    fn parse_optional_window_frame(&mut self) -> Result<WindowFrame> {
        if self.parse_optional_identifier_keyword_if("RANGE") {
            if !self.matches(&TokenKind::Between) {
                if self.parse_optional_identifier_keyword_if("CURRENT") {
                    self.expect_identifier_keyword("ROW")?;
                    return Ok(WindowFrame::GroupsCurrentRow);
                }
                if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
                    self.expect_identifier_keyword("PRECEDING")?;
                    return Ok(WindowFrame::Default);
                }
                let preceding =
                    self.parse_window_range_offset("CURRENT, UNBOUNDED, or non-negative number")?;
                self.expect_identifier_keyword("PRECEDING")?;
                return Ok(WindowFrame::RangePrecedingToCurrentRow(preceding));
            }

            let start = if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
                (None, -1_i8)
            } else if self.parse_optional_identifier_keyword_if("CURRENT") {
                self.expect_identifier_keyword("ROW")?;
                (None, 0_i8)
            } else {
                let value =
                    self.parse_window_range_offset("UNBOUNDED, CURRENT, or non-negative number")?;
                if self.parse_optional_identifier_keyword_if("PRECEDING") {
                    (Some(value), -1_i8)
                } else {
                    self.expect_identifier_keyword("FOLLOWING")?;
                    (Some(value), 1_i8)
                }
            };
            if start.0.is_none() && start.1 == -1 {
                self.expect_identifier_keyword("PRECEDING")?;
            }
            self.expect_keyword(TokenKind::And)?;
            if self.parse_optional_identifier_keyword_if("CURRENT") {
                self.expect_identifier_keyword("ROW")?;
                return Ok(match start {
                    (None, -1) => WindowFrame::Default,
                    (None, 0) => WindowFrame::GroupsCurrentRow,
                    (Some(value), -1) => WindowFrame::RangePrecedingToCurrentRow(value),
                    _ => {
                        return Err(DbError::sql(
                            "numeric RANGE frame form is not supported yet",
                        ));
                    }
                });
            }
            if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
                self.expect_identifier_keyword("FOLLOWING")?;
                return Ok(match start {
                    (None, -1) => WindowFrame::GroupsUnboundedPrecedingAndFollowing,
                    (None, 0) => WindowFrame::GroupsCurrentRowToUnboundedFollowing,
                    (Some(value), -1) => WindowFrame::RangePrecedingToUnboundedFollowing(value),
                    (Some(value), 1) => WindowFrame::RangeFollowingToUnboundedFollowing(value),
                    _ => {
                        return Err(DbError::sql(
                            "numeric RANGE start cannot end at UNBOUNDED FOLLOWING yet",
                        ));
                    }
                });
            }
            let end_value =
                self.parse_window_range_offset("UNBOUNDED, CURRENT, or non-negative number")?;
            let end_is_preceding = if self.parse_optional_identifier_keyword_if("PRECEDING") {
                true
            } else {
                self.expect_identifier_keyword("FOLLOWING")?;
                false
            };
            return Ok(if end_is_preceding {
                match start {
                    (None, -1) => WindowFrame::RangeUnboundedPrecedingToPreceding(end_value),
                    (Some(start), -1) => WindowFrame::RangePrecedingToPreceding {
                        start,
                        end: end_value,
                    },
                    _ => {
                        return Err(DbError::sql(
                            "numeric RANGE frame form is not supported yet",
                        ));
                    }
                }
            } else {
                match start {
                    (None, -1) => WindowFrame::RangeUnboundedPrecedingToFollowing(end_value),
                    (None, 0) => WindowFrame::RangeCurrentRowToFollowing(end_value),
                    (Some(preceding), -1) => WindowFrame::RangePrecedingToFollowing {
                        preceding,
                        following: end_value,
                    },
                    (Some(start), 1) => WindowFrame::RangeFollowingToFollowing {
                        start,
                        end: end_value,
                    },
                    _ => {
                        return Err(DbError::sql(
                            "numeric RANGE frame form is not supported yet",
                        ));
                    }
                }
            });
        }
        if self.parse_optional_identifier_keyword_if("GROUPS") {
            if !self.matches(&TokenKind::Between) {
                if self.parse_optional_identifier_keyword_if("CURRENT") {
                    self.expect_identifier_keyword("ROW")?;
                    return Ok(WindowFrame::GroupsCurrentRow);
                }
                if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
                    self.expect_identifier_keyword("PRECEDING")?;
                    return Ok(WindowFrame::GroupsUnboundedPrecedingToCurrentRow);
                }
                let preceding =
                    self.parse_window_frame_offset("CURRENT, UNBOUNDED, or non-negative integer")?;
                self.expect_identifier_keyword("PRECEDING")?;
                return Ok(WindowFrame::GroupsPrecedingToCurrentRow(preceding));
            }
            if self.parse_optional_identifier_keyword_if("CURRENT") {
                self.expect_identifier_keyword("ROW")?;
                self.expect_keyword(TokenKind::And)?;
                if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
                    self.expect_identifier_keyword("FOLLOWING")?;
                    return Ok(WindowFrame::GroupsCurrentRowToUnboundedFollowing);
                }
                if self.parse_optional_identifier_keyword_if("CURRENT") {
                    self.expect_identifier_keyword("ROW")?;
                    return Ok(WindowFrame::GroupsCurrentRow);
                }
                let following = self.parse_window_frame_offset("non-negative integer")?;
                self.expect_identifier_keyword("FOLLOWING")?;
                return Ok(WindowFrame::GroupsCurrentRowToFollowing(following));
            }
            let start = if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
                None
            } else {
                Some(self.parse_window_frame_offset("UNBOUNDED or non-negative integer")?)
            };
            let start_is_following = if start.is_some() {
                if self.parse_optional_identifier_keyword_if("PRECEDING") {
                    false
                } else {
                    self.expect_identifier_keyword("FOLLOWING")?;
                    true
                }
            } else {
                self.expect_identifier_keyword("PRECEDING")?;
                false
            };
            self.expect_keyword(TokenKind::And)?;
            if self.parse_optional_identifier_keyword_if("CURRENT") {
                self.expect_identifier_keyword("ROW")?;
                if start_is_following {
                    return Err(DbError::sql(
                        "FOLLOWING frame start cannot end at CURRENT ROW",
                    ));
                }
                return Ok(match start {
                    Some(value) => WindowFrame::GroupsPrecedingToCurrentRow(value),
                    None => WindowFrame::GroupsUnboundedPrecedingToCurrentRow,
                });
            }
            if let Some(start_value) = start {
                if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
                    self.expect_identifier_keyword("FOLLOWING")?;
                    return Ok(if start_is_following {
                        WindowFrame::GroupsFollowingToUnboundedFollowing(start_value)
                    } else {
                        WindowFrame::GroupsPrecedingToUnboundedFollowing(start_value)
                    });
                }
                let end_value = self.parse_window_frame_offset("non-negative integer")?;
                let end_is_preceding = if self.parse_optional_identifier_keyword_if("PRECEDING") {
                    true
                } else {
                    self.expect_identifier_keyword("FOLLOWING")?;
                    false
                };
                return Ok(if start_is_following {
                    if end_is_preceding {
                        return Err(DbError::sql(
                            "FOLLOWING frame start cannot end at PRECEDING",
                        ));
                    }
                    WindowFrame::GroupsFollowingToFollowing {
                        start: start_value,
                        end: end_value,
                    }
                } else if end_is_preceding {
                    WindowFrame::GroupsPrecedingToPreceding {
                        start: start_value,
                        end: end_value,
                    }
                } else {
                    WindowFrame::GroupsPrecedingToFollowing {
                        preceding: start_value,
                        following: end_value,
                    }
                });
            }
            self.expect_identifier_keyword("UNBOUNDED")?;
            self.expect_identifier_keyword("FOLLOWING")?;
            return Ok(WindowFrame::GroupsUnboundedPrecedingAndFollowing);
        }
        if !self.parse_optional_identifier_keyword_if("ROWS") {
            return Ok(WindowFrame::Default);
        }
        if !self.matches(&TokenKind::Between) {
            if self.parse_optional_identifier_keyword_if("CURRENT") {
                self.expect_identifier_keyword("ROW")?;
                return Ok(WindowFrame::RowsCurrentRow);
            }
            if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
                self.expect_identifier_keyword("PRECEDING")?;
                return Ok(WindowFrame::RowsUnboundedPrecedingToCurrentRow);
            }
            let preceding =
                self.parse_window_frame_offset("CURRENT, UNBOUNDED, or non-negative integer")?;
            self.expect_identifier_keyword("PRECEDING")?;
            return Ok(WindowFrame::RowsPrecedingToCurrentRow(preceding));
        }
        if self.parse_optional_identifier_keyword_if("CURRENT") {
            self.expect_identifier_keyword("ROW")?;
            self.expect_keyword(TokenKind::And)?;
            if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
                self.expect_identifier_keyword("FOLLOWING")?;
                return Ok(WindowFrame::RowsCurrentRowToUnboundedFollowing);
            }
            if self.parse_optional_identifier_keyword_if("CURRENT") {
                self.expect_identifier_keyword("ROW")?;
                return Ok(WindowFrame::RowsCurrentRow);
            }
            let following = self.parse_window_frame_offset("non-negative integer")?;
            self.expect_identifier_keyword("FOLLOWING")?;
            return Ok(WindowFrame::RowsCurrentRowToFollowing(following));
        }
        let start = if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
            None
        } else {
            Some(self.parse_window_frame_offset("UNBOUNDED or non-negative integer")?)
        };
        let start_is_following = if start.is_some() {
            if self.parse_optional_identifier_keyword_if("PRECEDING") {
                false
            } else {
                self.expect_identifier_keyword("FOLLOWING")?;
                true
            }
        } else {
            self.expect_identifier_keyword("PRECEDING")?;
            false
        };
        self.expect_keyword(TokenKind::And)?;
        if self.parse_optional_identifier_keyword_if("CURRENT") {
            self.expect_identifier_keyword("ROW")?;
            if start_is_following {
                return Err(DbError::sql(
                    "FOLLOWING frame start cannot end at CURRENT ROW",
                ));
            }
            return Ok(match start {
                Some(value) => WindowFrame::RowsPrecedingToCurrentRow(value),
                None => WindowFrame::RowsUnboundedPrecedingToCurrentRow,
            });
        }
        if let Some(start_value) = start {
            if self.parse_optional_identifier_keyword_if("UNBOUNDED") {
                self.expect_identifier_keyword("FOLLOWING")?;
                return Ok(if start_is_following {
                    WindowFrame::RowsFollowingToUnboundedFollowing(start_value)
                } else {
                    WindowFrame::RowsPrecedingToUnboundedFollowing(start_value)
                });
            }
            let end_value = self.parse_window_frame_offset("non-negative integer")?;
            let end_is_preceding = if self.parse_optional_identifier_keyword_if("PRECEDING") {
                true
            } else {
                self.expect_identifier_keyword("FOLLOWING")?;
                false
            };
            return Ok(if start_is_following {
                if end_is_preceding {
                    return Err(DbError::sql(
                        "FOLLOWING frame start cannot end at PRECEDING",
                    ));
                }
                WindowFrame::RowsFollowingToFollowing {
                    start: start_value,
                    end: end_value,
                }
            } else if end_is_preceding {
                WindowFrame::RowsPrecedingToPreceding {
                    start: start_value,
                    end: end_value,
                }
            } else {
                WindowFrame::RowsPrecedingToFollowing {
                    preceding: start_value,
                    following: end_value,
                }
            });
        }
        self.expect_identifier_keyword("UNBOUNDED")?;
        self.expect_identifier_keyword("FOLLOWING")?;
        Ok(WindowFrame::RowsUnboundedPrecedingAndFollowing)
    }

    fn parse_window_frame_offset(&mut self, expected: &str) -> Result<usize> {
        let _ = self.matches(&TokenKind::Plus);
        match self.peek_kind() {
            TokenKind::Integer(value) if *value >= 0 => {
                let value = usize::try_from(*value)
                    .map_err(|_| DbError::sql("window frame offset is too large"))?;
                self.advance();
                Ok(value)
            }
            _ => {
                let expr = self.parse_scalar_expr()?;
                let Some(value) = constant_limit_value(&expr) else {
                    return Err(DbError::sql(format!(
                        "window frame offset must be a constant {expected}"
                    )));
                };
                if value < 0.0 || value.fract() != 0.0 {
                    return Err(DbError::sql(format!(
                        "window frame offset must be a non-negative integer, got {value}"
                    )));
                }
                if value > usize::MAX as f64 {
                    return Err(DbError::sql("window frame offset is too large"));
                }
                Ok(value as usize)
            }
        }
    }

    fn parse_window_range_offset(&mut self, expected: &str) -> Result<WindowRangeOffset> {
        let _ = self.matches(&TokenKind::Plus);
        match self.peek_kind() {
            TokenKind::Integer(value) if *value >= 0 => {
                let value = *value as f64;
                self.advance();
                Ok(WindowRangeOffset::new(value))
            }
            TokenKind::Real(value) if *value >= 0.0 => {
                let value = *value;
                self.advance();
                Ok(WindowRangeOffset::new(value))
            }
            _ => {
                let expr = self.parse_scalar_expr()?;
                let Some(value) = constant_limit_value(&expr) else {
                    return Err(DbError::sql(format!(
                        "window RANGE offset must be a constant {expected}"
                    )));
                };
                if value < 0.0 {
                    return Err(DbError::sql(format!(
                        "window RANGE offset must be non-negative, got {value}"
                    )));
                }
                Ok(WindowRangeOffset::new(value))
            }
        }
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
        if self.parse_optional_identifier_keyword_if("OVER") {
            return Ok(SelectItem::Expr {
                expr: self.parse_aggregate_window_function(func, arg, filter)?,
                alias: None,
            });
        }
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
            "DECIMAL_SUM" => AggregateFunc::DecimalSum,
            "AVG" => AggregateFunc::Avg,
            "TOTAL" => AggregateFunc::Total,
            "MEDIAN" => AggregateFunc::Median,
            "PERCENTILE" => AggregateFunc::Percentile,
            "PERCENTILE_CONT" => AggregateFunc::PercentileCont,
            "PERCENTILE_DISC" => AggregateFunc::PercentileDisc,
            "GROUP_CONCAT" | "STRING_AGG" => AggregateFunc::GroupConcat,
            "JSON_GROUP_ARRAY" => AggregateFunc::JsonGroupArray,
            "JSONB_GROUP_ARRAY" => AggregateFunc::JsonbGroupArray,
            "JSON_GROUP_OBJECT" => AggregateFunc::JsonGroupObject,
            "JSONB_GROUP_OBJECT" => AggregateFunc::JsonbGroupObject,
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
                self.parse_order_by_items(false)?
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
                self.parse_order_by_items(false)?
            } else {
                Vec::new()
            };
            AggregateArg::Percentile {
                expr,
                fraction,
                order_by,
            }
        } else if matches!(
            func,
            AggregateFunc::JsonGroupObject | AggregateFunc::JsonbGroupObject
        ) {
            let key = self.parse_scalar_expr()?;
            self.expect_symbol(TokenKind::Comma)?;
            let value = self.parse_scalar_expr()?;
            let order_by = if self.matches(&TokenKind::Order) {
                self.expect_keyword(TokenKind::By)?;
                self.parse_order_by_items(false)?
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
                self.parse_order_by_items(false)?
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

    fn parse_aggregate_window_function(
        &mut self,
        func: AggregateFunc,
        arg: AggregateArg,
        filter: Option<Expr>,
    ) -> Result<ScalarExpr> {
        if !matches!(
            func,
            AggregateFunc::Count
                | AggregateFunc::Sum
                | AggregateFunc::Avg
                | AggregateFunc::Total
                | AggregateFunc::Min
                | AggregateFunc::Max
                | AggregateFunc::GroupConcat
                | AggregateFunc::JsonGroupArray
                | AggregateFunc::JsonGroupObject
        ) {
            return Err(DbError::sql(
                "only COUNT, SUM, AVG, TOTAL, MIN, MAX, GROUP_CONCAT, and JSON aggregate window functions are supported",
            ));
        }
        let (window_func, args) = match (func, arg) {
            (AggregateFunc::Count, AggregateArg::Wildcard) => (WindowFunc::Count, Vec::new()),
            (
                AggregateFunc::Count,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by,
                },
            ) => {
                if distinct || !order_by.is_empty() {
                    return Err(DbError::sql(
                        "DISTINCT or ORDER BY aggregate window arguments are not supported yet",
                    ));
                }
                (WindowFunc::Count, vec![expr])
            }
            (
                AggregateFunc::Sum,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by,
                },
            ) => {
                if distinct || !order_by.is_empty() {
                    return Err(DbError::sql(
                        "DISTINCT or ORDER BY aggregate window arguments are not supported yet",
                    ));
                }
                (WindowFunc::Sum, vec![expr])
            }
            (
                AggregateFunc::Avg,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by,
                },
            ) => {
                if distinct || !order_by.is_empty() {
                    return Err(DbError::sql(
                        "DISTINCT or ORDER BY aggregate window arguments are not supported yet",
                    ));
                }
                (WindowFunc::Avg, vec![expr])
            }
            (
                AggregateFunc::Total,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by,
                },
            ) => {
                if distinct || !order_by.is_empty() {
                    return Err(DbError::sql(
                        "DISTINCT or ORDER BY aggregate window arguments are not supported yet",
                    ));
                }
                (WindowFunc::Total, vec![expr])
            }
            (
                AggregateFunc::Min,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by,
                },
            ) => {
                if distinct || !order_by.is_empty() {
                    return Err(DbError::sql(
                        "DISTINCT or ORDER BY aggregate window arguments are not supported yet",
                    ));
                }
                (WindowFunc::Min, vec![expr])
            }
            (
                AggregateFunc::Max,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by,
                },
            ) => {
                if distinct || !order_by.is_empty() {
                    return Err(DbError::sql(
                        "DISTINCT or ORDER BY aggregate window arguments are not supported yet",
                    ));
                }
                (WindowFunc::Max, vec![expr])
            }
            (
                AggregateFunc::GroupConcat,
                AggregateArg::GroupConcat {
                    expr,
                    separator,
                    distinct,
                    order_by,
                },
            ) => {
                if distinct || !order_by.is_empty() {
                    return Err(DbError::sql(
                        "DISTINCT or ORDER BY aggregate window arguments are not supported yet",
                    ));
                }
                let mut args = vec![expr];
                if let Some(separator) = separator {
                    args.push(separator);
                }
                (WindowFunc::GroupConcat, args)
            }
            (
                AggregateFunc::JsonGroupArray,
                AggregateArg::Expr {
                    expr,
                    distinct,
                    order_by,
                },
            ) => {
                if distinct || !order_by.is_empty() {
                    return Err(DbError::sql(
                        "DISTINCT or ORDER BY aggregate window arguments are not supported yet",
                    ));
                }
                (WindowFunc::JsonGroupArray, vec![expr])
            }
            (
                AggregateFunc::JsonGroupObject,
                AggregateArg::JsonGroupObject {
                    key,
                    value,
                    order_by,
                },
            ) => {
                if !order_by.is_empty() {
                    return Err(DbError::sql(
                        "ORDER BY aggregate window arguments are not supported yet",
                    ));
                }
                (WindowFunc::JsonGroupObject, vec![key, value])
            }
            _ => {
                return Err(DbError::sql(
                    "unsupported aggregate window function arguments",
                ));
            }
        };
        self.parse_window_function_over_clause(window_func, args, filter.map(Box::new))
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
                let pattern = self.parse_scalar_expr()?;
                let escape = self.parse_optional_escape_clause()?;
                return match column {
                    Some(column) => Ok(Expr::Like {
                        column,
                        pattern: Box::new(pattern),
                        escape,
                        negated: true,
                    }),
                    None => Ok(Expr::LikeScalar {
                        expr: left_expr,
                        pattern: Box::new(pattern),
                        escape,
                        negated: true,
                    }),
                };
            }
            if self.matches(&TokenKind::Glob) {
                let pattern = self.parse_scalar_expr()?;
                return match column {
                    Some(column) => Ok(Expr::Glob {
                        column,
                        pattern: Box::new(pattern),
                        negated: true,
                    }),
                    None => Ok(Expr::GlobScalar {
                        expr: left_expr,
                        pattern: Box::new(pattern),
                        negated: true,
                    }),
                };
            }
            if self.matches(&TokenKind::Regexp) {
                let pattern = self.parse_scalar_expr()?;
                return Ok(Expr::IsBool {
                    expr: sqlite_binary_pattern_function(
                        ScalarFunc::RegexpFunc,
                        pattern,
                        left_expr,
                        true,
                    ),
                    value: true,
                    negated: false,
                    explicit: false,
                });
            }
            if self.matches(&TokenKind::Match) {
                let pattern = self.parse_scalar_expr()?;
                return Ok(Expr::IsBool {
                    expr: sqlite_binary_pattern_function(
                        ScalarFunc::MatchFunc,
                        pattern,
                        left_expr,
                        true,
                    ),
                    value: true,
                    negated: false,
                    explicit: false,
                });
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
            let pattern = self.parse_scalar_expr()?;
            let escape = self.parse_optional_escape_clause()?;
            return match column {
                Some(column) => Ok(Expr::Like {
                    column,
                    pattern: Box::new(pattern),
                    escape,
                    negated: false,
                }),
                None => Ok(Expr::LikeScalar {
                    expr: left_expr,
                    pattern: Box::new(pattern),
                    escape,
                    negated: false,
                }),
            };
        }
        if self.matches(&TokenKind::Glob) {
            let pattern = self.parse_scalar_expr()?;
            return match column {
                Some(column) => Ok(Expr::Glob {
                    column,
                    pattern: Box::new(pattern),
                    negated: false,
                }),
                None => Ok(Expr::GlobScalar {
                    expr: left_expr,
                    pattern: Box::new(pattern),
                    negated: false,
                }),
            };
        }
        if self.matches(&TokenKind::Regexp) {
            let pattern = self.parse_scalar_expr()?;
            return Ok(Expr::IsBool {
                expr: sqlite_binary_pattern_function(
                    ScalarFunc::RegexpFunc,
                    pattern,
                    left_expr,
                    false,
                ),
                value: true,
                negated: false,
                explicit: false,
            });
        }
        if self.matches(&TokenKind::Match) {
            let pattern = self.parse_scalar_expr()?;
            return Ok(Expr::IsBool {
                expr: sqlite_binary_pattern_function(
                    ScalarFunc::MatchFunc,
                    pattern,
                    left_expr,
                    false,
                ),
                value: true,
                negated: false,
                explicit: false,
            });
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
        let mut column = match self.parse_optional_column_type() {
            Some((column_type, declared_type)) => {
                ColumnDef::new(name, column_type).declared_type(declared_type)
            }
            None => ColumnDef::new(name, ColumnType::Any),
        };
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
        let word = match self.peek_kind() {
            TokenKind::Identifier(word) => word.clone(),
            TokenKind::Match => "MATCH".to_string(),
            _ => return None,
        };
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

    fn expect_identifier_keyword(&mut self, expected: &str) -> Result<()> {
        if self.parse_optional_identifier_keyword_if(expected) {
            Ok(())
        } else {
            Err(self.error_expected(&format!(
                "{expected}, found {}",
                display_token(self.peek_kind())
            )))
        }
    }

    fn check_expr_from_expr(expr: Expr) -> Result<CheckExpr> {
        Ok(match expr {
            Expr::Compare { column, op, value } => CheckExpr::Compare {
                column,
                op: Self::check_op_from_compare_op(op),
                value,
            },
            Expr::CompareScalar { left, op, right } => match (left, scalar_expr_literal_value(&right)) {
                (
                    ScalarExpr::Binary {
                        left,
                        op: binary_op,
                        right,
                    },
                    Some(value),
                ) if matches!(
                    binary_op,
                    ScalarBinaryOp::Add
                        | ScalarBinaryOp::Subtract
                        | ScalarBinaryOp::Multiply
                        | ScalarBinaryOp::Divide
                        | ScalarBinaryOp::Modulo
                ) =>
                {
                    Self::check_arithmetic_expr(
                        *left,
                        binary_op,
                        *right,
                        Self::check_op_from_compare_op(op),
                        value,
                    )?
                }
                (ScalarExpr::Function { func, args }, Some(value)) if !args.is_empty() => {
                    Self::check_function_expr(func, args, Self::check_op_from_compare_op(op), value)?
                }
                (ScalarExpr::Cast { expr, ty }, Some(value)) => {
                    let ScalarExpr::Column(column) = *expr else {
                        return Err(DbError::sql("unsupported CHECK expression"));
                    };
                    CheckExpr::CastCompare {
                        column,
                        target_type: ty,
                        op: Self::check_op_from_compare_op(op),
                        value,
                    }
                }
                (ScalarExpr::Collate { expr, collation }, Some(value))
                    if matches!(
                        collation.to_ascii_uppercase().as_str(),
                        "NOCASE" | "RTRIM"
                    ) =>
                {
                    let ScalarExpr::Column(column) = *expr else {
                        return Err(DbError::sql("unsupported CHECK expression"));
                    };
                    CheckExpr::NoCaseCompare {
                        column,
                        collation,
                        op: Self::check_op_from_compare_op(op),
                        value,
                    }
                }
                (ScalarExpr::Function { func, args }, None)
                    if matches!(func, ScalarFunc::Replace | ScalarFunc::MinScalar | ScalarFunc::MaxScalar) =>
                {
                    let ScalarExpr::Column(right_column) = right else {
                        return Err(DbError::sql("unsupported CHECK expression"));
                    };
                    if matches!(func, ScalarFunc::Replace) {
                        Self::check_replace_column_expr(
                            args,
                            Self::check_op_from_compare_op(op),
                            right_column,
                        )?
                    } else {
                        Self::check_min_max_column_expr(
                            args,
                            matches!(func, ScalarFunc::MinScalar),
                            Self::check_op_from_compare_op(op),
                            right_column,
                        )?
                    }
                }
                (ScalarExpr::UnaryMinus(expr), Some(value)) => {
                    Self::check_unary_expr(*expr, true, Self::check_op_from_compare_op(op), value)?
                }
                (ScalarExpr::UnaryPlus(expr), Some(value)) => {
                    Self::check_unary_expr(*expr, false, Self::check_op_from_compare_op(op), value)?
                }
                (ScalarExpr::Literal(value), None) => {
                    let reversed_op = Self::check_op_from_reversed_compare_op(op);
                    match right {
                        ScalarExpr::Column(column) => CheckExpr::Compare {
                            column,
                            op: reversed_op,
                            value,
                        },
                        ScalarExpr::Collate { expr, collation }
                            if matches!(
                                collation.to_ascii_uppercase().as_str(),
                                "NOCASE" | "RTRIM"
                            ) =>
                        {
                            let ScalarExpr::Column(column) = *expr else {
                                return Err(DbError::sql("unsupported CHECK expression"));
                            };
                            CheckExpr::NoCaseCompare {
                                column,
                                collation,
                                op: reversed_op,
                                value,
                            }
                        }
                        ScalarExpr::Function { func, args } if !args.is_empty() => {
                            Self::check_function_expr(func, args, reversed_op, value)?
                        }
                        ScalarExpr::UnaryMinus(expr) => {
                            Self::check_unary_expr(*expr, true, reversed_op, value)?
                        }
                        ScalarExpr::UnaryPlus(expr) => {
                            Self::check_unary_expr(*expr, false, reversed_op, value)?
                        }
                        ScalarExpr::Binary {
                            left,
                            op: binary_op,
                            right,
                        } if matches!(
                            binary_op,
                            ScalarBinaryOp::Add
                                | ScalarBinaryOp::Subtract
                                | ScalarBinaryOp::Multiply
                                | ScalarBinaryOp::Divide
                                | ScalarBinaryOp::Modulo
                        ) =>
                        {
                            Self::check_arithmetic_expr(
                                *left,
                                binary_op,
                                *right,
                                reversed_op,
                                value,
                            )?
                        }
                        _ => return Err(DbError::sql("unsupported CHECK expression")),
                    }
                }
                _ => return Err(DbError::sql("unsupported CHECK expression")),
            },
            Expr::IsNull { column, negated } => CheckExpr::IsNull { column, negated },
            Expr::IsNullScalar { expr, negated } => {
                Self::check_scalar_is_null_expr(expr, negated)?
            }
            Expr::Glob {
                column,
                pattern,
                negated,
            } => CheckExpr::Glob {
                column,
                pattern: scalar_expr_literal_value(&pattern)
                    .map(|value| sqlite_literal_to_text_like(&value))
                    .ok_or_else(|| {
                        DbError::sql(
                            "non-literal GLOB pattern expressions are not supported in CHECK constraints",
                        )
                    })?,
                negated,
            },
            Expr::Like {
                column,
                pattern,
                escape,
                negated,
            } => CheckExpr::Like {
                column,
                pattern: scalar_expr_literal_value(&pattern)
                    .map(|value| sqlite_literal_to_text_like(&value))
                    .ok_or_else(|| {
                        DbError::sql(
                            "non-literal LIKE pattern expressions are not supported in CHECK constraints",
                        )
                    })?,
                escape: escape
                    .as_deref()
                    .map(|expr| {
                        scalar_expr_literal_value(expr)
                            .map(|value| sqlite_literal_to_text_like(&value))
                            .ok_or_else(|| {
                                DbError::sql(
                                    "non-literal ESCAPE expressions are not supported in CHECK constraints",
                                )
                            })
                    })
                    .transpose()?,
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
                ScalarExpr::Function { func, args } if !explicit && value => {
                    let truthy = Self::check_likelihood_truthy_expr(func, args)?;
                    if negated {
                        CheckExpr::Not(Box::new(truthy))
                    } else {
                        truthy
                    }
                }
                ScalarExpr::Not(expr) if !explicit && value && !negated => {
                    let ScalarExpr::Function { func, args } = *expr else {
                        return Err(DbError::sql("unsupported CHECK expression"));
                    };
                    if !matches!(
                        func,
                        ScalarFunc::LikeFunc | ScalarFunc::GlobFunc | ScalarFunc::RegexpFunc
                    ) {
                        return Err(DbError::sql("unsupported CHECK expression"));
                    }
                    Self::check_pattern_function_expr(func, args, true)?
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

    fn check_unary_expr(
        expr: ScalarExpr,
        negated: bool,
        op: CheckOp,
        value: Value,
    ) -> Result<CheckExpr> {
        let ScalarExpr::Column(column) = expr else {
            return Err(DbError::sql("unsupported CHECK expression"));
        };
        Ok(if negated {
            CheckExpr::MultiplyCompare {
                column,
                factor: Value::Integer(-1),
                op,
                value,
            }
        } else {
            CheckExpr::Compare { column, op, value }
        })
    }

    fn check_scalar_is_null_expr(expr: ScalarExpr, negated: bool) -> Result<CheckExpr> {
        match expr {
            ScalarExpr::Function { func, mut args } => {
                if matches!(func, ScalarFunc::Unicode) {
                    if args.len() != 1 {
                        return Err(DbError::sql("unsupported CHECK expression"));
                    }
                    let ScalarExpr::Column(column) = args.remove(0) else {
                        return Err(DbError::sql("unsupported CHECK expression"));
                    };
                    Ok(CheckExpr::UnicodeIsNull { column, negated })
                } else if matches!(func, ScalarFunc::NullIf) {
                    if args.len() != 2 {
                        return Err(DbError::sql("unsupported CHECK expression"));
                    }
                    let ScalarExpr::Column(column) = args.remove(0) else {
                        return Err(DbError::sql("unsupported CHECK expression"));
                    };
                    let value = scalar_expr_literal_value(&args.remove(0))
                        .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
                    Ok(CheckExpr::NullIfIsNull {
                        column,
                        value,
                        negated,
                    })
                } else {
                    Err(DbError::sql("unsupported CHECK expression"))
                }
            }
            _ => Err(DbError::sql("unsupported CHECK expression")),
        }
    }

    fn check_likelihood_truthy_expr(
        func: ScalarFunc,
        mut args: Vec<ScalarExpr>,
    ) -> Result<CheckExpr> {
        let expected_args = match func {
            ScalarFunc::Likely | ScalarFunc::Unlikely => 1,
            ScalarFunc::Likelihood => 2,
            ScalarFunc::LikeFunc if matches!(args.len(), 2 | 3) => args.len(),
            ScalarFunc::GlobFunc => 2,
            ScalarFunc::RegexpFunc => 2,
            ScalarFunc::JsonValid if matches!(args.len(), 1 | 2) => args.len(),
            _ => return Err(DbError::sql("unsupported CHECK expression")),
        };
        if args.len() != expected_args {
            return Err(DbError::sql("unsupported CHECK expression"));
        }
        if matches!(func, ScalarFunc::JsonValid) {
            let ScalarExpr::Column(column) = args.remove(0) else {
                return Err(DbError::sql("unsupported CHECK expression"));
            };
            let flags = if args.is_empty() {
                None
            } else {
                match scalar_expr_literal_value(&args[0]) {
                    Some(Value::Integer(value)) if (1..=15).contains(&value) => Some(value),
                    _ => return Err(DbError::sql("unsupported CHECK expression")),
                }
            };
            return Ok(CheckExpr::JsonValidCompare {
                column,
                flags,
                compare: None,
            });
        }
        if matches!(
            func,
            ScalarFunc::LikeFunc | ScalarFunc::GlobFunc | ScalarFunc::RegexpFunc
        ) {
            return Self::check_pattern_function_expr(func, args, false);
        }
        let ScalarExpr::Column(column) = args.remove(0) else {
            return Err(DbError::sql("unsupported CHECK expression"));
        };
        if matches!(func, ScalarFunc::Likelihood) {
            let Some(Value::Real(_) | Value::Integer(_)) = scalar_expr_literal_value(&args[0])
            else {
                return Err(DbError::sql("unsupported CHECK expression"));
            };
        }
        Ok(CheckExpr::Truthy { column })
    }

    fn check_pattern_function_expr(
        func: ScalarFunc,
        mut args: Vec<ScalarExpr>,
        negated: bool,
    ) -> Result<CheckExpr> {
        let pattern = scalar_expr_literal_value(&args.remove(0))
            .map(|value| sqlite_literal_to_text_like(&value))
            .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
        let ScalarExpr::Column(column) = args.remove(0) else {
            return Err(DbError::sql("unsupported CHECK expression"));
        };
        let escape = if matches!(func, ScalarFunc::LikeFunc) && !args.is_empty() {
            Some(
                scalar_expr_literal_value(&args.remove(0))
                    .map(|value| sqlite_literal_to_text_like(&value))
                    .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?,
            )
        } else {
            None
        };
        Ok(match func {
            ScalarFunc::LikeFunc => CheckExpr::Like {
                column,
                pattern,
                escape,
                negated,
            },
            ScalarFunc::GlobFunc => CheckExpr::Glob {
                column,
                pattern,
                negated,
            },
            ScalarFunc::RegexpFunc => CheckExpr::Regexp {
                column,
                pattern,
                negated,
            },
            _ => unreachable!("matched pattern function"),
        })
    }

    fn check_arithmetic_expr(
        left: ScalarExpr,
        binary_op: ScalarBinaryOp,
        right: ScalarExpr,
        op: CheckOp,
        value: Value,
    ) -> Result<CheckExpr> {
        let ScalarExpr::Column(column) = left else {
            return Err(DbError::sql("unsupported CHECK expression"));
        };
        let Some(mut addend) = scalar_expr_literal_value(&right) else {
            return Err(DbError::sql("unsupported CHECK expression"));
        };
        Ok(if matches!(binary_op, ScalarBinaryOp::Multiply) {
            CheckExpr::MultiplyCompare {
                column,
                factor: addend,
                op,
                value,
            }
        } else if matches!(binary_op, ScalarBinaryOp::Divide) {
            CheckExpr::DivideCompare {
                column,
                divisor: addend,
                op,
                value,
            }
        } else if matches!(binary_op, ScalarBinaryOp::Modulo) {
            CheckExpr::ModuloCompare {
                column,
                divisor: addend,
                op,
                value,
                function_form: false,
            }
        } else {
            if matches!(binary_op, ScalarBinaryOp::Subtract) {
                addend = negate_check_literal(addend)?;
            }
            CheckExpr::ArithmeticCompare {
                column,
                addend,
                op,
                value,
            }
        })
    }

    fn check_function_expr(
        func: ScalarFunc,
        mut args: Vec<ScalarExpr>,
        op: CheckOp,
        value: Value,
    ) -> Result<CheckExpr> {
        if matches!(func, ScalarFunc::ConcatWs) {
            return Self::check_concat_ws_expr(args, op, value);
        }

        if matches!(func, ScalarFunc::Log) && args.len() == 2 {
            if let (Some(argument), ScalarExpr::Column(column)) =
                (scalar_expr_literal_value(&args[0]), &args[1])
            {
                return Ok(CheckExpr::BinaryMathCompare {
                    column: column.clone(),
                    func: BinaryMathFunc::Log,
                    argument,
                    column_is_second: true,
                    op,
                    value,
                });
            }
        }

        let ScalarExpr::Column(column) = args.remove(0) else {
            return Err(DbError::sql("unsupported CHECK expression"));
        };
        Ok(match func {
            ScalarFunc::Length if args.is_empty() => CheckExpr::LengthCompare { column, op, value },
            ScalarFunc::OctetLength if args.is_empty() => {
                CheckExpr::OctetLengthCompare { column, op, value }
            }
            ScalarFunc::Unicode if args.is_empty() => {
                CheckExpr::UnicodeCompare { column, op, value }
            }
            ScalarFunc::Sign if args.is_empty() => CheckExpr::SignCompare { column, op, value },
            ScalarFunc::Hex if args.is_empty() => CheckExpr::HexCompare { column, op, value },
            ScalarFunc::Quote if args.is_empty() => CheckExpr::QuoteCompare { column, op, value },
            ScalarFunc::Replace if args.len() == 2 => {
                let pattern = scalar_expr_literal_value(&args[0])
                    .map(|value| sqlite_literal_to_text_like(&value))
                    .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
                let replacement = scalar_expr_literal_value(&args[1])
                    .map(|value| sqlite_literal_to_text_like(&value))
                    .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
                CheckExpr::ReplaceCompare {
                    column,
                    pattern,
                    replacement,
                    op,
                    value,
                }
            }
            ScalarFunc::Round if args.len() <= 1 => {
                let precision = if args.len() == 1 {
                    match scalar_expr_literal_value(&args[0]) {
                        Some(Value::Integer(value)) => Some(
                            i32::try_from(value)
                                .map_err(|_| DbError::sql("unsupported CHECK expression"))?,
                        ),
                        _ => return Err(DbError::sql("unsupported CHECK expression")),
                    }
                } else {
                    None
                };
                CheckExpr::RoundCompare {
                    column,
                    precision,
                    op,
                    value,
                }
            }
            ScalarFunc::Ceil | ScalarFunc::Ceiling | ScalarFunc::Floor | ScalarFunc::Trunc
                if args.is_empty() =>
            {
                CheckExpr::RoundingCompare {
                    column,
                    func: match func {
                        ScalarFunc::Ceil => RoundingFunc::Ceil,
                        ScalarFunc::Ceiling => RoundingFunc::Ceiling,
                        ScalarFunc::Floor => RoundingFunc::Floor,
                        ScalarFunc::Trunc => RoundingFunc::Trunc,
                        _ => unreachable!("matched rounding function"),
                    },
                    op,
                    value,
                }
            }
            ScalarFunc::Concat if !args.is_empty() => {
                let suffix = Self::check_literal_args(args)?;
                CheckExpr::ConcatCompare {
                    column,
                    suffix,
                    op,
                    value,
                }
            }
            ScalarFunc::JsonValid if args.len() <= 1 => {
                let flags = if args.len() == 1 {
                    match scalar_expr_literal_value(&args[0]) {
                        Some(Value::Integer(value)) if (1..=15).contains(&value) => Some(value),
                        _ => return Err(DbError::sql("unsupported CHECK expression")),
                    }
                } else {
                    None
                };
                CheckExpr::JsonValidCompare {
                    column,
                    flags,
                    compare: Some((op, value)),
                }
            }
            ScalarFunc::Abs if args.is_empty() => CheckExpr::AbsCompare { column, op, value },
            ScalarFunc::Sqrt if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Sqrt,
                op,
                value,
            },
            ScalarFunc::Ln if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Ln,
                op,
                value,
            },
            ScalarFunc::Log10 if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Log10,
                op,
                value,
            },
            ScalarFunc::Log if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Log10,
                op,
                value,
            },
            ScalarFunc::Log2 if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Log2,
                op,
                value,
            },
            ScalarFunc::Exp if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Exp,
                op,
                value,
            },
            ScalarFunc::Sin if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Sin,
                op,
                value,
            },
            ScalarFunc::Cos if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Cos,
                op,
                value,
            },
            ScalarFunc::Tan if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Tan,
                op,
                value,
            },
            ScalarFunc::Sinh if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Sinh,
                op,
                value,
            },
            ScalarFunc::Cosh if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Cosh,
                op,
                value,
            },
            ScalarFunc::Tanh if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Tanh,
                op,
                value,
            },
            ScalarFunc::Atan if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Atan,
                op,
                value,
            },
            ScalarFunc::Acos if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Acos,
                op,
                value,
            },
            ScalarFunc::Asin if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Asin,
                op,
                value,
            },
            ScalarFunc::Acosh if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Acosh,
                op,
                value,
            },
            ScalarFunc::Asinh if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Asinh,
                op,
                value,
            },
            ScalarFunc::Atanh if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Atanh,
                op,
                value,
            },
            ScalarFunc::Degrees if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Degrees,
                op,
                value,
            },
            ScalarFunc::Radians if args.is_empty() => CheckExpr::UnaryMathCompare {
                column,
                func: UnaryMathFunc::Radians,
                op,
                value,
            },
            ScalarFunc::Power if args.len() == 1 => {
                let argument = scalar_expr_literal_value(&args[0])
                    .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
                CheckExpr::BinaryMathCompare {
                    column,
                    func: BinaryMathFunc::Power,
                    argument,
                    column_is_second: false,
                    op,
                    value,
                }
            }
            ScalarFunc::Atan2 if args.len() == 1 => {
                let argument = scalar_expr_literal_value(&args[0])
                    .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
                CheckExpr::BinaryMathCompare {
                    column,
                    func: BinaryMathFunc::Atan2,
                    argument,
                    column_is_second: false,
                    op,
                    value,
                }
            }
            ScalarFunc::Log if args.len() == 1 => {
                let argument = scalar_expr_literal_value(&args[0])
                    .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
                CheckExpr::BinaryMathCompare {
                    column,
                    func: BinaryMathFunc::Log,
                    argument,
                    column_is_second: false,
                    op,
                    value,
                }
            }
            ScalarFunc::TypeOf if args.is_empty() => CheckExpr::TypeOfCompare { column, op, value },
            ScalarFunc::Lower | ScalarFunc::Upper if args.is_empty() => {
                CheckExpr::CaseFoldCompare {
                    column,
                    upper: matches!(func, ScalarFunc::Upper),
                    op,
                    value,
                }
            }
            ScalarFunc::Trim | ScalarFunc::LTrim | ScalarFunc::RTrim if args.len() <= 1 => {
                let characters = if args.len() == 1 {
                    Some(
                        scalar_expr_literal_value(&args[0])
                            .map(|value| sqlite_literal_to_text_like(&value))
                            .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?,
                    )
                } else {
                    None
                };
                CheckExpr::TrimCompare {
                    column,
                    side: match func {
                        ScalarFunc::Trim => TrimSide::Both,
                        ScalarFunc::LTrim => TrimSide::Start,
                        ScalarFunc::RTrim => TrimSide::End,
                        _ => unreachable!("matched trim family function"),
                    },
                    characters,
                    op,
                    value,
                }
            }
            ScalarFunc::Coalesce if !args.is_empty() => {
                let fallbacks = args
                    .iter()
                    .map(|arg| {
                        scalar_expr_literal_value(arg)
                            .ok_or_else(|| DbError::sql("unsupported CHECK expression"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                CheckExpr::CoalesceCompare {
                    column,
                    fallbacks,
                    op,
                    value,
                }
            }
            ScalarFunc::IfNull if args.len() == 1 => {
                let fallback = scalar_expr_literal_value(&args[0])
                    .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
                CheckExpr::CoalesceCompare {
                    column,
                    fallbacks: vec![fallback],
                    op,
                    value,
                }
            }
            ScalarFunc::Instr if args.len() == 1 => {
                let needle = scalar_expr_literal_value(&args[0])
                    .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
                CheckExpr::InstrCompare {
                    column,
                    needle,
                    op,
                    value,
                }
            }
            ScalarFunc::Substr if matches!(args.len(), 1 | 2) => {
                let start = match scalar_expr_literal_value(&args[0]) {
                    Some(Value::Integer(value)) => value,
                    _ => return Err(DbError::sql("unsupported CHECK expression")),
                };
                let length = if args.len() == 2 {
                    match scalar_expr_literal_value(&args[1]) {
                        Some(Value::Integer(value)) => Some(value),
                        _ => return Err(DbError::sql("unsupported CHECK expression")),
                    }
                } else {
                    None
                };
                CheckExpr::SubstrCompare {
                    column,
                    start,
                    length,
                    op,
                    value,
                }
            }
            ScalarFunc::Mod if args.len() == 1 => {
                let divisor = scalar_expr_literal_value(&args[0])
                    .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
                CheckExpr::ModuloCompare {
                    column,
                    divisor,
                    op,
                    value,
                    function_form: true,
                }
            }
            _ => return Err(DbError::sql("unsupported CHECK expression")),
        })
    }

    fn check_concat_ws_expr(
        mut args: Vec<ScalarExpr>,
        op: CheckOp,
        value: Value,
    ) -> Result<CheckExpr> {
        if args.len() < 2 {
            return Err(DbError::sql("unsupported CHECK expression"));
        }
        let separator = scalar_expr_literal_value(&args.remove(0))
            .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
        let separator = if matches!(separator, Value::Null) {
            None
        } else {
            Some(sqlite_literal_to_text_like(&separator))
        };
        let ScalarExpr::Column(column) = args.remove(0) else {
            return Err(DbError::sql("unsupported CHECK expression"));
        };
        let suffix = Self::check_literal_args(args)?;
        Ok(CheckExpr::ConcatWsCompare {
            column,
            separator,
            suffix,
            op,
            value,
        })
    }

    fn check_literal_args(args: Vec<ScalarExpr>) -> Result<Vec<Value>> {
        args.iter()
            .map(|arg| {
                scalar_expr_literal_value(arg)
                    .ok_or_else(|| DbError::sql("unsupported CHECK expression"))
            })
            .collect()
    }

    fn check_replace_column_expr(
        mut args: Vec<ScalarExpr>,
        op: CheckOp,
        right_column: String,
    ) -> Result<CheckExpr> {
        if args.len() != 3 {
            return Err(DbError::sql("unsupported CHECK expression"));
        }
        let ScalarExpr::Column(column) = args.remove(0) else {
            return Err(DbError::sql("unsupported CHECK expression"));
        };
        if column != right_column {
            return Err(DbError::sql("unsupported CHECK expression"));
        }
        let pattern = scalar_expr_literal_value(&args[0])
            .map(|value| sqlite_literal_to_text_like(&value))
            .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
        let replacement = scalar_expr_literal_value(&args[1])
            .map(|value| sqlite_literal_to_text_like(&value))
            .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
        Ok(CheckExpr::ReplaceColumnCompare {
            column,
            pattern,
            replacement,
            op,
        })
    }

    fn check_min_max_column_expr(
        mut args: Vec<ScalarExpr>,
        min: bool,
        op: CheckOp,
        right_column: String,
    ) -> Result<CheckExpr> {
        if args.len() != 2 {
            return Err(DbError::sql("unsupported CHECK expression"));
        }
        let ScalarExpr::Column(column) = args.remove(0) else {
            return Err(DbError::sql("unsupported CHECK expression"));
        };
        if column != right_column {
            return Err(DbError::sql("unsupported CHECK expression"));
        }
        let limit = scalar_expr_literal_value(&args.remove(0))
            .ok_or_else(|| DbError::sql("unsupported CHECK expression"))?;
        Ok(CheckExpr::MinMaxColumnCompare {
            column,
            limit,
            min,
            op,
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

    fn check_op_from_reversed_compare_op(op: CompareOp) -> CheckOp {
        match op {
            CompareOp::Eq => CheckOp::Eq,
            CompareOp::Ne => CheckOp::Ne,
            CompareOp::Gt => CheckOp::Lt,
            CompareOp::Gte => CheckOp::Lte,
            CompareOp::Lt => CheckOp::Gt,
            CompareOp::Lte => CheckOp::Gte,
        }
    }

    fn parse_optional_column_type(&mut self) -> Option<(ColumnType, String)> {
        let start = self.index;
        let mut declared_type = vec![self.parse_optional_declared_type_word()?];

        while let Some(word) = self.parse_optional_declared_type_word() {
            declared_type.push(word);
        }

        if self.parse_optional_type_modifiers().is_err() {
            self.index = start;
            return None;
        }

        let declared_type_sql = join_declared_type_fragments(
            &self.tokens[start..self.index]
                .iter()
                .map(token_sql_fragment)
                .collect::<Vec<_>>(),
        );
        let declared_type = declared_type.join(" ");
        match sqlite_declared_type_affinity(&declared_type) {
            Some(column_type) => Some((column_type, declared_type_sql)),
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
        if declared_type.trim().eq_ignore_ascii_case("BOOLEAN") {
            return Some(ColumnType::Numeric);
        }
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
            if self.matches(&TokenKind::LParen) {
                let mut columns = vec![self.parse_simple_identifier()?];
                while self.matches(&TokenKind::Comma) {
                    columns.push(self.parse_simple_identifier()?);
                }
                self.expect_symbol(TokenKind::RParen)?;
                self.expect_symbol(TokenKind::Eq)?;
                let value = self.parse_scalar_expr()?;
                match value {
                    ScalarExpr::Tuple(values) => {
                        if columns.len() != values.len() {
                            return Err(DbError::sql(format!(
                                "{} columns assigned {} values",
                                columns.len(),
                                values.len()
                            )));
                        }
                        assignments.extend(
                            columns
                                .into_iter()
                                .zip(values.into_iter())
                                .map(|(column, value)| Assignment { column, value }),
                        );
                    }
                    ScalarExpr::Subquery { query } => {
                        if columns.len() != query.columns.len() {
                            return Err(DbError::sql(format!(
                                "{} columns assigned {} values",
                                columns.len(),
                                query.columns.len()
                            )));
                        }
                        assignments.extend(columns.into_iter().enumerate().map(
                            |(index, column)| {
                                let mut query = query.as_ref().clone();
                                query.columns = vec![query.columns[index].clone()];
                                Assignment {
                                    column,
                                    value: ScalarExpr::Subquery {
                                        query: Box::new(query),
                                    },
                                }
                            },
                        ));
                    }
                    _ => {
                        return Err(DbError::sql(
                            "row value assignment requires row value expression",
                        ));
                    }
                }
                if !self.matches(&TokenKind::Comma) {
                    break;
                }
                continue;
            }

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

    fn parse_order_by_items(&mut self, allow_result_positions: bool) -> Result<Vec<OrderBy>> {
        let mut items = Vec::new();
        loop {
            let expr = if allow_result_positions
                && let Some(position) = self.parse_order_by_position_if_bare()?
            {
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

    fn parse_order_by_position_if_bare(&mut self) -> Result<Option<usize>> {
        if let Some(position) = self.parse_signed_order_by_integer_position_if_bare()? {
            return Ok(Some(position));
        }

        let start = self.index;
        if self.matches(&TokenKind::LParen) {
            if let Some(position) = self.parse_signed_order_by_integer_position_if_bare()?
                && self.matches(&TokenKind::RParen)
                && self.is_order_by_term_boundary_at(self.index)
            {
                return Ok(Some(position));
            }
            self.index = start;
        }

        Ok(None)
    }

    fn parse_signed_order_by_integer_position_if_bare(&mut self) -> Result<Option<usize>> {
        if let TokenKind::Integer(value) = self.peek_kind()
            && self.is_order_by_term_boundary_at(self.index + 1)
        {
            let position = usize::try_from(*value).unwrap_or(0);
            self.advance();
            return Ok(Some(position));
        }

        let start = self.index;
        let negative = if self.matches(&TokenKind::Minus) {
            true
        } else if self.matches(&TokenKind::Plus) {
            false
        } else {
            return Ok(None);
        };
        if matches!(self.peek_kind(), TokenKind::Integer(_))
            && self.is_order_by_term_boundary_at(self.index + 1)
        {
            self.advance();
            return Ok(Some(if negative { 0 } else { 1 }));
        }
        self.index = start;
        Ok(None)
    }

    fn is_order_by_term_boundary_at(&self, index: usize) -> bool {
        matches!(
            self.tokens.get(index).map(|token| &token.kind),
            Some(
                TokenKind::Asc
                    | TokenKind::Desc
                    | TokenKind::Collate
                    | TokenKind::Nulls
                    | TokenKind::Comma
                    | TokenKind::Semicolon
                    | TokenKind::Eof
                    | TokenKind::RParen
            )
        )
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
        let expr = self.parse_scalar_expr()?;
        let value = constant_limit_value(&expr)
            .ok_or_else(|| DbError::sql("LIMIT/OFFSET expression must be a constant integer"))?;
        if !value.is_finite()
            || value.fract() != 0.0
            || value < i64::MIN as f64
            || value > i64::MAX as f64
        {
            return Err(DbError::sql("LIMIT/OFFSET literal is out of range"));
        }
        Ok(value as i64)
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
                    let value = value
                        .checked_neg()
                        .ok_or_else(|| DbError::sql("integer overflow"))?;
                    return Ok(Value::Integer(value));
                }
                TokenKind::Real(value) => {
                    let value = *value;
                    self.advance();
                    if value == 9_223_372_036_854_776_000.0 {
                        return Ok(Value::Integer(i64::MIN));
                    }
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

    fn parse_identifier(&mut self) -> Result<String> {
        let mut identifier = self.parse_simple_identifier()?;
        while self.matches(&TokenKind::Dot) {
            let segment = self.parse_simple_identifier()?;
            identifier.push('.');
            identifier.push_str(&segment);
        }
        Ok(identifier)
    }

    fn parse_schema_qualified_name(&mut self) -> Result<String> {
        self.parse_schema_qualified_name_with_schema()
            .map(|(name, _)| name)
    }

    fn parse_schema_qualified_name_with_schema(&mut self) -> Result<(String, Option<String>)> {
        let name = self.parse_simple_identifier()?;
        if !self.matches(&TokenKind::Dot) {
            return Ok((name, None));
        }

        if !name.eq_ignore_ascii_case("main") && !name.eq_ignore_ascii_case("temp") {
            return Err(DbError::sql(format!("unknown database {name}")));
        }
        let identifier = self.parse_simple_identifier()?;
        Ok((identifier, Some(name)))
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
            TokenKind::Savepoint => {
                self.advance();
                Ok("savepoint".to_string())
            }
            TokenKind::Release => {
                self.advance();
                Ok("release".to_string())
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
        if matches!(self.peek_kind(), TokenKind::Identifier(name) if name.eq_ignore_ascii_case("WINDOW"))
        {
            return Ok(None);
        }
        if is_identifier_token(self.peek_kind()) {
            return Ok(Some(self.parse_simple_identifier()?));
        }
        Ok(None)
    }

    fn parse_optional_insert_target_alias(&mut self) -> Result<Option<String>> {
        if self.matches(&TokenKind::As) {
            return Ok(Some(self.parse_simple_identifier()?));
        }
        if matches!(
            self.peek_kind(),
            TokenKind::Default
                | TokenKind::Select
                | TokenKind::Values
                | TokenKind::With
                | TokenKind::LParen
        ) {
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
            | TokenKind::Savepoint
            | TokenKind::Release
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

fn constant_limit_value(expr: &ScalarExpr) -> Option<f64> {
    match expr {
        ScalarExpr::Literal(Value::Integer(value)) => Some(*value as f64),
        ScalarExpr::Literal(Value::Real(value)) => Some(*value),
        ScalarExpr::UnaryPlus(expr) => constant_limit_value(expr),
        ScalarExpr::UnaryMinus(expr) => constant_limit_value(expr).map(|value| -value),
        ScalarExpr::Cast { expr, ty } => {
            let value = constant_limit_value(expr)?;
            match ty {
                ColumnType::Integer => Some(value.trunc()),
                ColumnType::Numeric | ColumnType::Real => Some(value),
                ColumnType::Any | ColumnType::Boolean | ColumnType::Blob | ColumnType::Text => None,
            }
        }
        ScalarExpr::Function {
            func: ScalarFunc::Abs,
            args,
        } if args.len() == 1 => constant_limit_value(&args[0]).map(f64::abs),
        ScalarExpr::Binary { left, op, right } => {
            let left = constant_limit_value(left)?;
            let right = constant_limit_value(right)?;
            match op {
                ScalarBinaryOp::Add => Some(left + right),
                ScalarBinaryOp::Subtract => Some(left - right),
                ScalarBinaryOp::Multiply => Some(left * right),
                ScalarBinaryOp::Divide => Some(left / right),
                ScalarBinaryOp::Modulo => Some(left % right),
                ScalarBinaryOp::BitAnd
                | ScalarBinaryOp::BitOr
                | ScalarBinaryOp::ShiftLeft
                | ScalarBinaryOp::ShiftRight
                | ScalarBinaryOp::Concat
                | ScalarBinaryOp::JsonExtract
                | ScalarBinaryOp::JsonExtractText => None,
            }
        }
        _ => None,
    }
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
                | TokenKind::Plus
                | TokenKind::Minus
                | TokenKind::Replace
                | TokenKind::Like
                | TokenKind::Glob
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

fn sqlite_binary_pattern_function(
    func: ScalarFunc,
    pattern: ScalarExpr,
    value: ScalarExpr,
    negated: bool,
) -> ScalarExpr {
    let expr = ScalarExpr::Function {
        func,
        args: vec![pattern, value],
    };
    if negated {
        ScalarExpr::Not(Box::new(expr))
    } else {
        expr
    }
}

fn scalar_expr_literal_value(expr: &ScalarExpr) -> Option<Value> {
    match expr {
        ScalarExpr::Literal(value) => Some(value.clone()),
        ScalarExpr::UnaryMinus(expr) => match expr.as_ref() {
            ScalarExpr::Literal(Value::Integer(value)) => value.checked_neg().map(Value::Integer),
            ScalarExpr::Literal(Value::Real(value)) if *value == 9_223_372_036_854_776_000.0 => {
                Some(Value::Integer(i64::MIN))
            }
            ScalarExpr::Literal(Value::Real(value)) => Some(Value::Real(-value)),
            _ => None,
        },
        ScalarExpr::Binary {
            left,
            op: ScalarBinaryOp::Concat,
            right,
        } => {
            let left = scalar_expr_literal_value(left)?;
            let right = scalar_expr_literal_value(right)?;
            Some(Value::Text(format!(
                "{}{}",
                sqlite_literal_to_text_like(&left),
                sqlite_literal_to_text_like(&right)
            )))
        }
        _ => None,
    }
}

fn negate_check_literal(value: Value) -> Result<Value> {
    match value {
        Value::Integer(value) => value
            .checked_neg()
            .map(Value::Integer)
            .ok_or_else(|| DbError::sql("unsupported CHECK expression")),
        Value::Real(value) => Ok(Value::Real(-value)),
        _ => Err(DbError::sql("unsupported CHECK expression")),
    }
}

fn sqlite_literal_to_text_like(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Boolean(value) => {
            if *value {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        Value::Integer(value) => value.to_string(),
        Value::Real(value) => value.to_string(),
        Value::Blob(value) => String::from_utf8_lossy(value).into_owned(),
        Value::Text(value) => value.clone(),
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
        ScalarExpr::UnaryPlus(expr)
        | ScalarExpr::UnaryMinus(expr)
        | ScalarExpr::BitNot(expr)
        | ScalarExpr::Not(expr)
        | ScalarExpr::Cast { expr, .. }
        | ScalarExpr::Collate { expr, .. }
        | ScalarExpr::IsBool { expr, .. } => scalar_expr_nested_aggregate_name(expr),
        ScalarExpr::Glob { expr, pattern, .. } => scalar_expr_nested_aggregate_name(expr)
            .or_else(|| scalar_expr_nested_aggregate_name(pattern)),
        ScalarExpr::Like {
            expr,
            pattern,
            escape,
            ..
        } => scalar_expr_nested_aggregate_name(expr).or_else(|| {
            scalar_expr_nested_aggregate_name(pattern).or_else(|| {
                escape
                    .as_deref()
                    .and_then(scalar_expr_nested_aggregate_name)
            })
        }),
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
        ScalarExpr::WindowFunction {
            func: _,
            args,
            partition_by,
            order_by,
            ..
        } => args
            .iter()
            .find_map(scalar_expr_nested_aggregate_name)
            .or_else(|| {
                partition_by
                    .iter()
                    .find_map(scalar_expr_nested_aggregate_name)
            })
            .or_else(|| order_by_nested_aggregate_name(order_by)),
        ScalarExpr::Literal(_) | ScalarExpr::Column(_) => None,
    }
}

fn aggregate_function_name(func: AggregateFunc) -> &'static str {
    match func {
        AggregateFunc::Count => "COUNT",
        AggregateFunc::Sum => "SUM",
        AggregateFunc::DecimalSum => "DECIMAL_SUM",
        AggregateFunc::Avg => "AVG",
        AggregateFunc::Total => "TOTAL",
        AggregateFunc::Median => "MEDIAN",
        AggregateFunc::Percentile => "PERCENTILE",
        AggregateFunc::PercentileCont => "PERCENTILE_CONT",
        AggregateFunc::PercentileDisc => "PERCENTILE_DISC",
        AggregateFunc::GroupConcat => "GROUP_CONCAT",
        AggregateFunc::JsonGroupArray => "JSON_GROUP_ARRAY",
        AggregateFunc::JsonbGroupArray => "JSONB_GROUP_ARRAY",
        AggregateFunc::JsonGroupObject => "JSON_GROUP_OBJECT",
        AggregateFunc::JsonbGroupObject => "JSONB_GROUP_OBJECT",
        AggregateFunc::Min => "MIN",
        AggregateFunc::Max => "MAX",
    }
}

fn is_aggregate_function_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT"
            | "SUM"
            | "DECIMAL_SUM"
            | "AVG"
            | "TOTAL"
            | "MEDIAN"
            | "PERCENTILE"
            | "PERCENTILE_CONT"
            | "PERCENTILE_DISC"
            | "GROUP_CONCAT"
            | "STRING_AGG"
            | "JSON_GROUP_ARRAY"
            | "JSONB_GROUP_ARRAY"
            | "JSON_GROUP_OBJECT"
            | "JSONB_GROUP_OBJECT"
            | "MIN"
            | "MAX"
    )
}

fn sqlite_declared_type_affinity(declared_type: &str) -> Option<ColumnType> {
    let normalized = declared_type.trim().to_ascii_uppercase();

    if normalized == "ANY" {
        return Some(ColumnType::Any);
    }
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
        "REAL" | "DOUBLE" | "DOUBLE PRECISION" | "FLOAT"
    ) {
        return Some(ColumnType::Real);
    }
    if matches!(normalized.as_str(), "NONE" | "NUM" | "NUMERIC" | "DECIMAL")
        || normalized.starts_with("NUMERIC(")
        || normalized.starts_with("DECIMAL(")
    {
        return Some(ColumnType::Numeric);
    }

    Some(ColumnType::Numeric)
}

fn validate_strict_table_declared_types(table: &str, columns: &[ColumnDef]) -> Result<()> {
    for column in columns {
        let declared_type = column.pragma_declared_type();
        let normalized = declared_type.trim().to_ascii_uppercase();
        if !matches!(
            normalized.as_str(),
            "INT" | "INTEGER" | "REAL" | "TEXT" | "BLOB" | "ANY"
        ) {
            return Err(DbError::sql(format!(
                "unknown datatype for {table}.{}: \"{}\"",
                column.name, declared_type
            )));
        }
    }
    Ok(())
}

fn from_item_qualifier(from: &FromItem) -> Option<String> {
    match from {
        FromItem::Table { name, alias, .. }
        | FromItem::TableIndexed { name, alias, .. }
        | FromItem::TableNotIndexed { name, alias, .. } => {
            Some(alias.clone().unwrap_or_else(|| name.clone()))
        }
        FromItem::Subquery { alias, .. } => (!alias.is_empty()).then(|| alias.clone()),
        FromItem::Values { alias, .. } => alias.clone(),
        FromItem::PragmaTableFunction { name, alias, .. } => {
            Some(alias.clone().unwrap_or_else(|| name.clone()))
        }
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
        TokenKind::Regexp => "REGEXP".to_string(),
        TokenKind::Match => "MATCH".to_string(),
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
        TokenKind::Savepoint => "SAVEPOINT".to_string(),
        TokenKind::Release => "RELEASE".to_string(),
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
        TokenKind::Regexp => "REGEXP".to_string(),
        TokenKind::Match => "MATCH".to_string(),
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
        TokenKind::Savepoint => "SAVEPOINT".to_string(),
        TokenKind::Release => "RELEASE".to_string(),
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

fn join_declared_type_fragments(fragments: &[String]) -> String {
    let mut sql = String::new();
    for fragment in fragments {
        let needs_space_before = !sql.is_empty()
            && !matches!(fragment.as_str(), "(" | ")" | "," | ";" | ".")
            && !matches!(sql.chars().last(), Some('(' | ',' | '.' | ' '));
        if needs_space_before {
            sql.push(' ');
        }
        sql.push_str(fragment);
    }
    sql
}

fn sqlite_pragma_boolean_string(value: &str) -> bool {
    if value.eq_ignore_ascii_case("on")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("true")
    {
        return true;
    }
    if value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("false")
    {
        return false;
    }
    value.parse::<i64>().is_ok_and(|number| number != 0)
}

fn sqlite_pragma_string_integer_prefix(value: &str) -> i64 {
    let trimmed = value.trim_start();
    let mut chars = trimmed.char_indices();
    let mut end = 0;

    if let Some((index, '+' | '-')) = chars.next() {
        end = index + 1;
    } else {
        chars = trimmed.char_indices();
    }

    let mut saw_digit = false;
    for (index, ch) in chars {
        if ch.is_ascii_digit() {
            saw_digit = true;
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }

    if !saw_digit {
        return 0;
    }

    trimmed[..end].parse::<i64>().unwrap_or_else(|_| {
        if trimmed.starts_with('-') {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}

fn is_pragma_table_function_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "pragma_table_info"
            | "pragma_table_xinfo"
            | "pragma_index_list"
            | "pragma_index_info"
            | "pragma_index_xinfo"
            | "pragma_foreign_key_list"
            | "pragma_foreign_key_check"
            | "pragma_table_list"
            | "pragma_database_list"
            | "pragma_pragma_list"
            | "pragma_function_list"
            | "pragma_compile_options"
            | "pragma_collation_list"
            | "pragma_module_list"
            | "pragma_optimize"
            | "pragma_quick_check"
            | "pragma_encoding"
            | "pragma_integrity_check"
            | "pragma_page_size"
            | "pragma_page_count"
            | "pragma_max_page_count"
            | "pragma_freelist_count"
            | "pragma_user_version"
            | "pragma_application_id"
            | "pragma_schema_version"
            | "pragma_data_version"
            | "pragma_journal_mode"
            | "pragma_synchronous"
            | "pragma_cache_size"
            | "pragma_cache_spill"
            | "pragma_temp_store"
            | "pragma_locking_mode"
            | "pragma_auto_vacuum"
            | "pragma_busy_timeout"
            | "pragma_analysis_limit"
            | "pragma_journal_size_limit"
            | "pragma_foreign_keys"
            | "pragma_defer_foreign_keys"
            | "pragma_read_uncommitted"
            | "pragma_query_only"
            | "pragma_count_changes"
            | "pragma_recursive_triggers"
            | "pragma_trusted_schema"
            | "pragma_ignore_check_constraints"
            | "pragma_automatic_index"
            | "pragma_cell_size_check"
            | "pragma_secure_delete"
            | "pragma_threads"
            | "pragma_soft_heap_limit"
            | "pragma_hard_heap_limit"
            | "pragma_full_column_names"
            | "pragma_short_column_names"
            | "pragma_fullfsync"
            | "pragma_checkpoint_fullfsync"
            | "pragma_empty_result_callbacks"
            | "pragma_reverse_unordered_selects"
    )
}

fn pragma_table_function_allows_no_argument(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "pragma_table_list"
            | "pragma_foreign_key_check"
            | "pragma_database_list"
            | "pragma_pragma_list"
            | "pragma_function_list"
            | "pragma_compile_options"
            | "pragma_collation_list"
            | "pragma_module_list"
            | "pragma_optimize"
            | "pragma_quick_check"
            | "pragma_encoding"
            | "pragma_integrity_check"
            | "pragma_page_size"
            | "pragma_page_count"
            | "pragma_max_page_count"
            | "pragma_freelist_count"
            | "pragma_user_version"
            | "pragma_application_id"
            | "pragma_schema_version"
            | "pragma_data_version"
            | "pragma_journal_mode"
            | "pragma_synchronous"
            | "pragma_cache_size"
            | "pragma_cache_spill"
            | "pragma_temp_store"
            | "pragma_locking_mode"
            | "pragma_auto_vacuum"
            | "pragma_busy_timeout"
            | "pragma_analysis_limit"
            | "pragma_journal_size_limit"
            | "pragma_foreign_keys"
            | "pragma_defer_foreign_keys"
            | "pragma_read_uncommitted"
            | "pragma_query_only"
            | "pragma_count_changes"
            | "pragma_recursive_triggers"
            | "pragma_trusted_schema"
            | "pragma_ignore_check_constraints"
            | "pragma_automatic_index"
            | "pragma_cell_size_check"
            | "pragma_secure_delete"
            | "pragma_threads"
            | "pragma_soft_heap_limit"
            | "pragma_hard_heap_limit"
            | "pragma_full_column_names"
            | "pragma_short_column_names"
            | "pragma_fullfsync"
            | "pragma_checkpoint_fullfsync"
            | "pragma_empty_result_callbacks"
            | "pragma_reverse_unordered_selects"
    )
}

fn validate_likelihood_probability_arg(args: &[ScalarExpr]) -> Result<()> {
    if args.len() != 2 {
        return Ok(());
    }
    let valid = matches!(
        args,
        [
            _,
            ScalarExpr::Literal(Value::Real(value))
        ] if (0.0..=1.0).contains(value)
    );
    if valid {
        Ok(())
    } else {
        Err(DbError::sql(
            "second argument to likelihood() must be a constant between 0.0 and 1.0",
        ))
    }
}
