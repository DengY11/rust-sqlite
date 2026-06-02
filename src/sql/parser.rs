use crate::common::error::{DbError, Result};
use crate::common::types::{
    CheckConstraint, CheckExpr, CheckOp, ColumnDef, ColumnType, ForeignKey, Value,
};
use crate::sql::ast::{
    AggregateArg, AggregateFunc, AlterTableAction, Assignment, CompareOp, Expr, JoinClause,
    JoinKind, NullOrder, OrderBy, OrderByExpr, ScalarBinaryOp, ScalarExpr, ScalarFunc, SelectItem,
    SelectStatement, Statement, TableConstraint,
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
            TokenKind::Alter => self.parse_alter(),
            TokenKind::Drop => self.parse_drop(),
            TokenKind::Insert => self.parse_insert(),
            TokenKind::Select => Ok(Statement::Select(self.parse_select_statement()?)),
            TokenKind::Explain => self.parse_explain(),
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

        self.expect_keyword(TokenKind::To)?;
        let new_name = self.parse_simple_identifier()?;
        Ok(Statement::AlterTable {
            table,
            action: AlterTableAction::RenameTable { new_name },
        })
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

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect_keyword(TokenKind::Create)?;
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
        let name = self.parse_identifier()?;
        self.expect_symbol(TokenKind::LParen)?;

        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            match self.peek_kind() {
                TokenKind::Check | TokenKind::Foreign | TokenKind::Constraint => {
                    constraints.push(self.parse_table_constraint(&name)?);
                }
                _ => columns.push(self.parse_column_def(Some(&name))?),
            }
            if !self.matches(&TokenKind::Comma) {
                break;
            }
        }

        self.expect_symbol(TokenKind::RParen)?;
        Ok(Statement::CreateTable {
            name,
            columns,
            constraints,
        })
    }

    fn parse_create_index(&mut self, unique: bool) -> Result<Statement> {
        self.expect_keyword(TokenKind::Index)?;
        let name = self.parse_identifier()?;
        self.expect_keyword(TokenKind::On)?;
        let table = self.parse_identifier()?;
        let columns = self.parse_parenthesized_identifier_list()?;

        Ok(Statement::CreateIndex {
            name,
            table,
            columns,
            unique,
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

        let mut item = if let TokenKind::Identifier(name) = self.peek_kind()
            && matches!(
                self.tokens.get(self.index + 1).map(|token| &token.kind),
                Some(TokenKind::LParen)
            )
            && is_aggregate_function_name(name)
        {
            let name = name.clone();
            self.advance();
            self.parse_aggregate_item(name)?
        } else {
            let expr = self.parse_scalar_expr()?;
            match expr {
                ScalarExpr::Column(name) => SelectItem::Column(name),
                expr => SelectItem::Expr { expr, alias: None },
            }
        };
        let alias = if self.matches(&TokenKind::As) || is_identifier_token(self.peek_kind()) {
            Some(self.parse_simple_identifier()?)
        } else {
            None
        };

        if let Some(alias) = alias {
            item = match item {
                SelectItem::Column(name) => SelectItem::AliasedColumn { name, alias },
                SelectItem::AliasedColumn { name, .. } => SelectItem::AliasedColumn { name, alias },
                SelectItem::Expr { expr, .. } => SelectItem::Expr {
                    expr,
                    alias: Some(alias),
                },
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

    fn parse_scalar_expr(&mut self) -> Result<ScalarExpr> {
        self.parse_concat_expr()
    }

    fn parse_concat_expr(&mut self) -> Result<ScalarExpr> {
        let mut expr = self.parse_additive_expr()?;
        while self.matches(&TokenKind::PipePipe) {
            expr = ScalarExpr::Binary {
                left: Box::new(expr),
                op: ScalarBinaryOp::Concat,
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
        if self.matches(&TokenKind::Minus) {
            if !is_scalar_expr_start(self.peek_kind()) {
                return Err(self.error_expected("integer literal after -"));
            }
            return Ok(ScalarExpr::UnaryMinus(Box::new(
                self.parse_unary_scalar_expr()?,
            )));
        }
        self.parse_primary_scalar_expr()
    }

    fn parse_primary_scalar_expr(&mut self) -> Result<ScalarExpr> {
        if self.matches(&TokenKind::LParen) {
            let expr = self.parse_scalar_expr()?;
            self.expect_symbol(TokenKind::RParen)?;
            return Ok(expr);
        }
        match self.peek_kind() {
            token if is_identifier_token(token) => {
                let name = self.parse_identifier()?;
                if self.matches(&TokenKind::LParen) {
                    self.parse_scalar_function(name)
                } else {
                    Ok(ScalarExpr::Column(name))
                }
            }
            TokenKind::Integer(_)
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

    fn parse_scalar_function(&mut self, function_name: String) -> Result<ScalarExpr> {
        let func = match function_name.to_ascii_uppercase().as_str() {
            "LENGTH" => ScalarFunc::Length,
            "LOWER" => ScalarFunc::Lower,
            "UPPER" => ScalarFunc::Upper,
            "ABS" => ScalarFunc::Abs,
            "COALESCE" => ScalarFunc::Coalesce,
            "IFNULL" => ScalarFunc::IfNull,
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
            AggregateArg::Expr {
                expr: self.parse_scalar_expr()?,
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
            if self.matches(&TokenKind::Exists) {
                let query = self.parse_subquery_after_exists()?;
                return Ok(Expr::ExistsSubquery {
                    query: Box::new(query),
                    negated: true,
                });
            }
            return Ok(Expr::Not(Box::new(self.parse_not_expr()?)));
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
                Ok(expr) => return Ok(expr),
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
        let column = if let ScalarExpr::Column(column) = &left_expr {
            Some(column.clone())
        } else {
            None
        };
        if self.matches(&TokenKind::Is) {
            let negated = self.matches(&TokenKind::Not);
            if !self.matches(&TokenKind::Null) {
                return Err(self.error_expected(&format!(
                    "NULL or NOT NULL, found {}",
                    display_token(self.peek_kind())
                )));
            }
            return match column {
                Some(column) => Ok(Expr::IsNull { column, negated }),
                None => Ok(Expr::IsNullScalar {
                    expr: left_expr,
                    negated,
                }),
            };
        }
        if self.matches(&TokenKind::Not) {
            if self.matches(&TokenKind::Like) {
                let pattern = self.parse_string_literal()?;
                return match column {
                    Some(column) => Ok(Expr::Like {
                        column,
                        pattern,
                        negated: true,
                    }),
                    None => Ok(Expr::LikeScalar {
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
            let query = self.parse_subquery()?;
            return match column {
                Some(column) => Ok(Expr::InSubquery {
                    column,
                    query: Box::new(query),
                    negated: true,
                }),
                None => Ok(Expr::InSubqueryScalar {
                    expr: left_expr,
                    query: Box::new(query),
                    negated: true,
                }),
            };
        }
        if self.matches(&TokenKind::In) {
            let query = self.parse_subquery()?;
            return match column {
                Some(column) => Ok(Expr::InSubquery {
                    column,
                    query: Box::new(query),
                    negated: false,
                }),
                None => Ok(Expr::InSubqueryScalar {
                    expr: left_expr,
                    query: Box::new(query),
                    negated: false,
                }),
            };
        }
        if self.matches(&TokenKind::Like) {
            let pattern = self.parse_string_literal()?;
            return match column {
                Some(column) => Ok(Expr::Like {
                    column,
                    pattern,
                    negated: false,
                }),
                None => Ok(Expr::LikeScalar {
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

    fn parse_column_def(&mut self, table_name: Option<&str>) -> Result<ColumnDef> {
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

            if self.matches(&TokenKind::Default) {
                column = column.default_value(self.parse_literal()?);
                continue;
            }

            if self.matches(&TokenKind::Check) {
                let check_name =
                    format!("{}_{}_check", table_name.unwrap_or("column"), column.name);
                column = column.check(self.parse_check_constraint(check_name)?);
                continue;
            }

            if self.matches(&TokenKind::References) {
                let ref_table = self.parse_simple_identifier()?;
                self.expect_symbol(TokenKind::LParen)?;
                let ref_column = self.parse_simple_identifier()?;
                self.expect_symbol(TokenKind::RParen)?;
                column = column.references(ref_table, ref_column);
                continue;
            }

            break;
        }

        Ok(column)
    }

    fn parse_table_constraint(&mut self, table_name: &str) -> Result<TableConstraint> {
        let name = if self.matches(&TokenKind::Constraint) {
            Some(self.parse_simple_identifier()?)
        } else {
            None
        };

        if self.matches(&TokenKind::Check) {
            let name = name.unwrap_or_else(|| format!("{table_name}_check"));
            return Ok(TableConstraint::Check(self.parse_check_constraint(name)?));
        }

        if self.matches(&TokenKind::Foreign) {
            self.expect_keyword(TokenKind::Key)?;
            return Ok(TableConstraint::ForeignKey(
                self.parse_foreign_key_constraint()?,
            ));
        }

        Err(self.error_expected(&format!(
            "CHECK or FOREIGN KEY constraint, found {}",
            display_token(self.peek_kind())
        )))
    }

    fn parse_check_constraint(&mut self, name: String) -> Result<CheckConstraint> {
        self.expect_symbol(TokenKind::LParen)?;
        let expr = self.parse_where_expr()?;
        self.expect_symbol(TokenKind::RParen)?;
        Ok(CheckConstraint {
            name,
            expr: Self::check_expr_from_expr(expr)?,
        })
    }

    fn parse_foreign_key_constraint(&mut self) -> Result<ForeignKey> {
        self.expect_symbol(TokenKind::LParen)?;
        let column = self.parse_simple_identifier()?;
        self.expect_symbol(TokenKind::RParen)?;
        self.expect_keyword(TokenKind::References)?;
        let ref_table = self.parse_simple_identifier()?;
        self.expect_symbol(TokenKind::LParen)?;
        let ref_column = self.parse_simple_identifier()?;
        self.expect_symbol(TokenKind::RParen)?;
        Ok(ForeignKey::single_column(column, ref_table, ref_column))
    }

    fn check_expr_from_expr(expr: Expr) -> Result<CheckExpr> {
        Ok(match expr {
            Expr::Compare { column, op, value } => CheckExpr::Compare {
                column,
                op: Self::check_op_from_compare_op(op),
                value,
            },
            Expr::IsNull { column, negated } => CheckExpr::IsNull { column, negated },
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
                    expr => OrderByExpr::Expr(expr),
                }
            };
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

    fn parse_subquery_after_exists(&mut self) -> Result<SelectStatement> {
        self.parse_subquery()
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
            token => {
                Err(self.error_expected(&format!("identifier, found {}", display_token(token))))
            }
        }
    }

    fn parse_optional_table_alias(&mut self) -> Result<Option<String>> {
        if self.matches(&TokenKind::As) {
            return Ok(Some(self.parse_simple_identifier()?));
        }
        if is_identifier_token(self.peek_kind()) {
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

fn is_identifier_token(token: &TokenKind) -> bool {
    matches!(
        token,
        TokenKind::Identifier(_) | TokenKind::Nulls | TokenKind::First | TokenKind::Last
    )
}

fn is_scalar_expr_start(token: &TokenKind) -> bool {
    is_identifier_token(token)
        || matches!(
            token,
            TokenKind::LParen
                | TokenKind::Minus
                | TokenKind::Integer(_)
                | TokenKind::String(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Null
        )
}

fn scalar_expr_literal_value(expr: &ScalarExpr) -> Option<Value> {
    match expr {
        ScalarExpr::Literal(value) => Some(value.clone()),
        ScalarExpr::UnaryMinus(expr) => match expr.as_ref() {
            ScalarExpr::Literal(Value::Integer(value)) => Some(Value::Integer(-value)),
            _ => None,
        },
        _ => None,
    }
}

fn is_aggregate_function_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
    )
}

fn display_token(token: &TokenKind) -> String {
    match token {
        TokenKind::Create => "CREATE".to_string(),
        TokenKind::Alter => "ALTER".to_string(),
        TokenKind::Add => "ADD".to_string(),
        TokenKind::Rename => "RENAME".to_string(),
        TokenKind::Drop => "DROP".to_string(),
        TokenKind::Table => "TABLE".to_string(),
        TokenKind::Column => "COLUMN".to_string(),
        TokenKind::To => "TO".to_string(),
        TokenKind::Unique => "UNIQUE".to_string(),
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
        TokenKind::Explain => "EXPLAIN".to_string(),
        TokenKind::Query => "QUERY".to_string(),
        TokenKind::Plan => "PLAN".to_string(),
        TokenKind::Default => "DEFAULT".to_string(),
        TokenKind::Check => "CHECK".to_string(),
        TokenKind::Constraint => "CONSTRAINT".to_string(),
        TokenKind::Foreign => "FOREIGN".to_string(),
        TokenKind::References => "REFERENCES".to_string(),
        TokenKind::Nulls => "NULLS".to_string(),
        TokenKind::First => "FIRST".to_string(),
        TokenKind::Last => "LAST".to_string(),
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
        TokenKind::Plus => "+".to_string(),
        TokenKind::Minus => "-".to_string(),
        TokenKind::Slash => "/".to_string(),
        TokenKind::PipePipe => "||".to_string(),
        TokenKind::Eof => "end of input".to_string(),
    }
}
