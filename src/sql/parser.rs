use crate::common::error::{DbError, Result};
use crate::common::types::{ColumnDef, ColumnType, Value};
use crate::sql::ast::{Expr, SelectItem, Statement};
use crate::sql::lexer::{Token, TokenKind, lex};

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
            TokenKind::Insert => self.parse_insert(),
            TokenKind::Select => self.parse_select(),
            TokenKind::Begin => {
                self.advance();
                Ok(Statement::Begin)
            }
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

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Create)?;
        match self.peek_kind() {
            TokenKind::Table => self.parse_create_table(),
            TokenKind::Index => self.parse_create_index(),
            token => {
                Err(self.error_expected(&format!("TABLE or INDEX, found {}", display_token(token))))
            }
        }
    }

    fn parse_create_table(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Table)?;
        let name = self.parse_identifier()?;
        self.expect_symbol(TokenKind::LParen)?;

        let mut columns = Vec::new();
        loop {
            columns.push(self.parse_column_def()?);
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }

        self.expect_symbol(TokenKind::RParen)?;
        Ok(Statement::CreateTable { name, columns })
    }

    fn parse_create_index(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Index)?;
        let name = self.parse_identifier()?;
        self.expect_keyword(TokenKind::On)?;
        let table = self.parse_identifier()?;
        let column = self.parse_parenthesized_identifier()?;

        Ok(Statement::CreateIndex {
            name,
            table,
            column,
        })
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Insert)?;
        self.expect_keyword(TokenKind::Into)?;
        let table = self.parse_identifier()?;
        self.expect_keyword(TokenKind::Values)?;
        let values = self.parse_parenthesized_literals()?;

        Ok(Statement::Insert { table, values })
    }

    fn parse_select(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Select)?;
        let columns = self.parse_select_list()?;
        self.expect_keyword(TokenKind::From)?;
        let table = self.parse_identifier()?;
        let filter = if self.matches(&TokenKind::Where) {
            Some(self.parse_where_expr()?)
        } else {
            None
        };

        Ok(Statement::Select {
            columns,
            table,
            filter,
        })
    }

    fn parse_select_list(&mut self) -> Result<Vec<SelectItem>> {
        if self.matches(&TokenKind::Star) {
            return Ok(vec![SelectItem::Wildcard]);
        }

        let mut columns = Vec::new();
        loop {
            columns.push(SelectItem::Column(self.parse_identifier()?));
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        Ok(columns)
    }

    fn parse_where_expr(&mut self) -> Result<Expr> {
        let column = self.parse_identifier()?;
        let op = match self.peek_kind() {
            TokenKind::Eq => {
                self.advance();
                Expr::Eq
            }
            TokenKind::Gt => {
                self.advance();
                Expr::Gt
            }
            TokenKind::Lt => {
                self.advance();
                Expr::Lt
            }
            token => {
                return Err(self.error_expected(&format!(
                    "comparison operator (=, >, <), found {}",
                    display_token(token)
                )));
            }
        };
        let value = self.parse_literal()?;

        Ok(op(column, value))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = self.parse_identifier()?;
        let column_type = self.parse_column_type()?;
        let mut column = ColumnDef::new(name, column_type);

        loop {
            if self.matches(&TokenKind::Primary) {
                self.expect_keyword(TokenKind::Key)?;
                column.primary_key = true;
                column.nullable = false;
                continue;
            }

            if self.matches(&TokenKind::Not) {
                self.expect_keyword(TokenKind::Null)?;
                column.nullable = false;
                continue;
            }

            break;
        }

        Ok(column)
    }

    fn parse_column_type(&mut self) -> Result<ColumnType> {
        match self.peek_kind() {
            TokenKind::IntegerType => {
                self.advance();
                Ok(ColumnType::Integer)
            }
            TokenKind::TextType => {
                self.advance();
                Ok(ColumnType::Text)
            }
            TokenKind::BooleanType => {
                self.advance();
                Ok(ColumnType::Boolean)
            }
            token => {
                Err(self.error_expected(&format!("column type, found {}", display_token(token))))
            }
        }
    }

    fn parse_parenthesized_identifier(&mut self) -> Result<String> {
        self.expect_symbol(TokenKind::LParen)?;
        let value = self.parse_identifier()?;
        self.expect_symbol(TokenKind::RParen)?;
        Ok(value)
    }

    fn parse_parenthesized_literals(&mut self) -> Result<Vec<Value>> {
        self.expect_symbol(TokenKind::LParen)?;
        let mut values = Vec::new();
        loop {
            values.push(self.parse_literal()?);
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_symbol(TokenKind::RParen)?;
        Ok(values)
    }

    fn parse_literal(&mut self) -> Result<Value> {
        match self.peek_kind() {
            TokenKind::Integer(value) => {
                let value = *value;
                self.advance();
                Ok(Value::Integer(value))
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

    fn parse_identifier(&mut self) -> Result<String> {
        match self.peek_kind() {
            TokenKind::Identifier(name) => {
                let name = name.clone();
                self.advance();
                Ok(name)
            }
            token => {
                Err(self.error_expected(&format!("identifier, found {}", display_token(token))))
            }
        }
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

fn display_token(token: &TokenKind) -> String {
    match token {
        TokenKind::Create => "CREATE".to_string(),
        TokenKind::Table => "TABLE".to_string(),
        TokenKind::Index => "INDEX".to_string(),
        TokenKind::On => "ON".to_string(),
        TokenKind::Insert => "INSERT".to_string(),
        TokenKind::Into => "INTO".to_string(),
        TokenKind::Values => "VALUES".to_string(),
        TokenKind::Select => "SELECT".to_string(),
        TokenKind::From => "FROM".to_string(),
        TokenKind::Where => "WHERE".to_string(),
        TokenKind::Begin => "BEGIN".to_string(),
        TokenKind::Commit => "COMMIT".to_string(),
        TokenKind::Rollback => "ROLLBACK".to_string(),
        TokenKind::Not => "NOT".to_string(),
        TokenKind::Primary => "PRIMARY".to_string(),
        TokenKind::Key => "KEY".to_string(),
        TokenKind::IntegerType => "INTEGER".to_string(),
        TokenKind::TextType => "TEXT".to_string(),
        TokenKind::BooleanType => "BOOLEAN".to_string(),
        TokenKind::True => "TRUE".to_string(),
        TokenKind::False => "FALSE".to_string(),
        TokenKind::Null => "NULL".to_string(),
        TokenKind::Identifier(name) => format!("identifier '{name}'"),
        TokenKind::Integer(value) => format!("integer literal {value}"),
        TokenKind::String(value) => format!("string literal '{value}'"),
        TokenKind::Star => "*".to_string(),
        TokenKind::Comma => ",".to_string(),
        TokenKind::Semicolon => ";".to_string(),
        TokenKind::LParen => "(".to_string(),
        TokenKind::RParen => ")".to_string(),
        TokenKind::Eq => "=".to_string(),
        TokenKind::Gt => ">".to_string(),
        TokenKind::Lt => "<".to_string(),
        TokenKind::Eof => "end of input".to_string(),
    }
}
