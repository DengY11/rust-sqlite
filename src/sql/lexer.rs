use crate::common::error::{DbError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Create,
    Table,
    Index,
    On,
    Insert,
    Into,
    Values,
    Select,
    From,
    Where,
    Begin,
    Commit,
    Rollback,
    Not,
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
    Gt,
    Lt,
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
                '*' => {
                    self.advance_char();
                    TokenKind::Star
                }
                ';' => {
                    self.advance_char();
                    TokenKind::Semicolon
                }
                '=' => {
                    self.advance_char();
                    TokenKind::Eq
                }
                '>' => {
                    self.advance_char();
                    TokenKind::Gt
                }
                '<' => {
                    self.advance_char();
                    TokenKind::Lt
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
            "TABLE" => TokenKind::Table,
            "INDEX" => TokenKind::Index,
            "ON" => TokenKind::On,
            "INSERT" => TokenKind::Insert,
            "INTO" => TokenKind::Into,
            "VALUES" => TokenKind::Values,
            "SELECT" => TokenKind::Select,
            "FROM" => TokenKind::From,
            "WHERE" => TokenKind::Where,
            "BEGIN" => TokenKind::Begin,
            "COMMIT" => TokenKind::Commit,
            "ROLLBACK" => TokenKind::Rollback,
            "NOT" => TokenKind::Not,
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
