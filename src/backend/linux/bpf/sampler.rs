//! The profiler's BPF engine (`sampler.bpf.c`): a program on the sampler's
//! own perf events that writes each sampled thread's Python frames to the
//! CPU's ring before the kernel writes the sample. This module loads it,
//! points each CPU's output event at that CPU's ring, keeps its maps (each
//! Python process's layout and each of its threads' states), and decodes
//! what it writes. Pairing the frames with their samples, and naming them,
//! is the recorder's.
//!
//! Records and map values cross the boundary as the structures of
//! `records.h`, which this module mirrors field for field; a test compares
//! both layouts through the object's BTF.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::AsFd as _;
use std::os::unix::fs::MetadataExt as _;

use aya::maps::{HashMap, Map, MapData, PerCpuArray};
use aya::programs::PerfEvent;
use aya::{Ebpf, EbpfLoader};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::super::perf;
use crate::image::python::{Constant, Field, PythonLayout};
use crate::image::v8::{Constant as V8Constant, Field as V8Field, V8Layout};

/// The program, as `build.rs` compiled it.
#[cfg(not(feature = "bpf-walk-timing"))]
static OBJECT: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/sampler.bpf.o"));

/// The program that also times each walk of JavaScript frames, for
/// measuring the walk alone.
#[cfg(feature = "bpf-walk-timing")]
static OBJECT: &[u8] =
    aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/sampler-timed.bpf.o"));

/// `RECORD_PY_STACK`.
const RECORD_PY_STACK: u32 = 9;

/// `RECORD_V8_STACK`.
const RECORD_V8_STACK: u32 = 10;

/// `PY_ABSENT`: a field the version does not have.
const ABSENT: u32 = u32::MAX;

/// `PY_NO_OFFSET`: an instruction offset that could not be read.
pub const NO_OFFSET: i32 = i32::MIN;

/// `struct record`.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct RawRecord {
    kind: u32,
    size: u32,
    tid: u32,
    tgid: u32,
}

/// `struct py_layout`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawPyLayout {
    thread_native_id: u32,
    thread_current_frame: u32,
    cframe_current_frame: u32,
    thread_base_frame: u32,
    thread_running_task: u32,
    thread_holds_gil: u32,
    thread_gil_requested: u32,
    thread_interp: u32,
    interp_ceval_gil: u32,
    gil_locked: u32,
    gil_holder: u32,
    gil_enabled: u32,
    frame_executable: u32,
    frame_previous: u32,
    frame_instruction: u32,
    frame_owner: u32,
    frame_tlbc_index: u32,
    code_qualname: u32,
    code_firstlineno: u32,
    code_linetable: u32,
    code_code_adaptive: u32,
    code_tlbc: u32,
    code_array_size: u32,
    code_array_entries: u32,
    stack_refs: u32,
    owner_entry: u32,
    runtime_interpreters_head: u32,
    interp_next: u32,
    interp_threads_head: u32,
    thread_next: u32,
    runtime: u64,
    ns_dev: u64,
    ns_ino: u64,
}

impl RawPyLayout {
    /// The layout of process `pid`, whose `_PyRuntime` is at `runtime`:
    /// its offsets, and the pid namespace that numbers its threads.
    ///
    /// # Errors
    ///
    /// When the process's pid namespace cannot be read, as when it exited.
    pub fn of_process(self, pid: u32, runtime: u64) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        let namespace = std::fs::metadata(format!("/proc/{pid}/ns/pid"))
            .map_err(|error| format!("its pid namespace cannot be read: {error}"))?;
        Ok(Self {
            runtime,
            ns_dev: namespace.dev(),
            ns_ino: namespace.ino(),
            ..self
        })
    }
}

/// `struct py_thread`.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct RawPyThread {
    state: u64,
    native: u64,
}

/// `struct py_frame`: one interpreter frame as the walker read it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawPyFrame {
    /// The code object it runs, which `qualname`, `linetable`, and
    /// `first_line` prove.
    pub code: u64,
    pub qualname: u64,
    pub linetable: u64,

    /// Its instruction's byte offset from its bytecode's start, or
    /// [`NO_OFFSET`].
    pub offset: i32,
    pub first_line: i32,

    /// Its owner, as this version of CPython numbers owners.
    pub owner: u32,
    pad: u32,
}

/// `struct py_stack_record`, which its frames follow.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct RawPyStackRecord {
    header: RawRecord,
    state: u64,
    running_task: u64,
    frames: u32,
    end: u8,
    gil: u8,
    pad: u16,
}

/// `struct v8_layout`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawV8Layout {
    fp_function: i32,
    fp_marker: i32,
    fp_bytecode_array: i32,
    fp_bytecode_offset: i32,
    heap_object_tag_mask: u32,
    heap_object_tag: u32,
    smi_tag_mask: u32,
    smi_tag: u32,
    object_map: u32,
    map_instance_type: u32,
    first_function_type: u32,
    last_function_type: u32,
    shared_type: u32,
    script_type: u32,
    function_shared: u32,
    shared_script: u32,
    shared_literal_id: u32,
    script_id: u32,
    isolate_place: u32,
    isolate_pad: u32,
    isolate_offset: i64,
    isolate_slot: u64,
}

