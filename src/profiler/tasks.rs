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

/// Why a live snapshot reads no thread of a runtime that runs its
/// functions on the native stack, as V8 does: a running thread's frame
/// pointer, where its frames begin, is not shown outside it.
const UNREAD_NATIVE: &str = "live snapshots do not read JavaScript frames: they begin at a \
                             thread's frame pointer, which only stopping the thread shows, as \
                             /proc gives a running or blocked thread's stack and instruction \
                             pointers but not its frame pointer; use --node snapshot or bpf";

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

    /// The labels the program's runtimes gave each thread, by its id, as
    /// they were when read.
    pub thread_labels: Vec<(u32, Labels)>,
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
}

/// A runtime a module carries, bound or with why it cannot be.
type Detected = Result<Arc<dyn runtime_model::RuntimeModel>, runtime_model::Unbound>;

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
    ) -> Live {
        let _span = crate::span!("live task snapshot");
        let mut snapshot = Snapshot::default();
        if exited(self.pid) {
            return Live::Exited;
        }

        if modules.is_empty() {
            snapshot
                .problems
                .push("the recording has not seen the program's mappings yet".to_owned());
            return Live::Taken(snapshot);
        }

        let threads = threads(self.pid);
        let learning = self.learn_thread_pointers(&threads);
        let pointers = threads
            .iter()
            .filter_map(|thread| {
                let tid = u32::try_from(thread.get()).ok()?;
                let start = thread_start(self.pid, tid)?;
                Some((tid, *self.thread_pointers.get(&(tid, start))?))
            })
            .collect();
        let unwinding = modules
            .iter()
            .map(|(loaded, info)| crate::unwind::UnwindModule {
                loaded: *loaded,
                image: &info.image,
                unwind: info.unwind.as_ref(),
            })
            .collect::<Vec<_>>();
        let mut unlearned = BTreeSet::new();

        for (loaded, info) in modules {
            let stop = LiveStop {
                pid: self.pid,
                bias: loaded.load_bias,
                threads: threads.clone(),
                pointers: &pointers,
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
                            model: model.as_ref(),
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

        for (loaded, info) in modules {
            let stop = LiveStop {
                pid: self.pid,
                bias: loaded.load_bias,
                threads: threads.clone(),
                pointers: &pointers,
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

    /// Learns the thread pointer of each of `threads` not yet learned from
    /// the C library's lists of its threads, or says why it cannot.
    fn learn_thread_pointers(&mut self, threads: &[crate::ThreadId]) -> Result<(), String> {
        let started = threads
            .iter()
            .filter_map(|thread| {
                let tid = u32::try_from(thread.get()).ok()?;
                Some((tid, thread_start(self.pid, tid)?))
            })
            .collect::<Vec<_>>();
        if started
            .iter()
            .all(|known| self.thread_pointers.contains_key(known))
        {
            return Ok(());
        }

        let listed = crate::backend::running_thread_pointers(self.pid)?
            .into_iter()
            .collect::<BTreeMap<_, _>>();

        // A thread that ended takes its pointer with it; a later thread
        // may have its id.
        self.thread_pointers.clear();

        for (tid, start) in started {
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
            unlearned: RefCell::new(BTreeSet::new()),
        }
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

    fn thread_pointer(&self, thread: crate::ThreadId) -> Option<u64> {
        let tid = u32::try_from(thread.get()).ok()?;
        let pointer = self.pointers.get(&tid).copied();
        if pointer.is_none() {
            self.unlearned.borrow_mut().insert(tid);
        }

        pointer
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
    model: &'a dyn runtime_model::RuntimeModel,
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
            snapshot.problems.push(UNREAD_NATIVE.to_owned());
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
        && task.thread.is_none()
        && matches!(
            task.state,
            TaskState::Runnable | TaskState::Blocked | TaskState::Unknown(_)
        )
}

#[cfg(test)]
mod tests {
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

    use std::time::{Duration, Instant};

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
        let live = snapshotter.take_live(reads, &[]);
        child.wait().expect("reap the program");
        assert!(matches!(live, Live::Exited), "{live:?}");
    }
}
