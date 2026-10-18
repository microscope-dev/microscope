//! Splits an expression's text into tokens.

use super::Span;
use super::ast::Suffix;
use crate::eval::error::ExpressionError;
use crate::eval::number::{Float, MAX_WIDTH};

/// A token and the text it was read from.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// A plain identifier, including keywords.
    Ident(String),

    /// A backticked name, which is never a keyword.
    Quoted(String),

    /// `$name`, without the `$`.
    Register(String),
    Integer {
        value: u128,
        suffix: Option<Suffix>,
    },
    Float(Float),
    Char(u32),
    Text(Vec<u8>),
    Punct(Punct),
    End,
}

/// Punctuation, longest spellings first so that matching takes the
/// longest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Punct {
    ShlEq,
    ShrEq,
    Shl,
    Shr,
    Le,
    Ge,
    EqEq,
    Ne,
    AndAnd,
    OrOr,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    Arrow,
    ColonColon,
    DotDot,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Bang,
    Lt,
    Gt,
    Assign,
    Question,
    Colon,
    Dot,
    OpenParen,
    CloseParen,
    OpenBracket,
    CloseBracket,

    /// Only `offsetof(TYPE, member)` in a view's expressions takes one.
    Comma,
}

impl Punct {
    const ALL: [Self; 42] = [
        Self::ShlEq,
        Self::ShrEq,
        Self::Shl,
        Self::Shr,
        Self::Le,
        Self::Ge,
        Self::EqEq,
        Self::Ne,
        Self::AndAnd,
        Self::OrOr,
        Self::PlusEq,
        Self::MinusEq,
        Self::StarEq,
        Self::SlashEq,
        Self::PercentEq,
        Self::AmpEq,
        Self::PipeEq,
        Self::CaretEq,
        Self::Arrow,
        Self::ColonColon,
        Self::DotDot,
        Self::Plus,
        Self::Minus,
        Self::Star,
        Self::Slash,
        Self::Percent,
        Self::Amp,
        Self::Pipe,
        Self::Caret,
        Self::Tilde,
        Self::Bang,
        Self::Lt,
        Self::Gt,
        Self::Assign,
        Self::Question,
        Self::Colon,
        Self::Dot,
        Self::OpenParen,
        Self::CloseParen,
        Self::OpenBracket,
        Self::CloseBracket,
        Self::Comma,
    ];

    pub const fn text(self) -> &'static str {
        match self {
            Self::ShlEq => "<<=",
            Self::ShrEq => ">>=",
            Self::Shl => "<<",
            Self::Shr => ">>",
            Self::Le => "<=",
            Self::Ge => ">=",
            Self::EqEq => "==",
            Self::Ne => "!=",
            Self::AndAnd => "&&",
            Self::OrOr => "||",
            Self::PlusEq => "+=",
            Self::MinusEq => "-=",
            Self::StarEq => "*=",
            Self::SlashEq => "/=",
            Self::PercentEq => "%=",
            Self::AmpEq => "&=",
            Self::PipeEq => "|=",
            Self::CaretEq => "^=",
            Self::Arrow => "->",
            Self::ColonColon => "::",
            Self::DotDot => "..",
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Star => "*",
            Self::Slash => "/",
            Self::Percent => "%",
            Self::Amp => "&",
            Self::Pipe => "|",
            Self::Caret => "^",
            Self::Tilde => "~",
            Self::Bang => "!",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::Assign => "=",
            Self::Question => "?",
            Self::Colon => ":",
            Self::Dot => ".",
            Self::OpenParen => "(",
            Self::CloseParen => ")",
            Self::OpenBracket => "[",
            Self::CloseBracket => "]",
            Self::Comma => ",",
        }
    }
}

/// The longest expression text accepted, in bytes.
pub const MAX_TEXT_BYTES: usize = 4096;

/// Splits `text` into tokens, ending with [`TokenKind::End`].
pub fn lex(text: &str) -> Result<Vec<Token>, ExpressionError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(ExpressionError::new(
            crate::eval::error::ErrorKind::Limit,
            Span::new(MAX_TEXT_BYTES, text.len()),
            format!("an expression may be at most {MAX_TEXT_BYTES} bytes long"),
        ));
    }

    let mut lexer = Lexer { text, cursor: 0 };
    let mut tokens = Vec::new();
    loop {
        lexer.skip_whitespace();
        let start = lexer.cursor;
        if start == text.len() {
            tokens.push(Token {
                kind: TokenKind::End,
                span: Span::new(start, start),
            });
            return Ok(tokens);
        }

        let after_dot = matches!(
            tokens.last(),
            Some(Token {
                kind: TokenKind::Punct(Punct::Dot | Punct::Arrow),
                ..
            })
        );
        let kind = lexer.token(after_dot)?;

        // Every token consumes text, so a mistake here cannot loop while
        // the token list grows.
        assert!(lexer.cursor > start, "the lexer made no progress");
        tokens.push(Token {
            kind,
            span: Span::new(start, lexer.cursor),
        });
    }
}

