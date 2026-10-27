//! Task snapshots: where an async program's tasks wait.
//!
//! A task that waits runs no code, so no CPU sample sees it. A snapshot
//! stops the program through the debugger's attach, lists every task of
//! its runtimes, reads each waiting task's stack, and detaches, leaving
//! the program to go on as it was. Each wait then stands for the time
//! since the snapshot before.
//!
//! A live snapshot reads the same from the program's memory while it
//! runs, through its runtimes' models alone, never stopping it. It reads
//! the runtimes of every module the recording has seen the process map,
//! through the debug information the recording loaded for them. A task may
//! change while it is read, so each is read twice: its context, and every
//! page of its stack or every future of its chain of awaits, must read the
//! same both times, or the task is left out and counted. A live snapshot
//! is then consistent for each task, not across them.
//!
//! A runtime found through a thread's thread-local storage, as tokio is,
//! needs the thread's thread pointer. A thread keeps it for life, so a
//! live snapshot learns each new thread's once, from the C library's own
//! lists of its threads in memory. A thread it cannot learn leaves its
//! tasks out, with the reason: a live snapshot never stops the program to
//! learn it.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use super::RecordError;
use crate::capture::records::{Truncation, role_number};
use crate::debug_info::DebugInfo;
use crate::runtime_model::{self, RuntimeStop};
use crate::{
    Debugger, DebuggerHandle, ExecutionContext, FrameKind, ModuleId, ModuleImage, ProcessId,
    Program, StackFrameId, StopContext, StopId, TaskSnapshot, TaskState, TypeInfo, TypeReference,
    UnwindTermination,
};

/// The most tasks one snapshot reads.
const MOST_TASKS: usize = 10_000;

/// What names an interpreted frame whose code could not be read.
const UNREADABLE_FRAME: &str = "[an interpreted frame whose code could not be read]";

/// Where one waiting task waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// What its runtime calls a task, and its number.
    pub noun: &'static str,
    pub number: u64,

    /// The runtime's words for what it waits for, or its state.
    pub state: String,

    /// Its labels, as `key=value` lines.
    pub labels: Option<String>,

    /// Its stack, innermost first.
    pub frames: Vec<WaitFrame>,
}

/// One frame of a waiting task's stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitFrame {
    /// Code, by the address that names its line as a sample's would: a
    /// caller's is one byte before its return address.
    Code { address: u64, role: u8 },

    /// The future a suspended task awaits, by what it waits for.
    Awaited(String),

    /// A frame an interpreter runs, whose code is in no module.
    Interpreted(InterpretedWait),

    /// Where its stack ends early.
    Truncated(Truncation),
}

/// Where an interpreted frame is, as its runtime recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretedWait {
    pub function: Arc<str>,
    pub file: Option<Arc<str>>,
    pub line: u32,

    /// The line its function's code begins at.
    pub first_line: Option<u32>,
}

impl InterpretedWait {
    /// A frame its runtime recorded as `recorded`, or one it could not
    /// read.
    pub(super) fn of(recorded: Option<&crate::RecordedFrame>) -> Self {
        recorded.map_or_else(Self::unreadable, |recorded| Self {
            function: Arc::clone(&recorded.function),
            file: recorded.place.as_ref().map(|place| Arc::clone(&place.path)),
            line: recorded.place.as_ref().map_or(0, |place| place.line),
            first_line: recorded.first_line,
        })
    }

    /// An interpreted frame a runtime model read.
    pub fn of_frame(frame: &runtime_model::InterpretedFrame) -> Self {
        frame
            .function
            .as_ref()
            .map_or_else(Self::unreadable, |function| Self {
                function: Arc::clone(function),
                file: frame.place.as_ref().map(|place| Arc::clone(&place.path)),
                line: frame.place.as_ref().map_or(0, |place| place.line),
                first_line: frame.first_line,
            })
    }

    fn unreadable() -> Self {
        Self {
            function: UNREADABLE_FRAME.into(),
            file: None,
            line: 0,
            first_line: None,
        }
    }
}

/// Where a task's runtime keeps it, as a snapshot found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub object: u64,
    pub noun: &'static str,
    pub number: u64,
    pub labels: Option<String>,
}

/// What a snapshot reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reads {
    /// Where each task of the program's runtimes waits.
    pub tasks: bool,

    /// Where each thread is, as a task of its own named [`THREAD`], with
    /// the labels the program's runtimes give it.
    pub threads: bool,
}

/// A thread's labels, each a key and a value.
pub type Labels = Vec<(Arc<str>, Arc<str>)>;

/// What a snapshot calls a thread it reads as a task.
pub const THREAD: &str = "thread";

/// What a thread a snapshot read was doing, in a runtime's words.
const SAMPLED: &str = "sampled";

/// Why a live snapshot does not read a thread that is not asleep: it runs,
/// or is about to, so its frames may change while they are read.
const NOT_ASLEEP: &str = "it was not asleep in a system call";

/// What begins the problem that names the threads a live read left out, as
/// `threads 12, 13: REASON; threads 14: REASON`.
const UNREAD_LIVE: &str = "frames not read live: ";

/// One snapshot's tasks, where each waits, and why it may miss some.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Every task the runtime keeps somewhere, running or not.
    pub located: Vec<Located>,
    pub waiting: Vec<Waiting>,

    /// Each task it could not read, or part of the runtime's tasks, and
    /// why.
    pub problems: Vec<String>,

    /// How many waiting tasks, or threads, a live snapshot left out
    /// because they changed while it read them.
    pub changed: u64,

    /// The published-frame threads among `changed`, by native id.
    pub changed_threads: Vec<u64>,

    /// The labels the program's runtimes gave each thread, by its id, as
    /// they were when read.
    pub thread_labels: Vec<(u32, Labels)>,

    /// The code a live read found a frame at in no piece the recording
    /// mapped, which a lost mapping record leaves unmapped: the recording
    /// maps it for the reads after.
    pub unmapped_code: Vec<u64>,
}

/// Takes snapshots of one process.
pub struct Snapshotter {
    pid: u32,
    program: Program,

    /// Each thread's thread pointer, by its id and when it started, as
    /// live snapshots learned them.
    thread_pointers: BTreeMap<(u32, u64), u64>,

    /// The runtimes each module's image carries, by the image, which live
    /// snapshots find once.
    runtimes: BTreeMap<crate::ModuleImageId, Vec<Detected>>,

    /// What live snapshots read again to find a module's thread-local
    /// storage for a thread.
    tls: crate::backend::RunningTls,

    /// What the last live read of each thread found of a runtime's frames,
    /// by the module's image, the thread's id, and when it started.
    kept: RefCell<BTreeMap<(crate::ModuleImageId, u32, u64), Kept>>,

    /// The `/proc` files live snapshots read of each thread, by its id,
    /// kept open between snapshots ([`keep_files`]).
    files: ThreadsFiles,
}

/// Each thread's kept `/proc` files, by its id.
type ThreadsFiles = BTreeMap<u32, crate::backend::ThreadFiles>;

/// The most threads of a process whose `/proc` files live snapshots keep
/// open between reads, three descriptors each, so that a program of
/// thousands of threads does not take thousands more descriptors beside
/// the one per thread and CPU that sampling it holds. A thread beyond
/// them has its files opened for its read and closed after it.
const MOST_KEPT_THREADS: usize = 256;

/// Why a thread is not read: it exited, or another thread took its id.
const EXITED: &str = "it exited";

