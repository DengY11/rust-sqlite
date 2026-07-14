use crate::common::error::{DbError, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Create,
    Alter,
    Add,
    Rename,
    Replace,
    Drop,
    Table,
    Column,
    To,
    Unique,
    Index,
    On,
    Off,
    Conflict,
    Do,
    Nothing,
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
    Offset,
    With,
    Recursive,
    Union,
    Intersect,
    Except,
    All,
    As,
    Inner,
    Cross,
    Left,
    Right,
    Full,
    Outer,
    Natural,
    Join,
    Using,
    Asc,
    Desc,
    And,
    Or,
    Like,
    Glob,
    Regexp,
    Match,
    Escape,
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
    Savepoint,
    Release,
    Case,
    When,
    Then,
    Else,
    End,
    If,
    Not,
    In,
    Is,
    Exists,
    Explain,
    Query,
    Plan,
    Pragma,
    Analyze,
    Reindex,
    Vacuum,
    Distinct,
    Default,
    Returning,
    Check,
    Collate,
    Constraint,
    Foreign,
    References,
    Generated,
    Always,
    Stored,
    Virtual,
    IsNull,
    NotNull,
    Nulls,
    First,
    Last,
    Primary,
    Key,
    Strict,
    Autoincrement,
    IntegerType,
    TextType,
    BlobType,
    BooleanType,
    True,
    False,
    Null,
    Identifier(String),
    Integer(i64),
    Real(f64),
    String(String),
    BlobLiteral(Vec<u8>),
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
    Percent,
    Ampersand,
    Pipe,
    ShiftLeft,
    ShiftRight,
    Tilde,
    PipePipe,
    Arrow,
    ArrowText,
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
            if ch == '-' && self.peek_next() == Some('-') {
                self.skip_line_comment();
                continue;
            }
            if ch == '/' && self.peek_next() == Some('*') {
                self.skip_block_comment()?;
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
                '.' if matches!(self.peek_next(), Some('0'..='9')) => self.lex_number()?,
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
                '%' => {
                    self.advance_char();
                    TokenKind::Percent
                }
                '&' => {
                    self.advance_char();
                    TokenKind::Ampersand
                }
                '~' => {
                    self.advance_char();
                    TokenKind::Tilde
                }
                ';' => {
                    self.advance_char();
                    TokenKind::Semicolon
                }
                '=' => {
                    self.advance_char();
                    if self.peek() == Some('=') {
                        self.advance_char();
                    }
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
                    } else if self.peek() == Some('>') {
                        self.advance_char();
                        TokenKind::ShiftRight
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
                    } else if self.peek() == Some('<') {
                        self.advance_char();
                        TokenKind::ShiftLeft
                    } else {
                        TokenKind::Lt
                    }
                }
                '-' => {
                    self.advance_char();
                    if self.peek() == Some('>') {
                        self.advance_char();
                        if self.peek() == Some('>') {
                            self.advance_char();
                            TokenKind::ArrowText
                        } else {
                            TokenKind::Arrow
                        }
                    } else {
                        TokenKind::Minus
                    }
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
                    self.advance_char();
                    TokenKind::Pipe
                }
                '\'' => self.lex_string()?,
                '"' => self.lex_quoted_identifier('"', '"')?,
                '`' => self.lex_quoted_identifier('`', '`')?,
                '[' => self.lex_quoted_identifier('[', ']')?,
                '0'..='9' => self.lex_number()?,
                'x' | 'X' if self.peek_next() == Some('\'') => self.lex_blob_literal()?,
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

    fn lex_quoted_identifier(&mut self, opener: char, closer: char) -> Result<TokenKind> {
        self.advance_char();
        let mut value = String::new();

        while let Some(ch) = self.advance_char() {
            if ch == closer {
                if opener == '"' && self.peek() == Some('"') {
                    value.push('"');
                    self.advance_char();
                    continue;
                }
                return Ok(TokenKind::Identifier(value));
            }
            value.push(ch);
        }

        Err(DbError::sql("unterminated quoted identifier"))
    }

    fn skip_line_comment(&mut self) {
        self.advance_char();
        self.advance_char();
        while let Some(ch) = self.peek() {
            self.advance_char();
            if ch == '\n' {
                break;
            }
        }
    }

    fn skip_block_comment(&mut self) -> Result<()> {
        self.advance_char();
        self.advance_char();
        while let Some(ch) = self.advance_char() {
            if ch == '*' && self.peek() == Some('/') {
                self.advance_char();
                return Ok(());
            }
        }
        Err(DbError::sql("unterminated block comment"))
    }

    fn lex_number(&mut self) -> Result<TokenKind> {
        let start = self.offset;
        if self.peek() == Some('0') && matches!(self.peek_next(), Some('x' | 'X')) {
            self.advance_char();
            self.advance_char();
            let hex_start = self.offset;
            while matches!(self.peek(), Some('0'..='9' | 'a'..='f' | 'A'..='F' | '_')) {
                self.advance_char();
            }
            if self.offset == hex_start {
                return Err(DbError::sql("invalid hexadecimal integer literal '0x'"));
            }
            let text = &self.input[hex_start..self.offset];
            let normalized = text.replace('_', "");
            let value = u64::from_str_radix(&normalized, 16).map_err(|error| {
                DbError::sql(format!(
                    "invalid hexadecimal integer literal '0x{text}': {error}"
                ))
            })?;
            return Ok(TokenKind::Integer(value as i64));
        }
        while matches!(self.peek(), Some('0'..='9' | '_')) {
            self.advance_char();
        }
        let mut is_real = false;
        if self.peek() == Some('.') {
            let saw_integer_digits = self.offset > start;
            let next_is_digit = matches!(self.peek_next(), Some('0'..='9'));
            if saw_integer_digits || next_is_digit {
                self.advance_char();
                while matches!(self.peek(), Some('0'..='9' | '_')) {
                    self.advance_char();
                }
                is_real = true;
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let exponent_start = self.offset;
            self.advance_char();
            if matches!(self.peek(), Some('+' | '-')) {
                self.advance_char();
            }
            if matches!(self.peek(), Some('0'..='9')) {
                while matches!(self.peek(), Some('0'..='9' | '_')) {
                    self.advance_char();
                }
                is_real = true;
            } else {
                self.offset = exponent_start;
            }
        }
        let text = &self.input[start..self.offset];
        if is_real {
            let value = text
                .replace('_', "")
                .parse::<f64>()
                .map_err(|error| DbError::sql(format!("invalid real literal '{text}': {error}")))?;
            Ok(TokenKind::Real(value))
        } else {
            let normalized = text.replace('_', "");
            match normalized.parse::<i64>() {
                Ok(value) => Ok(TokenKind::Integer(value)),
                Err(_) => {
                    let value = normalized.parse::<f64>().map_err(|error| {
                        DbError::sql(format!("invalid integer literal '{text}': {error}"))
                    })?;
                    Ok(TokenKind::Real(value))
                }
            }
        }
    }

    fn lex_blob_literal(&mut self) -> Result<TokenKind> {
        self.advance_char();
        let TokenKind::String(hex) = self.lex_string()? else {
            unreachable!("blob literal parser must reuse string literal parsing");
        };

        if hex.len() % 2 != 0 {
            return Err(DbError::sql(
                "blob literal must contain an even number of hex digits",
            ));
        }

        let mut bytes = Vec::with_capacity(hex.len() / 2);
        let mut index = 0;
        while index < hex.len() {
            let end = index + 2;
            let pair = &hex[index..end];
            let byte = u8::from_str_radix(pair, 16)
                .map_err(|_| DbError::sql(format!("invalid blob literal hex byte '{pair}'")))?;
            bytes.push(byte);
            index = end;
        }

        Ok(TokenKind::BlobLiteral(bytes))
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
            "REPLACE" => TokenKind::Replace,
            "DROP" => TokenKind::Drop,
            "TABLE" => TokenKind::Table,
            "COLUMN" => TokenKind::Column,
            "TO" => TokenKind::To,
            "UNIQUE" => TokenKind::Unique,
            "INDEX" => TokenKind::Index,
            "ON" => TokenKind::On,
            "OFF" => TokenKind::Off,
            "CONFLICT" => TokenKind::Conflict,
            "DO" => TokenKind::Do,
            "NOTHING" => TokenKind::Nothing,
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
            "OFFSET" => TokenKind::Offset,
            "WITH" => TokenKind::With,
            "RECURSIVE" => TokenKind::Recursive,
            "UNION" => TokenKind::Union,
            "INTERSECT" => TokenKind::Intersect,
            "EXCEPT" => TokenKind::Except,
            "ALL" => TokenKind::All,
            "AS" => TokenKind::As,
            "INNER" => TokenKind::Inner,
            "CROSS" => TokenKind::Cross,
            "LEFT" => TokenKind::Left,
            "RIGHT" => TokenKind::Right,
            "FULL" => TokenKind::Full,
            "OUTER" => TokenKind::Outer,
            "NATURAL" => TokenKind::Natural,
            "JOIN" => TokenKind::Join,
            "USING" => TokenKind::Using,
            "ASC" => TokenKind::Asc,
            "DESC" => TokenKind::Desc,
            "AND" => TokenKind::And,
            "OR" => TokenKind::Or,
            "LIKE" => TokenKind::Like,
            "GLOB" => TokenKind::Glob,
            "REGEXP" => TokenKind::Regexp,
            "MATCH" => TokenKind::Match,
            "ESCAPE" => TokenKind::Escape,
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
            "SAVEPOINT" => TokenKind::Savepoint,
            "RELEASE" => TokenKind::Release,
            "CASE" => TokenKind::Case,
            "WHEN" => TokenKind::When,
            "THEN" => TokenKind::Then,
            "ELSE" => TokenKind::Else,
            "END" => TokenKind::End,
            "IF" => TokenKind::If,
            "NOT" => TokenKind::Not,
            "IN" => TokenKind::In,
            "IS" => TokenKind::Is,
            "EXISTS" => TokenKind::Exists,
            "EXPLAIN" => TokenKind::Explain,
            "QUERY" => TokenKind::Query,
            "PLAN" => TokenKind::Plan,
            "PRAGMA" => TokenKind::Pragma,
            "ANALYZE" => TokenKind::Analyze,
            "REINDEX" => TokenKind::Reindex,
            "VACUUM" => TokenKind::Vacuum,
            "DISTINCT" => TokenKind::Distinct,
            "DEFAULT" => TokenKind::Default,
            "RETURNING" => TokenKind::Returning,
            "CHECK" => TokenKind::Check,
            "COLLATE" => TokenKind::Collate,
            "CONSTRAINT" => TokenKind::Constraint,
            "FOREIGN" => TokenKind::Foreign,
            "REFERENCES" => TokenKind::References,
            "GENERATED" => TokenKind::Generated,
            "ALWAYS" => TokenKind::Always,
            "STORED" => TokenKind::Stored,
            "VIRTUAL" => TokenKind::Virtual,
            "ISNULL" => TokenKind::IsNull,
            "NOTNULL" => TokenKind::NotNull,
            "NULLS" => TokenKind::Nulls,
            "FIRST" => TokenKind::First,
            "LAST" => TokenKind::Last,
            "PRIMARY" => TokenKind::Primary,
            "KEY" => TokenKind::Key,
            "STRICT" => TokenKind::Strict,
            "AUTOINCREMENT" => TokenKind::Autoincrement,
            "INTEGER" => TokenKind::IntegerType,
            "TEXT" => TokenKind::TextType,
            "BLOB" => TokenKind::BlobType,
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
