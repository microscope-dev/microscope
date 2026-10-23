//! V8, the JavaScript engine Node.js embeds: what its images say about
//! themselves.
//!
//! V8 compiles its builtins ahead of time into an embedded blob, which a
//! build that keeps its local symbols names: `v8_Default_embedded_blob_code_`
//! where the blob begins, and a `Builtins_` function symbol for each builtin.
//! No call-frame information describes them. Their frames are V8's own,
//! which keep the frame-pointer chain at each call they make: rbp holds the
//! address of the word that saved the caller's, with the return address in
//! the word above it.
//!
//! A module is a V8 runtime when it exports V8's `v8dbg_` constants, as
//! every Node executable and libnode does, for debuggers: about 480 `int`s
//! in its data, which give its frames' layout, its tags, its instance types,
//! and some of its objects' fields. [`runtime`] reads them by name with
//! V8's version, from `v8::internal::Version`, and binds the module to the
//! layout generated for that major.minor version from V8's Torque output
//! ([`layouts`]), which holds the fields the constants leave out. This is
//! the exception to reading layouts only from what the target describes
//! that CPython's provider takes as well: the layouts are generated, never
//! hand-copied, and checked against the binary's own constants wherever
//! both name a field or an instance type, and a version microscope has no
//! layout of is refused by name, never read by the nearest one's.

#[rustfmt::skip]
#[path = "v8/layouts.gen.rs"]
mod layouts;

#[cfg(test)]
#[path = "v8/conformance.rs"]
mod conformance;

#[cfg(test)]
#[path = "v8/translate.rs"]
mod translate;

use std::collections::BTreeMap;
use std::sync::Arc;

use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};

use crate::image::v8::{Constant, DYNAMIC_SYMBOLS, Field, V8Layout, V8Runtime, V8Version};

/// A constant every V8 build exports, in its dynamic table too, for
/// debuggers: where a frame keeps its function.
const FRAME_FUNCTION_OFFSET: &str = "v8dbg_off_fp_function";

/// Whether `image` is V8's, which generates code whose frames keep the
/// frame-pointer chain at its calls.
pub fn is_v8(image: &crate::ModuleImage) -> bool {
    image.symbol_named(FRAME_FUNCTION_OFFSET).is_ok()
}

/// The symbol at which the embedded builtins begin.
const EMBEDDED_BLOB: &str = "v8_Default_embedded_blob_code_";

/// What the symbol of each builtin's code begins with.
const BUILTIN: &str = "Builtins_";

/// The variable each thread's current isolate is kept in.
pub const CURRENT_ISOLATE: &str = "_ZN2v88internal18g_current_isolate_E";

/// The exported function that reads [`CURRENT_ISOLATE`].
const GET_CURRENT: &str = "_ZN2v87Isolate10GetCurrentEv";

/// Adds to `thread_locals` where each thread's current isolate is, when
/// `object` is V8's and no symbol names its variable, as in a shared
/// library stripped of its local symbols: the exported accessor's code
/// says. A variable a symbol names is left as the symbol places it.
pub(super) fn add_thread_locals(
    object: &object::File<'_>,
    thread_locals: &mut std::collections::BTreeMap<
        std::sync::Arc<str>,
        Result<crate::ThreadLocal, std::sync::Arc<str>>,
    >,
) {
    if thread_locals.contains_key(CURRENT_ISOLATE) {
        return;
    }
    let Some(accessor) = object
        .symbols()
        .chain(object.dynamic_symbols())
        .find(|symbol| symbol.name() == Ok(GET_CURRENT) && symbol.kind() == SymbolKind::Text)
    else {
        return;
    };
    thread_locals.insert(
        CURRENT_ISOLATE.into(),
        super::elf::accessor_thread_local(object, accessor.address()),
    );
}

