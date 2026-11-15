use std::path::PathBuf;

pub fn interpreter(name: &str) -> PathBuf {
    let root = std::env::var_os("MICROSCOPE_CPYTHON")
        .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell");
    PathBuf::from(root).join(name).join("python")
}

pub fn debug_directory(name: &str) -> Option<PathBuf> {
    let directory = interpreter(name).with_file_name("debug").join("lib/debug");
    directory.is_dir().then_some(directory)
}

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/python")
        .join(name)
}
