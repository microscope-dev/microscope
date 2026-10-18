//! The expression language: one small, exact language for reading and
//! changing program state, shared by every source language
//! (`docs/expressions.md`).
//!
//! This module is pure. It reaches a program only through the traits the
//! debugger implements for it, and never performs I/O, reads clocks, or
//! starts threads.

pub mod bind;
mod dynamic;
pub mod error;
#[cfg(any(test, feature = "fuzzing"))]
pub mod fake;
pub mod interp;
pub mod ir;
pub mod number;
pub mod syntax;
pub mod target;
pub mod types;

/// What evaluating an expression at a stop produced.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[expect(
    clippy::large_enum_variant,
    reason = "one evaluation is returned per request and moved, not stored"
)]
pub enum Evaluation {
    /// A value, or the unavailable state that stands for it, with the part
    /// of the expression whose value the program state could not provide.
    Value {
        value: crate::InspectedValue,
        cause: Option<syntax::Span>,
    },

    /// `base[start..end]`: the elements of an array or slice in a
    /// half-open range of source indices.
    Range(crate::ValueChildPage),
}

#[cfg(test)]
mod tests;