/// The address and code of each embedded builtin `object` names, in
/// address order. An image that is not V8's, or that names no builtin,
/// has none.
pub(super) fn builtins<'data>(object: &object::File<'data>) -> Vec<(u64, &'data [u8])> {
    let Some(blob) = object
        .symbols()
        .find(|symbol| symbol.name() == Ok(EMBEDDED_BLOB))
        .map(|symbol| symbol.address())
    else {
        return Vec::new();
    };

    let mut builtins: Vec<_> = object
        .symbols()
        .filter(|symbol| {
            symbol.kind() == SymbolKind::Text
                && symbol.size() > 0
                && symbol.address() >= blob
                && symbol.name().is_ok_and(|name| name.starts_with(BUILTIN))
        })
        .filter_map(|symbol| {
            let section = object.section_by_index(symbol.section_index()?).ok()?;
            if section.kind() != SectionKind::Text {
                return None;
            }
            let data = section.data().ok()?;
            let start = usize::try_from(symbol.address().checked_sub(section.address())?).ok()?;
            let end = start.checked_add(usize::try_from(symbol.size()).ok()?)?;
            Some((symbol.address(), data.get(start..end)?))
        })
        .collect();
    builtins.sort_unstable_by_key(|(address, _)| *address);
    builtins.dedup_by_key(|(address, _)| *address);
    builtins
}

/// A layout generated from one V8 version's Torque output.
pub(super) struct Generated {
    pub major: u32,
    pub minor: u32,
    pub fields: &'static [(Field, u32)],

    /// The instance types and bit fields [`Constant`] names.
    pub constants: &'static [(Constant, i64)],

    /// Every instance type Torque assigns, by name, which the binary's
    /// own `v8dbg_type_` constants are checked against.
    pub instance_types: &'static [(&'static str, u16)],
}

impl Generated {
    fn layout(&self) -> V8Layout {
        let mut layout = V8Layout::default();
        for &(field, offset) in self.fields {
            layout.set_field(field, u64::from(offset));
        }

        for &(constant, value) in self.constants {
            layout.set_constant(constant, value);
        }

        layout
    }

    fn instance_type(&self, name: &str) -> Option<u16> {
        self.instance_types
            .iter()
            .find(|(own, _)| *own == name)
            .map(|&(_, value)| value)
    }
}

/// What the names of V8's own constants begin with.
const PREFIX: &str = "v8dbg_";

/// `v8::internal::Version`'s `major_`, `minor_`, `build_`, and `patch_`,
/// which every build exports.
const VERSION: [&str; 4] = [
    "_ZN2v88internal7Version6major_E",
    "_ZN2v88internal7Version6minor_E",
    "_ZN2v88internal7Version6build_E",
    "_ZN2v88internal7Version6patch_E",
];

