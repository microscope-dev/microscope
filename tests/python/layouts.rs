//! The layouts microscope reads interpreters by, checked against the
//! headers of every interpreter in the matrix.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::schemas;
use crate::support::ScratchDir;

const GENERATED: &str = "src/debug_info/cpython/layouts.gen.rs";

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Each interpreter's directory, by name.
fn interpreters() -> Vec<(String, PathBuf)> {
    let root = std::env::var_os("MICROSCOPE_CPYTHON")
        .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell");
    let mut interpreters: Vec<(String, PathBuf)> = std::fs::read_dir(&root)
        .expect("list the interpreters")
        .map(|entry| {
            let entry = entry.expect("read an interpreter's entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect();
    interpreters.sort();
    assert!(!interpreters.is_empty(), "no interpreters in the matrix");
    interpreters
}

/// Fails when the checked-in layouts are not what the script makes of the
/// matrix's headers. Run `scripts/cpython-layouts.sh` to make them again.
#[test]
fn the_generated_layouts_are_the_headers() {
    let scratch = ScratchDir::new("python-layouts");
    let made = scratch.path().join("layouts.gen.rs");
    let output = Command::new("bash")
        .arg(root().join("scripts/cpython-layouts.sh"))
        .arg(&made)
        .output()
        .expect("run scripts/cpython-layouts.sh");
    assert!(
        output.status.success(),
        "scripts/cpython-layouts.sh failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let made = std::fs::read_to_string(&made).expect("read the made layouts");
    let checked_in = std::fs::read_to_string(root().join(GENERATED)).unwrap_or_default();
    assert!(
        made == checked_in,
        "{GENERATED} is stale; rerun scripts/cpython-layouts.sh in the dev shell"
    );
}

/// Each minor version's `_Py_DebugOffsets` holds the names its schema
/// lists, at the offsets the schema puts them, and nothing more.
#[test]
fn each_schema_is_its_versions_debug_offsets() {
    let mut source = String::from(
        "#include <Python.h>\n\
         #include <stddef.h>\n\
         #include \"internal/pycore_runtime.h\"\n\
         #if PY_MINOR_VERSION < 13\n\
         /* No table before 3.13. */\n",
    );
    for (minor, schema) in [
        (13, schemas::V3_13),
        (14, schemas::V3_14),
        (15, schemas::V3_15),
    ] {
        let _ = writeln!(source, "#elif PY_MINOR_VERSION == {minor}");
        for (index, name) in schema.iter().enumerate() {
            let _ = writeln!(
                source,
                "_Static_assert(offsetof(_Py_DebugOffsets, {name}) == {}, \"3.{minor} {name}\");",
                24 + 8 * index
            );
        }
        let _ = writeln!(
            source,
            "_Static_assert(sizeof(_Py_DebugOffsets) == {}, \"3.{minor} holds more\");",
            24 + 8 * schema.len()
        );
    }
    source.push_str("#else\n#error \"no schema of this version\"\n#endif\n");

    let scratch = ScratchDir::new("python-schemas");
    let file = scratch.path().join("schemas.c");
    std::fs::write(&file, &source).expect("write the schema check");
    for (name, directory) in interpreters() {
        let mut compile = Command::new("cc");
        compile.args(["-std=c11", "-fsyntax-only", "-w", "-DPy_BUILD_CORE=1"]);
        let arch = directory.join("include-arch");
        if arch.is_dir() {
            compile.arg("-I").arg(arch);
        }
        let output = compile
            .arg("-I")
            .arg(directory.join("include"))
            .arg(&file)
            .output()
            .expect("run cc");
        assert!(
            output.status.success(),
            "{name}'s headers disagree with its schema:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
