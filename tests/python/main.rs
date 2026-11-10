//! Python programs, run by each interpreter in the matrix and debugged
//! natively. Each fixture reports the truth about itself in `TRUTH` lines
//! (see `tests/fixtures/python/truth.py`), computed by the interpreter the
//! debugger inspects, and every assertion compares the debugger against it.

#[path = "../support/mod.rs"]
mod support;

mod agent;
mod arrays;
mod assignments;
mod bootstrap;
mod breakpoints;
mod expressions;
mod extension;
mod gallery;
mod group;
mod jit;
mod layouts;
mod locals;
mod oracles;
mod profile;
mod session;
mod stacks;
mod stepping;
mod syscalls;
mod tasks;
mod views;
mod watches;
