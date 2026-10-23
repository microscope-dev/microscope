//! The Node builds Node support is tested on, and the Node fixtures, which
//! run from their sources.

use std::path::PathBuf;

/// The executable of the build the flake names `name`, as `26` or
/// `nixpkgs-24`.
pub fn node(name: &str) -> PathBuf {
    let root = std::env::var_os("MICROSCOPE_NODE")
        .expect("MICROSCOPE_NODE names the Node builds; run tests in the dev shell");
    PathBuf::from(root).join(name).join("node")
}

/// The source of the Node fixture `name`, as `stacks.js`.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/node")
        .join(name)
}
