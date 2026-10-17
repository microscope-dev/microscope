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

/// What microscope assumes of every interpreter beyond the offsets its
/// layouts give: the width it reads each field at, and how a stack
/// reference tags what it holds. Each is checked by its name in the
/// headers, so a version that changes one fails here, not in a stack.
const ASSUMPTIONS: &str = r#"
#include <Python.h>
#include <stddef.h>
#include <stdint.h>
#include "internal/pycore_code.h"
#include "internal/pycore_dict.h"
#include "internal/pycore_long.h"
#include "internal/pycore_runtime.h"
#include "internal/pycore_interp.h"
#if PY_VERSION_HEX >= 0x030e0000
#include "internal/pycore_interpframe.h"
#include "internal/pycore_stackref.h"
#else
#include "internal/pycore_frame.h"
#endif

#define WIDTH(type, member, bytes)     _Static_assert(sizeof(((type *)0)->member) == bytes, #type "." #member " is not " #bytes " bytes")

/* Words: pointers, sizes, and ids, read as 8 bytes. */
WIDTH(PyObject, ob_type, 8);
WIDTH(PyVarObject, ob_size, 8);
WIDTH(PyInterpreterState, id, 8);
WIDTH(PyThreadState, native_thread_id, 8);
WIDTH(PyASCIIObject, length, 8);
WIDTH(PyLongObject, long_value.lv_tag, 8);
WIDTH(PyFloatObject, ob_fval, 8);
WIDTH(PyDictObject, ma_used, 8);
WIDTH(PyDictKeysObject, dk_nentries, 8);
WIDTH(PyDictKeyEntry, me_key, 8);
WIDTH(PyDictUnicodeEntry, me_value, 8);

/* Narrower fields, read at their own widths. */
WIDTH(_PyInterpreterFrame, owner, 1);
WIDTH(PyCodeObject, co_firstlineno, 4);
WIDTH(PyCodeObject, co_flags, 4);
WIDTH(PyCodeObject, co_argcount, 4);
WIDTH(PyCodeObject, co_kwonlyargcount, 4);
WIDTH(PyASCIIObject, state, 4);
WIDTH(PyDictKeysObject, dk_log2_size, 1);
WIDTH(PyDictKeysObject, dk_log2_index_bytes, 1);
WIDTH(PyDictKeysObject, dk_kind, 1);
WIDTH(_PyRuntimeState, initialized, 4);
WIDTH(_PyRuntimeState, _finalizing, 8);

#if PY_VERSION_HEX >= 0x030e0000
/* The PEP 768 control fields are written at exactly these widths. */
WIDTH(PyThreadState, eval_breaker, 8);
WIDTH(PyInterpreterState, config.remote_debug, 4);
WIDTH(_PyRemoteDebuggerSupport, debugger_pending_call, 4);
WIDTH(_PyRemoteDebuggerSupport, debugger_script_path[0], 1);
#endif

/* An int's digits are 4 bytes wide where they hold more than 16 bits. */
_Static_assert(sizeof(digit) == (PyLong_SHIFT > 16 ? 4 : 2), "an int's digit width");

#if defined(Py_GIL_DISABLED) && PY_VERSION_HEX >= 0x030e0000
/* A frame indexes its code's copies of the bytecode, an array of
   pointers after the array's size. */
WIDTH(_PyInterpreterFrame, tlbc_index, 4);
WIDTH(_PyCodeArray, size, 8);
WIDTH(_PyCodeArray, entries[0], 8);
#endif

#if PY_VERSION_HEX >= 0x030e0000
/* A stack reference's low two bits tag it: a pointer whose low bit
   counts references, or, with both set, a tagged integer. */
_Static_assert(Py_TAG_REFCNT == 1, "a stack reference's counting bit");
_Static_assert(Py_INT_TAG == 3, "a tagged integer's tag");
_Static_assert((Py_TAG_BITS & ~(uintptr_t)3) == 0, "a stack reference's tag bits");
#endif
"#;

/// Each interpreter's headers agree with what microscope assumes of it
/// beyond its layout.
#[test]
fn the_assumptions_beyond_the_layouts_are_the_headers() {
    let scratch = ScratchDir::new("python-assumptions");
    let file = scratch.path().join("assumptions.c");
    std::fs::write(&file, ASSUMPTIONS).expect("write the assumptions");
    for (name, directory) in interpreters() {
        let output = compile(&directory, &file);
        assert!(
            output.status.success(),
            "{name}'s headers disagree with microscope's assumptions:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Checks the C file `file` against the headers of the interpreter in
/// `directory`, as CPython's own core code sees them.
fn compile(directory: &Path, file: &Path) -> std::process::Output {
    let mut compile = Command::new("cc");
    compile.args(["-std=c11", "-fsyntax-only", "-w", "-DPy_BUILD_CORE=1"]);
    let arch = directory.join("include-arch");
    if arch.is_dir() {
        compile.arg("-I").arg(arch);
    }
    compile
        .arg("-I")
        .arg(directory.join("include"))
        .arg(file)
        .output()
        .expect("run cc")
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
        let output = compile(&directory, &file);
        assert!(
            output.status.success(),
            "{name}'s headers disagree with its schema:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