/// `enum v8_isolate_place`.
const V8_ISOLATE_UNPLACED: u32 = 0;
const V8_ISOLATE_OFFSET: u32 = 1;
const V8_ISOLATE_SLOT: u32 = 2;

impl RawV8Layout {
    /// Tells the walker where each thread of a process whose V8 module
    /// `bias` loaded keeps its current isolate, which the module's image
    /// places at `place`; or why it cannot read it there, as from a
    /// library's dynamic block of thread-local storage, which only the
    /// loader's tables find.
    pub const fn place_isolate(
        &mut self,
        place: crate::ThreadLocal,
        bias: crate::LoadBias,
    ) -> Result<(), &'static str> {
        match place {
            crate::ThreadLocal::Offset(offset) => {
                self.isolate_place = V8_ISOLATE_OFFSET;
                self.isolate_offset = offset;
            }
            crate::ThreadLocal::Slot(slot) => {
                self.isolate_place = V8_ISOLATE_SLOT;
                self.isolate_slot = bias.wrapping_load(slot).get();
            }
            crate::ThreadLocal::Dynamic(_) => {
                return Err(
                    "the module keeps the current isolate in its dynamic block of \
                            thread-local storage, which the walker does not read",
                );
            }
        }
        Ok(())
    }
}

/// `struct v8_frame`: one JavaScript frame as the walker read it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawV8Frame {
    pub fp: u64,
    pub pc: u64,
    pub function: u64,
    pub shared: u64,
    pub script: u64,
    pub script_id: u64,
    pub bytecode: u64,
    pub bytecode_offset: u64,
    pub literal_id: i32,

    /// 1 where `pc` is the frame's instruction, 0 where it is the return
    /// address of a call.
    pub at_instruction: u32,
}

/// `struct v8_stack_record`, which its frames follow.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct RawV8StackRecord {
    header: RawRecord,
    frames: u32,
    end: u8,
    isolate_read: u8,
    pad: [u8; 2],
    isolate: u64,
}

/// What a record says of its thread's current isolate: `enum v8_isolate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V8Isolate {
    /// Not read, as its process's layout does not place it.
    NotRead,

    /// The isolate's address, 0 where the thread has none.
    Read(u64),

    /// Placed, but its thread pointer or the word there was unreadable.
    Unreadable,
}

/// Why a thread's JavaScript frames end: `enum v8_end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V8End {
    /// At the end of the frame-pointer chain.
    Complete,

    /// At the most frames a record holds, or steps a walk takes.
    Bounded,

    /// At a frame that could not be read, or a chain that does not climb.
    Unreadable,

    /// Before the walk: the process's layout was not yet known.
    NoLayout,
}

/// A thread's JavaScript frames, as the walker wrote them.
#[derive(Debug, Clone, Copy)]
pub struct V8Stack<'a> {
    pub end: V8End,
    pub isolate: V8Isolate,

    /// Innermost first.
    pub frames: &'a [RawV8Frame],
}

/// Whether `bytes` is a record of JavaScript frames, as opposed to Python's.
pub fn is_v8(bytes: &[u8]) -> bool {
    RawRecord::ref_from_prefix(bytes).is_ok_and(|(record, _)| record.kind == RECORD_V8_STACK)
}

/// The record of JavaScript frames `bytes` holds.
pub fn decode_v8(bytes: &[u8]) -> Result<V8Stack<'_>, Malformed> {
    let (record, rest) = RawV8StackRecord::ref_from_prefix(bytes).map_err(|_| Malformed)?;
    let size = usize::try_from(record.header.size).map_err(|_| Malformed)?;
    if record.header.kind != RECORD_V8_STACK || size > bytes.len() {
        return Err(Malformed);
    }
    let count = usize::try_from(record.frames).map_err(|_| Malformed)?;
    let frames_bytes = count
        .checked_mul(size_of::<RawV8Frame>())
        .filter(|bytes| size_of::<RawV8StackRecord>() + bytes == size)
        .ok_or(Malformed)?;
    let frames = <[RawV8Frame]>::ref_from_bytes(rest.get(..frames_bytes).ok_or(Malformed)?)
        .map_err(|_| Malformed)?;
    let end = match record.end {
        0 => V8End::Complete,
        1 => V8End::Bounded,
        2 => V8End::Unreadable,
        3 => V8End::NoLayout,
        _ => return Err(Malformed),
    };
    let isolate = match record.isolate_read {
        0 => V8Isolate::NotRead,
        1 => V8Isolate::Read(record.isolate),
        2 => V8Isolate::Unreadable,
        _ => return Err(Malformed),
    };
    Ok(V8Stack {
        end,
        isolate,
        frames,
    })
}

