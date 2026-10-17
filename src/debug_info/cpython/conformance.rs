//! The generated layouts checked against DWARF, a third account of each
//! interpreter's structures beside its headers and its own offsets table:
//! the compiler's record of the build itself. Every interpreter in the
//! matrix whose build has a debug output is checked on every field its
//! layout holds and every constant DWARF describes.

use std::path::{Path, PathBuf};

use super::layouts;
use crate::debug_info::{DebugFileSearch, load_module};
use crate::image::python::{Constant, Field, PyVersion};
use crate::{
    DebugFileOptions, IntegerValue, ModuleImage, ModuleImageId, NamedTypeRelationship,
    RecordMember, RecordMemberLayout, TypeInfo, TypeKind, TypeNode, TypeReference,
};

/// The interpreters in the matrix whose builds have debug outputs:
/// nixpkgs' 3.12, 3.13, 3.13t, 3.14, and 3.14t.
const DESCRIBED: usize = 5;

/// Where a field is in C: the type that holds it, as the headers name it,
/// and the members that lead to it from there; no members for the type's
/// size, and [`END`] last for where the member before it ends.
type Member = (&'static str, &'static [&'static str]);

/// The last of a member's path that stands for where the member it
/// follows ends, as the end of an array.
const END: &str = "<end>";

/// Where each field is in the C of a minor version, or `None` for a field
/// no build of that version has.
#[expect(clippy::too_many_lines, reason = "one arm per field")]
const fn member(field: Field, minor: u32) -> Option<Member> {
    let before_13 = minor < 13;
    Some(match field {
        Field::RuntimeInterpretersHead => ("_PyRuntimeState", &["interpreters", "head"]),
        Field::InterpreterId => ("PyInterpreterState", &["id"]),
        Field::InterpreterNext => ("PyInterpreterState", &["next"]),
        Field::InterpreterThreadsHead => ("PyInterpreterState", &["threads", "head"]),
        Field::ThreadSize => ("PyThreadState", &[]),
        Field::ThreadPrev => ("PyThreadState", &["prev"]),
        Field::ThreadNext => ("PyThreadState", &["next"]),
        Field::ThreadInterp => ("PyThreadState", &["interp"]),
        Field::ThreadCurrentFrame if before_13 => ("PyThreadState", &["cframe"]),
        Field::ThreadCurrentFrame => ("PyThreadState", &["current_frame"]),
        Field::ThreadBaseFrame if minor >= 15 => ("PyThreadState", &["base_frame"]),
        Field::ThreadId => ("PyThreadState", &["thread_id"]),
        Field::ThreadNativeId => ("PyThreadState", &["native_thread_id"]),
        Field::CframeCurrentFrame if before_13 => ("_PyCFrame", &["current_frame"]),
        Field::FrameSize => ("_PyInterpreterFrame", &[]),
        Field::FramePrevious => ("_PyInterpreterFrame", &["previous"]),
        Field::FrameExecutable if before_13 => ("_PyInterpreterFrame", &["f_code"]),
        Field::FrameExecutable => ("_PyInterpreterFrame", &["f_executable"]),
        Field::FrameInstruction if before_13 => ("_PyInterpreterFrame", &["prev_instr"]),
        Field::FrameInstruction => ("_PyInterpreterFrame", &["instr_ptr"]),
        Field::FrameLocalsplus => ("_PyInterpreterFrame", &["localsplus"]),
        Field::FrameOwner => ("_PyInterpreterFrame", &["owner"]),
        Field::FrameGlobals => ("_PyInterpreterFrame", &["f_globals"]),
        Field::FrameBuiltins => ("_PyInterpreterFrame", &["f_builtins"]),
        Field::FrameTlbcIndex => ("_PyInterpreterFrame", &["tlbc_index"]),
        Field::CodeSize => ("PyCodeObject", &[]),
        Field::CodeFilename => ("PyCodeObject", &["co_filename"]),
        Field::CodeName => ("PyCodeObject", &["co_name"]),
        Field::CodeQualname => ("PyCodeObject", &["co_qualname"]),
        Field::CodeLinetable => ("PyCodeObject", &["co_linetable"]),
        Field::CodeFirstlineno => ("PyCodeObject", &["co_firstlineno"]),
        Field::CodeArgcount => ("PyCodeObject", &["co_argcount"]),
        Field::CodeKwonlyargcount => ("PyCodeObject", &["co_kwonlyargcount"]),
        Field::CodeLocalsplusnames => ("PyCodeObject", &["co_localsplusnames"]),
        Field::CodeLocalspluskinds => ("PyCodeObject", &["co_localspluskinds"]),
        Field::CodeCodeAdaptive => ("PyCodeObject", &["co_code_adaptive"]),
        Field::CodeTlbc => ("PyCodeObject", &["co_tlbc"]),
        Field::CodeArraySize => ("_PyCodeArray", &["size"]),
        Field::CodeArrayEntries => ("_PyCodeArray", &["entries"]),
        Field::CodeFlags => ("PyCodeObject", &["co_flags"]),
        Field::CodeNlocalsplus => ("PyCodeObject", &["co_nlocalsplus"]),
        Field::ObjectType => ("PyObject", &["ob_type"]),
        Field::VarObjectSize => ("PyVarObject", &["ob_size"]),
        Field::TypeName => ("PyTypeObject", &["tp_name"]),
        Field::TypeFlags => ("PyTypeObject", &["tp_flags"]),
        Field::TypeBasicsize => ("PyTypeObject", &["tp_basicsize"]),
        Field::TypeDictoffset => ("PyTypeObject", &["tp_dictoffset"]),
        Field::TypeDict => ("PyTypeObject", &["tp_dict"]),
        Field::TypeMro => ("PyTypeObject", &["tp_mro"]),
        Field::TypeBase => ("PyTypeObject", &["tp_base"]),
        Field::TypeSubclasses => ("PyTypeObject", &["tp_subclasses"]),
        Field::InterpreterStaticTypes if before_13 => {
            ("PyInterpreterState", &["types", "builtins"])
        }
        Field::InterpreterStaticTypes => {
            ("PyInterpreterState", &["types", "builtins", "initialized"])
        }
        Field::InterpreterStaticTypesEnd if before_13 => {
            ("PyInterpreterState", &["types", "builtins", END])
        }
        Field::InterpreterStaticTypesEnd => (
            "PyInterpreterState",
            &["types", "builtins", "initialized", END],
        ),
        Field::StaticTypeStateSize if before_13 => ("static_builtin_state", &[]),
        Field::StaticTypeStateSize => ("managed_static_type_state", &[]),
        Field::StaticTypeStateDict if before_13 => ("static_builtin_state", &["tp_dict"]),
        Field::StaticTypeStateDict => ("managed_static_type_state", &["tp_dict"]),
        Field::StaticTypeStateType if before_13 => ("static_builtin_state", &["type"]),
        Field::StaticTypeStateType => ("managed_static_type_state", &["type"]),
        Field::InterpreterStaticExtensionTypes if !before_13 => (
            "PyInterpreterState",
            &["types", "for_extensions", "initialized"],
        ),
        Field::InterpreterStaticExtensionTypesEnd if !before_13 => (
            "PyInterpreterState",
            &["types", "for_extensions", "initialized", END],
        ),
        Field::TypeGetattro => ("PyTypeObject", &["tp_getattro"]),
        Field::TypeDescrGet => ("PyTypeObject", &["tp_descr_get"]),
        Field::TypeDescrSet => ("PyTypeObject", &["tp_descr_set"]),
        Field::HeapTypeQualname => ("PyHeapTypeObject", &["ht_qualname"]),
        Field::HeapTypeCachedKeys => ("PyHeapTypeObject", &["ht_cached_keys"]),
        Field::HeapTypeSlots => ("PyHeapTypeObject", &["ht_slots"]),
        Field::MemberDescrMember => ("PyMemberDescrObject", &["d_member"]),
        Field::MemberDefType => ("PyMemberDef", &["type"]),
        Field::MemberDefOffset => ("PyMemberDef", &["offset"]),
        Field::CellRef => ("PyCellObject", &["ob_ref"]),
        Field::SetUsed => ("PySetObject", &["used"]),
        Field::SetMask => ("PySetObject", &["mask"]),
        Field::SetTable => ("PySetObject", &["table"]),
        Field::SetEntrySize => ("setentry", &[]),
        Field::SetEntryKey => ("setentry", &["key"]),
        Field::SetEntryHash => ("setentry", &["hash"]),
        Field::ModuleDict => ("PyModuleObject", &["md_dict"]),
        Field::FunctionQualname => ("PyFunctionObject", &["func_qualname"]),
        Field::BuiltinMethod => ("PyCFunctionObject", &["m_ml"]),
        Field::BuiltinSelf => ("PyCFunctionObject", &["m_self"]),
        Field::MethodDefName => ("PyMethodDef", &["ml_name"]),
        Field::WrapperDescrWrapped => ("PyWrapperDescrObject", &["d_wrapped"]),
        Field::TupleItems => ("PyTupleObject", &["ob_item"]),
        Field::TupleSize => ("PyTupleObject", &["ob_base", "ob_size"]),
        Field::ListItems => ("PyListObject", &["ob_item"]),
        Field::ListSize => ("PyListObject", &["ob_base", "ob_size"]),
        Field::DictKeys => ("PyDictObject", &["ma_keys"]),
        Field::DictValues => ("PyDictObject", &["ma_values"]),
        Field::DictUsed => ("PyDictObject", &["ma_used"]),
        Field::DictKeysLog2Size => ("PyDictKeysObject", &["dk_log2_size"]),
        Field::DictKeysLog2IndexBytes => ("PyDictKeysObject", &["dk_log2_index_bytes"]),
        Field::DictKeysKind => ("PyDictKeysObject", &["dk_kind"]),
        Field::DictKeysNentries => ("PyDictKeysObject", &["dk_nentries"]),
        Field::DictKeysIndices => ("PyDictKeysObject", &["dk_indices"]),
        Field::DictEntrySize => ("PyDictKeyEntry", &[]),
        Field::DictEntryHash => ("PyDictKeyEntry", &["me_hash"]),
        Field::DictEntryKey => ("PyDictKeyEntry", &["me_key"]),
        Field::DictEntryValue => ("PyDictKeyEntry", &["me_value"]),
        Field::DictUnicodeEntrySize => ("PyDictUnicodeEntry", &[]),
        Field::DictUnicodeEntryKey => ("PyDictUnicodeEntry", &["me_key"]),
        Field::DictUnicodeEntryValue => ("PyDictUnicodeEntry", &["me_value"]),
        Field::DictValuesValues => ("PyDictValues", &["values"]),
        Field::DictValuesCapacity if !before_13 => ("PyDictValues", &["capacity"]),
        Field::DictValuesSize if !before_13 => ("PyDictValues", &["size"]),
        Field::DictValuesValid if !before_13 => ("PyDictValues", &["valid"]),
        Field::FloatValue => ("PyFloatObject", &["ob_fval"]),
        Field::LongTag => ("PyLongObject", &["long_value", "lv_tag"]),
        Field::LongDigits => ("PyLongObject", &["long_value", "ob_digit"]),
        Field::BytesSize => ("PyBytesObject", &["ob_base", "ob_size"]),
        Field::BytesData => ("PyBytesObject", &["ob_sval"]),
        Field::UnicodeState => ("PyASCIIObject", &["state"]),
        Field::UnicodeHash => ("PyASCIIObject", &["hash"]),
        Field::UnicodeLength => ("PyASCIIObject", &["length"]),
        Field::UnicodeAsciiSize => ("PyASCIIObject", &[]),
        Field::UnicodeCompactSize => ("PyCompactUnicodeObject", &[]),
        Field::UnicodeData => ("PyUnicodeObject", &["data"]),
        Field::DequeLeftBlock => ("dequeobject", &["leftblock"]),
        Field::DequeLeftIndex => ("dequeobject", &["leftindex"]),
        Field::DequeMaxlen => ("dequeobject", &["maxlen"]),
        Field::DequeObjectSize => ("dequeobject", &[]),
        Field::DequeBlockData => ("BLOCK", &["data"]),
        Field::DequeBlockRightLink => ("BLOCK", &["rightlink"]),
        Field::DefaultDictFactory => ("defdictobject", &["default_factory"]),
        Field::DefaultDictObjectSize => ("defdictobject", &[]),
        Field::OrderedDictFirst => ("_odictobject", &["od_first"]),
        Field::OrderedDictObjectSize => ("_odictobject", &[]),
        Field::OrderedDictNodeKey => ("_odictnode", &["key"]),
        Field::OrderedDictNodeHash => ("_odictnode", &["hash"]),
        Field::OrderedDictNodeNext => ("_odictnode", &["next"]),
        // Threads before 3.15 have no base frame, and from 3.13 none
        // keeps a C frame; 3.12 manages no extension's static types.
        Field::ThreadBaseFrame
        | Field::CframeCurrentFrame
        | Field::InterpreterStaticExtensionTypes
        | Field::InterpreterStaticExtensionTypesEnd
        | Field::DictValuesCapacity
        | Field::DictValuesSize
        | Field::DictValuesValid => return None,
        Field::DateHasTzinfo => ("PyDateTime_Date", &["hastzinfo"]),
        Field::DateData => ("PyDateTime_Date", &["data"]),
        Field::DateObjectSize => ("PyDateTime_Date", &[]),
        Field::TimeData => ("PyDateTime_Time", &["data"]),
        Field::TimeFold => ("PyDateTime_Time", &["fold"]),
        Field::TimeTzinfo => ("PyDateTime_Time", &["tzinfo"]),
        Field::TimeObjectSize => ("PyDateTime_Time", &[]),
        Field::DateTimeData => ("PyDateTime_DateTime", &["data"]),
        Field::DateTimeFold => ("PyDateTime_DateTime", &["fold"]),
        Field::DateTimeTzinfo => ("PyDateTime_DateTime", &["tzinfo"]),
        Field::DateTimeObjectSize => ("PyDateTime_DateTime", &[]),
        Field::DeltaDays => ("PyDateTime_Delta", &["days"]),
        Field::DeltaSeconds => ("PyDateTime_Delta", &["seconds"]),
        Field::DeltaMicroseconds => ("PyDateTime_Delta", &["microseconds"]),
        Field::DeltaObjectSize => ("PyDateTime_Delta", &[]),
        Field::TimeZoneOffset => ("PyDateTime_TimeZone", &["offset"]),
        Field::TimeZoneName => ("PyDateTime_TimeZone", &["name"]),
        Field::TimeZoneObjectSize => ("PyDateTime_TimeZone", &[]),
        Field::DecimalFlags => ("PyDecObject", &["dec", "flags"]),
        Field::DecimalExponent => ("PyDecObject", &["dec", "exp"]),
        Field::DecimalDigits => ("PyDecObject", &["dec", "digits"]),
        Field::DecimalLength => ("PyDecObject", &["dec", "len"]),
        Field::DecimalData => ("PyDecObject", &["dec", "data"]),
        Field::DecimalObjectSize => ("PyDecObject", &[]),
    })
}

/// Whether a field is of a type an extension module defines, whose debug
/// information is the extension's rather than the runtime's: `datetime`'s,
/// whose layouts come from its public header, and `decimal.Decimal`'s,
/// which tests/python/views.rs checks by the values its objects show.
const fn in_extension(field: Field) -> bool {
    matches!(
        field,
        Field::DateHasTzinfo
            | Field::DateData
            | Field::DateObjectSize
            | Field::TimeData
            | Field::TimeFold
            | Field::TimeTzinfo
            | Field::TimeObjectSize
            | Field::DateTimeData
            | Field::DateTimeFold
            | Field::DateTimeTzinfo
            | Field::DateTimeObjectSize
            | Field::DeltaDays
            | Field::DeltaSeconds
            | Field::DeltaMicroseconds
            | Field::DeltaObjectSize
            | Field::TimeZoneOffset
            | Field::TimeZoneName
            | Field::TimeZoneObjectSize
            | Field::DecimalFlags
            | Field::DecimalExponent
            | Field::DecimalDigits
            | Field::DecimalLength
            | Field::DecimalData
            | Field::DecimalObjectSize
    )
}

/// The enumerator each constant DWARF describes is, by its C name. The
/// frame owners are enumerators too, but a frame keeps its owner in a
/// `char`, so no declaration uses their enumeration and DWARF leaves it
/// out; the headers check them by name.
const fn enumerator(constant: Constant) -> Option<&'static str> {
    match constant {
        Constant::DictKeysGeneral => Some("DICT_KEYS_GENERAL"),
        Constant::DictKeysUnicode => Some("DICT_KEYS_UNICODE"),
        Constant::DictKeysSplit => Some("DICT_KEYS_SPLIT"),
        _ => None,
    }
}

/// The bitfield of a `str`'s state each shift is the place of.
const fn state_bit(constant: Constant) -> Option<&'static str> {
    match constant {
        Constant::UnicodeKindShift => Some("kind"),
        Constant::UnicodeCompactShift => Some("compact"),
        Constant::UnicodeAsciiShift => Some("ascii"),
        _ => None,
    }
}

