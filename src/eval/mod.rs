//! Expression language for inspecting and mutating debuggee state.
//!
//! Expressions flow through `syntax` (context-free parsing), `bind` (name and
//! type resolution against a [`target::Scope`]), `ir` (typed operations), and
//! `interp` (side-effect-free execution against a [`target::Machine`]). Target
//! code is never executed; assignments return encoded bytes for the caller to
//! write.

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

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[expect(
    clippy::large_enum_variant,
    reason = "one evaluation is returned per request and moved, not stored"
)]
pub enum Evaluation {
    Value {
        value: crate::InspectedValue,
        cause: Option<syntax::Span>,
    },

    Range(crate::ValueChildPage),
}

#[cfg(test)]
mod tests;
