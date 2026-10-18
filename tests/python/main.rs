//! Python programs, run by each interpreter in the matrix and debugged
//! natively. Each fixture reports the truth about itself in `TRUTH` lines
//! (see `tests/fixtures/python/truth.py`), computed by the interpreter the
//! debugger inspects, and every assertion compares the debugger against it.

#[path = "../support/mod.rs"]
mod support;

// The schemas the provider reads each version's table by, which the
// layout tests check against the headers.
#[path = "../../src/debug_info/cpython/schemas.rs"]
#[allow(
    clippy::redundant_pub_crate,
    reason = "the provider's module, whose items its parent reads"
)]
mod schemas;

mod agent;
mod arrays;
mod breakpoints;
mod expressions;
mod extension;
mod gallery;
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