/// The layout the walker reads a V8 of `layout` by, or the field or
/// constant it needs that the layout lacks.
pub fn v8_layout(layout: &V8Layout) -> Result<RawV8Layout, &'static str> {
    let constant = |constant: V8Constant| {
        layout
            .constant(constant)
            .ok_or_else(|| constant.name())
            .and_then(|value| u32::try_from(value).map_err(|_| constant.name()))
    };
    let slot = |constant: V8Constant| {
        layout
            .constant(constant)
            .ok_or_else(|| constant.name())
            .and_then(|value| i32::try_from(value).map_err(|_| constant.name()))
    };
    let field = |field: V8Field| {
        layout
            .field(field)
            .ok_or_else(|| field.name())
            .and_then(|offset| u32::try_from(offset).map_err(|_| field.name()))
    };
    Ok(RawV8Layout {
        fp_function: slot(V8Constant::OffFpFunction)?,
        fp_marker: slot(V8Constant::OffFpContextOrFrameType)?,
        fp_bytecode_array: slot(V8Constant::OffFpBytecodeArray)?,
        fp_bytecode_offset: slot(V8Constant::OffFpBytecodeOffset)?,
        heap_object_tag_mask: constant(V8Constant::HeapObjectTagMask)?,
        heap_object_tag: constant(V8Constant::HeapObjectTag)?,
        smi_tag_mask: constant(V8Constant::SmiTagMask)?,
        smi_tag: constant(V8Constant::SmiTag)?,
        object_map: field(V8Field::HeapObjectMap)?,
        map_instance_type: field(V8Field::MapInstanceType)?,
        first_function_type: constant(V8Constant::FirstJSFunctionType)?,
        last_function_type: constant(V8Constant::LastJSFunctionType)?,
        shared_type: constant(V8Constant::SharedFunctionInfoType)?,
        script_type: constant(V8Constant::ScriptType)?,
        function_shared: field(V8Field::JSFunctionSharedFunctionInfo)?,
        shared_script: field(V8Field::SharedFunctionInfoScript)?,
        shared_literal_id: field(V8Field::SharedFunctionInfoFunctionLiteralId)?,
        script_id: field(V8Field::ScriptId)?,
        isolate_place: V8_ISOLATE_UNPLACED,
        isolate_pad: 0,
        isolate_offset: 0,
        isolate_slot: 0,
    })
}

/// Map values cross as bytes, which aya can hold without unsafe code.
type Bytes<const N: usize> = [u8; N];
const LAYOUT_BYTES: usize = size_of::<RawPyLayout>();
const V8_LAYOUT_BYTES: usize = size_of::<RawV8Layout>();
const THREAD_BYTES: usize = size_of::<RawPyThread>();

/// Why a thread's frames end: `enum py_end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackEnd {
    /// At the end of its chain.
    Complete,

    /// At the most frames a record holds.
    Bounded,

    /// At a frame or field that could not be read.
    Unreadable,

    /// Before the walk: the process's layout was not yet known.
    NoLayout,

    /// Before the walk: no state was known for the thread.
    NoThread,

    /// Before the walk: the state known for the thread was another's.
    Stale,

    /// From 3.15, at no previous frame, before its base frame.
    BeforeBase,
}

/// What the thread did with its interpreter's GIL: `enum py_gil`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gil {
    Unknown,
    Held,
    Waiting,
    Released,

    /// A free-threaded build's GIL is off.
    Off,
}

/// A thread's Python frames, as the program wrote them for one sample.
#[derive(Debug, Clone, Copy)]
pub struct PyStack<'a> {
    /// From 3.14, the asyncio task the thread ran, if any.
    pub running_task: Option<u64>,
    pub end: StackEnd,
    pub gil: Gil,

    /// Innermost first.
    pub frames: &'a [RawPyFrame],
}

/// A record whose bytes contradict its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the BPF engine wrote a malformed record of Python frames")]
pub struct Malformed;

/// The record of Python frames `bytes` holds.
pub fn decode(bytes: &[u8]) -> Result<PyStack<'_>, Malformed> {
    let (record, rest) = RawPyStackRecord::ref_from_prefix(bytes).map_err(|_| Malformed)?;
    let size = usize::try_from(record.header.size).map_err(|_| Malformed)?;
    if record.header.kind != RECORD_PY_STACK || size > bytes.len() {
        return Err(Malformed);
    }

    let count = usize::try_from(record.frames).map_err(|_| Malformed)?;
    let frames_bytes = count
        .checked_mul(size_of::<RawPyFrame>())
        .filter(|bytes| size_of::<RawPyStackRecord>() + bytes == size)
        .ok_or(Malformed)?;
    let frames = <[RawPyFrame]>::ref_from_bytes(rest.get(..frames_bytes).ok_or(Malformed)?)
        .map_err(|_| Malformed)?;

    let end = match record.end {
        0 => StackEnd::Complete,
        1 => StackEnd::Bounded,
        2 => StackEnd::Unreadable,
        3 => StackEnd::NoLayout,
        4 => StackEnd::NoThread,
        5 => StackEnd::Stale,
        6 => StackEnd::BeforeBase,
        _ => return Err(Malformed),
    };

    let gil = match record.gil {
        0 => Gil::Unknown,
        1 => Gil::Held,
        2 => Gil::Waiting,
        3 => Gil::Released,
        4 => Gil::Off,
        _ => return Err(Malformed),
    };

    Ok(PyStack {
        running_task: Some(record.running_task).filter(|task| *task != 0),
        end,
        gil,
        frames,
    })
}