/// Keeps `files` to those of the first `most` of `threads`, the process
/// `pid`'s threads in the order of their ids, opening those of a thread
/// not yet kept. Files of a thread that is gone are dropped, and opened
/// again if its id is listed: a later thread may have it, with a start
/// time of its own. Files that cannot be opened are not kept.
fn keep_files(files: &mut ThreadsFiles, pid: u32, threads: &[u32], most: usize) {
    let kept = threads.get(..most).unwrap_or(threads);
    files.retain(|tid, files| kept.binary_search(tid).is_ok() && files.live());
    for &tid in kept {
        if !files.contains_key(&tid)
            && let Ok(opened) = crate::backend::ThreadFiles::open(pid, tid)
        {
            files.insert(tid, opened);
        }
    }
}

/// Opens the files of thread `tid` of process `pid` for one read, if they
/// are still those of the thread that started at `start`.
fn open_unkept(
    pid: u32,
    tid: u32,
    start: Option<u64>,
) -> Result<crate::backend::ThreadFiles, Arc<str>> {
    let opened = crate::backend::ThreadFiles::open(pid, tid)
        .map_err(|error| Arc::from(error.to_string()))?;
    if start != Some(opened.start()) {
        return Err(EXITED.into());
    }
    Ok(opened)
}

/// The frames a live read proven quiet found of a thread, with the proof
/// that the thread has not run since: its context-switch counts after the
/// read and the system call it slept in. A thread that runs gives up its
/// CPU to sleep again, which counts, so while both are as they were, its
/// stack and the objects its frames name are as they were too: only the
/// thread itself, or its runtime's collector, which runs only while the
/// thread does, changes them. V8 collects an isolate's heap on its own
/// thread. The one exception is the heap isolates share under V8's flags
/// for shared structs and strings, which another isolate's thread may
/// collect; Node leaves those flags off, and nothing here checks them.
///
/// The counts are read after the record. A thread asleep when its record
/// is read left its CPU after any run it made since the kept read, and
/// counted it, so counts read after the record that are still those kept
/// prove it has not run since. Read before the record, they would miss a
/// run between the two reads that ended asleep in the same call.
#[derive(Debug)]
struct Kept {
    switches: (u64, u64),
    asleep: crate::backend::Asleep,
    frames: Vec<runtime_model::InterpretedFrame>,
}

/// What a live read proves a thread quiet by, each read anew: its system
/// call record and its context-switch counts.
trait ThreadState {
    fn asleep(&self) -> std::io::Result<Option<crate::backend::Asleep>>;
    fn switches(&self) -> std::io::Result<(u64, u64)>;
}

impl ThreadState for crate::backend::ThreadFiles {
    fn asleep(&self) -> std::io::Result<Option<crate::backend::Asleep>> {
        Self::asleep(self)
    }

    fn switches(&self) -> std::io::Result<(u64, u64)> {
        Self::switches(self)
    }
}

/// The record `thread` sleeps in and its counts, the record first: see
/// [`Kept`]. A thread not asleep in a system call is not read.
fn quiet_state(
    thread: &impl ThreadState,
) -> Result<(crate::backend::Asleep, (u64, u64)), Arc<str>> {
    let io = |error: std::io::Error| Arc::<str>::from(error.to_string());
    let asleep = thread.asleep().map_err(io)?.ok_or(NOT_ASLEEP)?;
    let switches = thread.switches().map_err(io)?;
    Ok((asleep, switches))
}

/// A runtime a module carries, bound or with why it cannot be.
type Detected = Result<Arc<dyn runtime_model::RuntimeModel>, runtime_model::Unbound>;

/// A piece of a process's code as the recording mapped it: the image whose
/// code it is and the bias it is mapped with, a copy of an image's code
/// among them, or code a runtime generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedCode {
    pub start: u64,
    pub end: u64,
    pub image: crate::ModuleImageId,
    pub bias: crate::LoadBias,
    pub generated: bool,
}

/// Whose code at `address` is, by the first piece of `code` that holds it,
/// as a sample's frame is placed, for the runtime the image `image`
/// carries.
fn code_at(
    code: &[MappedCode],
    image: crate::ModuleImageId,
    address: crate::VirtualAddress,
) -> runtime_model::CodeAt {
    let at = address.get();
    match code
        .iter()
        .find(|piece| piece.start <= at && at < piece.end)
    {
        Some(piece) if piece.generated => runtime_model::CodeAt::Generated,
        Some(piece) if piece.image == image => {
            runtime_model::CodeAt::Image(piece.bias.wrapping_unload(address))
        }
        _ => runtime_model::CodeAt::Other,
    }
}

impl Snapshotter {
    /// Reads the debug information of `program`, which process `pid` runs,
    /// once for every snapshot.
    pub fn new(
        pid: u32,
        program: &Path,
        debug_files: &crate::DebugFileOptions,
    ) -> Result<Self, RecordError> {
        let program = Program::load(program, debug_files)
            .map_err(|error| RecordError::Program(error.to_string()))?;
        Ok(Self {
            pid,
            program,
            thread_pointers: BTreeMap::new(),
            runtimes: BTreeMap::new(),
            tls: crate::backend::RunningTls::new(pid),
            kept: RefCell::default(),
            files: BTreeMap::new(),
        })
    }

    /// Stops the process, reads what `reads` asks of it, and lets it go
    /// on. The labels of its threads are read through `modules`, as a live
    /// snapshot's are, while it is stopped. A thread of its own runs the
    /// snapshot in a runtime of its own, as the caller may run in one.
    pub fn take(
        &mut self,
        reads: Reads,
        modules: &[(crate::LoadedModule, DebugInfo)],
    ) -> Result<Snapshot, crate::Error> {
        let _span = crate::span!("task snapshot");
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?
                        .block_on(self.read(reads, modules))
                })
                .join()
                .unwrap_or_else(|_| Err(crate::Error::RequestCancelled))
        })
    }

    async fn read(
        &mut self,
        reads: Reads,
        modules: &[(crate::LoadedModule, DebugInfo)],
    ) -> Result<Snapshot, crate::Error> {
        // A program the recording started is not reaped while the debugger
        // may trace it, so that its exit is the debugger's to see.
        let _held = crate::backend::reaped::REAPED.hold(self.pid);
        let debugger = Debugger::for_program(self.program.clone())?;
        let handle = debugger.handle();

        let read = match handle
            .observe_process(ProcessId::new(u64::from(self.pid)))
            .await
        {
            Ok(stop) => {
                let mut read = read(&handle, stop, reads).await;
                if reads.threads
                    && let Ok(snapshot) = &mut read
                {
                    self.label_threads(modules, snapshot);
                }

                read
            }
            Err(error) => Err(error),
        };

        // The process goes on whether or not reading it failed.
        let detached = debugger.shutdown().await;
        let read = read?;
        detached?;
        Ok(read)
    }
}

/// When thread `tid` of process `pid` started, in clock ticks since boot,
/// which tells it from a later thread given its id.
fn thread_start(pid: u32, tid: u32) -> Option<u64> {
    crate::backend::thread_start_time(crate::model::Pid::new(pid), crate::model::Tid::new(tid))
}

/// What `reads` asks of the program at `stop`.
async fn read(
    handle: &DebuggerHandle,
    stop: StopId,
    reads: Reads,
) -> Result<Snapshot, crate::Error> {
    let mut snapshot = Snapshot::default();
    if reads.tasks {
        read_tasks(handle, stop, &mut snapshot).await?;
    }

    if reads.threads {
        read_threads(handle, stop, &mut snapshot).await?;
    }

    Ok(snapshot)
}