/// The `v8dbg_class_` constants, as `Class__field`, that give a field the
/// generated layout gives too, which must agree. A field may have more
/// than one, and each constant is suffixed with its field's type.
const CHECKED_FIELDS: &[(Field, &str)] = &[
    (Field::HeapObjectMap, "HeapObject__map"),
    (Field::StringLength, "String__length"),
    (Field::ConsStringFirst, "ConsString__first"),
    (Field::ConsStringSecond, "ConsString__second"),
    (Field::SlicedStringParent, "SlicedString__parent"),
    (Field::SlicedStringOffset, "SlicedString__offset"),
    (Field::ThinStringActual, "ThinString__actual"),
    (Field::SymbolDescription, "Symbol__name"),
    (Field::ExternalStringResource, "ExternalString__resource"),
    (Field::SeqOneByteStringChars, "SeqOneByteString__chars"),
    (Field::SeqTwoByteStringChars, "SeqTwoByteString__chars"),
    (Field::FixedArrayBaseLength, "FixedArrayBase__length"),
    (Field::FixedArrayObjects, "FixedArray__data"),
    (Field::MapInstanceSizeInWords, "Map__instance_size_in_words"),
    (
        Field::MapInobjectPropertiesStartOrConstructorFunctionIndex,
        "Map__inobject_properties_start_or_constructor_function_index",
    ),
    (Field::MapInstanceType, "Map__instance_type"),
    (Field::MapBitField, "Map__bit_field"),
    (Field::MapBitField2, "Map__bit_field2"),
    (Field::MapBitField3, "Map__bit_field3"),
    (Field::MapPrototype, "Map__prototype"),
    (
        Field::MapConstructorOrBackPointerOrNativeContext,
        "Map__constructor_or_back_pointer",
    ),
    (Field::MapInstanceDescriptors, "Map__instance_descriptors"),
    (Field::JSFunctionCode, "JSFunction__code"),
    (Field::JSFunctionSharedFunctionInfo, "JSFunction__shared"),
    (Field::JSFunctionContext, "JSFunction__context"),
    (
        Field::JSFunctionFeedbackCell,
        "JSFunction__raw_feedback_cell",
    ),
    (Field::FeedbackCellValue, "FeedbackCell__value"),
    (
        Field::SharedFunctionInfoNameOrScopeInfo,
        "SharedFunctionInfo__name_or_scope_info",
    ),
    (
        Field::SharedFunctionInfoOuterScopeInfoOrFeedbackMetadata,
        "SharedFunctionInfo__feedback_metadata",
    ),
    (
        Field::SharedFunctionInfoScript,
        "SharedFunctionInfo__script",
    ),
    (
        Field::SharedFunctionInfoScript,
        "SharedFunctionInfo__raw_script",
    ),
    (
        Field::SharedFunctionInfoLength,
        "SharedFunctionInfo__length",
    ),
    (
        Field::SharedFunctionInfoFormalParameterCount,
        "SharedFunctionInfo__internal_formal_parameter_count",
    ),
    (
        Field::SharedFunctionInfoFunctionTokenOffset,
        "SharedFunctionInfo__raw_function_token_offset",
    ),
    (Field::SharedFunctionInfoFlags, "SharedFunctionInfo__flags"),
    (
        Field::SharedFunctionInfoWrapperSharedInfo,
        "SharedFunctionInfoWrapper__shared_info",
    ),
    (
        Field::UncompiledDataInferredName,
        "UncompiledData__inferred_name",
    ),
    (
        Field::UncompiledDataStartPosition,
        "UncompiledData__start_position",
    ),
    (
        Field::UncompiledDataEndPosition,
        "UncompiledData__end_position",
    ),
    (Field::BytecodeArrayLength, "BytecodeArray__length"),
    (Field::BytecodeArrayWrapper, "BytecodeArray__wrapper"),
    (Field::BytecodeArrayBytes, "BytecodeArray__data"),
    (Field::ScriptSource, "Script__source"),
    (Field::ScriptName, "Script__name"),
    (Field::ScriptLineEnds, "Script__line_ends"),
    (
        Field::ScriptEvalFromSharedOrWrappedArguments,
        "Script__eval_from_shared_or_wrapped_arguments",
    ),
    (Field::ScriptEvalFromPosition, "Script__eval_from_position"),
];

/// The fields only the binary's `v8dbg_class_` constants give, which
/// Torque has no readers of.
const BINARY_FIELDS: [(Field, &str); 3] = [
    (Field::CodeInstructionStart, "Code__instruction_start"),
    (Field::CodeInstructionSize, "Code__instruction_size"),
    (Field::CodeFlags, "Code__flags"),
];

/// What a module exports of V8's constants and version.
#[derive(Debug, Default)]
struct Exports {
    /// Every `v8dbg_` constant, by its symbol's name.
    values: BTreeMap<String, i64>,
    version: [Option<u32>; 4],

    /// Whether it names any `v8dbg_` constant in its own symbol table, not
    /// only in its dynamic one.
    in_symbol_table: bool,
    sandbox: bool,
}

/// The V8 runtime the module carries, read from the module's own file: a
/// separate debug file keeps no data sections' bytes. A module that
/// exports no `v8dbg_` constants carries none.
pub(super) fn runtime(object: &object::File<'_>) -> Option<V8Runtime> {
    describe(&exports(object))
}

