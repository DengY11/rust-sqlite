use crate::common::error::{DbError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Create,
    Alter,
    Add,
    Rename,
    Drop,
    Table,
    Column,
    To,
    Unique,
    Index,
    On,
    Insert,
    Into,
    Values,
    Select,
    Delete,
    Update,
    Set,
    From,
    Where,
    Group,
    Having,
    Order,
    By,
    Limit,
    With,
    Recursive,
    Union,
    All,
    As,
    Inner,
    Left,
    Outer,
    Join,
    Asc,
    Desc,
    And,
    Or,
    Like,
    Between,
    Begin,
    Start,
    Transaction,
    Isolation,
    Level,
    Read,
    Committed,
    Repeatable,
    Serializable,
    Commit,
    Rollback,
    Not,
    In,
    Is,
    Exists,
    Explain,
    Query,
    Plan,
    Distinct,
    Default,
    Check,
    Constraint,
    Foreign,
    References,
    Nulls,
    First,
    Last,
    Primary,
    Key,
    IntegerType,
    TextType,
    BooleanType,
    True,
    False,
    Null,
    Identifier(String),
    Integer(i64),
    String(String),
    Star,
    Comma,
    Semicolon,
    LParen,
    RParen,
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    Plus,
    Minus,
    Slash,
    PipePipe,
    Dot,
    Eof,
}

pub fn lex(input: &str) -> Result<Vec<Token>> {
    let mut lexer = Lexer::new(input);
    lexer.lex()
}