/// Each thread's stack at `stop`, as the debugger's backtrace has it: an
/// interpreter's frames among the frames that run them.
async fn read_threads(
    handle: &DebuggerHandle,
    stop: StopId,
    snapshot: &mut Snapshot,
) -> Result<(), crate::Error> {
    let state = handle.snapshot().await?;
    for thread in state.threads.iter() {
        let context = StopContext {
            stop,
            execution: ExecutionContext::Thread(thread.id),
            frame: StackFrameId::INNERMOST,
        };

        match handle.at(context).backtrace().await {
            Ok(backtrace) => snapshot.waiting.push(Waiting {
                noun: THREAD,
                number: thread.id.get(),
                state: SAMPLED.to_owned(),
                labels: None,
                frames: frames(&backtrace, Vec::new()),
            }),
            Err(error) => snapshot
                .problems
                .push(format!("{THREAD} {}: {error}", thread.id)),
        }
    }

    Ok(())
}

/// Every waiting task's stack at `stop`.
async fn read_tasks(
    handle: &DebuggerHandle,
    stop: StopId,
    snapshot: &mut Snapshot,
) -> Result<(), crate::Error> {
    let mut images = BTreeMap::new();
    let mut cursor = None;
    let mut seen = 0;
    loop {
        let page = handle.tasks(cursor, 256).await?;
        snapshot
            .problems
            .extend(page.gaps.iter().map(ToString::to_string));
        snapshot
            .located
            .extend(page.tasks.iter().filter_map(|task| {
                Some(Located {
                    object: task.object?.get(),
                    noun: task.noun,
                    number: task.id.number,
                    labels: labels(task),
                })
            }));

        for task in page.tasks.iter().filter(|task| waits(task)) {
            let context = StopContext {
                stop,
                execution: ExecutionContext::Task(task.id),
                frame: StackFrameId::INNERMOST,
            };

            match handle.at(context).backtrace().await {
                Ok(backtrace) => {
                    let mut futures = Vec::new();
                    for frame in backtrace.frames.iter() {
                        if let FrameKind::Awaited { ty, .. } = frame.kind {
                            futures.push(future_name(handle, &mut images, frame.module, ty).await);
                        }
                    }

                    snapshot.waiting.push(Waiting {
                        noun: task.noun,
                        number: task.id.number,
                        state: state(task),
                        labels: labels(task),
                        frames: frames(&backtrace, futures),
                    });
                }
                Err(error) => snapshot
                    .problems
                    .push(format!("{} {}: {error}", task.noun, task.id)),
            }
        }

        seen += page.tasks.len();
        match page.next {
            Some(next) if seen < MOST_TASKS => cursor = Some(next),
            Some(_) => {
                snapshot
                    .problems
                    .push(format!("only the first {MOST_TASKS} tasks were read"));
                return Ok(());
            }
            None => return Ok(()),
        }
    }
}

/// How a backtrace names the future of type `ty` a suspended task awaits,
/// which `module`'s image describes: `awaiting tokio::time::sleep::Sleep`.
/// What it waits for, which changes as it waits, is its task's state.
async fn future_name(
    handle: &DebuggerHandle,
    images: &mut BTreeMap<ModuleId, Option<Arc<ModuleImage>>>,
    module: Option<ModuleId>,
    ty: TypeReference,
) -> String {
    let image = match module {
        Some(module) => {
            if let std::collections::btree_map::Entry::Vacant(entry) = images.entry(module) {
                entry.insert(handle.loaded_module_image(module).await.ok());
            }

            images.get(&module).cloned().flatten()
        }
        None => None,
    };

    let name = image
        .filter(|image| image.id() == ty.image)
        .and_then(|image| image.type_info(ty).map(TypeInfo::qualified_name))
        .unwrap_or_else(|| "a future".to_owned());
    format!("awaiting {name}")
}

/// Whether a task waits: it is on no thread, and is the program's. A task
/// that runs is a CPU sample's to see, and the runtime's own, such as Go's
/// garbage collector's workers, wait for the runtime, as Go's goroutine
/// profile leaves them out.
const fn waits(task: &TaskSnapshot) -> bool {
    !task.internal
        && matches!(
            task.state,
            TaskState::Runnable | TaskState::Blocked | TaskState::Unknown(_)
        )
}

fn state(task: &TaskSnapshot) -> String {
    state_text(task.detail.as_deref(), &task.state)
}

/// The runtime's words for what a task waits for, or else its state.
fn state_text(detail: Option<&str>, state: &TaskState) -> String {
    detail.map_or_else(
        || {
            match state {
                TaskState::Running => "running",
                TaskState::Runnable => "runnable",
                TaskState::Blocked => "blocked",
                TaskState::Exited => "exited",
                TaskState::Unknown(_) => "unknown",
            }
            .to_owned()
        },
        str::to_owned,
    )
}

fn labels(task: &TaskSnapshot) -> Option<String> {
    label_text(&task.labels)
}

