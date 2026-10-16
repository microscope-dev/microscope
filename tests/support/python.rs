//! The CPython interpreters Python support is tested on, and the Python
//! fixtures, which run from their sources.

use std::path::PathBuf;

/// The interpreter the flake names `name`, as `3.13` or `3.14t`.
pub fn interpreter(name: &str) -> PathBuf {
    let root = std::env::var_os("MICROSCOPE_CPYTHON")
        .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell");
    PathBuf::from(root).join(name).join("python")
}

/// Where the separate debug files of the interpreter `name` are, for an
/// interpreter whose build has them.
pub fn debug_directory(name: &str) -> Option<PathBuf> {
    let directory = interpreter(name).with_file_name("debug").join("lib/debug");
    directory.is_dir().then_some(directory)
}

/// The source of the Python fixture `name`, as `perf.py`.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/python")
        .join(name)
}
