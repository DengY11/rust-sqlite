use crate::common::error::{DbError, Result};
use crate::common::types::{ColumnDef, ColumnType, Value};
use crate::sql::ast::{
    AggregateArg, AggregateFunc, Assignment, CompareOp, Expr, JoinClause, JoinKind, OrderBy,
    OrderByExpr, SelectItem, SelectStatement, Statement,
};
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
            TokenKind::Drop => self.parse_drop(),
            TokenKind::Insert => self.parse_insert(),
            TokenKind::Select => Ok(Statement::Select(self.parse_select_statement()?)),
            TokenKind::Delete => self.parse_delete(),
            TokenKind::Update => self.parse_update(),
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
        let columns = self.parse_parenthesized_identifier_list()?;

        Ok(Statement::CreateIndex {
            name,
            table,
            columns,
        })
    }

    fn parse_drop(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Drop)?;
        match self.peek_kind() {
            TokenKind::Table => {
                self.advance();
                Ok(Statement::DropTable {
                    name: self.parse_simple_identifier()?,
                })
            }
            TokenKind::Index => {
                self.advance();
                Ok(Statement::DropIndex {
                    name: self.parse_simple_identifier()?,
                })
            }
            token => {
                Err(self.error_expected(&format!("TABLE or INDEX, found {}", display_token(token))))
            }
        }
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Insert)?;
        self.expect_keyword(TokenKind::Into)?;
        let table = self.parse_simple_identifier()?;
        let columns = if matches!(self.peek_kind(), TokenKind::LParen) {
            Some(self.parse_parenthesized_identifier_list()?)
        } else {
            None
        };
        self.expect_keyword(TokenKind::Values)?;
        let values = self.parse_parenthesized_literals()?;

        Ok(Statement::Insert {
            table,
            columns,
            values,
        })
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

        Ok(Statement::Delete {
            table,
            table_alias,
            filter,
        })
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

        Ok(Statement::Update {
            table,
            table_alias,
            assignments,
            filter,
        })
    }

    fn parse_select_statement(&mut self) -> Result<SelectStatement> {
        self.expect_keyword(TokenKind::Select)?;
        let distinct = self.matches(&TokenKind::Distinct);
        let columns = self.parse_select_list()?;
        self.expect_keyword(TokenKind::From)?;
        let table = self.parse_simple_identifier()?;
        let table_alias = self.parse_optional_table_alias()?;
        let joins = self.parse_join_clauses()?;
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
        let order_by = if self.matches(&TokenKind::Order) {
            self.expect_keyword(TokenKind::By)?;
            self.parse_order_by_items()?
        } else {
            Vec::new()
        };
        let limit = if self.matches(&TokenKind::Limit) {
            Some(self.parse_limit_value()?)
        } else {
            None
        };

        Ok(SelectStatement {
            distinct,
            columns,
            table,
            table_alias,
            joins,
            filter,
            group_by,
            having,
            order_by,
            limit,
        })
    }

    fn parse_select_list(&mut self) -> Result<Vec<SelectItem>> {
        if self.matches(&TokenKind::Star) {
            return Ok(vec![SelectItem::Wildcard]);
        }

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

        let name = self.parse_simple_identifier()?;
        let mut item = if matches!(self.peek_kind(), TokenKind::LParen) {
            self.parse_aggregate_item(name)?
        } else {
            let mut name = name;
            while self.matches(&TokenKind::Dot) {
                let segment = self.parse_simple_identifier()?;
                name.push('.');
                name.push_str(&segment);
            }
            SelectItem::Column(name)
        };
        let alias = if self.matches(&TokenKind::As)
            || matches!(self.peek_kind(), TokenKind::Identifier(_))
        {
            Some(self.parse_simple_identifier()?)
        } else {
            None
        };

        if let Some(alias) = alias {
            item = match item {
                SelectItem::Column(name) => SelectItem::AliasedColumn { name, alias },
                SelectItem::AliasedColumn { name, .. } => SelectItem::AliasedColumn { name, alias },
                SelectItem::Aggregate { func, arg, .. } => SelectItem::Aggregate {
                    func,
                    arg,
                    alias: Some(alias),
                },
                SelectItem::Wildcard => SelectItem::Wildcard,
            };
        }

        Ok(item)
    }

    fn parse_aggregate_item(&mut self, function_name: String) -> Result<SelectItem> {
        self.expect_symbol(TokenKind::LParen)?;
        let func = match function_name.to_ascii_uppercase().as_str() {
            "COUNT" => AggregateFunc::Count,
            "SUM" => AggregateFunc::Sum,
            "AVG" => AggregateFunc::Avg,
            "MIN" => AggregateFunc::Min,
            "MAX" => AggregateFunc::Max,
            _ => {
                return Err(DbError::sql(format!(
                    "unsupported select function: {function_name}"
                )));
            }
        };
        let arg = if self.matches(&TokenKind::Star) {
            AggregateArg::Wildcard
        } else {
            let distinct = self.matches(&TokenKind::Distinct);
            AggregateArg::Column {
                name: self.parse_identifier()?,
                distinct,
            }
        };
        self.expect_symbol(TokenKind::RParen)?;
        Ok(SelectItem::Aggregate {
            func,
            arg,
            alias: None,
        })
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
            return Ok(Expr::Not(Box::new(self.parse_not_expr()?)));
        }

        self.parse_primary_expr()
    }

    fn parse_primary_expr(&mut self) -> Result<Expr> {
        if self.matches(&TokenKind::LParen) {
            let expr = self.parse_or_expr()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(expr);
        }

        self.parse_comparison_expr()
    }

    fn parse_comparison_expr(&mut self) -> Result<Expr> {
        let column = self.parse_identifier()?;
        if self.matches(&TokenKind::Is) {
            let negated = self.matches(&TokenKind::Not);
            if !self.matches(&TokenKind::Null) {
                return Err(self.error_expected(&format!(
                    "NULL or NOT NULL, found {}",
                    display_token(self.peek_kind())
                )));
            }
            return Ok(Expr::IsNull { column, negated });
        }
        if self.matches(&TokenKind::Not) {
            if self.matches(&TokenKind::Like) {
                return Ok(Expr::Like {
                    column,
                    pattern: self.parse_string_literal()?,
                    negated: true,
                });
            }
            if self.matches(&TokenKind::Between) {
                let low = self.parse_literal()?;
                self.expect_keyword(TokenKind::And)?;
                let high = self.parse_literal()?;
                return Ok(Expr::Between {
                    column,
                    low,
                    high,
                    negated: true,
                });
            }
            self.expect_keyword(TokenKind::In)?;
            let query = self.parse_subquery()?;
            return Ok(Expr::InSubquery {
                column,
                query: Box::new(query),
                negated: true,
            });
        }
        if self.matches(&TokenKind::In) {
            let query = self.parse_subquery()?;
            return Ok(Expr::InSubquery {
                column,
                query: Box::new(query),
                negated: false,
            });
        }
        if self.matches(&TokenKind::Like) {
            return Ok(Expr::Like {
                column,
                pattern: self.parse_string_literal()?,
                negated: false,
            });
        }
        if self.matches(&TokenKind::Between) {
            let low = self.parse_literal()?;
            self.expect_keyword(TokenKind::And)?;
            let high = self.parse_literal()?;
            return Ok(Expr::Between {
                column,
                low,
                high,
                negated: false,
            });
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
                return Err(self.error_expected(&format!(
                    "comparison operator (=, !=, <>, >, >=, <, <=) or IS NULL, found {}",
                    display_token(token)
                )));
            }
        };
        if self.is_subquery_start() {
            let query = self.parse_subquery()?;
            return Ok(Expr::CompareSubquery {
                column,
                op,
                query: Box::new(query),
            });
        }
        if matches!(self.peek_kind(), TokenKind::Identifier(_)) {
            return Ok(Expr::CompareColumns {
                left: column,
                op,
                right: self.parse_identifier()?,
            });
        }
        let value = self.parse_literal()?;

        Ok(Expr::Compare { column, op, value })
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = self.parse_simple_identifier()?;
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

    fn parse_parenthesized_identifier_list(&mut self) -> Result<Vec<String>> {
        self.expect_symbol(TokenKind::LParen)?;
        let mut values = vec![self.parse_simple_identifier()?];
        while self.matches(&TokenKind::Comma) {
            values.push(self.parse_simple_identifier()?);
        }
        self.expect_symbol(TokenKind::RParen)?;
        Ok(values)
    }

    fn parse_assignments(&mut self) -> Result<Vec<Assignment>> {
        let mut assignments = Vec::new();
        loop {
            let column = self.parse_simple_identifier()?;
            self.expect_symbol(TokenKind::Eq)?;
            let value = self.parse_literal()?;
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
            let expr = match self.peek_kind() {
                TokenKind::Integer(value) if *value > 0 => {
                    let position = usize::try_from(*value)
                        .map_err(|_| DbError::sql("ORDER BY position is too large"))?;
                    self.advance();
                    OrderByExpr::Position(position)
                }
                _ => OrderByExpr::Column(self.parse_identifier()?),
            };
            let descending = if self.matches(&TokenKind::Desc) {
                true
            } else {
                self.matches(&TokenKind::Asc);
                false
            };
            items.push(OrderBy { expr, descending });
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn parse_group_by_items(&mut self) -> Result<Vec<String>> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_identifier()?);
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn parse_join_clauses(&mut self) -> Result<Vec<JoinClause>> {
        let mut joins = Vec::new();
        loop {
            let kind = if self.matches(&TokenKind::Inner) {
                self.expect_keyword(TokenKind::Join)?;
                JoinKind::Inner
            } else if self.matches(&TokenKind::Left) {
                let _ = self.matches(&TokenKind::Outer);
                self.expect_keyword(TokenKind::Join)?;
                JoinKind::Left
            } else if self.matches(&TokenKind::Join) {
                JoinKind::Inner
            } else {
                break;
            };

            let table = self.parse_simple_identifier()?;
            let table_alias = self.parse_optional_table_alias()?;
            self.expect_keyword(TokenKind::On)?;
            joins.push(JoinClause {
                kind,
                table,
                table_alias,
                on: self.parse_where_expr()?,
            });
        }
        Ok(joins)
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

    fn parse_limit_value(&mut self) -> Result<usize> {
        match self.peek_kind() {
            TokenKind::Integer(value) if *value >= 0 => {
                let value = *value as usize;
                self.advance();
                Ok(value)
            }
            token => Err(self.error_expected(&format!(
                "non-negative integer literal, found {}",
                display_token(token)
            ))),
        }
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
        if self.matches(&TokenKind::Minus) {
            match self.peek_kind() {
                TokenKind::Integer(value) => {
                    let value = *value;
                    self.advance();
                    return Ok(Value::Integer(-value));
                }
                _ => return Err(self.error_expected("integer literal after -")),
            }
        }

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
            token => {
                Err(self.error_expected(&format!("identifier, found {}", display_token(token))))
            }
        }
    }

    fn parse_optional_table_alias(&mut self) -> Result<Option<String>> {
        if self.matches(&TokenKind::As) {
            return Ok(Some(self.parse_simple_identifier()?));
        }
        if matches!(self.peek_kind(), TokenKind::Identifier(_)) {
            return Ok(Some(self.parse_simple_identifier()?));
        }
        Ok(None)
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
        TokenKind::Drop => "DROP".to_string(),
        TokenKind::Table => "TABLE".to_string(),
        TokenKind::Index => "INDEX".to_string(),
        TokenKind::On => "ON".to_string(),
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
        TokenKind::As => "AS".to_string(),
        TokenKind::Inner => "INNER".to_string(),
        TokenKind::Left => "LEFT".to_string(),
        TokenKind::Outer => "OUTER".to_string(),
        TokenKind::Join => "JOIN".to_string(),
        TokenKind::Having => "HAVING".to_string(),
        TokenKind::Distinct => "DISTINCT".to_string(),
        TokenKind::Asc => "ASC".to_string(),
        TokenKind::Desc => "DESC".to_string(),
        TokenKind::And => "AND".to_string(),
        TokenKind::Or => "OR".to_string(),
        TokenKind::Like => "LIKE".to_string(),
        TokenKind::Between => "BETWEEN".to_string(),
        TokenKind::Begin => "BEGIN".to_string(),
        TokenKind::Commit => "COMMIT".to_string(),
        TokenKind::Rollback => "ROLLBACK".to_string(),
        TokenKind::Not => "NOT".to_string(),
        TokenKind::In => "IN".to_string(),
        TokenKind::Is => "IS".to_string(),
        TokenKind::Exists => "EXISTS".to_string(),
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
        TokenKind::Minus => "-".to_string(),
        TokenKind::Eof => "end of input".to_string(),
    }
}