/// A task's labels as `key=value` lines, if it has any.
fn label_text(labels: &[(Arc<str>, Arc<str>)]) -> Option<String> {
    (!labels.is_empty()).then(|| {
        labels
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// A backtrace's frames as a sample's stack would have them, with the
/// names of the futures it awaits: an inline frame is its physical frame's
/// lines, and tail calls, which only debug information shows, are left out.
fn frames(backtrace: &crate::Backtrace, futures: Vec<String>) -> Vec<WaitFrame> {
    let mut futures = futures.into_iter();
    let mut frames = Vec::with_capacity(backtrace.frames.len() + 1);

    // The innermost frame, and one a signal interrupted, are where their
    // code is; a caller's address returns past its call.
    let mut exact = true;

    for frame in backtrace.frames.iter() {
        // A frame whose code a runtime owns runs the functions it recorded,
        // which stand for it in an interpreted look, as an interpreter's
        // frames that run interpreted ones do.
        let recorded = frame
            .recorded
            .as_ref()
            .filter(|_| matches!(frame.kind, FrameKind::Inline | FrameKind::Physical));
        if let Some(recorded) = recorded {
            frames.push(WaitFrame::Interpreted(InterpretedWait::of(Some(recorded))));
        }
        let role = role_number(if recorded.is_some() {
            crate::CodeRole::Interpreter
        } else {
            frame.role
        });
        match frame.kind {
            FrameKind::Inline | FrameKind::TailCall => {}

            // An interpreted frame's code is in no image, so it is named by
            // where it is. The interpreter's own frame that runs it stays
            // in the stack, which the native look shows in its place.
            FrameKind::Interpreted { .. } => {
                frames.push(WaitFrame::Interpreted(InterpretedWait::of(
                    frame.recorded.as_ref(),
                )));
            }
            FrameKind::Physical => {
                if let Some(instruction) = frame.instruction {
                    let address = instruction.get();
                    let address = if exact {
                        address
                    } else {
                        address.wrapping_sub(1)
                    };

                    frames.push(WaitFrame::Code { address, role });
                }

                exact = false;
            }
            FrameKind::Signal => {
                if let Some(instruction) = frame.instruction {
                    frames.push(WaitFrame::Code {
                        address: instruction.get(),
                        role,
                    });
                }

                exact = true;
            }
            FrameKind::Async { .. } => {
                if let Some(instruction) = frame.instruction {
                    frames.push(WaitFrame::Code {
                        address: instruction.get(),
                        role,
                    });
                }
            }
            FrameKind::Awaited { .. } => {
                let name = futures.next().unwrap_or_else(|| "awaiting a future".into());
                frames.push(WaitFrame::Awaited(name));
            }
        }
    }

    if let Some(reason) = truncation(&backtrace.termination) {
        frames.push(WaitFrame::Truncated(reason));
    }

    frames
}

/// Why a task's stack ends early, if it does.
const fn truncation(termination: &UnwindTermination) -> Option<Truncation> {
    Some(match termination {
        UnwindTermination::Complete => return None,
        UnwindTermination::NoUnwindInfo { .. } => Truncation::NoUnwindInfo,
        UnwindTermination::ModuleNotFound { .. } => Truncation::UnmappedAddress,
        UnwindTermination::InvalidCaller { .. } | UnwindTermination::CycleDetected => {
            Truncation::InvalidCaller
        }
        UnwindTermination::UnresolvedStackSwitch { .. } => Truncation::StackSwitch,
        UnwindTermination::BrokenAwaitChain { .. } => Truncation::AwaitChainEnded,
        UnwindTermination::DepthLimit => Truncation::DepthLimit,
        UnwindTermination::UnsupportedUnwindInfo { .. }
        | UnwindTermination::CorruptUnwindInfo { .. }
        | UnwindTermination::RegisterUnavailable { .. }
        | UnwindTermination::MemoryReadFailed { .. }
        | UnwindTermination::StackNotSaved { .. } => Truncation::Unreadable,
    })
}

/// The most bytes of a parked task's stack a live snapshot reads.
const LIVE_STACK_BYTES: u64 = 1 << 20;

/// The deepest stack a live snapshot unwinds.
const LIVE_FRAMES: usize = 512;

/// What a live snapshot found.
#[derive(Debug)]
pub enum Live {
    Taken(Snapshot),

    /// The process exited, before or while it was read, so it has no
    /// tasks, and what was read of it may show it partly gone.
    Exited,
}

impl Snapshotter {
    /// Reads what `reads` asks of the process while it runs, through
    /// `modules`, each module it maps with the debug information the
    /// recording read for it (see the module documentation). A thread is
    /// read only where an interpreter runs frames on it, as those frames
    /// alone: the thread runs on, so its registers, and the native frames
    /// they begin, cannot be read.
    pub fn take_live(
        &mut self,
        reads: Reads,
        modules: &[(crate::LoadedModule, DebugInfo)],
        code: &[MappedCode],
    ) -> Live {
        let _span = crate::span!("live task snapshot");
        let mut snapshot = Snapshot::default();
        if exited(self.pid) {
            self.files.clear();
            return Live::Exited;
        }

        if modules.is_empty() {
            snapshot
                .problems
                .push("the recording has not seen the program's mappings yet".to_owned());
            return Live::Taken(snapshot);
        }

        let threads = threads(self.pid);
        let starts = self.thread_starts(&threads);
        let learning = self.learn_thread_pointers(&starts);
        let pointers = starts
            .iter()
            .filter_map(|(&tid, &start)| Some((tid, *self.thread_pointers.get(&(tid, start))?)))
            .collect();
        self.kept
            .get_mut()
            .retain(|&(_, tid, start), _| starts.get(&tid) == Some(&start));
        let unwinding = modules
            .iter()
            .map(|(loaded, info)| crate::unwind::UnwindModule {
                loaded: *loaded,
                image: &info.image,
                unwind: info.unwind.as_ref(),
            })
            .collect::<Vec<_>>();
        let mut unlearned = BTreeSet::new();

        // A copy of a module's code, as V8's of its builtins, runs the
        // module's runtime, which the module itself reads.
        for (loaded, info) in modules.iter().filter(|(loaded, _)| loaded.window.is_none()) {
            let stop = LiveStop {
                pid: self.pid,
                bias: loaded.load_bias,
                threads: threads.clone(),
                pointers: &pointers,
                tls: Some(&self.tls),
                unlearned: RefCell::new(BTreeSet::new()),
            };

            let detected = self.runtimes.entry(info.image.id()).or_insert_with(|| {
                let image: Arc<dyn runtime_model::RuntimeImage + Send + Sync> =
                    Arc::clone(&info.image) as _;
                runtime_model::detect(&image)
            });

            for model in detected.iter() {
                match model {
                    Ok(model) => {
                        let live = LiveRuntime {
                            image: &info.image,
                            stop: &stop,
                            modules: &unwinding,
                            code,
                            model: model.as_ref(),
                            starts: &starts,
                            kept: &self.kept,
                            files: &self.files,
                        };

                        if reads.tasks {
                            live.read(&mut snapshot);
                        }

                        if reads.threads {
                            live.threads(&mut snapshot);
                            label(model.as_ref(), &stop, &mut snapshot);
                        }
                    }
                    Err(unbound) => snapshot.problems.push(unbound.reason.to_string()),
                }
            }

            unlearned.append(&mut stop.unlearned.into_inner());
        }

        if !unlearned.is_empty() {
            let threads = unlearned
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let why = learning
                .err()
                .unwrap_or_else(|| "the C library does not list them".into());
            snapshot.problems.push(format!(
                "the tasks of threads {threads} are not read: their thread pointers are unknown: {why}"
            ));
        }

        if exited(self.pid) {
            Live::Exited
        } else {
            Live::Taken(snapshot)
        }
    }

    /// Adds the labels each runtime of `modules` gives the process's
    /// threads to `snapshot`, read from its memory.
    fn label_threads(
        &mut self,
        modules: &[(crate::LoadedModule, DebugInfo)],
        snapshot: &mut Snapshot,
    ) {
        let threads = threads(self.pid);
        let pointers = BTreeMap::new();

        for (loaded, info) in modules.iter().filter(|(loaded, _)| loaded.window.is_none()) {
            let stop = LiveStop {
                pid: self.pid,
                bias: loaded.load_bias,
                threads: threads.clone(),
                pointers: &pointers,
                tls: None,
                unlearned: RefCell::new(BTreeSet::new()),
            };

            let detected = self.runtimes.entry(info.image.id()).or_insert_with(|| {
                let image: Arc<dyn runtime_model::RuntimeImage + Send + Sync> =
                    Arc::clone(&info.image) as _;
                runtime_model::detect(&image)
            });

            for model in detected.iter().flatten() {
                label(model.as_ref(), &stop, snapshot);
            }
        }
    }

    /// When each of `threads` started, by its id, from the files kept of
    /// it, which this first keeps to the threads there now
    /// ([`keep_files`]), or for a thread beyond them, from its `stat`.
    fn thread_starts(&mut self, threads: &[crate::ThreadId]) -> BTreeMap<u32, u64> {
        let tids: Vec<u32> = threads
            .iter()
            .filter_map(|thread| u32::try_from(thread.get()).ok())
            .collect();
        keep_files(&mut self.files, self.pid, &tids, MOST_KEPT_THREADS);
        tids.iter()
            .filter_map(|&tid| {
                let start = match self.files.get(&tid) {
                    Some(files) => Some(files.start()),
                    None => thread_start(self.pid, tid),
                };
                Some((tid, start?))
            })
            .collect()
    }

    /// Learns the thread pointer of each thread of `started`, by its id
    /// and when it started, not yet learned from the C library's lists of
    /// its threads, or says why it cannot.
    fn learn_thread_pointers(&mut self, started: &BTreeMap<u32, u64>) -> Result<(), String> {
        if started
            .iter()
            .all(|(&tid, &start)| self.thread_pointers.contains_key(&(tid, start)))
        {
            return Ok(());
        }

        let listed = crate::backend::running_thread_pointers(self.pid)?
            .into_iter()
            .collect::<BTreeMap<_, _>>();

        // A thread that ended takes its pointer with it; a later thread
        // may have its id.
        self.thread_pointers.clear();

        for (&tid, &start) in started {
            if let Some(pointer) = listed.get(&tid) {
                self.thread_pointers.insert((tid, start), *pointer);
            }
        }

        Ok(())
    }
}

/// Adds the labels `model` gives the threads of `stop` to `snapshot`.
fn label(model: &dyn runtime_model::RuntimeModel, stop: &LiveStop<'_>, snapshot: &mut Snapshot) {
    let labels = model.thread_labels(stop);
    snapshot
        .problems
        .extend(labels.gaps.iter().map(ToString::to_string));
    snapshot.thread_labels.extend(
        labels
            .value
            .into_iter()
            .filter_map(|labels| Some((u32::try_from(labels.thread.get()).ok()?, labels.labels))),
    );
}

/// Whether process `pid` has exited: it is gone, or waits to be reaped.
pub fn exited(pid: u32) -> bool {
    crate::backend::process_exited(crate::model::Pid::new(pid))
}

/// The threads of process `pid`, in the order of their ids.
fn threads(pid: u32) -> Vec<crate::ThreadId> {
    let mut threads: Vec<crate::ThreadId> = std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .map(crate::ThreadId::new)
        .collect();
    threads.sort_unstable();
    threads
}

/// The running process, as a runtime model of one module reads it: its
/// memory, and of its threads' registers, which run on, only the thread
/// pointers learned.
pub struct LiveStop<'a> {
    pid: u32,
    bias: crate::LoadBias,
    threads: Vec<crate::ThreadId>,
    pointers: &'a BTreeMap<u32, u64>,

    /// Where the module's thread-local storage is found for a thread, by
    /// its thread pointer; `None` where the stop does not find it.
    tls: Option<&'a crate::backend::RunningTls>,

    /// The threads whose thread pointers a model asked for and none was
    /// learned.
    unlearned: RefCell<BTreeSet<u32>>,
}

/// No thread pointers.
static NO_POINTERS: BTreeMap<u32, u64> = BTreeMap::new();

impl LiveStop<'static> {
    /// A live thread catalog for namespace identity and interpreter reads.
    pub(super) fn for_threads(pid: u32, bias: crate::LoadBias) -> Self {
        Self {
            threads: threads(pid),
            ..Self::memory(pid, bias)
        }
    }

    /// Process `pid`'s memory and its thread `tid` alone, as the runtime
    /// model of a module loaded with `bias` reads them.
    pub fn for_thread(pid: u32, tid: u32, bias: crate::LoadBias) -> Self {
        Self {
            threads: vec![crate::ThreadId::new(u64::from(tid))],
            ..Self::memory(pid, bias)
        }
    }

    /// Process `pid`'s memory alone, as the runtime model of a module
    /// loaded with `bias` reads it: no thread of it.
    pub fn memory(pid: u32, bias: crate::LoadBias) -> Self {
        Self {
            pid,
            bias,
            threads: Vec::new(),
            pointers: &NO_POINTERS,
            tls: None,
            unlearned: RefCell::new(BTreeSet::new()),
        }
    }
}

impl<'a> LiveStop<'a> {
    /// Process `pid`'s memory and its stopped thread `tid` alone, whose
    /// thread pointer `pointers` holds, as the runtime model of a module
    /// loaded with `bias` reads them.
    pub fn for_stopped_thread(
        pid: u32,
        tid: u32,
        pointers: &'a BTreeMap<u32, u64>,
        bias: crate::LoadBias,
    ) -> Self {
        Self {
            pid,
            bias,
            threads: vec![crate::ThreadId::new(u64::from(tid))],
            pointers,
            tls: None,
            unlearned: RefCell::new(BTreeSet::new()),
        }
    }
}

impl LiveStop<'_> {
    /// The addresses thread `tid`'s stack may hold that no other thread's
    /// does. glibc keeps each thread's descriptor at its thread pointer,
    /// on top of the stack it allocated for it or the program gave it
    /// (`allocate_stack`), and the first thread's stack lies above every
    /// mapping the program makes. So a thread's stack lies below its own
    /// thread pointer and above every lower one, and the first thread's
    /// above every other thread's thread pointer. A thread whose thread
    /// pointer is not known may have its stack anywhere.
    fn stack(&self, tid: u32) -> Result<std::ops::Range<u64>, Arc<str>> {
        let mut pointers = Vec::with_capacity(self.threads.len());
        let mut unknown = Vec::new();
        for thread in &self.threads {
            let Ok(other) = u32::try_from(thread.get()) else {
                continue;
            };
            match self.pointers.get(&other) {
                Some(&pointer) => pointers.push((other, pointer)),
                None => unknown.push(other.to_string()),
            }
        }
        if !unknown.is_empty() {
            return Err(format!(
                "the stacks of threads {} are not known, as their thread pointers are not",
                unknown.join(", ")
            )
            .into());
        }
        let others = pointers.iter().filter(|(other, _)| *other != tid);
        if tid == self.pid {
            let highest = others.map(|&(_, pointer)| pointer).max().unwrap_or(0);
            return Ok(highest.saturating_add(1)..u64::MAX);
        }
        let own = pointers
            .iter()
            .find(|(other, _)| *other == tid)
            .map(|&(_, pointer)| pointer)
            .ok_or("its thread pointer is not known")?;
        let below = others
            .map(|&(_, pointer)| pointer)
            .filter(|&pointer| pointer < own)
            .max()
            .unwrap_or(0);
        Ok(below.saturating_add(1)..own)
    }
}