/// The layout the walker reads process `pid`'s CPython by, whose `layout`
/// its image gives and whose runtime state is at `runtime`, or why it
/// cannot.
///
/// # Errors
///
/// When the image gives no layout, the version lacks a field the walker
/// needs, the runtime lies outside the process, or the process's pid
/// namespace cannot be read.
pub fn process_layout(
    layout: Result<&PythonLayout, &str>,
    runtime: Option<crate::VirtualAddress>,
    pid: u32,
) -> Result<RawPyLayout, String> {
    let layout =
        self::layout(layout?).map_err(|field| format!("CPython's layout has no {field}"))?;
    let runtime = runtime.ok_or("CPython's runtime state lies outside the process")?;
    layout.of_process(pid, runtime.get())
}

/// The layout the walker reads a CPython of `layout` by, or the field it
/// needs that the version lacks.
pub fn layout(layout: &PythonLayout) -> Result<RawPyLayout, &'static str> {
    let optional = |field: Field| {
        layout
            .field(field)
            .and_then(|offset| u32::try_from(offset).ok())
            .unwrap_or(ABSENT)
    };

    let needed = |field: Field| match optional(field) {
        ABSENT => Err(field.name()),
        offset => Ok(offset),
    };

    // The GIL's `locked` and `last_holder` lie where the interpreter's own
    // copy of the GIL holds them, less where that copy begins.
    let within_gil = |field: Field| {
        let (Some(offset), Some(gil)) = (layout.field(field), layout.field(Field::InterpreterGil))
        else {
            return ABSENT;
        };

        offset
            .checked_sub(gil)
            .and_then(|offset| u32::try_from(offset).ok())
            .unwrap_or(ABSENT)
    };

    Ok(RawPyLayout {
        thread_native_id: needed(Field::ThreadNativeId)?,
        thread_current_frame: needed(Field::ThreadCurrentFrame)?,
        cframe_current_frame: optional(Field::CframeCurrentFrame),
        thread_base_frame: optional(Field::ThreadBaseFrame),
        thread_running_task: optional(Field::ThreadAsyncioRunningTask),
        thread_holds_gil: optional(Field::ThreadHoldsGil),
        thread_gil_requested: optional(Field::ThreadGilRequested),
        thread_interp: optional(Field::ThreadInterp),
        interp_ceval_gil: optional(Field::InterpreterCevalGil),
        gil_locked: within_gil(Field::InterpreterGilLocked),
        gil_holder: within_gil(Field::InterpreterGilHolder),
        gil_enabled: optional(Field::GilEnabled),
        frame_executable: needed(Field::FrameExecutable)?,
        frame_previous: needed(Field::FramePrevious)?,
        frame_instruction: needed(Field::FrameInstruction)?,
        frame_owner: needed(Field::FrameOwner)?,
        frame_tlbc_index: optional(Field::FrameTlbcIndex),
        code_qualname: needed(Field::CodeQualname)?,
        code_firstlineno: needed(Field::CodeFirstlineno)?,
        code_linetable: needed(Field::CodeLinetable)?,
        code_code_adaptive: needed(Field::CodeCodeAdaptive)?,
        code_tlbc: optional(Field::CodeTlbc),
        code_array_size: optional(Field::CodeArraySize),
        code_array_entries: optional(Field::CodeArrayEntries),
        stack_refs: u32::from(layout.constant(Constant::StackRefs)),
        owner_entry: u32::from(layout.constant(Constant::OwnerEntry)),
        runtime_interpreters_head: optional(Field::RuntimeInterpretersHead),
        interp_next: optional(Field::InterpreterNext),
        interp_threads_head: optional(Field::InterpreterThreadsHead),
        thread_next: optional(Field::ThreadNext),
        runtime: 0,
        ns_dev: 0,
        ns_ino: 0,
    })
}

/// Why the engine cannot run, or failed.
#[derive(Debug, thiserror::Error)]
pub enum SamplerError {
    #[error("{0}")]
    Unavailable(String),

    #[error("loading the BPF program for Python stacks failed: {0}")]
    Load(String),

