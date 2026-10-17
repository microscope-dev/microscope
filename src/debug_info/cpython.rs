//! CPython, as the module that carries it describes it: an image is a
//! Python runtime when it has a `.PyRuntime` section, as `libpython3.x.so`
//! has, and an executable that links CPython statically or embeds it.
//!
//! CPython 3.13 and later begin that section with `_Py_DebugOffsets`, a
//! table of their own structures' offsets, which a debugger reads under
//! a schema for each minor version ([`schemas`]). Go's runtime model never
//! takes offsets from a table of versions; CPython describes itself in
//! no DWARF a release ships with, so this provider does, where the target
//! describes nothing: all of 3.12, which has no table, and the few fields
//! and numbers the tables of later versions leave out. Those come from
//! [`layouts`], generated from each interpreter's headers in the matrix,
//! and a version microscope has no layout of is refused by name, never
//! read by the nearest one's.
//!
//! The provider also gives the evaluation loop and the call machinery
//! around it their code role.

mod schemas;

#[rustfmt::skip]
#[path = "cpython/layouts.gen.rs"]
mod layouts;

#[cfg(test)]
#[path = "cpython/conformance.rs"]
mod conformance;

use std::sync::Arc;

use object::{Object, ObjectSection, ObjectSymbol};

use crate::image::python::{
    Constant, FREE_THREADED, Field, GENERATED_TABLE, MAX_STRUCTURE, PyVersion, PythonLayout,
    PythonRuntime, TARGET_TABLE,
};
use crate::{AddressRange, CodeRole, FunctionInfo, ImageAddress, SymbolInfo};

/// The section CPython's runtime state is in, which it names so that
/// debuggers can find it.
pub(super) const RUNTIME_SECTION: &str = ".PyRuntime";

/// What `_Py_DebugOffsets` begins with.
const COOKIE: &[u8; 8] = b"xdebugpy";

/// Where the table's offsets begin: past the cookie, the version, and
/// the free-threaded flag.
const TABLE_START: usize = 24;

/// A layout generated from one interpreter's headers.
pub(super) struct Generated {
    /// The interpreter's name in the matrix, as `3.12`.
    pub name: &'static str,

    /// `PY_VERSION_HEX`.
    pub version: u32,
    pub free_threaded: bool,
    pub fields: &'static [(Field, u32)],
    pub constants: &'static [(Constant, u8)],
}

impl Generated {
    fn layout(&self) -> PythonLayout {
        let mut layout = PythonLayout::default();
        for &(field, offset) in self.fields {
            layout.set_field(field, u64::from(offset));
        }

        for &(constant, value) in self.constants {
            layout.set_constant(constant, value);
        }

        layout
    }
}

/// The layout generated for the interpreter the matrix names `name`, as
/// `3.14t`.
#[cfg(test)]
pub(super) fn generated(name: &str) -> Option<PythonLayout> {
    layouts::GENERATED
        .iter()
        .find(|generated| generated.name == name)
        .map(Generated::layout)
}

/// The fields without which no thread's frames can be read.
const REQUIRED: [Field; 34] = [
    Field::RuntimeInterpretersHead,
    Field::InterpreterNext,
    Field::InterpreterThreadsHead,
    Field::ThreadSize,
    Field::ThreadNext,
    Field::ThreadCurrentFrame,
    Field::ThreadNativeId,
    Field::FrameSize,
    Field::FramePrevious,
    Field::FrameExecutable,
    Field::FrameInstruction,
    Field::FrameLocalsplus,
    Field::FrameOwner,
    Field::CodeSize,
    Field::CodeFilename,
    Field::CodeQualname,
    Field::CodeLinetable,
    Field::CodeFirstlineno,
    Field::CodeCodeAdaptive,
    Field::CodeLocalsplusnames,
    Field::CodeLocalspluskinds,
    Field::CodeArgcount,
    Field::CodeKwonlyargcount,
    Field::CodeFlags,
    Field::ObjectType,
    Field::VarObjectSize,
    Field::TupleItems,
    Field::TupleSize,
    Field::BytesSize,
    Field::BytesData,
    Field::UnicodeState,
    Field::UnicodeLength,
    Field::UnicodeAsciiSize,
    Field::UnicodeCompactSize,
];

/// Whether the module carries a Python runtime.
pub(super) fn carries_runtime(object: &object::File<'_>) -> bool {
    runtime_section(object).is_some()
}

