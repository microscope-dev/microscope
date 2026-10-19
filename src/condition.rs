//! Breakpoint and watchpoint conditions, and breakpoint log messages, in the
//! expression language (`docs/expressions.md`).
//!
//! A condition is an expression with a truth value, which cannot assign;
//! `&&` and `||` short-circuit, so `p != null && p->x > 3` is safe. A log
//! message is text with expressions in braces: `x = {x}, sum = {a + b}`.

use std::fmt;
use std::sync::Arc;

use crate::eval::syntax::Expression;
use crate::eval::syntax::ast::NodeKind;
use crate::{Error, Result};

/// The longest condition or log message accepted, in bytes.
const MAX_TEXT_BYTES: usize = 4096;

/// A parsed breakpoint or watchpoint condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    expression: Expression,
}

/// Parses an expression a breakpoint reads, which may not assign.
fn read_only(text: &str, invalid: fn(String) -> Error) -> Result<Expression> {
    let expression = Expression::parse(text).map_err(|error| invalid(error.to_string()))?;

    // An assignment anywhere in any reading, not only at its root.
    let assigns = (0..1 << expression.ambiguities().len()).any(|casts| {
        expression.reading(casts).is_ok_and(|tree| {
            let mut pending = vec![tree.root()];
            while let Some(node) = pending.pop() {
                let kind = tree.kind(node);
                if matches!(kind, NodeKind::Assign { .. }) {
                    return true;
                }

                pending.extend(kind.children());
            }

            false
        })
    });
    if assigns {
        return Err(invalid(
            "conditions and log messages cannot assign; compare with `==`".to_owned(),
        ));
    }

    Ok(expression)
}

impl Condition {
    /// Parses a condition.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::InvalidCondition(
                "a condition must not be empty".into(),
            ));
        }

        Ok(Self {
            expression: read_only(text, Error::InvalidCondition)?,
        })
    }

    /// The condition's expression.
    #[must_use]
    pub const fn expression(&self) -> &Expression {
        &self.expression
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.expression.text())
    }
}

/// A logpoint's message: text with values interpolated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogMessage {
    text: Arc<str>,
    segments: Arc<[LogSegment]>,
}

/// One piece of a log message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogSegment {
    /// Literal text.
    Text(Arc<str>),

    /// An expression whose value is shown in its place.
    Value(Expression),
}

impl LogMessage {
    /// Parses a message whose `{expression}` parts show values; `{{` and
    /// `}}` stand for braces.
    pub fn parse(text: &str) -> Result<Self> {
        let invalid = |description: &str| Error::InvalidLogMessage(description.to_owned());
        if text.len() > MAX_TEXT_BYTES {
            return Err(invalid("the message is too long"));
        }

        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut rest = text;

        while let Some(index) = rest.find(['{', '}']) {
            literal.push_str(&rest[..index]);
            let brace = &rest[index..];
            if brace.starts_with("{{") || brace.starts_with("}}") {
                literal.push_str(&brace[..1]);
                rest = &brace[2..];
                continue;
            }

            if brace.starts_with('}') {
                return Err(invalid("a '}' has no '{'; write '}}' for a brace"));
            }

            let Some(end) = brace.find('}') else {
                return Err(invalid("a '{' has no '}'; write '{{' for a brace"));
            };

            let expression = read_only(brace[1..end].trim(), Error::InvalidLogMessage)?;

            if !literal.is_empty() {
                segments.push(LogSegment::Text(std::mem::take(&mut literal).into()));
            }

            segments.push(LogSegment::Value(expression));
            rest = &brace[end + 1..];
        }

        literal.push_str(rest);
        if !literal.is_empty() {
            segments.push(LogSegment::Text(literal.into()));
        }

        Ok(Self {
            text: text.into(),
            segments: segments.into(),
        })
    }

    /// The message's parts in order.
    #[must_use]
    pub fn segments(&self) -> &[LogSegment] {
        &self.segments
    }
}

impl fmt::Display for LogMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_and_messages_hold_expressions_that_do_not_assign() {
        assert_eq!(
            Condition::parse(" a + 1 > b ")
                .expect("a condition")
                .to_string(),
            "a + 1 > b"
        );
        for text in ["", "a = 1", "a += 1", "a +", "a == (b = 1)", "(a = 1) && b"] {
            assert!(Condition::parse(text).is_err(), "`{text}`");
        }

        let message = LogMessage::parse("sum {a + b}, {{literal}} {p->x}").expect("a message");
        let parts: Vec<String> = message
            .segments()
            .iter()
            .map(|segment| match segment {
                LogSegment::Text(text) => format!("text {text}"),
                LogSegment::Value(expression) => format!("value {}", expression.text()),
            })
            .collect();
        assert_eq!(
            parts,
            [
                "text sum ",
                "value a + b",
                "text , {literal} ",
                "value p->x"
            ]
        );
        for text in ["{a = 1}", "{", "}", "{a +}", "{a + (b -= 1)}"] {
            assert!(LogMessage::parse(text).is_err(), "`{text}`");
        }
    }
}
