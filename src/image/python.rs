//! CPython version and validated layouts for threads, frames, code, and objects.
//! One row per module with `.PyRuntime` (see `crate::debug_info::cpython`).
//!
//! [`Field`] offsets and [`Constant`] values are enum-indexed, keeping version-specific
//! numbers and names out of the runtime model.

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
        RuntimeInitialized = "runtime_state.initialized",
        RuntimeFinalizing = "runtime_state.finalizing",

        InterpreterId = "interpreter_state.id",
        InterpreterNext = "interpreter_state.next",
        InterpreterThreadsHead = "interpreter_state.threads_head",

        /// The states an interpreter keeps of the static built-in types,
        /// from the first to the end of the last, each of
        /// [`Field::StaticTypeStateSize`] bytes, and where each keeps its
        /// type and its type's dict. From 3.13 it keeps the static types
        /// of extensions it manages as well, as `_datetime`'s, apart.
        InterpreterStaticTypes = "interpreter_state.static_types",
        InterpreterStaticTypesEnd = "interpreter_state.static_types_end",
        InterpreterStaticExtensionTypes = "interpreter_state.static_extension_types",
        InterpreterStaticExtensionTypesEnd = "interpreter_state.static_extension_types_end",
        StaticTypeStateSize = "static_type_state.size",
        StaticTypeStateDict = "static_type_state.tp_dict",
        StaticTypeStateType = "static_type_state.type",

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

        /// PEP 768, from 3.14. The first two offsets are relative to
        /// `PyThreadState`, Enabled to `PyInterpreterState`, and Pending and
        /// Script to the embedded remote-debugger support structure.
        DebugEvalBreaker = "debugger_support.eval_breaker",
        DebugSupport = "debugger_support.remote_debugger_support",
        DebugEnabled = "debugger_support.remote_debugging_enabled",
        DebugPending = "debugger_support.debugger_pending_call",
        DebugScript = "debugger_support.debugger_script_path",
        DebugScriptSize = "debugger_support.debugger_script_path_size",

        /// From 3.15, whether a thread holds its interpreter's GIL, and
        /// whether it asks for it, each an `int`.
        ThreadHoldsGil = "thread_state.holds_gil",
        ThreadGilRequested = "thread_state.gil_requested",

        /// The GIL an interpreter takes, `ceval.gil`: its own
        /// [`Field::InterpreterGil`], or another interpreter's it shares.
        InterpreterCevalGil = "interpreter_state.ceval_gil",

        /// An interpreter's own GIL, and its `locked` and `last_holder`,
        /// each from the start of the interpreter's state, which the
        /// GIL's structure holds at the difference.
        InterpreterGil = "interpreter_state.gil_runtime_state",
        InterpreterGilLocked = "interpreter_state.gil_runtime_state_locked",
        InterpreterGilHolder = "interpreter_state.gil_runtime_state_holder",

        /// In a free-threaded build, whether its GIL is on, an `int` from
        /// the start of the GIL's structure.
        GilEnabled = "interpreter_state.gil_runtime_state_enabled",

        /// From 3.14, the asyncio task a thread runs, and the head of the
        /// circular list of the `asyncio.Task`s it made, linked through
        /// each task's [`Field::TaskNode`], in `_PyThreadStateImpl`.
        ThreadAsyncioRunningTask = "thread_state_impl.asyncio_running_task",
        ThreadAsyncioTasksHead = "thread_state_impl.asyncio_tasks_head",

        /// From 3.14, the head of the list of the tasks of threads that
        /// have exited.
        InterpreterAsyncioTasksHead = "interpreter_state.asyncio_tasks_head",
        LlistNext = "llist_node.next",
        LlistPrev = "llist_node.prev",

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

        /// From 3.14, the top of the frame's stack of values, which
        /// points past the last.
        FrameStackpointer = "interpreter_frame.stackpointer",

        /// A generator's or coroutine's state, one of the
        /// `Constant::Generator*` bytes, and the frame it embeds.
        GenFrameState = "gen_object.gi_frame_state",
        GenFrame = "gen_object.gi_iframe",

        /// From 3.14, `asyncio.Task`, as `Modules/_asynciomodule.c`
        /// defines it: its future's state, who awaits it (one future, or
        /// a set of them where `TaskAwaitedByIsSet` is 1), whether it is
        /// a task, the future it waits for, its coroutine, its name (an
        /// `int` until Python asks for it as `Task-N`), and its node in
        /// its thread's list.
        TaskState = "task_object.task_state",
        TaskAwaitedBy = "task_object.task_awaited_by",
        TaskIsTask = "task_object.task_is_task",
        TaskAwaitedByIsSet = "task_object.task_awaited_by_is_set",
        TaskFutWaiter = "task_object.task_fut_waiter",
        TaskCoro = "task_object.task_coro",
        TaskName = "task_object.task_name",
        TaskNode = "task_object.task_node",
        TaskObjectSize = "task_object.size",

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

        /// The size of every object's header, which an extension's own
        /// structure starts after, as a numpy array's does.
        ObjectSize = "pyobject.size",
        VarObjectSize = "pyvarobject.ob_size",

        TypeName = "type_object.tp_name",
        TypeFlags = "type_object.tp_flags",
        TypeBasicsize = "type_object.tp_basicsize",
        TypeDictoffset = "type_object.tp_dictoffset",
        TypeDict = "type_object.tp_dict",
        TypeMro = "type_object.tp_mro",
        TypeBase = "type_object.tp_base",

        /// A static built-in type's index, plus one, among the states
        /// its interpreter keeps of such types, which hold their dicts.
        TypeSubclasses = "type_object.tp_subclasses",

        /// How the type reads its instances' attributes, and how a value
        /// of it binds or sets as a descriptor; null where it does not.
        TypeGetattro = "type_object.tp_getattro",
        TypeDescrGet = "type_object.tp_descr_get",
        TypeDescrSet = "type_object.tp_descr_set",

        /// A class's qualified name, the keys its instances' split dicts
        /// share, and the names of its slots, sorted, or null.
        HeapTypeQualname = "heap_type_object.ht_qualname",
        HeapTypeCachedKeys = "heap_type_object.ht_cached_keys",
        HeapTypeSlots = "heap_type_object.ht_slots",

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
        FunctionQualname = "function_object.func_qualname",

        /// A built-in function's method definition, whose name is its
        /// name, and the object it is bound to, NULL or a module for a
        /// function.
        BuiltinMethod = "builtin_function.m_ml",
        BuiltinSelf = "builtin_function.m_self",
        MethodDefName = "method_def.ml_name",

        /// The C function a method definition calls, and the definition a
        /// type's method descriptor, as `list.append`, calls through.
        MethodDefMeth = "method_def.ml_meth",
        MethodDescrMethod = "method_descr_object.d_method",

        /// The C function a slot's wrapper, as `int.__getattribute__`,
        /// wraps.
        WrapperDescrWrapped = "wrapper_descr_object.d_wrapped",

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

        /// The standard library's types that only their own C defines
        /// (`scripts/cpython-layouts.c` copies them), and the sizes their
        /// types give their objects, which tell them from others of the
        /// same name. A `collections.deque` keeps its items in blocks of
        /// [`Constant::DequeBlockLength`], linked to the right, the first
        /// item at `leftindex` of its first block.
        DequeLeftBlock = "deque_object.leftblock",
        DequeLeftIndex = "deque_object.leftindex",
        DequeMaxlen = "deque_object.maxlen",
        DequeObjectSize = "deque_object.size",
        DequeBlockData = "deque_block.data",
        DequeBlockRightLink = "deque_block.rightlink",
        DefaultDictFactory = "defdict_object.default_factory",
        DefaultDictObjectSize = "defdict_object.size",

        /// An `OrderedDict` keeps its order in a list of nodes, each
        /// holding a key of its dict and the key's hash.
        OrderedDictFirst = "odict_object.od_first",
        OrderedDictObjectSize = "odict_object.size",
        OrderedDictNodeKey = "odict_node.key",
        OrderedDictNodeHash = "odict_node.hash",
        OrderedDictNodeNext = "odict_node.next",

        /// `datetime`'s objects keep their fields packed in bytes, and a
        /// time or datetime its tzinfo only where `hastzinfo` says so.
        DateHasTzinfo = "date_object.hastzinfo",
        DateData = "date_object.data",
        DateObjectSize = "date_object.size",
        TimeData = "time_object.data",
        TimeFold = "time_object.fold",
        TimeTzinfo = "time_object.tzinfo",
        TimeObjectSize = "time_object.size",
        DateTimeData = "datetime_object.data",
        DateTimeFold = "datetime_object.fold",
        DateTimeTzinfo = "datetime_object.tzinfo",
        DateTimeObjectSize = "datetime_object.size",
        DeltaDays = "delta_object.days",
        DeltaSeconds = "delta_object.seconds",
        DeltaMicroseconds = "delta_object.microseconds",
        DeltaObjectSize = "delta_object.size",
        TimeZoneOffset = "timezone_object.offset",
        TimeZoneName = "timezone_object.name",
        TimeZoneObjectSize = "timezone_object.size",

        /// A `decimal.Decimal` keeps libmpdec's number: its sign and kind
        /// in `flags`, its exponent, and its coefficient's digits in words
        /// of 19 decimal digits each, least significant first.
        DecimalFlags = "decimal_object.dec.flags",
        DecimalExponent = "decimal_object.dec.exp",
        DecimalDigits = "decimal_object.dec.digits",
        DecimalLength = "decimal_object.dec.len",
        DecimalData = "decimal_object.dec.data",
        DecimalObjectSize = "decimal_object.size",
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

        /// The bit of `tp_flags` that marks `type` and its subclasses,
        /// whose instances are classes.
        TypeTypeSubclassBit = "Py_TPFLAGS_TYPE_SUBCLASS",
        TypeManagedDictBit = "Py_TPFLAGS_MANAGED_DICT",
        TypeInlineValuesBit = "Py_TPFLAGS_INLINE_VALUES",

        /// How many words before an object its managed dict's pointer is,
        /// and 1 where that word tags values by its low bit, as in 3.12.
        ManagedDictWords = "managed_dict_words",
        ManagedDictTagged = "managed_dict_tagged",

        /// The member type of a slot, which keeps an object or none.
        MemberObjectEx = "Py_T_OBJECT_EX",

        /// The bit of `tp_flags` that marks a type whose attributes cannot
        /// be set: every type defined in C, as no class is.
        TypeImmutableBit = "Py_TPFLAGS_IMMUTABLETYPE",

        /// How many items each block of a `collections.deque` holds.
        DequeBlockLength = "BLOCKLEN",

        /// The bytes a generator's [`Field::GenFrameState`] holds: not yet
        /// started; suspended; suspended in an `await` or `yield from` of
        /// the object atop its frame's stack, which 3.15 may lock as
        /// well; and running. Any other state is finished.
        GeneratorCreated = "FRAME_CREATED",
        GeneratorSuspended = "FRAME_SUSPENDED",
        GeneratorYieldFrom = "FRAME_SUSPENDED_YIELD_FROM",
        GeneratorYieldFromLocked = "FRAME_SUSPENDED_YIELD_FROM_LOCKED",
        GeneratorExecuting = "FRAME_EXECUTING",

        /// How far below the top of a generator's frame's stack the object
        /// it awaits is, in values: 1, or from 3.15, where the value below
        /// the top is the iterator's index, 2.
        GeneratorAwaitedDepth = "awaited_depth",

        /// The state of a future that has neither a result nor been
        /// cancelled, `STATE_PENDING`.
        FuturePending = "STATE_PENDING",

        /// The eval breaker bit PEP 768 asks a debugger to set.
        DebugStopBit = "_PY_EVAL_PLEASE_STOP_BIT",
    }
}

/// The largest fixed structure read whole: a thread state, including its
/// remote-debugger buffer. Headers and target tables are checked against it.
pub const MAX_STRUCTURE: u64 = 1 << 13;

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