impl RuntimeStop for LiveStop<'_> {
    fn target_thread_id(&self, thread: crate::ThreadId) -> Option<runtime_model::TargetThreadId> {
        if self.threads.binary_search(&thread).is_err() {
            return None;
        }
        crate::backend::target_thread_id(self.pid, u32::try_from(thread.get()).ok()?)
    }

    fn target_thread_ids(
        &self,
        threads: &[crate::ThreadId],
    ) -> Vec<(crate::ThreadId, runtime_model::TargetThreadId)> {
        let threads = threads
            .iter()
            .filter(|thread| self.threads.binary_search(thread).is_ok())
            .filter_map(|thread| u32::try_from(thread.get()).ok())
            .collect::<Vec<_>>();
        crate::backend::target_thread_ids(self.pid, &threads)
            .into_iter()
            .map(|(thread, target)| (crate::ThreadId::new(u64::from(thread)), target))
            .collect()
    }

    fn read(&self, address: crate::VirtualAddress, bytes: &mut [u8]) -> bool {
        super::perf::read_memory(self.pid, address.get(), bytes)
            .is_ok_and(|read| read == bytes.len())
    }

    fn read_each(&self, reads: &mut [(crate::VirtualAddress, &mut [u8])]) -> Vec<bool> {
        super::perf::read_memory_each(self.pid, reads)
    }

    fn thread_pointer(&self, thread: crate::ThreadId) -> Option<u64> {
        let tid = u32::try_from(thread.get()).ok()?;
        let pointer = self.pointers.get(&tid).copied();
        if pointer.is_none() {
            self.unlearned.borrow_mut().insert(tid);
        }

        pointer
    }

    fn module_thread_local(
        &self,
        thread: crate::ThreadId,
        offset: u64,
    ) -> Result<crate::VirtualAddress, Arc<str>> {
        let tls = self
            .tls
            .ok_or("a live read of memory alone finds no module's thread-local storage")?;
        let pointer = self
            .thread_pointer(thread)
            .ok_or("the thread's thread pointer is unknown")?;
        tls.address(pointer, self.bias, offset)
            .map(crate::VirtualAddress::new)
            .map_err(Arc::from)
    }

    fn instruction(&self, _thread: crate::ThreadId) -> Option<crate::VirtualAddress> {
        None
    }

    fn load_bias(&self) -> crate::LoadBias {
        self.bias
    }

    fn threads(&self) -> Vec<crate::ThreadId> {
        self.threads.clone()
    }
}