    #[error("{0}")]
    Events(#[from] perf::OpenError),

    #[error("attaching the BPF program for Python stacks to the perf events failed: {0}")]
    Attach(#[source] std::io::Error),
}

fn load_error(error: impl std::fmt::Display) -> SamplerError {
    SamplerError::Load(error.to_string())
}

/// What to do if the engine lacks a privilege.
const REMEDY: &str =
    "  The BPF engine reads each thread's Python frames in the kernel as each sample is
  taken, which needs both. To continue, one of:
    - run microscope from a unit with AmbientCapabilities=CAP_BPF CAP_PERFMON
    - on NixOS or system-manager, import microscope's bpf module and run through
      /run/wrappers/bin/microscope-bpf (docs/syscalls.md#give-microscope-the-bpf-privileges)
    - record with --python snapshot or --python live, which need no privilege";

/// The kernel's initial PID namespace inode (linux/nsfs.h). BPF's
/// `bpf_get_current_pid_tgid` uses it, while perf uses the event creator's
/// namespace. An nsfs descriptor proves which ID space the recorder uses.
const INITIAL_PID_NAMESPACE: u64 = 0xefff_fffc;

fn initial_pid_namespace(namespace: &std::fs::File) -> std::io::Result<bool> {
    let filesystem = nix::sys::statfs::fstatfs(namespace).map_err(std::io::Error::from)?;
    Ok(filesystem.filesystem_type() == nix::sys::statfs::NSFS_MAGIC
        && namespace.metadata()?.ino() == INITIAL_PID_NAMESPACE)
}

/// The sampler's kernel keys must agree with perf's sample IDs. Targets
/// may occupy nested namespaces; the recorder currently must use the initial one.
///
/// # Errors
///
/// Why the recorder's namespace cannot be proved to match the kernel keys.
pub fn check_namespace() -> Result<(), SamplerError> {
    let observed = std::fs::File::open("/proc/self/ns/pid")
        .and_then(|namespace| initial_pid_namespace(&namespace));
    match observed {
        Ok(true) => Ok(()),
        Ok(false) => Err(SamplerError::Unavailable("Python BPF sampling requires the recorder in the initial PID namespace; run it on the host, or use --python snapshot or --python live".into())),
        Err(error) => Err(SamplerError::Unavailable(format!("cannot prove the Python BPF recorder uses the initial PID namespace: {error}; use --python snapshot or --python live"))),
    }
}

/// A program's `LAYOUTS`: the layout the walker reads each Python
/// process by, by its process id as the kernel's initial pid namespace
/// numbers it.
pub struct PythonLayouts(HashMap<MapData, u32, Bytes<LAYOUT_BYTES>>);

impl PythonLayouts {
    /// The map `LAYOUTS` of a program, which it gives up.
    ///
    /// # Errors
    ///
    /// When the program has no such map, or it is another kind.
    pub fn take(ebpf: &mut Ebpf) -> Result<Self, SamplerError> {
        let map = ebpf
            .take_map("LAYOUTS")
            .ok_or_else(|| load_error("the program has no LAYOUTS map"))?;
        Ok(Self(HashMap::try_from(map).map_err(load_error)?))
    }

    /// Sets the layout the walker reads process `tgid`'s frames by.
    ///
    /// # Errors
    ///
    /// When the kernel refuses the update, as when the map is full.
    pub fn set(&mut self, tgid: u32, layout: &RawPyLayout) -> Result<(), SamplerError> {
        let mut value = [0; LAYOUT_BYTES];
        value.copy_from_slice(layout.as_bytes());
        self.0.insert(tgid, value, 0).map_err(load_error)
    }

    /// Forgets process `tgid`'s layout, as after it exited.
    pub fn forget(&mut self, tgid: u32) {
        // A process that is gone may be gone from the map already.
        let _ = self.0.remove(&tgid);
    }
}

/// A program's `V8_LAYOUTS`: the layout the walker reads each Node
/// process by, by its process id as the kernel's initial pid namespace
/// numbers it.
pub struct V8Layouts(HashMap<MapData, u32, Bytes<V8_LAYOUT_BYTES>>);

impl V8Layouts {
    /// The map `V8_LAYOUTS` of a program, which it gives up.
    ///
    /// # Errors
    ///
    /// When the program has no such map, or it is another kind.
    pub fn take(ebpf: &mut Ebpf) -> Result<Self, SamplerError> {
        let map = ebpf
            .take_map("V8_LAYOUTS")
            .ok_or_else(|| load_error("the program has no V8_LAYOUTS map"))?;
        Ok(Self(HashMap::try_from(map).map_err(load_error)?))
    }

    /// Sets the layout the walker reads process `tgid`'s frames by.
    ///
    /// # Errors
    ///
    /// When the kernel refuses the update, as when the map is full.
    pub fn set(&mut self, tgid: u32, layout: &RawV8Layout) -> Result<(), SamplerError> {
        let mut value = [0; V8_LAYOUT_BYTES];
        value.copy_from_slice(layout.as_bytes());
        self.0.insert(tgid, value, 0).map_err(load_error)
    }

    /// Forgets process `tgid`'s layout, as after it exited.
    pub fn forget(&mut self, tgid: u32) {
        // A process that is gone may be gone from the map already.
        let _ = self.0.remove(&tgid);
    }
}

/// The loaded program, attached to a recording's events, and its maps.
pub struct Sampler {
    /// Holds the program, which the events run, and the maps.
    _ebpf: Ebpf,
    layouts: PythonLayouts,
    v8_layouts: V8Layouts,
    threads: HashMap<MapData, u32, Bytes<THREAD_BYTES>>,
    lost: PerCpuArray<MapData, u64>,

    /// Each CPU's walks of JavaScript frames, their nanoseconds, their
    /// frames, the most nanoseconds one took, and the walks that filled
    /// their record and their nanoseconds: `struct v8_walk_times`.
    #[cfg(feature = "bpf-walk-timing")]
    walk_times: PerCpuArray<MapData, [u64; 6]>,

    /// Each CPU's output event, which `OUTPUT` holds as well.
    _outputs: Vec<perf::Output>,

    /// The threads put in `threads`, by their process.
    known: BTreeMap<u32, BTreeSet<u32>>,
}

impl std::fmt::Debug for Sampler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sampler")
            .field("known", &self.known)
            .finish_non_exhaustive()
    }
}

impl Sampler {
    /// Loads the program and attaches it to `events`, which must not have
    /// begun to sample, with an output event on each CPU; and the output
    /// events' identifiers, which the records they write begin with.
    ///
    /// # Errors
    ///
    /// When this process lacks the privileges, or the kernel refuses the
    /// program or the events.
    pub fn load(events: &perf::Events) -> Result<(Self, Vec<u64>), SamplerError> {
        check_namespace()?;
        let probe = super::super::syscalls::bpf::Probe::read();
        if let Some(missing) = probe.lacks_privileges() {
            return Err(SamplerError::Unavailable(format!(
                "the BPF engine for Python stacks needs CAP_BPF and CAP_PERFMON, and this \
                 process {missing}.\n{REMEDY}"
            )));
        }

        let cpus = aya::util::nr_cpus().map_err(|(_, error)| load_error(error))?;
        let mut ebpf = EbpfLoader::new()
            .map_max_entries("OUTPUT", u32::try_from(cpus).unwrap_or(u32::MAX))
            .load(OBJECT)
            .map_err(load_error)?;
        let outputs = events.outputs()?;

        {
            let Some(Map::PerfEventArray(array)) = ebpf.map("OUTPUT") else {
                return Err(load_error("the program has no OUTPUT array"));
            };

            for output in &outputs {
                perf::sys::set_output_slot(array.fd().as_fd(), output.cpu, &output.fd)
                    .map_err(SamplerError::Attach)?;
            }
        }

        let program: &mut PerfEvent = ebpf
            .program_mut("python_sample")
            .ok_or_else(|| load_error("the object has no python_sample program"))?
            .try_into()
            .map_err(load_error)?;
        program.load().map_err(load_error)?;
        events
            .run_program(program.fd().map_err(load_error)?.as_fd())
            .map_err(SamplerError::Attach)?;

        let mut take = |name: &str| {
            ebpf.take_map(name)
                .ok_or_else(|| load_error(format!("the program has no {name} map")))
        };

        let layouts = PythonLayouts(HashMap::try_from(take("LAYOUTS")?).map_err(load_error)?);
        let v8_layouts = V8Layouts(HashMap::try_from(take("V8_LAYOUTS")?).map_err(load_error)?);
        let threads = HashMap::try_from(take("THREADS")?).map_err(load_error)?;
        let lost = PerCpuArray::try_from(take("LOST")?).map_err(load_error)?;
        #[cfg(feature = "bpf-walk-timing")]
        let walk_times = PerCpuArray::try_from(take("V8_WALK_TIMES")?).map_err(load_error)?;
        let ids = outputs.iter().map(|output| output.id).collect();
        record!("python sampler: attached, {} outputs", outputs.len());

        Ok((
            Self {
                _ebpf: ebpf,
                layouts,
                v8_layouts,
                threads,
                lost,
                #[cfg(feature = "bpf-walk-timing")]
                walk_times,
                _outputs: outputs,
                known: BTreeMap::new(),
            },
            ids,
        ))
    }

