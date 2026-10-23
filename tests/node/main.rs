//! Node programs, run by each build in the matrix and debugged natively.
//! Each fixture reports the truth about itself in `TRUTH` lines (see
//! `tests/fixtures/node/truth.js`), computed by the V8 the debugger
//! inspects, and every assertion compares against it.

#[path = "../support/mod.rs"]
mod support;

mod debian;
mod frames;
mod native;
mod session;
mod stacks;
mod truth;
mod values;