/// A parked task's stack, read a page at a time as unwinding reaches it,
/// so that the pages read can be read again and compared.
struct LivePages<'a> {
    stop: &'a LiveStop<'a>,

    /// Where reading may begin, and the most bytes past it.
    start: u64,
    pages: BTreeMap<u64, Option<Box<[u8; PAGE]>>>,
}

const PAGE: usize = 4096;

impl LivePages<'_> {
    fn page(&mut self, page: u64) -> Option<&[u8; PAGE]> {
        let stop = self.stop;
        self.pages
            .entry(page)
            .or_insert_with(|| {
                let mut bytes = Box::new([0; PAGE]);
                stop.read(crate::VirtualAddress::new(page), &mut bytes[..])
                    .then_some(bytes)
            })
            .as_deref()
    }

    /// Whether every page read reads the same again.
    fn unchanged(&self) -> bool {
        self.pages.iter().all(|(&page, bytes)| {
            let mut again = [0; PAGE];
            let read = self.stop.read(crate::VirtualAddress::new(page), &mut again);
            bytes
                .as_ref()
                .map_or(!read, |bytes| read && **bytes == again)
        })
    }
}

impl crate::unwind::MemoryReader for LivePages<'_> {
    fn read_u64(&mut self, address: crate::VirtualAddress) -> Option<u64> {
        let address = address.get();
        let end = address.checked_add(8)?;
        if address < self.start || end > self.start.saturating_add(LIVE_STACK_BYTES) {
            return None;
        }

        let mut word = [0; 8];
        for (index, byte) in word.iter_mut().enumerate() {
            let at = address + index as u64;
            let page = at & !(PAGE as u64 - 1);
            *byte = self.page(page)?[usize::try_from(at - page).ok()?];
        }

        Some(u64::from_le_bytes(word))
    }
}

/// One runtime of the running process, as a live snapshot reads it.
struct LiveRuntime<'a> {
    image: &'a Arc<ModuleImage>,
    stop: &'a LiveStop<'a>,
    modules: &'a [crate::unwind::UnwindModule<'a>],
    code: &'a [MappedCode],
    model: &'a dyn runtime_model::RuntimeModel,

    /// When each thread started, by its id.
    starts: &'a BTreeMap<u32, u64>,
    kept: &'a RefCell<BTreeMap<(crate::ModuleImageId, u32, u64), Kept>>,

    /// The threads' kept `/proc` files, by their ids.
    files: &'a ThreadsFiles,
}