/// The module of the interpreter in `directory` that carries its runtime:
/// the libpython it links, or the executable itself.
pub(super) fn runtime_module(directory: &Path) -> PathBuf {
    let executable =
        std::fs::canonicalize(directory.join("python")).expect("resolve the interpreter");
    let bytes = std::fs::read(&executable).expect("read the interpreter");
    if super::carries_runtime(&object::File::parse(&*bytes).expect("parse the interpreter")) {
        return executable;
    }

    let lib = executable
        .parent()
        .and_then(Path::parent)
        .expect("the interpreter is in a bin directory")
        .join("lib");
    std::fs::read_dir(&lib)
        .expect("list the interpreter's lib directory")
        .map(|entry| entry.expect("read an entry").path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libpython") && name.ends_with(".so.1.0"))
        })
        .expect("the interpreter links a libpython")
}

/// The type a reference names, past its typedefs and qualifiers.
fn underlying(image: &ModuleImage, mut reference: TypeReference) -> &TypeInfo {
    loop {
        let info = image.type_info(reference).expect("a described type");
        match &info.kind {
            TypeKind::Named {
                target: Some(target),
                relationship: NamedTypeRelationship::Synonym,
            }
            | TypeKind::Modified { target, .. } => reference = *target,
            _ => return info,
        }
    }
}

