//! The CPython interpreters Python support is tested on, and the Python
//! fixtures, which run from their sources.

use std::path::PathBuf;

/// The interpreter the flake names `name`, as `3.13` or `3.14t`.
pub fn interpreter(name: &str) -> PathBuf {
    let root = std::env::var_os("MICROSCOPE_CPYTHON")
        .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell");
    PathBuf::from(root).join(name).join("python")
}

/// The source of the Python fixture `name`, as `perf.py`.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/python")
        .join(name)
}