impl LiveRuntime<'_> {
    /// Adds the runtime's tasks, and where those that wait wait, to
    /// `snapshot`.
    fn read(&self, snapshot: &mut Snapshot) {
        let noun = self.model.task_noun();
        let mut start = 0;
        loop {
            let page = self.model.tasks(self.stop, start, 256, false);
            snapshot
                .problems
                .extend(page.gaps.iter().map(ToString::to_string));

            for task in &page.value.tasks {
                let labels = label_text(&task.labels);
                if task.locator != 0 {
                    snapshot.located.push(Located {
                        object: task.locator,
                        noun,
                        number: task.number,
                        labels: labels.clone(),
                    });
                }

                if !live_waits(task) {
                    continue;
                }

                match self.frames(task) {
                    Ok(Some(frames)) => snapshot.waiting.push(Waiting {
                        noun,
                        number: task.number,
                        state: state_text(task.detail.as_deref(), &task.state),
                        labels,
                        frames,
                    }),
                    Ok(None) => snapshot.changed += 1,
                    Err(error) => snapshot
                        .problems
                        .push(format!("{noun} {}: {error}", task.number)),
                }
            }

            match page.value.next {
                Some(next) if snapshot.located.len() < MOST_TASKS => start = next,
                Some(_) => {
                    snapshot
                        .problems
                        .push(format!("only the first {MOST_TASKS} tasks were read"));
                    return;
                }
                None => return,
            }
        }
    }

    /// Adds where each thread the runtime's interpreter runs frames on
    /// is to `snapshot`, as one read of its frames finds them. A read
    /// that may have seen them change, whose frames end early or one of
    /// which is unreadable, is left out and counted, never kept as a
    /// shorter stack.
    fn threads(&self, snapshot: &mut Snapshot) {
        if self.model.owns_generated_code() {
            self.published(snapshot);
            return;
        }
        let Some(stacks) = self.model.interpreted(self.stop) else {
            return;
        };

        snapshot
            .problems
            .extend(stacks.gaps.iter().map(ToString::to_string));

        for thread in &stacks.value.threads {
            let frames = stacks.value.frames_of(thread);
            if thread.end.is_some() || frames.iter().any(|frame| frame.problem.is_some()) {
                snapshot.changed += 1;
                continue;
            }

            snapshot.waiting.push(Waiting {
                noun: THREAD,
                number: thread.thread.get(),
                state: SAMPLED.to_owned(),
                labels: None,
                frames: frames
                    .iter()
                    .map(|frame| WaitFrame::Interpreted(InterpretedWait::of_frame(frame)))
                    .collect(),
            });
        }
    }

    /// Adds where each thread is in the runtime's code to `snapshot`, as
    /// the runtime publishes its frames in memory
    /// ([`runtime_model::RuntimeModel::live_frames`]), for each thread
    /// that holds something of the runtime's. A thread is read only while
    /// it is proven not to run: asleep in a system call before and after
    /// the read, with the same record of it and the same stack pointer,
    /// and having given up its CPU no more times, since a thread that ran
    /// meanwhile gave it up to sleep again. One that may have run is
    /// counted as changed; one left out for another reason is said, by
    /// the reason.
    fn published(&self, snapshot: &mut Snapshot) {
        let stop = self.stop;
        let image = self.image.id();
        let unmapped = RefCell::new(BTreeSet::new());
        let code = |address: crate::VirtualAddress| {
            let at = address.get();
            if !self
                .code
                .iter()
                .any(|piece| piece.start <= at && at < piece.end)
            {
                unmapped.borrow_mut().insert(at);
            }
            code_at(self.code, image, address)
        };
        let mut unread: BTreeMap<Arc<str>, Vec<u64>> = BTreeMap::new();
        for &thread in &stop.threads {
            if self.model.thread_holding(stop, thread).is_none() {
                continue;
            }
            let number = thread.get();
            let read = u32::try_from(number)
                .map_err(|_| Arc::<str>::from("it has no Linux id"))
                .and_then(|tid| self.published_thread(tid, &code));
            match read {
                Ok(Some(frames)) if frames.is_empty() => {}
                Ok(Some(frames)) => snapshot.waiting.push(Waiting {
                    noun: THREAD,
                    number,
                    state: SAMPLED.to_owned(),
                    labels: None,
                    frames: frames
                        .iter()
                        .map(|frame| WaitFrame::Interpreted(InterpretedWait::of_frame(frame)))
                        .collect(),
                }),
                Ok(None) => {
                    snapshot.changed += 1;
                    snapshot.changed_threads.push(number);
                }
                Err(reason) => unread.entry(reason).or_default().push(number),
            }
        }
        snapshot.unmapped_code.extend(unmapped.into_inner());
        // A snapshot keeps one problem, so every thread left out is in it,
        // by its reason.
        let unread: Vec<String> = unread
            .into_iter()
            .map(|(reason, threads)| {
                let threads = threads
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("threads {threads}: {reason}")
            })
            .collect();
        if !unread.is_empty() {
            snapshot
                .problems
                .insert(0, format!("{UNREAD_LIVE}{}", unread.join("; ")));
        }
    }

    /// The frames of thread `tid`, which holds something of the runtime's,
    /// as one read proven quiet found them: none where it runs none of the
    /// runtime's code, `None` where it may have run while read. A thread
    /// that has not run since such a read is not read again ([`Kept`]).
    fn published_thread(
        &self,
        tid: u32,
        code: &dyn Fn(crate::VirtualAddress) -> runtime_model::CodeAt,
    ) -> Result<Option<Vec<runtime_model::InterpretedFrame>>, Arc<str>> {
        let stop = self.stop;
        let stack = stop.stack(tid)?;
        let start = self.starts.get(&tid).copied();
        let opened;
        let files = if let Some(files) = self.files.get(&tid) {
            files
        } else {
            opened = open_unkept(stop.pid, tid, start)?;
            &opened
        };
        let io = |error: std::io::Error| Arc::<str>::from(error.to_string());

        let (asleep, before) = quiet_state(files)?;
        let key = start.map(|start| (self.image.id(), tid, start));
        let kept = key.and_then(|key| {
            self.kept
                .borrow()
                .get(&key)
                .filter(|kept| kept.asleep == asleep && kept.switches == before)
                .map(|kept| kept.frames.clone())
        });
        if let Some(frames) = kept {
            return Ok(Some(frames));
        }
        let thread = runtime_model::LiveThread {
            thread: crate::ThreadId::new(u64::from(tid)),
            stack_pointer: asleep.stack_pointer,
            stack,
            code,
        };
        let read = self.model.live_frames(stop, &thread);
        let again = files.asleep().map_err(io)?;
        let after = files.switches().map_err(io)?;
        if again.as_ref() != Some(&asleep) || after != before {
            return Ok(None);
        }
        let frames = match read {
            None | Some(runtime_model::LiveFrames::None) => Vec::new(),
            Some(runtime_model::LiveFrames::Frames(frames)) => frames,
            Some(runtime_model::LiveFrames::Unread(reason)) => return Err(reason),
        };
        if let Some(key) = key {
            self.kept.borrow_mut().insert(
                key,
                Kept {
                    switches: after,
                    asleep,
                    frames: frames.clone(),
                },
            );
        }
        Ok(Some(frames))
    }

    /// Where a waiting task waits, or `None` when it changed while read.
    fn frames(&self, task: &runtime_model::RuntimeTask) -> Result<Option<Vec<WaitFrame>>, String> {
        let task_ref = runtime_model::TaskRef {
            number: task.number,
            locator: (task.locator != 0).then_some(task.locator),
        };

        let context = || self.model.task_context(self.stop, task_ref);
        let first = context().map_err(|error| error.to_string())?;

        let frames = match &first {
            None | Some(runtime_model::TaskContext::OnThread(_)) => return Ok(None),
            Some(runtime_model::TaskContext::Saved {
                registers,
                after_call,
            }) => {
                let Some((frames, pages)) = self.parked(registers, *after_call) else {
                    return Ok(None);
                };

                if !pages.unchanged() {
                    return Ok(None);
                }

                frames
            }
            Some(runtime_model::TaskContext::Interpreted { frames, end }) => {
                let mut waits: Vec<WaitFrame> = frames
                    .iter()
                    .map(|frame| WaitFrame::Interpreted(InterpretedWait::of_frame(frame)))
                    .collect();
                if end.is_some() {
                    waits.push(WaitFrame::Truncated(Truncation::AwaitChainEnded));
                }

                waits
            }
            Some(runtime_model::TaskContext::Suspended { future, ty }) => {
                let walk =
                    || runtime_model::futures::walk(self.image.as_ref(), self.stop, *future, *ty);
                let chain = walk();
                if walk() != chain {
                    return Ok(None);
                }

                self.awaits(&chain)
            }
        };

        let again = context().map_err(|error| error.to_string())?;
        Ok(same_context(first.as_ref(), again.as_ref()).then_some(frames))
    }

    /// A parked task's frames, unwound from the registers its runtime
    /// saved, with the pages of stack read for them.
    fn parked(
        &self,
        registers: &crate::unwind::RegisterFile,
        after_call: bool,
    ) -> Option<(Vec<WaitFrame>, LivePages<'_>)> {
        let mut pages = LivePages {
            stop: self.stop,
            start: registers.get(crate::unwind::RSP)?,
            pages: BTreeMap::new(),
        };

        let registers: Vec<_> = registers.iter().collect();
        let (unwound, end) =
            crate::unwind::sample::unwind_memory(self.modules, &registers, &mut pages, LIVE_FRAMES);
        let mut frames: Vec<WaitFrame> = unwound
            .iter()
            .enumerate()
            .map(|(index, frame)| WaitFrame::Code {
                // A task parked by a call is in that call, as a caller is.
                address: frame
                    .address
                    .get()
                    .wrapping_sub(u64::from(index == 0 && after_call)),
                role: role_number(frame.role),
            })
            .collect();

        if let crate::unwind::sample::StackEnd::Truncated(reason, _) = end {
            frames.push(WaitFrame::Truncated(reason));
        }

        Some((frames, pages))
    }

    /// A suspended task's chain of awaits as frames: each coroutine where
    /// it resumes, and the future it awaits by its type.
    fn awaits(&self, chain: &runtime_model::futures::AwaitChain) -> Vec<WaitFrame> {
        use runtime_model::futures::{AsyncFrameKind, ChainEnd};
        let bias = self.stop.bias;
        let mut frames = Vec::with_capacity(chain.frames.len() + 1);

        for frame in &chain.frames {
            match &frame.kind {
                AsyncFrameKind::Coroutine { state, .. } => {
                    let functions = self.image.coroutine_functions(frame.ty.id);
                    let role = functions
                        .first()
                        .map_or(crate::CodeRole::Ordinary, |function| function.info().role);
                    if let Some(address) =
                        crate::backend::resume_address(self.image, &functions, *state)
                    {
                        frames.push(WaitFrame::Code {
                            address: bias.wrapping_load(address).get(),
                            role: role_number(role),
                        });
                    }
                }
                AsyncFrameKind::Leaf => {
                    let name = self
                        .image
                        .type_info(frame.ty)
                        .map_or_else(|| "a future".to_owned(), TypeInfo::qualified_name);
                    frames.push(WaitFrame::Awaited(format!("awaiting {name}")));
                }
            }
        }

        let reason = match chain.end {
            ChainEnd::Leaf | ChainEnd::Unresumed | ChainEnd::Finished => None,
            ChainEnd::NoAwaitee | ChainEnd::Broken(_) => Some(Truncation::AwaitChainEnded),
            ChainEnd::TooDeep => Some(Truncation::DepthLimit),
            ChainEnd::Cycle => Some(Truncation::InvalidCaller),
        };

        frames.extend(reason.map(WaitFrame::Truncated));
        frames
    }
}

/// Whether two reads of a task's context found it where it was.
fn same_context(
    first: Option<&runtime_model::TaskContext>,
    again: Option<&runtime_model::TaskContext>,
) -> bool {
    use runtime_model::TaskContext;
    match (first, again) {
        (
            Some(TaskContext::Saved {
                registers,
                after_call,
            }),
            Some(TaskContext::Saved {
                registers: other,
                after_call: other_call,
            }),
        ) => registers == other && after_call == other_call,
        (
            Some(TaskContext::Suspended { future, ty }),
            Some(TaskContext::Suspended {
                future: other,
                ty: other_ty,
            }),
        ) => future == other && ty == other_ty,
        (
            Some(TaskContext::Interpreted { frames, end }),
            Some(TaskContext::Interpreted {
                frames: other,
                end: other_end,
            }),
        ) => frames == other && end == other_end,
        _ => false,
    }
}