/// The complete structure or union the C name `name` names.
fn aggregate<'a>(image: &'a ModuleImage, name: &str) -> &'a TypeInfo {
    image
        .types_named(name)
        .into_iter()
        .map(|reference| underlying(image, reference))
        .find(|info| match &info.kind {
            TypeKind::Record { incomplete, .. } => !incomplete,
            TypeKind::Union { .. } => true,
            _ => false,
        })
        .unwrap_or_else(|| panic!("DWARF describes no complete {name}"))
}

fn members(info: &TypeInfo) -> &[RecordMember] {
    match &info.kind {
        TypeKind::Record { members, .. } | TypeKind::Union { members, .. } => members,
        kind => panic!("{} is no aggregate: {kind:?}", info.name),
    }
}

/// The member `name` of an aggregate.
fn member_named<'a>(info: &'a TypeInfo, name: &str) -> &'a RecordMember {
    members(info)
        .iter()
        .find(|member| member.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("{} has no member {name}", info.name))
}

/// Where a field is, by DWARF: its offset, or its type's size.
fn described(image: &ModuleImage, (ty, path): Member) -> u64 {
    let mut info = aggregate(image, ty);
    if path.is_empty() {
        return info.byte_size.expect("a complete type has a size");
    }

    let mut offset = 0;
    for name in path {
        if *name == END {
            return offset + info.byte_size.expect("a complete member has a size");
        }

        let member = member_named(info, name);
        let RecordMemberLayout::ByteOffset(at) = member.layout else {
            panic!("{ty}.{name} is not at a byte offset: {:?}", member.layout);
        };
        offset += at;
        info = underlying(image, member.type_ref);
    }

    offset
}

