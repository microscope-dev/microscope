//! Node programs, run by each build in the matrix and debugged natively.
//! Each fixture reports the truth about itself in `TRUTH` lines (see
//! `tests/fixtures/node/truth.js`), computed by the V8 the debugger
//! inspects, and every assertion compares against it.

#[path = "../support/mod.rs"]
mod support;

mod addon;
mod breakpoints;
mod debian;
mod exceptions;
mod frames;
mod inspector;
mod native;
mod oracle;
mod session;
mod stacks;
mod steps;
mod truth;
mod values;
mod views;