/// Whether a task a live snapshot lists waits: as [`waits`] says, and on
/// no thread, since a running thread's registers are not read.
const fn live_waits(task: &runtime_model::RuntimeTask) -> bool {
    !task.internal
        && !task.stackless
        && task.thread.is_none()
        && matches!(
            task.state,
            TaskState::Runnable | TaskState::Blocked | TaskState::Unknown(_)
        )
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    /// A thread blocked in a read of a pipe until the returned descriptor
    /// is written or dropped, with its id.
    fn blocked() -> (std::thread::JoinHandle<()>, std::os::fd::OwnedFd, u32) {
        let (ready, started) = std::sync::mpsc::channel();
        let (reader, writer) = nix::unistd::pipe().unwrap();
        let thread = std::thread::spawn(move || {
            ready
                .send(u32::try_from(nix::unistd::gettid().as_raw()).unwrap())
                .unwrap();
            let mut byte = [0];
            let _ = nix::unistd::read(&reader, &mut byte);
        });
        let tid = started.recv_timeout(Duration::from_secs(20)).unwrap();
        (thread, writer, tid)
    }

    /// A thread that runs between the two reads of the check and sleeps
    /// again in the same call is seen to have run: its counts are read
    /// after its record, so they hold the run. Read before it, they would
    /// still be those a kept read left, and its stale frames be reused.
    #[test]
    fn a_thread_that_ran_between_the_checks_reads_is_seen_to_have_run() {
        use std::cell::Cell;

        /// A thread asleep in one read that runs once, after the first of
        /// its files is read, and sleeps again in the same read.
        struct Woke {
            reads: Cell<u32>,
            asleep: crate::backend::Asleep,
        }
        impl Woke {
            fn ran(&self) -> bool {
                let ran = self.reads.get() > 0;
                self.reads.set(self.reads.get() + 1);
                ran
            }
        }
        impl super::ThreadState for Woke {
            fn asleep(&self) -> std::io::Result<Option<crate::backend::Asleep>> {
                self.ran();
                Ok(Some(self.asleep.clone()))
            }

            fn switches(&self) -> std::io::Result<(u64, u64)> {
                Ok(if self.ran() { (6, 1) } else { (5, 1) })
            }
        }

        let asleep = crate::backend::Asleep::from_record(
            b"0 0x3 0x7ffd0000 0x1 0x0 0x0 0x0 0x7ffc0000 0x7f0000001234\n",
        )
        .unwrap();
        let kept = (asleep.clone(), (5, 1));
        let thread = Woke {
            reads: Cell::new(0),
            asleep,
        };
        let now = super::quiet_state(&thread).unwrap();
        assert_eq!(now.0, kept.0, "asleep in the same call");
        assert_ne!(now, kept, "a thread that ran reuses its stale frames");
    }

    /// Files kept of a thread that exited fail to read, and the next
    /// snapshot drops them rather than keep reading a thread that is gone:
    /// a later thread given its id is opened anew.
    #[test]
    fn the_files_of_a_thread_that_exited_are_dropped_and_opened_again() {
        let pid = std::process::id();
        let (thread, writer, tid) = blocked();
        let mut files = super::ThreadsFiles::new();
        super::keep_files(&mut files, pid, &[tid], super::MOST_KEPT_THREADS);
        assert!(files[&tid].switches().is_ok());

        // A joined thread may still be leaving: until the kernel releases
        // it, its files read it as it exits.
        drop(writer);
        thread.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let error = loop {
            match files[&tid].asleep() {
                Err(error) => break error,
                Ok(asleep) => assert_eq!(asleep, None, "asleep as it exits"),
            }
            assert!(Instant::now() < deadline, "the thread never went away");
            std::thread::yield_now();
        };
        assert_eq!(error.raw_os_error(), Some(nix::libc::ESRCH), "{error}");

        // As though another thread had its id: it is listed again.
        super::keep_files(&mut files, pid, &[tid], super::MOST_KEPT_THREADS);
        assert!(!files.contains_key(&tid), "kept the files of a thread gone");

        // So are the files of a thread that left unread, whose start time
        // a later thread with its id would otherwise be given.
        let (thread, writer, tid) = blocked();
        super::keep_files(&mut files, pid, &[tid], super::MOST_KEPT_THREADS);
        assert!(files.contains_key(&tid));
        drop(writer);
        thread.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::fs::metadata(format!("/proc/{pid}/task/{tid}")).is_ok() {
            assert!(Instant::now() < deadline, "the thread never went away");
            std::thread::yield_now();
        }
        super::keep_files(&mut files, pid, &[tid], super::MOST_KEPT_THREADS);
        assert!(!files.contains_key(&tid), "kept the files of a thread gone");
    }

    /// Files are kept for at most so many threads, and dropped with the
    /// threads that left.
    #[test]
    fn files_are_kept_for_so_many_threads_as_they_stay() {
        let pid = std::process::id();
        let threads: Vec<_> = (0..3).map(|_| blocked()).collect();
        let mut tids: Vec<u32> = threads.iter().map(|&(_, _, tid)| tid).collect();
        tids.sort_unstable();
        let mut files = super::ThreadsFiles::new();
        super::keep_files(&mut files, pid, &tids, 2);
        assert_eq!(files.keys().copied().collect::<Vec<_>>(), tids[..2]);
        super::keep_files(&mut files, pid, &tids[1..], 2);
        assert_eq!(files.keys().copied().collect::<Vec<_>>(), tids[1..]);
        for (thread, writer, _) in threads {
            drop(writer);
            thread.join().unwrap();
        }
    }

    /// A thread whose files are not kept is read through files opened for
    /// the read only if they are of the thread the snapshot found.
    #[test]
    fn files_opened_for_one_read_are_of_the_thread_found() {
        let pid = std::process::id();
        let (thread, writer, tid) = blocked();
        let start = super::thread_start(pid, tid);
        assert!(super::open_unkept(pid, tid, start).is_ok());
        let other = start.map(|start| start + 1);
        assert_eq!(
            super::open_unkept(pid, tid, other).unwrap_err().as_ref(),
            super::EXITED
        );
        drop(writer);
        thread.join().unwrap();
    }

    #[test]
    fn a_live_thread_with_a_non_utf8_comm_is_not_mistaken_for_an_exited_process() {
        let original = nix::sys::prctl::get_name().unwrap();
        let name = c"microscope_\xff";
        nix::sys::prctl::set_name(name).unwrap();
        let thread = u32::try_from(nix::unistd::gettid().as_raw()).unwrap();
        let exited = super::exited(thread);
        nix::sys::prctl::set_name(&original).unwrap();
        assert!(!exited, "invalid UTF-8 in comm does not imply process exit");
    }

    use super::{Live, Snapshotter};

    /// Whether process `pid` has exited and waits to be reaped.
    fn zombie(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| Some(stat[stat.rfind(')')? + 2..].starts_with('Z')))
            .unwrap_or(false)
    }

    /// A process that has exited, before it is reaped, keeps no mappings
    /// and runs no tasks: a live snapshot of it says it exited, rather
    /// than that it could not read the program.
    #[test]
    fn a_live_snapshot_of_an_exited_process_says_it_exited() {
        let program =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/basic");
        let mut child = std::process::Command::new(&program)
            .spawn()
            .expect("start the program");
        let pid = child.id();
        let mut snapshotter = Snapshotter::new(pid, &program, &crate::DebugFileOptions::default())
            .expect("read the program");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !zombie(pid) {
            assert!(Instant::now() < deadline, "the program never exited");
            std::thread::yield_now();
        }

        let reads = super::Reads {
            tasks: true,
            threads: true,
        };
        let live = snapshotter.take_live(reads, &[], &[]);
        child.wait().expect("reap the program");
        assert!(matches!(live, Live::Exited), "{live:?}");
    }
}
