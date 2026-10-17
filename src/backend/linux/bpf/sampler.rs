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

/// The program, as `build.rs` compiled it.
static OBJECT: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/sampler.bpf.o"));

/// `RECORD_PY_STACK`.
const RECORD_PY_STACK: u32 = 9;

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
    code_code_adaptive: u32,
    code_tlbc: u32,
    code_array_size: u32,
    code_array_entries: u32,
    stack_refs: u32,
    owner_entry: u32,
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
    /// The code object it runs, which `qualname` and `first_line` prove.
    pub code: u64,
    pub qualname: u64,

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

/// Map values cross as bytes, which aya can hold without unsafe code.
type Bytes<const N: usize> = [u8; N];
const LAYOUT_BYTES: usize = size_of::<RawPyLayout>();
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
        code_code_adaptive: needed(Field::CodeCodeAdaptive)?,
        code_tlbc: optional(Field::CodeTlbc),
        code_array_size: optional(Field::CodeArraySize),
        code_array_entries: optional(Field::CodeArrayEntries),
        stack_refs: u32::from(layout.constant(Constant::StackRefs)),
        owner_entry: u32::from(layout.constant(Constant::OwnerEntry)),
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

/// The loaded program, attached to a recording's events, and its maps.
pub struct Sampler {
    /// Holds the program, which the events run, and the maps.
    _ebpf: Ebpf,
    layouts: HashMap<MapData, u32, Bytes<LAYOUT_BYTES>>,
    threads: HashMap<MapData, u32, Bytes<THREAD_BYTES>>,
    lost: PerCpuArray<MapData, u64>,

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

        let layouts = HashMap::try_from(take("LAYOUTS")?).map_err(load_error)?;
        let threads = HashMap::try_from(take("THREADS")?).map_err(load_error)?;
        let lost = PerCpuArray::try_from(take("LOST")?).map_err(load_error)?;
        let ids = outputs.iter().map(|output| output.id).collect();
        record!("python sampler: attached, {} outputs", outputs.len());
        Ok((
            Self {
                _ebpf: ebpf,
                layouts,
                threads,
                lost,
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
        let mut value = [0; LAYOUT_BYTES];
        value.copy_from_slice(layout.as_bytes());
        self.layouts.insert(tgid, value, 0).map_err(load_error)
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

        let _ = self.layouts.remove(&tgid);
    }

    /// How many records found no room in their rings.
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.lost
            .get(&0, 0)
            .map_or(0, |counts| counts.iter().copied().sum())
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
                    code_code_adaptive,
                    code_tlbc,
                    code_array_size,
                    code_array_entries,
                    stack_refs,
                    owner_entry,
                ]
            ),
            (RawPyThread, "py_thread", [state, native]),
            (
                RawPyFrame,
                "py_frame",
                [code, qualname, offset, first_line, owner, pad]
            ),
            (
                RawPyStackRecord,
                "py_stack_record",
                [header, state, running_task, frames, end, gil, pad]
            ),
        );
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
