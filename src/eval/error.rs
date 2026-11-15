use std::fmt;

use super::syntax::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    Syntax,

    Limit,

    UnknownName,

    AmbiguousName,

    Type,

    Bounds,

    MissingKey,

    Arithmetic,

    NotAnLvalue,

    Mode,

    Assignment,

    Unsupported,
}

impl ErrorKind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Limit => "limit",
            Self::UnknownName => "unknown-name",
            Self::AmbiguousName => "ambiguous-name",
            Self::Type => "type",
            Self::Bounds => "bounds",
            Self::MissingKey => "missing-key",
            Self::Arithmetic => "arithmetic",
            Self::NotAnLvalue => "not-an-lvalue",
            Self::Mode => "mode",
            Self::Assignment => "assignment",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionError {
    pub kind: ErrorKind,

    pub span: Span,
    pub message: String,

    pub hint: Option<String>,
}

impl ExpressionError {
    pub fn new(kind: ErrorKind, span: Span, message: impl Into<String>) -> Self {
        Self {
            kind,
            span,
            message: message.into(),
            hint: None,
        }
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn syntax(span: Span, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Syntax, span, message)
    }
}

impl fmt::Display for ExpressionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)?;
        if let Some(hint) = &self.hint {
            write!(formatter, " ({hint})")?;
        }

        Ok(())
    }
}

impl std::error::Error for ExpressionError {}
