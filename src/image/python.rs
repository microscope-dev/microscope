//! The Python runtime a module carries: CPython's version, and where the
//! structures it keeps its threads, frames, code, and objects in hold
//! each field the runtime model reads, validated at load. One row, for a
//! module with a `.PyRuntime` section (see `crate::debug_info::cpython`).
//!
//! A layout is the offsets of [`Field`]s and the values of [`Constant`]s,
//! each indexed by its enum, so that no per-version number or name
//! reaches the runtime model.

use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::strings::{StrId, StringsBuilder};
use super::{Builder, Image, NONE, Record, TableKind};
use crate::{AddressRange, ImageAddress};

/// Defines an enum of a layout's entries, each with the name CPython's
/// `_Py_DebugOffsets` or headers give it.
macro_rules! entries {
    ($(#[$meta:meta])* $kind:ident { $($(#[$doc:meta])* $variant:ident = $name:literal,)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(u8)]
        pub enum $kind {
            $($(#[$doc])* $variant,)*
        }

        impl $kind {
            /// Every entry, in order.
            pub const ALL: &[Self] = &[$(Self::$variant,)*];
            pub const COUNT: usize = Self::ALL.len();

            /// The entry's name: `struct.member` for a field, as
            /// `_Py_DebugOffsets` spells it where it has the field, or the
            /// C name of a constant.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// The entry `name` names.
            #[must_use]
            pub fn named(name: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|entry| entry.name() == name)
            }

            const fn index(self) -> usize {
                self as usize
            }
        }
    };
}

entries! {
    /// A field of one of CPython's structures, by its offset in bytes, or
    /// a structure's size.
    Field {
        RuntimeInterpretersHead = "runtime_state.interpreters_head",

        InterpreterId = "interpreter_state.id",
        InterpreterNext = "interpreter_state.next",
        InterpreterThreadsHead = "interpreter_state.threads_head",

        /// The states an interpreter keeps of the static built-in types,
        /// from the first to the end of the last, each of
        /// [`Field::StaticTypeStateSize`] bytes, and where each keeps its
        /// type's dict.
        InterpreterStaticTypes = "interpreter_state.static_types",
        InterpreterStaticTypesEnd = "interpreter_state.static_types_end",
        StaticTypeStateSize = "static_type_state.size",
        StaticTypeStateDict = "static_type_state.tp_dict",

        ThreadSize = "thread_state.size",
        ThreadPrev = "thread_state.prev",
        ThreadNext = "thread_state.next",
        ThreadInterp = "thread_state.interp",

        /// The innermost frame; in 3.12, the `_PyCFrame` pointer whose
        /// [`Field::CframeCurrentFrame`] holds it.
        ThreadCurrentFrame = "thread_state.current_frame",
        ThreadBaseFrame = "thread_state.base_frame",
        ThreadId = "thread_state.thread_id",
        ThreadNativeId = "thread_state.native_thread_id",

        /// 3.12's `_PyCFrame.current_frame`.
        CframeCurrentFrame = "cframe.current_frame",

        FrameSize = "interpreter_frame.size",
        FramePrevious = "interpreter_frame.previous",

        /// The code object, `f_code` in 3.12.
        FrameExecutable = "interpreter_frame.executable",

        /// The instruction, `prev_instr` in 3.12.
        FrameInstruction = "interpreter_frame.instr_ptr",
        FrameLocalsplus = "interpreter_frame.localsplus",
        FrameOwner = "interpreter_frame.owner",

        /// The globals and the builtins the frame's code reads, each a
        /// dict.
        FrameGlobals = "interpreter_frame.globals",
        FrameBuiltins = "interpreter_frame.builtins",
        FrameTlbcIndex = "interpreter_frame.tlbc_index",

        CodeSize = "code_object.size",
        CodeFilename = "code_object.filename",
        CodeName = "code_object.name",
        CodeQualname = "code_object.qualname",
        CodeLinetable = "code_object.linetable",
        CodeFirstlineno = "code_object.firstlineno",
        CodeArgcount = "code_object.argcount",
        CodeKwonlyargcount = "code_object.co_kwonlyargcount",
        CodeLocalsplusnames = "code_object.localsplusnames",
        CodeLocalspluskinds = "code_object.localspluskinds",
        CodeCodeAdaptive = "code_object.co_code_adaptive",
        CodeTlbc = "code_object.co_tlbc",
        CodeFlags = "code_object.co_flags",
        CodeNlocalsplus = "code_object.co_nlocalsplus",

        /// A free-threaded build's `_PyCodeArray`: how many copies of a
        /// code object's bytecode there are, and the first, whose index
        /// a frame's [`Field::FrameTlbcIndex`] gives.
        CodeArraySize = "code_array.size",
        CodeArrayEntries = "code_array.entries",

        ObjectType = "pyobject.ob_type",
        VarObjectSize = "pyvarobject.ob_size",

        TypeName = "type_object.tp_name",
        TypeFlags = "type_object.tp_flags",
        TypeBasicsize = "type_object.tp_basicsize",
        TypeDictoffset = "type_object.tp_dictoffset",
        TypeDict = "type_object.tp_dict",
        TypeMro = "type_object.tp_mro",

        /// A static built-in type's index, plus one, among the states
        /// its interpreter keeps of such types, which hold their dicts.
        TypeSubclasses = "type_object.tp_subclasses",

        /// How the type reads its instances' attributes, and how a value
        /// of it binds or sets as a descriptor; null where it does not.
        TypeGetattro = "type_object.tp_getattro",
        TypeDescrGet = "type_object.tp_descr_get",
        TypeDescrSet = "type_object.tp_descr_set",

        /// A class's qualified name, and the keys its instances' split
        /// dicts share.
        HeapTypeQualname = "heap_type_object.ht_qualname",
        HeapTypeCachedKeys = "heap_type_object.ht_cached_keys",

        /// A member descriptor's definition, such as a slot's: the type
        /// of the value it keeps, and where in an instance it keeps it.
        MemberDescrMember = "member_descr_object.d_member",
        MemberDefType = "member_def.type",
        MemberDefOffset = "member_def.offset",

        CellRef = "cell_object.ob_ref",
        SetUsed = "set_object.used",
        SetMask = "set_object.mask",
        SetTable = "set_object.table",
        SetEntrySize = "set_entry.size",
        SetEntryKey = "set_entry.key",
        SetEntryHash = "set_entry.hash",
        ModuleDict = "module_object.md_dict",

        TupleItems = "tuple_object.ob_item",
        TupleSize = "tuple_object.ob_size",
        ListItems = "list_object.ob_item",
        ListSize = "list_object.ob_size",

        DictKeys = "dict_object.ma_keys",
        DictValues = "dict_object.ma_values",
        DictUsed = "dict_object.ma_used",
        DictKeysLog2Size = "dict_keys.dk_log2_size",
        DictKeysLog2IndexBytes = "dict_keys.dk_log2_index_bytes",
        DictKeysKind = "dict_keys.dk_kind",
        DictKeysNentries = "dict_keys.dk_nentries",
        DictKeysIndices = "dict_keys.dk_indices",
        DictEntrySize = "dict_entry.size",

        /// The hash a general dict's entry keeps of its key.
        DictEntryHash = "dict_entry.me_hash",
        DictEntryKey = "dict_entry.me_key",
        DictEntryValue = "dict_entry.me_value",
        DictUnicodeEntrySize = "dict_unicode_entry.size",
        DictUnicodeEntryKey = "dict_unicode_entry.me_key",
        DictUnicodeEntryValue = "dict_unicode_entry.me_value",
        DictValuesValues = "dict_values.values",

        /// From 3.13, how many values the values hold room for and hold,
        /// and whether an instance's inline values are its attributes.
        DictValuesCapacity = "dict_values.capacity",
        DictValuesSize = "dict_values.size",
        DictValuesValid = "dict_values.valid",

        FloatValue = "float_object.ob_fval",
        LongTag = "long_object.lv_tag",
        LongDigits = "long_object.ob_digit",
        BytesSize = "bytes_object.ob_size",
        BytesData = "bytes_object.ob_sval",

        UnicodeState = "unicode_object.state",
        UnicodeLength = "unicode_object.length",

        /// The hash a str keeps of itself once it is hashed, or -1.
        UnicodeHash = "unicode_object.hash",
        UnicodeAsciiSize = "unicode_object.asciiobject_size",
        UnicodeCompactSize = "unicode_object.compactunicodeobject_size",
        UnicodeData = "unicode_object.data",
    }
}

entries! {
    /// A number of CPython's that no structure holds.
    Constant {
        OwnerThread = "FRAME_OWNED_BY_THREAD",
        OwnerGenerator = "FRAME_OWNED_BY_GENERATOR",
        OwnerFrameObject = "FRAME_OWNED_BY_FRAME_OBJECT",

        /// The owner of the frame each run of the evaluation loop pushes
        /// before its first: `FRAME_OWNED_BY_CSTACK` through 3.13, then
        /// `FRAME_OWNED_BY_INTERPRETER`.
        OwnerEntry = "entry_frame_owner",

        /// Where `PyASCIIObject.state`'s fields begin, by bit.
        UnicodeKindShift = "unicode_state.kind",
        UnicodeCompactShift = "unicode_state.compact",
        UnicodeAsciiShift = "unicode_state.ascii",

        LongShift = "PyLong_SHIFT",
        LongNonSizeBits = "_PyLong_NON_SIZE_BITS",

        /// 1 where frames hold `_PyStackRef`s, which tag their low bits,
        /// as from 3.14; 0 where they hold `PyObject *`s.
        StackRefs = "stack_refs",

        FastLocal = "CO_FAST_LOCAL",
        FastCell = "CO_FAST_CELL",
        FastFree = "CO_FAST_FREE",

        /// 0 before 3.13, which hides no locals.
        FastHidden = "CO_FAST_HIDDEN",
        Optimized = "CO_OPTIMIZED",
        VarArgs = "CO_VARARGS",
        VarKeywords = "CO_VARKEYWORDS",

        DictKeysGeneral = "DICT_KEYS_GENERAL",
        DictKeysUnicode = "DICT_KEYS_UNICODE",
        DictKeysSplit = "DICT_KEYS_SPLIT",

        /// The bits of `tp_flags` that mark a class, a type whose
        /// instances' dicts CPython manages, and, from 3.13, one whose
        /// instances keep their attributes' values inline.
        TypeHeapBit = "Py_TPFLAGS_HEAPTYPE",
        TypeStaticBuiltinBit = "_Py_TPFLAGS_STATIC_BUILTIN",
        TypeManagedDictBit = "Py_TPFLAGS_MANAGED_DICT",
        TypeInlineValuesBit = "Py_TPFLAGS_INLINE_VALUES",

        /// How many words before an object its managed dict's pointer is,
        /// and 1 where that word tags values by its low bit, as in 3.12.
        ManagedDictWords = "managed_dict_words",
        ManagedDictTagged = "managed_dict_tagged",

        /// The member type of a slot, which keeps an object or none.
        MemberObjectEx = "Py_T_OBJECT_EX",
    }
}

/// The function each activation of CPython's evaluation loop runs in.
pub const EVALUATION_LOOP: &str = "_PyEval_EvalFrameDefault";

/// What a build of CPython whose evaluation loop tail-calls a function for
/// each instruction names those functions: each activation of the loop is
/// the instruction's it runs.
pub const TAIL_CALL_PREFIX: &str = "_TAIL_CALL_";

/// An offset no field has: the version does not have the field.
const ABSENT: u32 = u32::MAX;

/// The runtime is CPython's free-threaded build.
pub const FREE_THREADED: u16 = 1;

/// Its layout came from the runtime's own `_Py_DebugOffsets`.
pub const TARGET_TABLE: u16 = 2;

/// Its layout, or the part the runtime's table leaves out, came from
/// microscope's table generated from the version's headers.
pub const GENERATED_TABLE: u16 = 4;

/// The Python runtime a module carries.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct PythonRuntimeRecord {
    /// `PY_VERSION_HEX`.
    pub version: U32,
    pub flags: U16,
    pub reserved: U16,

    /// The `.PyRuntime` section, where `_PyRuntime` is.
    pub runtime_start: U64,
    pub runtime_end: U64,

    /// Why the module's runtime cannot be read, a string, or [`NONE`]
    /// when its layout is complete.
    pub problem: U32,
    pub offsets: [U32; Field::COUNT],
    pub constants: [u8; Constant::COUNT],
}

impl Record for PythonRuntimeRecord {
    const KIND: TableKind = TableKind::PythonRuntime;
}

/// A CPython version, as `PY_VERSION_HEX` encodes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PyVersion(pub u32);

impl PyVersion {
    #[must_use]
    pub const fn major(self) -> u32 {
        self.0 >> 24
    }

    #[must_use]
    pub const fn minor(self) -> u32 {
        (self.0 >> 16) & 0xff
    }

    #[must_use]
    pub const fn micro(self) -> u32 {
        (self.0 >> 8) & 0xff
    }

    /// Whether it is a final release, not an alpha, beta, or candidate.
    #[must_use]
    pub const fn is_final(self) -> bool {
        (self.0 >> 4) & 0xf == 0xf
    }
}

impl std::fmt::Display for PyVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}.{}.{}",
            self.major(),
            self.minor(),
            self.micro()
        )?;
        let serial = self.0 & 0xf;
        match (self.0 >> 4) & 0xf {
            0xa => write!(formatter, "a{serial}"),
            0xb => write!(formatter, "b{serial}"),
            0xc => write!(formatter, "rc{serial}"),
            _ => Ok(()),
        }
    }
}

