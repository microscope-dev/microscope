use std::path::PathBuf;

pub fn node(name: &str) -> PathBuf {
    let root = std::env::var_os("MICROSCOPE_NODE")
        .expect("MICROSCOPE_NODE names the Node builds; run tests in the dev shell");
    PathBuf::from(root).join(name).join("node")
}

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/node")
        .join(name)
}