fn runtime_section<'data, 'file>(
    object: &'file object::File<'data>,
) -> Option<object::Section<'data, 'file>> {
    object
        .section_by_name(RUNTIME_SECTION)
        .filter(|section| section.size() > 0)
}

/// The Python runtime the module carries, read from the module's own
/// file: a separate debug file keeps no data sections' bytes.
pub(super) fn runtime(object: &object::File<'_>) -> Option<PythonRuntime> {
    let section = runtime_section(object)?;
    let runtime = AddressRange {
        start: ImageAddress::new(section.address()),
        end: ImageAddress::new(section.address().saturating_add(section.size())),
    };
    let bytes = section.data().ok().filter(|bytes| !bytes.is_empty());
    let (version, flags, layout) = describe(bytes, py_version(object));
    Some(PythonRuntime {
        version,
        flags,
        runtime,
        layout: layout.and_then(|layout| validated(version, flags & FREE_THREADED != 0, layout)),
    })
}

/// The runtime's version, how its layout was found, and the layout or
/// why it cannot be: from `bytes`, the `.PyRuntime` section's, when it
/// holds a table, and `symbol`, the module's `Py_Version`.
fn describe(
    bytes: Option<&[u8]>,
    symbol: Option<u32>,
) -> (PyVersion, u16, Result<PythonLayout, Arc<str>>) {
    let Some(table) = bytes.filter(|bytes| bytes.starts_with(COOKIE)) else {
        let Some(version) = symbol.map(PyVersion) else {
            return (
                PyVersion(0),
                0,
                Err(
                    "it has no offsets table and names no Py_Version, so its version is unknown"
                        .into(),
                ),
            );
        };

        let layout = if version.minor() >= 13 {
            Err(format!("CPython {version} has no offsets table in its .PyRuntime section").into())
        } else {
            generated_exactly(version, false)
        };
        return (version, GENERATED_TABLE, layout);
    };

    let word = |at: usize| {
        table
            .get(at..at + 8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes)
    };
    let (Some(version), Some(free_threaded)) = (word(8), word(16)) else {
        return (
            PyVersion(0),
            TARGET_TABLE,
            Err("its offsets table is cut short".into()),
        );
    };

    let Ok(version) = u32::try_from(version).map(PyVersion) else {
        return (
            PyVersion(0),
            TARGET_TABLE,
            Err(format!("its offsets table names version {version:#x}, which is none").into()),
        );
    };

    let free_threaded = free_threaded != 0;
    let flags = TARGET_TABLE | GENERATED_TABLE | if free_threaded { FREE_THREADED } else { 0 };
    if let Some(symbol) = symbol.filter(|symbol| *symbol != version.0) {
        let named = PyVersion(symbol);
        return (
            version,
            flags,
            Err(format!("its Py_Version, {named}, is not its offsets table's, {version}").into()),
        );
    }

    (version, flags, from_table(version, free_threaded, table))
}

/// The layout a 3.13 or later runtime's own table gives, completed by
/// the layout generated for its minor version.
fn from_table(
    version: PyVersion,
    free_threaded: bool,
    table: &[u8],
) -> Result<PythonLayout, Arc<str>> {
    let build = if free_threaded { "free-threaded " } else { "" };
    let schema = match version.minor() {
        13 => schemas::V3_13,
        14 => schemas::V3_14,
        15 => schemas::V3_15,
        _ => return Err(unsupported(version)),
    };

    // A prerelease's table may differ from its final release's, as
    // 3.15.0b3's lacks a field 3.15.0's has.
    let mut layout = if version.is_final() {
        layouts::GENERATED
            .iter()
            .find(|generated| {
                PyVersion(generated.version).minor() == version.minor()
                    && generated.free_threaded == free_threaded
            })
            .map(Generated::layout)
            .ok_or_else(|| -> Arc<str> {
                format!(
                    "microscope has no layout of {build}CPython 3.{}",
                    version.minor()
                )
                .into()
            })?
    } else {
        generated_exactly(version, free_threaded)?
    };

    let end = TABLE_START + 8 * schema.len();
    let offsets = table
        .get(TABLE_START..end)
        .ok_or_else(|| -> Arc<str> { "its offsets table is cut short".into() })?;
    for (name, bytes) in schema.iter().zip(offsets.as_chunks::<8>().0) {
        let Some(field) = Field::named(name) else {
            continue;
        };

        // Only a free-threaded build has thread-local bytecode, or a GIL
        // that may be off: a build with the GIL names 0 for those fields,
        // which it lacks.
        if !free_threaded
            && matches!(
                field,
                Field::FrameTlbcIndex | Field::CodeTlbc | Field::GilEnabled
            )
        {
            continue;
        }

        let offset = u64::from_le_bytes(*bytes);
        if u32::try_from(offset).is_err() {
            return Err(format!("its table puts {name} at {offset}, past any structure").into());
        }

        layout.set_field(field, offset);
    }

    Ok(layout)
}