/// Where each field is, and each constant's value, for one version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonLayout {
    offsets: [u32; Field::COUNT],
    constants: [u8; Constant::COUNT],
}

impl Default for PythonLayout {
    fn default() -> Self {
        Self {
            offsets: [ABSENT; Field::COUNT],
            constants: [0; Constant::COUNT],
        }
    }
}

impl PythonLayout {
    /// The field's offset, or its structure's size; `None` when the
    /// version does not have it.
    #[must_use]
    pub const fn field(&self, field: Field) -> Option<u64> {
        match self.offsets[field.index()] {
            ABSENT => None,
            offset => Some(offset as u64),
        }
    }

    #[must_use]
    pub const fn constant(&self, constant: Constant) -> u8 {
        self.constants[constant.index()]
    }

    /// Sets a field, or leaves it absent when its offset does not fit.
    pub fn set_field(&mut self, field: Field, offset: u64) {
        self.offsets[field.index()] = u32::try_from(offset)
            .ok()
            .filter(|offset| *offset != ABSENT)
            .unwrap_or(ABSENT);
    }

    pub const fn set_constant(&mut self, constant: Constant, value: u8) {
        self.constants[constant.index()] = value;
    }

    /// Whether the version has the field.
    #[must_use]
    pub const fn has(&self, field: Field) -> bool {
        self.field(field).is_some()
    }
}