/// The V8 runtime of a module that exports `exports`, if it is V8's.
fn describe(exports: &Exports) -> Option<V8Runtime> {
    if !exports.values.contains_key(FRAME_FUNCTION_OFFSET) {
        return None;
    }

    let flags = if exports.in_symbol_table {
        0
    } else {
        DYNAMIC_SYMBOLS
    };
    let [Some(major), Some(minor), Some(build), Some(patch)] = exports.version else {
        return Some(V8Runtime {
            version: V8Version::default(),
            flags,
            layout: Err("it exports V8's v8dbg_ constants but not \
                 v8::internal::Version, so its version is unknown"
                .into()),
        });
    };

    let version = V8Version {
        major,
        minor,
        build,
        patch,
    };
    Some(V8Runtime {
        version,
        flags,
        layout: bind(version, &exports.values, exports.sandbox),
    })
}

/// Reads every `v8dbg_` constant and V8's version from `object`'s symbol
/// and dynamic symbol tables, the first a name is defined in.
fn exports(object: &object::File<'_>) -> Exports {
    let mut exports = Exports::default();
    let tables = object
        .symbols()
        .map(|symbol| (symbol, true))
        .chain(object.dynamic_symbols().map(|symbol| (symbol, false)));
    for (symbol, in_symbol_table) in tables {
        let Ok(name) = symbol.name() else {
            continue;
        };

        exports.sandbox |= is_sandbox_api(name);
        if !symbol.is_definition() {
            continue;
        }

        if name.starts_with(PREFIX) {
            exports.in_symbol_table |= in_symbol_table;
            if !exports.values.contains_key(name)
                && let Some(value) = int_value(object, &symbol)
            {
                exports.values.insert(name.to_owned(), value);
            }
        } else if let Some(at) = VERSION.iter().position(|own| *own == name)
            && exports.version[at].is_none()
        {
            exports.version[at] =
                int_value(object, &symbol).and_then(|value| u32::try_from(value).ok());
        }
    }

    exports
}

/// Whether a symbol named `name` exists only in a build with V8's
/// sandbox: the `v8::V8::*Sandbox*` functions of its public API, which
/// `v8.h` declares only under `V8_ENABLE_SANDBOX` and every build exports
/// with the rest of the API, or a member of `v8::internal::Sandbox`.
fn is_sandbox_api(name: &str) -> bool {
    (name.starts_with("_ZN2v82V8") && name.contains("Sandbox"))
        || name.starts_with("_ZN2v88internal7Sandbox")
}

/// The `int` a symbol names: its bytes in the file, or 0 where its section
/// keeps none, as `.bss`.
fn int_value(object: &object::File<'_>, symbol: &object::Symbol<'_, '_>) -> Option<i64> {
    if !matches!(symbol.size(), 0 | 4) {
        return None;
    }

    let section = object.section_by_index(symbol.section_index()?).ok()?;
    if section.file_range().is_none() {
        return Some(0);
    }

    let bytes = section.data_range(symbol.address(), 4).ok()??;
    Some(i64::from(i32::from_le_bytes(bytes.try_into().ok()?)))
}

/// The layout of the V8 runtime of version `version` whose `v8dbg_`
/// constants are `values`, by symbol name, with `sandbox` saying whether
/// it was built with V8's sandbox; or why it cannot be read.
fn bind(
    version: V8Version,
    values: &BTreeMap<String, i64>,
    sandbox: bool,
) -> Result<V8Layout, Arc<str>> {
    let generated = layouts::GENERATED
        .iter()
        .find(|generated| (generated.major, generated.minor) == (version.major, version.minor))
        .ok_or_else(|| -> Arc<str> {
            format!(
                "V8 {}.{} has no layout in this build of microscope",
                version.major, version.minor
            )
            .into()
        })?;
    refuse_configuration(version, values, sandbox)?;

    let mut layout = generated.layout();
    for &constant in Constant::ALL {
        if !constant.is_torque()
            && let Some(&value) = values.get(&format!("{PREFIX}{}", constant.name()))
        {
            layout.set_constant(constant, value);
        }
    }

    for (field, name) in BINARY_FIELDS {
        if let Some((_, value)) = class_values(values, name).next() {
            let offset = u64::try_from(value).map_err(|_| -> Arc<str> {
                format!("V8 {version}'s v8dbg_class_{name} is {value}").into()
            })?;
            layout.set_field(field, offset);
        }
    }

    check_fields(version, &layout, values)?;
    check_instance_types(version, generated, values)?;
    check_complete(version, &layout)?;
    Ok(layout)
}

