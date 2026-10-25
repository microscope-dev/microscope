//! Node programs, run by each build in the matrix and debugged natively.
//! Each fixture reports the truth about itself in `TRUTH` lines (see
//! `tests/fixtures/node/truth.js`), computed by the V8 the debugger
//! inspects, and every assertion compares against it.

#[path = "../support/mod.rs"]
mod support;

mod addon;
mod addon_steps;
mod assignments;
mod attach;
mod awaits;
mod breakpoints;
mod debian;
mod exceptions;
mod frames;
mod functions;
mod inspector;
mod interrupts;
mod live;
mod maps;
mod native;
mod oracle;
mod profile;
mod session;
mod stacks;
mod statements;
mod steps;
mod syscalls;
mod tasks;
mod truth;
mod values;
mod views;
mod watches;
mod workers;