struct Lexer<'text> {
    text: &'text str,
    cursor: usize,
}

const fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_name_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

impl Lexer<'_> {
    fn rest(&self) -> &str {
        &self.text[self.cursor..]
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.cursor).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.text.as_bytes().get(self.cursor + offset).copied()
    }

    fn skip_whitespace(&mut self) {
        let rest = self.rest();
        let trimmed = rest.trim_start();
        self.cursor += rest.len() - trimmed.len();
    }

    /// Consumes bytes while `accept` holds, returning them.
    fn take_while(&mut self, accept: impl Fn(u8) -> bool) -> &str {
        let start = self.cursor;
        let length = self.rest().bytes().take_while(|&byte| accept(byte)).count();
        self.cursor += length;
        &self.text[start..self.cursor]
    }

    fn error(&self, start: usize, message: impl Into<String>) -> ExpressionError {
        ExpressionError::syntax(Span::new(start, self.cursor.max(start + 1)), message)
    }

    fn token(&mut self, after_dot: bool) -> Result<TokenKind, ExpressionError> {
        let start = self.cursor;
        let Some(byte) = self.peek() else {
            return Err(self.error(start, "the text ends where a token should begin"));
        };

        if is_name_start(byte) {
            return Ok(TokenKind::Ident(
                self.take_while(is_name_continue).to_owned(),
            ));
        }

        if byte.is_ascii_digit() {
            return if after_dot {
                self.field_index(start)
            } else {
                self.number(start)
            };
        }

        match byte {
            b'`' => self.quoted(start),
            b'$' => {
                self.cursor += 1;
                let name = self.take_while(is_name_continue);
                if name.is_empty() {
                    return Err(self.error(start, "expected a register name after `$`"));
                }

                Ok(TokenKind::Register(name.to_owned()))
            }
            b'\'' => self.char_literal(start),
            b'"' => self.text_literal(start),
            _ => {
                let rest = self.rest();
                let Some(punct) = Punct::ALL
                    .into_iter()
                    .find(|punct| rest.starts_with(punct.text()))
                else {
                    let character = rest.chars().next().unwrap_or_default();
                    self.cursor += character.len_utf8();
                    let error = self.error(start, format!("unexpected character `{character}`"));
                    return Err(if character.is_alphabetic() {
                        error.with_hint("quote names with other characters in backticks")
                    } else {
                        error
                    });
                };

                self.cursor += punct.text().len();
                Ok(TokenKind::Punct(punct))
            }
        }
    }

    fn quoted(&mut self, start: usize) -> Result<TokenKind, ExpressionError> {
        self.cursor += 1;
        let Some(length) = self.rest().find('`') else {
            self.cursor = self.text.len();
            return Err(self.error(start, "a backticked name needs a closing backtick"));
        };

        let name = self.rest()[..length].to_owned();
        self.cursor += length + 1;
        if name.is_empty() {
            return Err(self.error(start, "a backticked name cannot be empty"));
        }

        Ok(TokenKind::Quoted(name))
    }

    /// A tuple field's index after `.`, such as the `0` and `1` of `t.0.1`.
    fn field_index(&mut self, start: usize) -> Result<TokenKind, ExpressionError> {
        let digits = self.take_while(|byte| byte.is_ascii_digit());
        let value = digits.parse::<u32>();
        if (digits.len() > 1 && digits.starts_with('0')) || value.is_err() {
            return Err(self.error(start, "a tuple field is a decimal index"));
        }

        Ok(TokenKind::Integer {
            value: u128::from(value.unwrap_or_default()),
            suffix: None,
        })
    }

    fn number(&mut self, start: usize) -> Result<TokenKind, ExpressionError> {
        let radix = match (self.peek(), self.peek_at(1)) {
            (Some(b'0'), Some(b'x' | b'X')) => 16,
            (Some(b'0'), Some(b'o' | b'O')) => 8,
            (Some(b'0'), Some(b'b' | b'B')) => 2,
            _ => 10,
        };

        if radix != 10 {
            self.cursor += 2;
            let body = self.take_while(is_name_continue).to_owned();
            let digits_end = body
                .find(|character: char| character != '_' && !character.is_digit(radix))
                .unwrap_or(body.len());
            let (digits, suffix) = body.split_at(digits_end);
            let suffix = self.suffix(start, suffix)?;
            if matches!(suffix, Some(Suffix::F32 | Suffix::F64)) {
                return Err(self.error(start, "a float literal must be decimal"));
            }

            return self.integer(start, digits, radix, suffix);
        }

        let digits = self.take_while(|byte| byte.is_ascii_digit() || byte == b'_');
        let digits = digits.to_owned();
        let fraction =
            self.peek() == Some(b'.') && self.peek_at(1).is_some_and(|byte| byte.is_ascii_digit());
        if fraction {
            self.cursor += 1;
            self.take_while(|byte| byte.is_ascii_digit() || byte == b'_');
        }

        let exponent = matches!(self.peek(), Some(b'e' | b'E'))
            && match self.peek_at(1) {
                Some(b'+' | b'-') => self.peek_at(2).is_some_and(|byte| byte.is_ascii_digit()),
                next => next.is_some_and(|byte| byte.is_ascii_digit()),
            };
        if exponent {
            self.cursor += 2;
            self.take_while(|byte| byte.is_ascii_digit() || byte == b'_');
        }

        let number_end = self.cursor;
        let suffix_text = self.take_while(is_name_continue).to_owned();
        let suffix = self.suffix(start, &suffix_text)?;
        if digits.len() > 1 && digits.starts_with('0') && !fraction && !exponent {
            return Err(self
                .error(start, "an integer cannot begin with 0")
                .with_hint("write 0o17 for octal or 17 for decimal"));
        }

        if fraction || exponent || matches!(suffix, Some(Suffix::F32 | Suffix::F64)) {
            if matches!(suffix, Some(Suffix::Int { .. } | Suffix::Size { .. })) {
                return Err(self.error(start, "a float literal cannot have an integer suffix"));
            }

            let text: String = self.text[start..number_end]
                .chars()
                .filter(|&character| character != '_')
                .collect();
            return self.float(start, &text, suffix);
        }

        self.integer(start, &digits, 10, suffix)
    }

    fn integer(
        &self,
        start: usize,
        digits: &str,
        radix: u32,
        suffix: Option<Suffix>,
    ) -> Result<TokenKind, ExpressionError> {
        let digits: String = digits
            .chars()
            .filter(|&character| character != '_')
            .collect();
        if digits.is_empty() {
            return Err(self.error(start, "expected digits"));
        }

        let value = u128::from_str_radix(&digits, radix)
            .map_err(|_| self.error(start, "an integer literal may have at most 128 bits"))?;
        Ok(TokenKind::Integer { value, suffix })
    }

    fn float(
        &self,
        start: usize,
        text: &str,
        suffix: Option<Suffix>,
    ) -> Result<TokenKind, ExpressionError> {
        let parsed = if suffix == Some(Suffix::F32) {
            text.parse::<f32>()
                .ok()
                .filter(|value| value.is_finite())
                .map(Float::from_f32)
        } else {
            text.parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
                .map(Float::from_f64)
        };

        parsed
            .map(TokenKind::Float)
            .ok_or_else(|| self.error(start, "the float literal is too large"))
    }

    fn suffix(&self, start: usize, suffix: &str) -> Result<Option<Suffix>, ExpressionError> {
        let parsed = match suffix {
            "" => return Ok(None),
            "f32" => Suffix::F32,
            "f64" => Suffix::F64,
            "isize" => Suffix::Size { signed: true },
            "usize" => Suffix::Size { signed: false },
            _ => {
                let suffix = suffix.strip_prefix('_').unwrap_or(suffix);
                if let Some(parsed) = int_suffix(suffix) {
                    return Ok(Some(parsed));
                }

                let error = self.error(start, format!("unknown literal suffix `{suffix}`"));
                return Err(
                    if suffix.chars().all(|character| "uUlLzZ".contains(character)) {
                        error.with_hint("C literal suffixes are not supported; cast instead, as in `17 as unsigned long`")
                    } else {
                        error
                    },
                );
            }
        };

        Ok(Some(parsed))
    }

    fn char_literal(&mut self, start: usize) -> Result<TokenKind, ExpressionError> {
        self.cursor += 1;
        let value = match self.peek() {
            None => return Err(self.error(start, "a character literal needs one character")),
            Some(b'\'') => {
                self.cursor += 1;
                return Err(self.error(start, "a character literal needs one character"));
            }
            Some(b'\\') => match self.escape(start)? {
                Escaped::Byte(byte) if byte > 0x7f => {
                    return Err(
                        self.error(start, "`\\x` in a character literal must be at most 7f")
                    );
                }
                Escaped::Byte(byte) => u32::from(byte),
                Escaped::Char(character) => u32::from(character),
            },
            Some(_) => {
                let character = self.rest().chars().next().unwrap_or_default();
                self.cursor += character.len_utf8();
                u32::from(character)
            }
        };

        if self.peek() != Some(b'\'') {
            let error = self.error(
                start,
                "a character literal holds one character and a closing `'`",
            );
            // Text in single quotes, as Python and JavaScript write it.
            let quoted = self
                .text
                .get(start + 1..)
                .and_then(|after| after.split_once('\''))
                .map(|(text, _)| text)
                .filter(|text| !text.contains(['"', '\\', '\n']));
            return Err(match quoted {
                Some(text) => {
                    error.with_hint(format!("text goes in double quotes, as in `\"{text}\"`"))
                }
                None => error,
            });
        }

        self.cursor += 1;
        Ok(TokenKind::Char(value))
    }

    fn text_literal(&mut self, start: usize) -> Result<TokenKind, ExpressionError> {
        self.cursor += 1;
        let mut bytes = Vec::new();
        loop {
            let before = self.cursor;
            match self.peek() {
                None => return Err(self.error(start, "a string needs a closing `\"`")),
                Some(b'"') => {
                    self.cursor += 1;
                    return Ok(TokenKind::Text(bytes));
                }
                Some(b'\\') => match self.escape(start)? {
                    Escaped::Byte(byte) => bytes.push(byte),
                    Escaped::Char(character) => {
                        let mut buffer = [0; 4];
                        bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                    }
                },
                Some(_) => {
                    let character = self.rest().chars().next().unwrap_or_default();
                    self.cursor += character.len_utf8();
                    let mut buffer = [0; 4];
                    bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                }
            }

            assert!(self.cursor > before, "the string lexer made no progress");
        }
    }

    /// Reads the escape at the cursor.
    fn escape(&mut self, start: usize) -> Result<Escaped, ExpressionError> {
        self.cursor += 1;
        let Some(byte) = self.peek() else {
            return Err(self.error(start, "an escape needs a character after `\\`"));
        };

        self.cursor += 1;
        Ok(match byte {
            b'n' => Escaped::Char('\n'),
            b'r' => Escaped::Char('\r'),
            b't' => Escaped::Char('\t'),
            b'0' => Escaped::Char('\0'),
            b'\\' => Escaped::Char('\\'),
            b'\'' => Escaped::Char('\''),
            b'"' => Escaped::Char('"'),
            b'x' => {
                let digits = self
                    .rest()
                    .get(..2)
                    .filter(|digits| digits.bytes().all(|byte| byte.is_ascii_hexdigit()));
                let Some(digits) = digits else {
                    return Err(self.error(start, "`\\x` needs two hexadecimal digits"));
                };

                let value = u8::from_str_radix(digits, 16).unwrap_or_default();
                self.cursor += 2;
                Escaped::Byte(value)
            }
            b'u' => {
                let body = self.rest().strip_prefix('{').and_then(|rest| {
                    let end = rest.find('}')?;
                    Some(&rest[..end])
                });
                let value = body
                    .filter(|digits| (1..=6).contains(&digits.len()))
                    .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                    .and_then(char::from_u32);
                let (Some(body), Some(value)) = (body, value) else {
                    return Err(self.error(
                        start,
                        "`\\u` needs a code point in braces, such as `\\u{e9}`",
                    ));
                };

                self.cursor += body.len() + 2;
                Escaped::Char(value)
            }
            _ => return Err(self.error(start, format!("unknown escape `\\{}`", char::from(byte)))),
        })
    }
}

/// What an escape in a literal denotes.
enum Escaped {
    /// `\xHH`, one byte.
    Byte(u8),
    Char(char),
}

/// `iN` or `uN` for N from 1 to 128.
fn int_suffix(text: &str) -> Option<Suffix> {
    let (signed, digits) = match text.as_bytes().first()? {
        b'i' => (true, &text[1..]),
        b'u' => (false, &text[1..]),
        _ => return None,
    };

    if digits.starts_with('0') {
        return None;
    }

    let width = digits
        .parse::<u8>()
        .ok()
        .filter(|&width| (1..=MAX_WIDTH).contains(&width))?;
    Some(Suffix::Int { width, signed })
}