struct Lexer<'a> {
    input: &'a str,
    offset: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, offset: 0 }
    }

    fn lex(&mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();

        while let Some(ch) = self.peek() {
            if ch.is_whitespace() {
                self.advance_char();
                continue;
            }

            let position = self.offset;
            let kind = match ch {
                '(' => {
                    self.advance_char();
                    TokenKind::LParen
                }
                ')' => {
                    self.advance_char();
                    TokenKind::RParen
                }
                ',' => {
                    self.advance_char();
                    TokenKind::Comma
                }
                '.' => {
                    self.advance_char();
                    TokenKind::Dot
                }
                '*' => {
                    self.advance_char();
                    TokenKind::Star
                }
                '+' => {
                    self.advance_char();
                    TokenKind::Plus
                }
                ';' => {
                    self.advance_char();
                    TokenKind::Semicolon
                }
                '=' => {
                    self.advance_char();
                    TokenKind::Eq
                }
                '!' if self.peek_next() == Some('=') => {
                    self.advance_char();
                    self.advance_char();
                    TokenKind::Ne
                }
                '!' => {
                    return Err(DbError::sql(format!(
                        "unexpected character '{}' at position {}",
                        ch, position
                    )));
                }
                '>' => {
                    self.advance_char();
                    if self.peek() == Some('=') {
                        self.advance_char();
                        TokenKind::Gte
                    } else {
                        TokenKind::Gt
                    }
                }
                '<' => {
                    self.advance_char();
                    if self.peek() == Some('=') {
                        self.advance_char();
                        TokenKind::Lte
                    } else if self.peek() == Some('>') {
                        self.advance_char();
                        TokenKind::Ne
                    } else {
                        TokenKind::Lt
                    }
                }
                '-' => {
                    self.advance_char();
                    TokenKind::Minus
                }
                '/' => {
                    self.advance_char();
                    TokenKind::Slash
                }
                '|' if self.peek_next() == Some('|') => {
                    self.advance_char();
                    self.advance_char();
                    TokenKind::PipePipe
                }
                '|' => {
                    return Err(DbError::sql(format!(
                        "unexpected character '{}' at position {}",
                        ch, position
                    )));
                }
                '\'' => self.lex_string()?,
                '0'..='9' => self.lex_integer()?,
                _ if is_identifier_start(ch) => self.lex_word(),
                _ => {
                    return Err(DbError::sql(format!(
                        "unexpected character '{}' at position {}",
                        ch, position
                    )));
                }
            };

            tokens.push(Token { kind, position });
        }

        tokens.push(Token {
            kind: TokenKind::Eof,
            position: self.input.len(),
        });
        Ok(tokens)
    }

    fn peek(&self) -> Option<char> {
        self.input[self.offset..].chars().next()
    }

    fn peek_next(&self) -> Option<char> {
        let mut chars = self.input[self.offset..].chars();
        chars.next()?;
        chars.next()
    }

    fn advance_char(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.offset += ch.len_utf8();
        Some(ch)
    }

    fn lex_string(&mut self) -> Result<TokenKind> {
        self.advance_char();
        let mut value = String::new();

        while let Some(ch) = self.advance_char() {
            if ch == '\'' {
                if self.peek() == Some('\'') {
                    value.push('\'');
                    self.advance_char();
                    continue;
                }
                return Ok(TokenKind::String(value));
            }
            value.push(ch);
        }

        Err(DbError::sql("unterminated string literal"))
    }

    fn lex_integer(&mut self) -> Result<TokenKind> {
        let start = self.offset;
        while matches!(self.peek(), Some('0'..='9')) {
            self.advance_char();
        }
        let text = &self.input[start..self.offset];
        let value = text
            .parse::<i64>()
            .map_err(|error| DbError::sql(format!("invalid integer literal '{text}': {error}")))?;
        Ok(TokenKind::Integer(value))
    }

    fn lex_word(&mut self) -> TokenKind {
        let start = self.offset;
        while matches!(self.peek(), Some(ch) if is_identifier_continue(ch)) {
            self.advance_char();
        }

        let text = &self.input[start..self.offset];
        match text.to_ascii_uppercase().as_str() {
            "CREATE" => TokenKind::Create,
            "ALTER" => TokenKind::Alter,
            "ADD" => TokenKind::Add,
            "RENAME" => TokenKind::Rename,
            "DROP" => TokenKind::Drop,
            "TABLE" => TokenKind::Table,
            "COLUMN" => TokenKind::Column,
            "TO" => TokenKind::To,
            "UNIQUE" => TokenKind::Unique,
            "INDEX" => TokenKind::Index,
            "ON" => TokenKind::On,
            "INSERT" => TokenKind::Insert,
            "INTO" => TokenKind::Into,
            "VALUES" => TokenKind::Values,
            "SELECT" => TokenKind::Select,
            "DELETE" => TokenKind::Delete,
            "UPDATE" => TokenKind::Update,
            "SET" => TokenKind::Set,
            "FROM" => TokenKind::From,
            "WHERE" => TokenKind::Where,
            "GROUP" => TokenKind::Group,
            "HAVING" => TokenKind::Having,
            "ORDER" => TokenKind::Order,
            "BY" => TokenKind::By,
            "LIMIT" => TokenKind::Limit,
            "WITH" => TokenKind::With,
            "RECURSIVE" => TokenKind::Recursive,
            "UNION" => TokenKind::Union,
            "ALL" => TokenKind::All,
            "AS" => TokenKind::As,
            "INNER" => TokenKind::Inner,
            "LEFT" => TokenKind::Left,
            "OUTER" => TokenKind::Outer,
            "JOIN" => TokenKind::Join,
            "ASC" => TokenKind::Asc,
            "DESC" => TokenKind::Desc,
            "AND" => TokenKind::And,
            "OR" => TokenKind::Or,
            "LIKE" => TokenKind::Like,
            "BETWEEN" => TokenKind::Between,
            "BEGIN" => TokenKind::Begin,
            "START" => TokenKind::Start,
            "TRANSACTION" => TokenKind::Transaction,
            "ISOLATION" => TokenKind::Isolation,
            "LEVEL" => TokenKind::Level,
            "READ" => TokenKind::Read,
            "COMMITTED" => TokenKind::Committed,
            "REPEATABLE" => TokenKind::Repeatable,
            "SERIALIZABLE" => TokenKind::Serializable,
            "COMMIT" => TokenKind::Commit,
            "ROLLBACK" => TokenKind::Rollback,
            "NOT" => TokenKind::Not,
            "IN" => TokenKind::In,
            "IS" => TokenKind::Is,
            "EXISTS" => TokenKind::Exists,
            "EXPLAIN" => TokenKind::Explain,
            "QUERY" => TokenKind::Query,
            "PLAN" => TokenKind::Plan,
            "DISTINCT" => TokenKind::Distinct,
            "DEFAULT" => TokenKind::Default,
            "CHECK" => TokenKind::Check,
            "CONSTRAINT" => TokenKind::Constraint,
            "FOREIGN" => TokenKind::Foreign,
            "REFERENCES" => TokenKind::References,
            "NULLS" => TokenKind::Nulls,
            "FIRST" => TokenKind::First,
            "LAST" => TokenKind::Last,
            "PRIMARY" => TokenKind::Primary,
            "KEY" => TokenKind::Key,
            "INTEGER" => TokenKind::IntegerType,
            "TEXT" => TokenKind::TextType,
            "BOOLEAN" => TokenKind::BooleanType,
            "TRUE" => TokenKind::True,
            "FALSE" => TokenKind::False,
            "NULL" => TokenKind::Null,
            _ => TokenKind::Identifier(text.to_string()),
        }
    }
}

fn is_identifier_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_identifier_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}