/// Refuses a build whose objects are not laid out as the generated
/// layouts assume: one with pointer compression, whose tagged values are
/// 4 bytes; with 31-bit Smis; or with V8's sandbox, which requires
/// pointer compression too and keeps external pointers in tables.
fn refuse_configuration(
    version: V8Version,
    values: &BTreeMap<String, i64>,
    sandbox: bool,
) -> Result<(), Arc<str>> {
    let scalar = |constant: Constant| {
        values
            .get(&format!("{PREFIX}{}", constant.name()))
            .copied()
            .ok_or_else(|| -> Arc<str> {
                format!("V8 {version} exports no {PREFIX}{}", constant.name()).into()
            })
    };

    match scalar(Constant::TaggedSize)? {
        8 => {}
        size => {
            return Err(format!(
                "V8 {version} keeps tagged values in {size} bytes, built with pointer \
                 compression, which microscope does not read"
            )
            .into());
        }
    }

    match scalar(Constant::SmiShiftSize)? {
        31 => {}
        0 => {
            return Err(format!(
                "V8 {version} has 31-bit Smis (v8dbg_SmiShiftSize is 0), which microscope \
                 does not read"
            )
            .into());
        }
        shift => {
            return Err(
                format!("V8 {version}'s v8dbg_SmiShiftSize is {shift}, not 31 or 0").into(),
            );
        }
    }

    if sandbox {
        return Err(format!(
            "V8 {version} is built with V8's sandbox, which microscope does not read"
        )
        .into());
    }

    Ok(())
}

/// Every `v8dbg_class_` constant of `field`, written `Class__field`, with
/// the type its name ends with.
fn class_values<'a>(
    values: &'a BTreeMap<String, i64>,
    field: &str,
) -> impl Iterator<Item = (&'a str, i64)> {
    let prefix = format!("{PREFIX}class_{field}__");
    values
        .range(prefix.clone()..)
        .take_while(move |(name, _)| name.starts_with(&prefix))
        .map(|(name, &value)| (name.as_str(), value))
}

/// Checks that each field the binary's constants give is where the
/// generated layout puts it, and that the layout has it.
fn check_fields(
    version: V8Version,
    layout: &V8Layout,
    values: &BTreeMap<String, i64>,
) -> Result<(), Arc<str>> {
    for &(field, name) in CHECKED_FIELDS {
        for (symbol, value) in class_values(values, name) {
            let Some(offset) = layout.field(field) else {
                return Err(format!(
                    "V8 {version} exports {symbol}, but its generated layout has no {}",
                    field.name()
                )
                .into());
            };

            if u64::try_from(value) != Ok(offset) {
                return Err(format!(
                    "V8 {version}'s {symbol} is {value}, but its generated layout puts {} \
                     at {offset}",
                    field.name()
                )
                .into());
            }
        }
    }

    Ok(())
}

/// Checks that each instance type the binary exports, whose name Torque
/// assigns, has the number Torque gave it: a build configured unlike the
/// release, as without Intl or Temporal, numbers them differently.
fn check_instance_types(
    version: V8Version,
    generated: &Generated,
    values: &BTreeMap<String, i64>,
) -> Result<(), Arc<str>> {
    let prefix = format!("{PREFIX}type_");
    let types = values
        .range(prefix.clone()..)
        .take_while(|(name, _)| name.starts_with(&prefix));
    for (symbol, &value) in types {
        let Some((_, name)) = symbol.rsplit_once("__") else {
            continue;
        };

        let Some(own) = generated.instance_type(name) else {
            continue;
        };

        if value != i64::from(own) {
            return Err(format!(
                "V8 {version}'s {symbol} is {value}, but its Torque output numbers {name} \
                 {own}: the build is configured unlike the release"
            )
            .into());
        }
    }

    Ok(())
}