/// What a module's loader found of its Python runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonRuntime {
    pub version: PyVersion,
    pub flags: u16,

    /// The `.PyRuntime` section.
    pub runtime: AddressRange<ImageAddress>,

    /// The layout, or why the runtime cannot be read, as for a version
    /// microscope does not know.
    pub layout: Result<PythonLayout, std::sync::Arc<str>>,
}

impl PythonRuntime {
    #[must_use]
    pub const fn free_threaded(&self) -> bool {
        self.flags & FREE_THREADED != 0
    }
}

/// Why a runtime could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the Python runtime does not fit an image")]
pub struct TooLarge;

/// Adds the module's Python runtime, if it has one, to `builder`.
pub fn add_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    runtime: Option<&PythonRuntime>,
) -> Result<(), TooLarge> {
    let records = runtime
        .map(|runtime| {
            let (layout, problem) = match &runtime.layout {
                Ok(layout) => (layout.clone(), NONE),
                Err(problem) => (
                    PythonLayout::default(),
                    strings.push(problem).ok_or(TooLarge)?.0,
                ),
            };

            Ok(PythonRuntimeRecord {
                version: runtime.version.0.into(),
                flags: runtime.flags.into(),
                reserved: 0.into(),
                runtime_start: runtime.runtime.start.get().into(),
                runtime_end: runtime.runtime.end.get().into(),
                problem: problem.into(),
                offsets: layout.offsets.map(U32::new),
                constants: layout.constants,
            })
        })
        .transpose()?;
    builder.owned_table(records.into_iter().collect());
    Ok(())
}