    /// Sets the layout the walker reads process `tgid`'s frames by.
    ///
    /// # Errors
    ///
    /// When the kernel refuses the update, as when the map is full.
    pub fn set_layout(&mut self, tgid: u32, layout: &RawPyLayout) -> Result<(), SamplerError> {
        self.layouts.set(tgid, layout)
    }

    /// Sets the layout the walker reads process `tgid`'s JavaScript frames
    /// by.
    ///
    /// # Errors
    ///
    /// When the kernel refuses the update, as when the map is full.
    pub fn set_v8_layout(&mut self, tgid: u32, layout: &RawV8Layout) -> Result<(), SamplerError> {
        self.v8_layouts.set(tgid, layout)
    }

    /// Sets the states of process `tgid`'s threads to `threads`, each
    /// thread's id as this process sees it, its state's address, and the
    /// id its state names it by, forgetting those of its threads not among
    /// them.
    ///
    /// # Errors
    ///
    /// When the kernel refuses an update, as when the map is full.
    pub fn set_threads(
        &mut self,
        tgid: u32,
        threads: &[(u32, u64, u64)],
    ) -> Result<(), SamplerError> {
        let now: BTreeSet<u32> = threads.iter().map(|&(tid, _, _)| tid).collect();
        let before = self.known.insert(tgid, now.clone()).unwrap_or_default();
        for gone in before.difference(&now) {
            // A thread that is gone may be gone from the map already.
            let _ = self.threads.remove(gone);
        }

        for &(tid, state, native) in threads {
            let mut value = [0; THREAD_BYTES];
            value.copy_from_slice(RawPyThread { state, native }.as_bytes());
            self.threads.insert(tid, value, 0).map_err(load_error)?;
        }

        Ok(())
    }

    /// Forgets process `tgid`, which exited.
    pub fn forget(&mut self, tgid: u32) {
        for tid in self.known.remove(&tgid).unwrap_or_default() {
            let _ = self.threads.remove(&tid);
        }

        self.layouts.forget(tgid);
        self.v8_layouts.forget(tgid);
    }