/// The value of the enumerator `name`, from whichever enumeration of the
/// image holds it.
fn enumerator_value(image: &ModuleImage, name: &str) -> Option<i128> {
    image.types().find_map(|node| match node {
        TypeNode::Resolved(TypeInfo {
            kind: TypeKind::Enumeration { enumerators, .. },
            ..
        }) => enumerators
            .iter()
            .find(|enumerator| &*enumerator.name == name)
            .map(|enumerator| match enumerator.value {
                IntegerValue::Signed(value) => value,
                IntegerValue::Unsigned(value) => {
                    i128::try_from(value).expect("an enumerator fits in i128")
                }
            }),
        _ => None,
    })
}

/// Every interpreter in the matrix whose build has a debug output agrees
/// with its generated layout on every field the layout holds, and on
/// every constant DWARF describes.
#[test]
fn each_described_interpreter_agrees_with_its_dwarf() {
    let root = std::env::var_os("MICROSCOPE_CPYTHON")
        .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell");
    let mut checked = 0;
    for generated in layouts::GENERATED {
        let directory = Path::new(&root).join(generated.name);
        let debug = directory.join("debug/lib/debug");
        if !debug.is_dir() {
            continue;
        }

        let search = DebugFileSearch::new(&DebugFileOptions {
            directories: vec![debug],
            ..DebugFileOptions::default()
        });
        let module = runtime_module(&directory);
        let info = load_module(&module, ModuleImageId::new(0), &search)
            .unwrap_or_else(|error| panic!("{}: {error}", generated.name));
        let image = &info.image;
        let name = generated.name;
        let minor = PyVersion(generated.version).minor();
        let layout = generated.layout();
        for &field in Field::ALL {
            let Some(at) = layout.field(field) else {
                continue;
            };

            if in_extension(field) {
                continue;
            }

            let member = member(field, minor)
                .unwrap_or_else(|| panic!("{name}: no C member for {}", field.name()));
            assert_eq!(described(image, member), at, "{name}: {}", field.name());
        }

        for &constant in Constant::ALL {
            let value = i128::from(layout.constant(constant));
            if let Some(enumerator) = enumerator(constant) {
                assert_eq!(
                    enumerator_value(image, enumerator),
                    Some(value),
                    "{name}: {enumerator}"
                );
            }

            if let Some(bit) = state_bit(constant) {
                let state = underlying(
                    image,
                    member_named(aggregate(image, "PyASCIIObject"), "state").type_ref,
                );
                let RecordMemberLayout::BitRange { bit_offset, .. } =
                    member_named(state, bit).layout
                else {
                    panic!("{name}: state.{bit} is no bitfield");
                };
                assert_eq!(i128::from(bit_offset), value, "{name}: state.{bit}");
            }
        }

        checked += 1;
    }

    assert_eq!(checked, DESCRIBED, "the interpreters with debug outputs");
}
