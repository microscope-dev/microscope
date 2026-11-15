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