/// Checks that the layout has every field and constant its version needs.
fn check_complete(version: V8Version, layout: &V8Layout) -> Result<(), Arc<str>> {
    let minor = (version.major, version.minor);
    if let Some(field) = Field::ALL
        .iter()
        .find(|&&field| requires_field(field, minor) && !layout.has(field))
    {
        return Err(format!("V8 {version}'s layout has no {}", field.name()).into());
    }

    if let Some(constant) = Constant::ALL
        .iter()
        .find(|&&constant| requires_constant(constant, minor) && !layout.has_constant(constant))
    {
        let source = if constant.is_torque() { "" } else { PREFIX };
        return Err(format!("V8 {version}'s layout has no {source}{}", constant.name()).into());
    }

    Ok(())
}

/// The first version with V8's leaptiering, in which a function has a
/// dispatch handle instead of its code, and the trusted space.
const LEAPTIERING: (u32, u32) = (13, 6);

/// Whether V8 `major.minor` has `field`, which its layout must then give.
fn requires_field(field: Field, version: (u32, u32)) -> bool {
    match field {
        Field::JSFunctionCode | Field::SharedFunctionInfoFunctionData => version < LEAPTIERING,
        Field::JSFunctionDispatchHandle
        | Field::SharedFunctionInfoTrustedFunctionData
        | Field::SharedFunctionInfoUntrustedFunctionData
        | Field::SharedFunctionInfoWrapperSharedInfo
        | Field::ScopeInfoPositionInfo => version >= LEAPTIERING,
        _ => true,
    }
}

