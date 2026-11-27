//! Context-free expression parser, AST, and round-tripping printer.
//!
//! Parsing does not consult a symbol table. Because C-style casts like
//! `(T) - x`, `(T) * x`, and `(T) & x` are syntactically ambiguous with binary
//! operators until `T` is resolved as a type or value, [`Expression`] pre-parses
//! all `2^N` interpretations (capped by [`MAX_AMBIGUITIES`]) and lets `bind`
//! select the matching reading via a bitmask.

pub mod ast;
mod lexer;
mod parser;
mod print;

use std::fmt;
use std::sync::Arc;

use ast::Path;
pub use ast::Tree;
pub(super) use parser::binary_text;

pub(super) use print::path_text;

use super::error::ExpressionError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    #[must_use]
    pub fn new(start: usize, end: usize) -> Self {
        let offset = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
        Self {
            start: offset(start),
            end: offset(end),
        }
    }

    #[must_use]
    pub fn to(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    #[must_use]
    pub fn text(self, text: &str) -> &str {
        text.get(self.start as usize..self.end as usize)
            .unwrap_or_default()
    }
}

/// Syntax dialect controlling which built-ins and forms are accepted.
///
/// `Console` accepts user REPL/watch expressions (including tuple keys for map
/// indexing). `View` is used inside `.views` files, enabling low-level layout
/// intrinsics (`offsetof`, `container_of`, `global`, `inner`) and forbidding
/// tuples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Console,

    View,
}

pub const MAX_AMBIGUITIES: usize = 4;

#[derive(Debug, Clone)]
pub struct Ambiguity {
    pub name: Path,

    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Expression {
    parsed: Arc<Parsed>,
}

#[derive(Debug)]
struct Parsed {
    text: String,
    ambiguities: Vec<Ambiguity>,

    readings: Vec<Result<Tree, ExpressionError>>,
}

impl Expression {
    pub fn parse(text: &str) -> Result<Self, ExpressionError> {
        Self::parse_in(text, Dialect::Console)
    }

    pub fn parse_view(text: &str) -> Result<Self, ExpressionError> {
        Self::parse_in(text, Dialect::View)
    }

    fn parse_in(text: &str, dialect: Dialect) -> Result<Self, ExpressionError> {
        let (ambiguities, readings) = parser::parse(text, dialect)?;
        Ok(Self {
            parsed: Arc::new(Parsed {
                text: text.to_owned(),
                ambiguities,
                readings,
            }),
        })
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.parsed.text
    }

    #[must_use]
    pub fn ambiguities(&self) -> &[Ambiguity] {
        &self.parsed.ambiguities
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "the parser makes `1 << ambiguities` readings, and callers choose one"
    )]
    pub fn reading(&self, casts: usize) -> Result<&Tree, &ExpressionError> {
        debug_assert!(
            casts < self.parsed.readings.len(),
            "reading {casts} of {}",
            self.parsed.readings.len(),
        );
        self.parsed.readings[casts].as_ref()
    }

    #[must_use]
    pub fn tree(&self) -> Option<&Tree> {
        match self.parsed.readings.as_slice() {
            [Ok(tree)] => Some(tree),
            _ => None,
        }
    }

    #[must_use]
    pub fn name(name: &str) -> Option<Self> {
        Self::parse(&print::name_text(name)).ok()
    }

    #[must_use]
    pub fn outermost(name: &str) -> Option<Self> {
        Self::parse(&format!("::{}", print::name_text(name))).ok()
    }

    #[must_use]
    pub fn member(&self, name: &str) -> Option<Self> {
        Self::parse(&format!("({self}).{}", print::name_text(name))).ok()
    }

    #[must_use]
    pub fn indexed(&self, indices: &[i128]) -> Option<Self> {
        let indices = indices.iter().fold(String::new(), |mut text, index| {
            use std::fmt::Write as _;
            let _ = write!(text, "[{index}]");
            text
        });
        Self::parse(&format!("({self}){indices}")).ok()
    }

    #[must_use]
    pub fn dereferenced(&self) -> Option<Self> {
        Self::parse(&format!("*({self})")).ok()
    }