    /// What every CPU's walks of JavaScript frames took: the walks, their
    /// nanoseconds, their frames, the most nanoseconds one took, and the
    /// walks that filled their record and their nanoseconds.
    ///
    /// # Errors
    ///
    /// When the kernel refuses the read.
    #[cfg(feature = "bpf-walk-timing")]
    pub fn walk_times(&self) -> Result<[u64; 6], SamplerError> {
        let cpus = self.walk_times.get(&0, 0).map_err(load_error)?;
        Ok(cpus
            .iter()
            .fold([0; 6], |[walks, ns, frames, most, full, full_ns], cpu| {
                [
                    walks.saturating_add(cpu[0]),
                    ns.saturating_add(cpu[1]),
                    frames.saturating_add(cpu[2]),
                    most.max(cpu[3]),
                    full.saturating_add(cpu[4]),
                    full_ns.saturating_add(cpu[5]),
                ]
            }))
    }

    /// How many records found no room in their rings.
    pub fn lost(&self) -> Result<u64, SamplerError> {
        self.lost
            .get(&0, 0)
            .map(|counts| counts.iter().copied().sum())
            .map_err(load_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_key_namespace_proof_rejects_other_namespace_types_and_ordinary_files() {
        let pid = std::fs::File::open("/proc/self/ns/pid").unwrap();
        let initial = std::fs::read_link("/proc/self/ns/pid").unwrap()
            == std::path::Path::new("pid:[4026531836]");
        assert_eq!(initial_pid_namespace(&pid).unwrap(), initial);
        let user = std::fs::File::open("/proc/self/ns/user").unwrap();
        assert!(!initial_pid_namespace(&user).unwrap());
        let ordinary =
            std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        assert!(!initial_pid_namespace(&ordinary).unwrap());
    }

    #[allow(clippy::too_many_lines, reason = "one row per record")]
    #[test]
    fn records_h_and_the_sampler_agree_field_for_field() {
        crate::backend::linux::bpf::assert_mirrors!(
            OBJECT,
            (RawRecord, "record", [kind, size, tid, tgid]),
            (
                RawPyLayout,
                "py_layout",
                [
                    thread_native_id,
                    thread_current_frame,
                    cframe_current_frame,
                    thread_base_frame,
                    thread_running_task,
                    thread_holds_gil,
                    thread_gil_requested,
                    thread_interp,
                    interp_ceval_gil,
                    gil_locked,
                    gil_holder,
                    gil_enabled,
                    frame_executable,
                    frame_previous,
                    frame_instruction,
                    frame_owner,
                    frame_tlbc_index,
                    code_qualname,
                    code_firstlineno,
                    code_linetable,
                    code_code_adaptive,
                    code_tlbc,
                    code_array_size,
                    code_array_entries,
                    stack_refs,
                    owner_entry,
                    runtime_interpreters_head,
                    interp_next,
                    interp_threads_head,
                    thread_next,
                    runtime,
                    ns_dev,
                    ns_ino,
                ]
            ),
            (RawPyThread, "py_thread", [state, native]),
            (
                RawPyFrame,
                "py_frame",
                [code, qualname, linetable, offset, first_line, owner, pad]
            ),
            (
                RawPyStackRecord,
                "py_stack_record",
                [header, state, running_task, frames, end, gil, pad]
            ),
            (
                RawV8Layout,
                "v8_layout",
                [
                    fp_function,
                    fp_marker,
                    fp_bytecode_array,
                    fp_bytecode_offset,
                    heap_object_tag_mask,
                    heap_object_tag,
                    smi_tag_mask,
                    smi_tag,
                    object_map,
                    map_instance_type,
                    first_function_type,
                    last_function_type,
                    shared_type,
                    script_type,
                    function_shared,
                    shared_script,
                    shared_literal_id,
                    script_id,
                    isolate_place,
                    isolate_pad,
                    isolate_offset,
                    isolate_slot,
                ]
            ),
            (
                RawV8Frame,
                "v8_frame",
                [
                    fp,
                    pc,
                    function,
                    shared,
                    script,
                    script_id,
                    bytecode,
                    bytecode_offset,
                    literal_id,
                    at_instruction
                ]
            ),
            (
                RawV8StackRecord,
                "v8_stack_record",
                [header, frames, end, isolate_read, pad, isolate]
            ),
        );
    }

    fn v8_record(isolate_read: u8, isolate: u64) -> Vec<u8> {
        let header = RawV8StackRecord {
            header: RawRecord {
                kind: RECORD_V8_STACK,
                size: u32::try_from(size_of::<RawV8StackRecord>()).expect("small"),
                tid: 12,
                tgid: 10,
            },
            frames: 0,
            end: 0,
            isolate_read,
            pad: [0; 2],
            isolate,
        };
        header.as_bytes().to_vec()
    }

    /// A record of JavaScript frames says whether it read its thread's
    /// isolate, and the isolate it read; any other state is malformed.
    #[test]
    fn a_v8_record_says_what_it_read_of_its_threads_isolate() {
        let isolate =
            |read, address| decode_v8(&v8_record(read, address)).map(|stack| stack.isolate);
        assert_eq!(isolate(0, 0), Ok(V8Isolate::NotRead));
        assert_eq!(isolate(1, 0x1234_0000), Ok(V8Isolate::Read(0x1234_0000)));
        assert_eq!(isolate(1, 0), Ok(V8Isolate::Read(0)));
        assert_eq!(isolate(2, 0), Ok(V8Isolate::Unreadable));
        assert_eq!(isolate(3, 0), Err(Malformed));
    }

    /// The walker reads an isolate at a fixed offset from the thread
    /// pointer, or at the offset a slot of the loaded image holds, and
    /// refuses one in a library's dynamic block, which it does not find.
    #[test]
    fn the_isolate_is_placed_by_its_offset_or_its_slot() {
        use zerocopy::FromZeros as _;
        let unplaced = RawV8Layout::new_zeroed();
        let mut layout = unplaced;
        assert_eq!(
            layout.place_isolate(
                crate::ThreadLocal::Offset(-0x40),
                crate::LoadBias::new(0x1000)
            ),
            Ok(())
        );
        assert_eq!(
            (layout.isolate_place, layout.isolate_offset),
            (V8_ISOLATE_OFFSET, -0x40)
        );
        let mut layout = unplaced;
        assert_eq!(
            layout.place_isolate(
                crate::ThreadLocal::Slot(crate::ImageAddress::new(0x2000)),
                crate::LoadBias::new(0x5000_0000)
            ),
            Ok(())
        );
        assert_eq!(
            (layout.isolate_place, layout.isolate_slot),
            (V8_ISOLATE_SLOT, 0x5000_2000)
        );
        let mut layout = unplaced;
        assert!(
            layout
                .place_isolate(crate::ThreadLocal::Dynamic(0x170), crate::LoadBias::new(0))
                .is_err()
        );
        assert_eq!(layout, unplaced);
    }

    fn record(frames: &[RawPyFrame], end: u8, gil: u8) -> Vec<u8> {
        let size = size_of::<RawPyStackRecord>() + size_of_val(frames);
        let header = RawPyStackRecord {
            header: RawRecord {
                kind: RECORD_PY_STACK,
                size: u32::try_from(size).expect("small"),
                tid: 12,
                tgid: 10,
            },
            state: 0x5000,
            running_task: 0,
            frames: u32::try_from(frames.len()).expect("few"),
            end,
            gil,
            pad: 0,
        };

        let mut bytes = header.as_bytes().to_vec();
        bytes.extend_from_slice(frames.as_bytes());

        // The kernel pads a sample's raw bytes.
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }

    /// A record decodes with its frames, innermost first; one whose sizes
    /// disagree, or whose numbers name nothing, is malformed.
    #[test]
    fn records_decode_with_their_frames_and_reject_what_disagrees() {
        let frame = |code| RawPyFrame {
            code,
            qualname: code + 8,
            linetable: code + 16,
            offset: 6,
            first_line: 3,
            owner: 0,
            pad: 0,
        };

        let frames = [frame(0x100), frame(0x200)];
        let bytes = record(&frames, 0, 1);
        let stack = decode(&bytes).expect("a record");
        assert_eq!((stack.end, stack.gil), (StackEnd::Complete, Gil::Held));
        assert_eq!(stack.running_task, None);
        assert_eq!(stack.frames, &frames);

        let empty = record(&[], 4, 0);
        let stack = decode(&empty).expect("a record");
        assert_eq!((stack.end, stack.frames.len()), (StackEnd::NoThread, 0));

        assert_eq!(decode(&record(&frames, 7, 0)).err(), Some(Malformed));
        assert_eq!(decode(&record(&frames, 0, 5)).err(), Some(Malformed));
        assert_eq!(decode(&bytes[..bytes.len() - 40]).err(), Some(Malformed));
        let mut lying = bytes.clone();
        lying[32..36].copy_from_slice(&3_u32.to_le_bytes());
        assert_eq!(decode(&lying).err(), Some(Malformed));
        let mut other = bytes;
        other[0..4].copy_from_slice(&8_u32.to_le_bytes());
        assert_eq!(decode(&other).err(), Some(Malformed));
    }

    /// Each version's layout gives the walker what it needs: 3.12 reaches
    /// its frames through its `_PyCFrame`, 3.14t reads each frame's copy of
    /// its bytecode, and 3.15 ends each chain at its base frame and says
    /// who waits for the GIL.
    #[test]
    fn each_versions_layout_gives_the_walker_its_fields() {
        let of = |name: &str| {
            let layout =
                crate::debug_info::generated_python_layout(name).expect("a generated layout");
            super::layout(&layout).expect("every field the walker needs")
        };

        let old = of("3.12");
        assert_ne!(old.cframe_current_frame, ABSENT);
        assert_eq!(old.thread_base_frame, ABSENT);
        assert_eq!(old.stack_refs, 0);
        assert_eq!(old.thread_holds_gil, ABSENT);
        assert_ne!(old.gil_locked, ABSENT);
        assert_ne!(old.gil_holder, ABSENT);

        let free = of("3.14t");
        assert_ne!(free.frame_tlbc_index, ABSENT);
        assert_ne!(free.code_tlbc, ABSENT);
        assert_ne!(free.gil_enabled, ABSENT);
        assert_eq!(free.stack_refs, 1);
        assert_ne!(free.thread_running_task, ABSENT);

        let new = of("3.15");
        assert_eq!(new.cframe_current_frame, ABSENT);
        assert_ne!(new.thread_base_frame, ABSENT);
        assert_ne!(new.thread_holds_gil, ABSENT);
        assert_ne!(new.thread_gil_requested, ABSENT);
        assert_eq!(new.frame_tlbc_index, ABSENT);
    }
}