/// Whether V8 `major.minor` has `constant`, which its layout must then
/// give. A frame type that no frame of the model's needs, or that only
/// some versions have, is read where the binary exports it.
fn requires_constant(constant: Constant, version: (u32, u32)) -> bool {
    use Constant as C;

    let since_13_6 = version >= LEAPTIERING;
    let since_14_6 = version >= (14, 6);
    match constant {
        C::FrameEntry
        | C::FrameConstructEntry
        | C::FrameExit
        | C::FrameBuiltinExit
        | C::FrameInterpreted
        | C::FrameBaseline
        | C::FrameMaglev
        | C::FrameStub
        | C::FrameBuiltin
        | C::FrameInternal
        | C::FrameConstruct => true,
        C::FrameTurbofan | C::DeoptimizationDataSharedFunctionInfoIndex => !since_13_6,
        C::FrameTurbofanJS | C::DeoptimizationDataWrappedSharedFunctionInfoIndex => since_13_6,
        constant if constant.name().starts_with("frametype_") => false,

        C::SharedFunctionInfoWrapperType
        | C::SfiLiveEditedShift
        | C::SfiLiveEditedMask
        | C::ScopeSomeContextHasExtensionShift
        | C::ScopeSomeContextHasExtensionMask
        | C::ScopeIsHiddenShift
        | C::ScopeIsHiddenMask
        | C::ScopeIsWrappedFunctionShift
        | C::ScopeIsWrappedFunctionMask => since_13_6,
        C::ScopeIsReplModeScopeShift
        | C::ScopeIsReplModeScopeMask
        | C::ScopeHasLocalsBlockListShift
        | C::ScopeHasLocalsBlockListMask => !since_13_6,
        C::ScopeHasNewTargetShift | C::ScopeHasNewTargetMask => !since_14_6,
        C::ScopeAllocatesArgumentsShift
        | C::ScopeAllocatesArgumentsMask
        | C::ScopeHasContextCellsShift
        | C::ScopeHasContextCellsMask => since_14_6,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every build in the dev shell's matrix places each thread's current
    /// isolate: by its symbol, or, in Debian's libnode, which names none,
    /// by the code of the accessor it exports.
    #[test]
    fn every_build_places_the_current_isolate() {
        let root = std::env::var("MICROSCOPE_NODE").expect("MICROSCOPE_NODE; run in the dev shell");
        for entry in [
            "26",
            "24",
            "22",
            "nixpkgs-26",
            "nixpkgs-24",
            "nixpkgs-22",
            "debian-24",
        ] {
            let path = if entry == "debian-24" {
                format!("{root}/debian-24/root/usr/lib/x86_64-linux-gnu/libnode.so.137")
            } else {
                format!("{root}/{entry}/node")
            };
            let data = std::fs::read(&path).expect("the build");
            let object = object::File::parse(data.as_slice()).expect("an ELF file");
            let mut thread_locals = super::super::elf::load_thread_locals(&object);
            add_thread_locals(&object, &mut thread_locals);
            let place = thread_locals.get(CURRENT_ISOLATE);
            match entry {
                // F28: the offset its debug package's symbol gives.
                "debian-24" => assert_eq!(place, Some(&Ok(crate::ThreadLocal::Dynamic(0x170)))),
                _ => assert!(
                    matches!(
                        place,
                        Some(Ok(
                            crate::ThreadLocal::Offset(_) | crate::ThreadLocal::Slot(_)
                        ))
                    ),
                    "{entry}: {place:?}"
                ),
            }
        }
    }

    const V14_6: V8Version = V8Version {
        major: 14,
        minor: 6,
        build: 202,
        patch: 34,
    };

    /// The constants of a build of V8 14.6 that binds: each scalar its
    /// layout needs, the fields of `Code`, and one field and one instance
    /// type that agree with its generated layout.
    fn exported() -> BTreeMap<String, i64> {
        let mut values: BTreeMap<String, i64> = Constant::ALL
            .iter()
            .filter(|constant| !constant.is_torque())
            .map(|constant| (format!("{PREFIX}{}", constant.name()), 0))
            .collect();
        values.insert("v8dbg_TaggedSize".to_owned(), 8);
        values.insert("v8dbg_SmiShiftSize".to_owned(), 31);
        values.insert("v8dbg_off_fp_function".to_owned(), -16);
        for (_, name) in BINARY_FIELDS {
            values.insert(format!("{PREFIX}class_{name}__int"), 40);
        }

        values.insert(
            "v8dbg_class_JSFunction__shared__SharedFunctionInfo".to_owned(),
            32,
        );
        values.insert("v8dbg_type_Script__SCRIPT_TYPE".to_owned(), 166);
        values
    }

    fn refused(values: &BTreeMap<String, i64>, sandbox: bool) -> String {
        bind(V14_6, values, sandbox)
            .expect_err("the build is refused")
            .to_string()
    }

    #[test]
    fn a_build_whose_constants_agree_binds() {
        let layout = bind(V14_6, &exported(), false).expect("a layout");
        assert_eq!(layout.field(Field::JSFunctionSharedFunctionInfo), Some(32));
        assert_eq!(layout.field(Field::CodeInstructionStart), Some(40));
        assert_eq!(layout.field(Field::JSFunctionCode), None);
        assert_eq!(layout.constant(Constant::OffFpFunction), Some(-16));
        assert_eq!(layout.constant(Constant::ScriptType), Some(166));
        assert_eq!(layout.constant(Constant::FrameTurbofan), Some(0));
        assert!(layout.has_constant(Constant::ScopeHasContextCellsMask));
        assert_eq!(layout.constant(Constant::ScopeHasNewTargetShift), None);
    }

    #[test]
    fn a_version_with_no_layout_is_refused_by_name() {
        let version = V8Version {
            major: 12,
            minor: 3,
            ..V14_6
        };
        let refused = bind(version, &exported(), false).expect_err("no layout");
        assert_eq!(
            &*refused,
            "V8 12.3 has no layout in this build of microscope"
        );

        let mut exports = Exports {
            values: exported(),
            ..Exports::default()
        };
        let unknown = describe(&exports).expect("a V8 runtime");
        assert!(
            unknown
                .layout
                .expect_err("no version")
                .contains("its version is unknown")
        );
        assert_eq!(unknown.flags, DYNAMIC_SYMBOLS);

        exports.values.remove(FRAME_FUNCTION_OFFSET);
        assert_eq!(describe(&exports), None, "not V8's");
    }

    #[test]
    fn builds_unlike_the_layouts_are_refused_with_the_reason() {
        let mut compressed = exported();
        compressed.insert("v8dbg_TaggedSize".to_owned(), 4);
        assert!(refused(&compressed, false).contains("pointer compression"));

        let mut small = exported();
        small.insert("v8dbg_SmiShiftSize".to_owned(), 0);
        assert!(refused(&small, false).contains("31-bit Smis"));

        assert!(refused(&exported(), true).contains("sandbox"));

        let mut silent = exported();
        silent.remove("v8dbg_TaggedSize");
        assert!(refused(&silent, false).contains("exports no v8dbg_TaggedSize"));
    }

    #[test]
    fn the_sandbox_is_told_by_the_api_only_it_declares() {
        assert!(is_sandbox_api("_ZN2v82V821GetSandboxSizeInBytesEv"));
        assert!(is_sandbox_api("_ZN2v82V826IsSandboxConfiguredSecurelyEv"));
        assert!(is_sandbox_api(
            "_ZN2v88internal7Sandbox10InitializeEPNS_19VirtualAddressSpaceE"
        ));
        assert!(!is_sandbox_api("_ZN2v82V810GetVersionEv"));
        assert!(!is_sandbox_api(
            "_ZNK2v88internal10JSFunction4codeENS0_17IsolateForSandboxE"
        ));
        assert!(!is_sandbox_api(
            "_ZN2v88internal22SandboxHardwareSupport25NotifyReadOnlyPageCreatedEmm"
        ));
    }

    #[test]
    fn a_field_or_type_unlike_the_layouts_names_the_first_that_differs() {
        let mut moved = exported();
        moved.insert(
            "v8dbg_class_JSFunction__shared__SharedFunctionInfo".to_owned(),
            40,
        );
        let refused_moved = refused(&moved, false);
        assert!(
            refused_moved.contains("v8dbg_class_JSFunction__shared__SharedFunctionInfo is 40")
                && refused_moved.contains("JSFunction::SharedFunctionInfo at 32"),
            "{refused_moved}"
        );

        let mut extra = exported();
        extra.insert("v8dbg_class_JSFunction__code__Tagged_Code_".to_owned(), 24);
        assert!(refused(&extra, false).contains("has no JSFunction::Code"));

        let mut renumbered = exported();
        renumbered.insert("v8dbg_type_Script__SCRIPT_TYPE".to_owned(), 167);
        let refused_type = refused(&renumbered, false);
        assert!(
            refused_type.contains("v8dbg_type_Script__SCRIPT_TYPE is 167")
                && refused_type.contains("configured unlike the release"),
            "{refused_type}"
        );

        // A type Torque does not assign, as a string's, is not checked.
        let mut string = exported();
        string.insert(
            "v8dbg_type_SeqOneByteString__SEQ_ONE_BYTE_STRING_TYPE".to_owned(),
            1,
        );
        assert!(bind(V14_6, &string, false).is_ok());
    }

    #[test]
    fn a_build_missing_a_constant_or_field_it_needs_is_refused() {
        let mut values = exported();
        values.remove("v8dbg_frametype_InterpretedFrame");
        assert!(refused(&values, false).contains("layout has no v8dbg_frametype_InterpretedFrame"));

        let mut values = exported();
        values.remove("v8dbg_class_Code__instruction_start__int");
        assert!(refused(&values, false).contains("layout has no Code::InstructionStart"));

        // A frame type only some versions have is read where it is
        // exported.
        let mut values = exported();
        values.remove("v8dbg_frametype_WasmJspiFrame");
        assert!(bind(V14_6, &values, false).is_ok());
    }
}