/// The Python runtime of a validated image, if it has one.
#[must_use]
pub fn runtime(image: &Image) -> Option<PythonRuntime> {
    let record: &PythonRuntimeRecord = image.table().first()?;
    let problem = record.problem.get();
    Some(PythonRuntime {
        version: PyVersion(record.version.get()),
        flags: record.flags.get(),
        runtime: AddressRange {
            start: ImageAddress::new(record.runtime_start.get()),
            end: ImageAddress::new(record.runtime_end.get()),
        },
        layout: if problem == NONE {
            Ok(PythonLayout {
                offsets: record.offsets.map(U32::get),
                constants: record.constants,
            })
        } else {
            Err(image.strings().get(StrId(problem)).into())
        },
    })
}

/// Checks that an image has at most one runtime, which sets only the
/// flags this format defines, whose problem names a string, and whose
/// section is a range.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    let records: &[PythonRuntimeRecord] = image.table();
    if records.len() > 1 {
        return Err(format!("{} Python runtimes in one image", records.len()));
    }

    let Some(record) = records.first() else {
        return Ok(());
    };

    if record.flags.get() & !(FREE_THREADED | TARGET_TABLE | GENERATED_TABLE) != 0
        || record.reserved.get() != 0
    {
        return Err("the Python runtime sets flags it does not define".to_owned());
    }

    if record.runtime_start.get() > record.runtime_end.get() {
        return Err("the Python runtime's section ends before it begins".to_owned());
    }

    let problem = record.problem.get();
    if problem != NONE && !image.strings().contains(StrId(problem)) {
        return Err(format!(
            "the Python runtime's problem {problem} is no string"
        ));
    }

    Ok(())
}