/// The layout generated for exactly `version`.
fn generated_exactly(version: PyVersion, free_threaded: bool) -> Result<PythonLayout, Arc<str>> {
    layouts::GENERATED
        .iter()
        .find(|generated| {
            generated.version == version.0 && generated.free_threaded == free_threaded
        })
        .map(Generated::layout)
        .ok_or_else(|| {
            if (12..=15).contains(&version.minor()) {
                let known = layouts::GENERATED
                    .iter()
                    .filter(|generated| {
                        PyVersion(generated.version).minor() == version.minor()
                            && generated.free_threaded == free_threaded
                    })
                    .map(|generated| {
                        format!("{} ({})", PyVersion(generated.version), generated.name)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "CPython {version} is not a release microscope has checked the layout of; \
                     it has checked {known}"
                )
                .into()
            } else {
                unsupported(version)
            }
        })
}

fn unsupported(version: PyVersion) -> Arc<str> {
    format!("CPython {version} is not supported: microscope reads 3.12 through 3.15").into()
}

/// The fields a free-threaded build since 3.14 needs as well, which find
/// the copy of the bytecode a frame runs.
const THREAD_LOCAL_BYTECODE: [Field; 4] = [
    Field::FrameTlbcIndex,
    Field::CodeTlbc,
    Field::CodeArraySize,
    Field::CodeArrayEntries,
];

/// The sizes of the structures the model reads whole, which a table in
/// the process may make anything.
const READ_WHOLE: [Field; 6] = [
    Field::ThreadSize,
    Field::FrameSize,
    Field::CodeSize,
    Field::UnicodeCompactSize,
    Field::DictEntrySize,
    Field::DictUnicodeEntrySize,
];

/// The layout, once each field it needs is there and lies within its
/// structure, and no structure read whole is larger than [`MAX_STRUCTURE`].
fn validated(
    version: PyVersion,
    free_threaded: bool,
    layout: PythonLayout,
) -> Result<PythonLayout, Arc<str>> {
    let thread_local = if free_threaded && version.minor() >= 14 {
        &THREAD_LOCAL_BYTECODE[..]
    } else {
        &[]
    };

    if let Some(missing) = REQUIRED
        .iter()
        .chain(thread_local)
        .find(|field| !layout.has(**field))
    {
        return Err(format!("CPython {version}'s layout has no {}", missing.name()).into());
    }

    if let Some((field, size)) = READ_WHOLE
        .iter()
        .filter_map(|&field| Some((field, layout.field(field)?)))
        .find(|(_, size)| *size > MAX_STRUCTURE)
    {
        return Err(format!(
            "CPython {version}'s layout makes {} {size} bytes",
            field.name()
        )
        .into());
    }

    for &field in Field::ALL {
        let Some(offset) = layout.field(field) else {
            continue;
        };

        let Some(size) = structure_size(field).and_then(|size| layout.field(size)) else {
            continue;
        };

        if offset >= size {
            return Err(format!(
                "CPython {version}'s layout puts {} at {offset}, past its structure's {size} bytes",
                field.name()
            )
            .into());
        }
    }

    let owners = [
        Constant::OwnerThread,
        Constant::OwnerGenerator,
        Constant::OwnerFrameObject,
        Constant::OwnerEntry,
    ]
    .map(|owner| layout.constant(owner));
    if owners
        .iter()
        .enumerate()
        .any(|(at, owner)| owners[..at].contains(owner))
    {
        return Err(format!("CPython {version}'s frame owners {owners:?} repeat").into());
    }

    Ok(layout)
}

/// The size field of the structure a field is in, for those whose size
/// a layout records.
const fn structure_size(field: Field) -> Option<Field> {
    use Field::{
        CodeArgcount, CodeFilename, CodeFirstlineno, CodeFlags, CodeKwonlyargcount, CodeLinetable,
        CodeLocalspluskinds, CodeLocalsplusnames, CodeName, CodeNlocalsplus, CodeQualname,
        CodeSize, CodeTlbc, FrameBuiltins, FrameExecutable, FrameGlobals, FrameInstruction,
        FrameOwner, FramePrevious, FrameSize, FrameTlbcIndex, ThreadBaseFrame, ThreadCurrentFrame,
        ThreadGilRequested, ThreadHoldsGil, ThreadId, ThreadInterp, ThreadNativeId, ThreadNext,
        ThreadPrev, ThreadSize,
    };

    match field {
        ThreadPrev | ThreadNext | ThreadInterp | ThreadCurrentFrame | ThreadBaseFrame
        | ThreadId | ThreadNativeId | ThreadHoldsGil | ThreadGilRequested => Some(ThreadSize),
        FramePrevious | FrameExecutable | FrameInstruction | FrameOwner | FrameTlbcIndex
        | FrameGlobals | FrameBuiltins => Some(FrameSize),

        CodeFilename | CodeName | CodeQualname | CodeLinetable | CodeFirstlineno | CodeArgcount
        | CodeKwonlyargcount | CodeLocalsplusnames | CodeLocalspluskinds | CodeTlbc | CodeFlags
        | CodeNlocalsplus => Some(CodeSize),

        // The locals follow the frame and the bytecode follows the code
        // object, each from its structure's last member on.
        _ => None,
    }
}

/// The module's `Py_Version`, the version it was built as, which every
/// version since 3.11 exports.
fn py_version(object: &object::File<'_>) -> Option<u32> {
    let symbol = object
        .symbols()
        .chain(object.dynamic_symbols())
        .find(|symbol| symbol.name() == Ok("Py_Version") && symbol.is_definition())?;
    let address = symbol.address();
    let section = object.section_by_index(symbol.section_index()?).ok()?;
    let bytes = section.data_range(address, 4).ok()??;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// Gives the interpreter's functions and symbols their role: the module
/// carries a Python runtime, so CPython's names are CPython's code.
pub(super) fn assign_roles(functions: &mut [FunctionInfo], symbols: &mut [SymbolInfo]) {
    for function in functions {
        if function.role == CodeRole::Ordinary
            && let Some(role) = super::roles::cpython_role(&function.name)
        {
            function.role = role;
        }
    }

    for symbol in symbols {
        if symbol.role == CodeRole::Ordinary
            && let Some(role) = super::roles::cpython_role(&symbol.name)
        {
            symbol.role = role;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V3_12_13: u32 = 0x030c_0df0;

    /// A 3.13 table: the cookie, the version, the flag, and each field of
    /// 3.13's schema at a distinct offset, its position.
    fn table(version: u32, free_threaded: bool, fields: usize) -> Vec<u8> {
        let mut bytes = COOKIE.to_vec();
        bytes.extend(u64::from(version).to_le_bytes());
        bytes.extend(u64::from(free_threaded).to_le_bytes());
        for position in 0..fields {
            bytes.extend((position as u64 + 8).to_le_bytes());
        }

        bytes
    }

    #[test]
    fn a_table_gives_each_field_its_schema_names_and_the_rest_are_generated() {
        let version = layouts::GENERATED
            .iter()
            .find(|generated| generated.name == "3.13")
            .expect("3.13 is in the matrix")
            .version;
        let bytes = table(version, false, schemas::V3_13.len());
        let (found, flags, layout) = describe(Some(&bytes), Some(version));
        assert_eq!(found, PyVersion(version));
        assert_eq!(flags, TARGET_TABLE | GENERATED_TABLE);
        let layout = layout.expect("a layout");
        let at = |name| {
            schemas::V3_13
                .iter()
                .position(|candidate| *candidate == name)
                .map(|position| position as u64 + 8)
        };
        assert_eq!(layout.field(Field::ThreadNext), at("thread_state.next"));
        assert_eq!(
            layout.field(Field::CodeQualname),
            at("code_object.qualname")
        );

        // 3.13's table has no co_flags; the generated layout does.
        assert!(at("code_object.co_flags").is_none());
        assert!(layout.has(Field::CodeFlags));
    }

    #[test]
    fn remote_debugging_fields_are_taken_from_each_targets_table() {
        for (version, schema) in [(0x030e_00f0, schemas::V3_14), (0x030f_00f0, schemas::V3_15)] {
            for free_threaded in [false, true] {
                let bytes = table(version, free_threaded, schema.len());
                let layout = describe(Some(&bytes), Some(version)).2.expect("layout");
                for name in schema
                    .iter()
                    .filter(|name| name.starts_with("debugger_support."))
                {
                    let field = Field::named(name).unwrap_or_else(|| panic!("missing {name}"));
                    let index = schema
                        .iter()
                        .position(|entry| entry == name)
                        .expect("in schema");
                    assert_eq!(layout.field(field), Some(index as u64 + 8), "{name}");
                }
            }
        }
    }

    #[test]
    fn versions_microscope_has_not_checked_are_refused_by_name() {
        let refused = |bytes: Option<&[u8]>, symbol| match describe(bytes, symbol).2 {
            Err(reason) => reason.to_string(),
            Ok(_) => panic!("{symbol:x?} was read"),
        };

        assert!(refused(None, Some(0x030c_01f0)).contains("CPython 3.12.1 is not a release"));
        assert!(refused(None, Some(0x030b_04f0)).contains("3.11.4 is not supported"));
        assert!(refused(None, Some(0x030d_02f0)).contains("has no offsets table"));
        assert!(refused(None, None).contains("version is unknown"));
        let next = table(0x0310_00f0, false, schemas::V3_15.len());
        assert!(refused(Some(&next), Some(0x0310_00f0)).contains("3.16.0 is not supported"));
        let beta = table(0x030f_00b3, false, schemas::V3_15.len());
        assert!(refused(Some(&beta), Some(0x030f_00b3)).contains("3.15.0b3 is not a release"));
        let cut = table(0x030d_0df0, false, 3);
        assert!(refused(Some(&cut), None).contains("cut short"));
        let other = table(0x030d_0df0, false, schemas::V3_13.len());
        assert!(refused(Some(&other), Some(0x030d_0cf0)).contains("is not its offsets table's"));
    }

    #[test]
    fn a_checked_312_release_is_read_by_its_generated_layout() {
        let (version, flags, layout) = describe(Some(&[1, 0, 0, 0]), Some(V3_12_13));
        assert_eq!(version.to_string(), "3.12.13");
        assert_eq!(flags, GENERATED_TABLE);
        let layout = validated(version, false, layout.expect("a layout")).expect("valid");
        assert!(layout.has(Field::CframeCurrentFrame));
    }

    /// Every interpreter in the matrix is read by the layout generated
    /// from its own headers, and a table it carries agrees with them on
    /// every field.
    #[test]
    fn each_interpreter_in_the_matrix_agrees_with_its_headers() {
        let root = std::env::var_os("MICROSCOPE_CPYTHON")
            .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell");
        for generated in layouts::GENERATED {
            let module =
                conformance::runtime_module(&std::path::Path::new(&root).join(generated.name));
            let bytes = std::fs::read(module).expect("read the module");
            let object = object::File::parse(&*bytes).expect("parse the module");
            let runtime = runtime(&object).expect("a Python runtime");
            assert_eq!(
                (runtime.version.0, runtime.flags & FREE_THREADED != 0),
                (generated.version, generated.free_threaded),
                "{}",
                generated.name
            );

            let layout = runtime
                .layout
                .unwrap_or_else(|problem| panic!("{}: {problem}", generated.name));
            let headers = generated.layout();
            for &field in Field::ALL {
                assert_eq!(
                    layout.field(field),
                    headers.field(field),
                    "{}: {}",
                    generated.name,
                    field.name()
                );
            }

            assert_eq!(
                runtime.flags & TARGET_TABLE != 0,
                runtime.version.minor() >= 13,
                "{}",
                generated.name
            );
        }
    }

    /// Each interpreter's generated layout is complete and consistent.
    #[test]
    fn every_generated_layout_is_valid() {
        for generated in layouts::GENERATED {
            validated(
                PyVersion(generated.version),
                generated.free_threaded,
                generated.layout(),
            )
            .unwrap_or_else(|problem| panic!("{}: {problem}", generated.name));
        }
    }

    #[test]
    fn a_layout_whose_structure_is_too_large_to_read_is_refused() {
        let generated = &layouts::GENERATED[0];
        for size in READ_WHOLE {
            let mut layout = generated.layout();
            layout.set_field(size, MAX_STRUCTURE + 1);
            let refused = validated(
                PyVersion(generated.version),
                generated.free_threaded,
                layout,
            )
            .expect_err(size.name());
            assert!(
                refused.contains(&format!("makes {} ", size.name())),
                "{refused}"
            );
        }
    }
}