    #[must_use]
    pub fn at(ty: &str, address: u64) -> Option<Self> {
        Self::parse(&format!("*({ty}*){address:#x}"))
            .ok()
            .or_else(|| Self::parse(&format!("*({}*){address:#x}", print::name_text(ty))).ok())
    }

    #[must_use]
    pub fn range(&self, start: i128, end: i128) -> Option<Self> {
        Self::parse(&format!("({self})[{start}..{end}]")).ok()
    }

    #[must_use]
    pub fn is_name(&self) -> bool {
        self.tree()
            .is_some_and(|tree| matches!(tree.kind(tree.root()), ast::NodeKind::Name(_)))
    }

    #[must_use]
    pub fn range_base(&self) -> Option<Self> {
        let tree = self.tree()?;
        match tree.kind(tree.root()) {
            ast::NodeKind::Range { base, .. } | ast::NodeKind::Slice { base, .. } => {
                Self::parse(tree.span(*base).text(self.text())).ok()
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn assignment_target(&self) -> Option<&str> {
        let tree = self.tree()?;
        match tree.kind(tree.root()) {
            ast::NodeKind::Assign { target, .. } => Some(tree.span(*target).text(self.text())),
            _ => None,
        }
    }

    #[must_use]
    pub fn assigned_name(&self) -> Option<&str> {
        let tree = self.tree()?;
        match tree.kind(tree.root()) {
            ast::NodeKind::Assign {
                op: None, value, ..
            } => match tree.kind(*value) {
                ast::NodeKind::Name(_) => Some(tree.span(*value).text(self.text())),
                _ => None,
            },
            _ => None,
        }
    }

    #[must_use]
    pub fn same_shape(&self, other: &Self) -> bool {
        self.parsed.ambiguities.len() == other.parsed.ambiguities.len()
            && self
                .parsed
                .ambiguities
                .iter()
                .zip(&other.parsed.ambiguities)
                .all(|(left, right)| left.name == right.name)
            && self
                .parsed
                .readings
                .iter()
                .zip(&other.parsed.readings)
                .all(|pair| match pair {
                    (Ok(left), Ok(right)) => left.same_shape(right),
                    (Err(left), Err(right)) => left.kind == right.kind,
                    _ => false,
                })
    }
}

impl PartialEq for Expression {
    fn eq(&self, other: &Self) -> bool {
        self.parsed.text == other.parsed.text
    }
}

impl Eq for Expression {}

impl fmt::Display for Expression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.tree() {
            Some(tree) => formatter.write_str(&print::print(tree)),
            None => formatter.write_str(self.parsed.text.trim()),
        }
    }
}

#[cfg(any(test, feature = "fuzzing"))]
pub fn check_invariants(text: &str) -> Result<(), String> {
    check_invariants_in(text, Dialect::Console)?;
    check_invariants_in(text, Dialect::View)
}

#[cfg(any(test, feature = "fuzzing"))]
fn check_invariants_in(text: &str, dialect: Dialect) -> Result<(), String> {
    let expression = match Expression::parse_in(text, dialect) {
        Ok(expression) => expression,
        Err(error)
            if error.span.start <= error.span.end
                && error.span.end as usize <= text.len().max(1) =>
        {
            return Ok(());
        }
        Err(error) => return Err(format!("{error:?} points outside `{text}`")),
    };

    for casts in 0..1 << expression.ambiguities().len() {
        if let Ok(tree) = expression.reading(casts) {
            tree.check_spans(text)?;
        }
    }

    let printed = expression.to_string();
    let reparsed = match Expression::parse_in(&printed, dialect) {
        Ok(reparsed) => reparsed,
        Err(error) if error.kind == super::error::ErrorKind::Limit => return Ok(()),
        Err(error) => return Err(format!("`{printed}` from `{text}`: {error}")),
    };

    if !reparsed.same_shape(&expression) {
        return Err(format!(
            "`{text}` printed as `{printed}`, which reads differently"
        ));
    }

    if reparsed.to_string() != printed {
        return Err(format!("`{printed}` printed again as `{reparsed}`"));
    }

    Ok(())
}

#[cfg(test)]
mod tests;
