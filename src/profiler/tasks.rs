//! Periodic task and thread snapshotter that briefly observes a running process
//! to capture async/goroutine wait stacks and thread labels.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use super::RecordError;
use crate::capture::builder::InterpretedPlace;
use crate::capture::records::{Truncation, role_number};
use crate::debug_info::DebugInfo;
use crate::runtime_model::{self, RuntimeStop};
use crate::{
    Debugger, DebuggerHandle, ExecutionContext, FrameKind, ModuleId, ModuleImage, ProcessId,
    Program, StackFrameId, StopContext, StopId, TaskSnapshot, TaskState, TypeInfo, TypeReference,
    UnwindTermination,
};

const MOST_TASKS: usize = 10_000;

const UNREADABLE_FRAME: &str = "[an interpreted frame whose code could not be read]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub noun: &'static str,
    pub number: u64,

    pub state: String,

    pub labels: Option<String>,

    pub frames: Vec<WaitFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitFrame {
    Code { address: u64, role: u8 },

    Awaited(String),

    Interpreted(InterpretedWait),

    Truncated(Truncation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretedWait {
    pub function: Arc<str>,
    pub file: Option<Arc<str>>,
    pub line: u32,

    pub first_line: Option<u32>,
    pub declared_in: Option<Arc<str>>,
}

impl InterpretedWait {
    pub(super) fn of(recorded: Option<&crate::RecordedFrame>) -> Self {
        recorded.map_or_else(Self::unreadable, |recorded| Self {
            function: Arc::clone(&recorded.function),
            file: recorded.place.as_ref().map(|place| Arc::clone(&place.path)),
            line: recorded.place.as_ref().map_or(0, |place| place.line),
            first_line: recorded.first_line,
            declared_in: recorded.declared_in.clone(),
        })
    }

    pub fn of_frame(frame: &runtime_model::InterpretedFrame) -> Self {
        frame
            .function
            .as_ref()
            .map_or_else(Self::unreadable, |function| Self {
                function: Arc::clone(function),
                file: frame.place.as_ref().map(|place| Arc::clone(&place.path)),
                line: frame.place.as_ref().map_or(0, |place| place.line),
                first_line: frame.first_line,
                declared_in: None,
            })
    }

    fn unreadable() -> Self {
        Self {
            function: UNREADABLE_FRAME.into(),
            file: None,
            line: 0,
            first_line: None,
            declared_in: None,
        }
    }

    pub fn place(&self) -> InterpretedPlace<'_> {
        InterpretedPlace {
            function: &self.function,
            file: self.file.as_deref(),
            line: self.line,
            declared: self
                .file
                .as_deref()
                .or(self.declared_in.as_deref())
                .zip(self.first_line),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub object: u64,
    pub noun: &'static str,
    pub number: u64,
    pub labels: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reads {
    pub tasks: bool,

    pub threads: bool,
}

pub type Labels = Vec<(Arc<str>, Arc<str>)>;

pub const THREAD: &str = "thread";

const SAMPLED: &str = "sampled";

const NOT_ASLEEP: &str = "it was not asleep in a system call";

const UNREAD_LIVE: &str = "frames not read live: ";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub located: Vec<Located>,
    pub waiting: Vec<Waiting>,

    pub problems: Vec<String>,

    pub notes: Vec<String>,

    pub changed: u64,

    pub changed_threads: Vec<u64>,

    pub thread_labels: Vec<(u32, Labels)>,

    pub unmapped_code: Vec<u64>,
}

impl Snapshot {
    fn note_empty_tasks(&mut self, seen: usize) {
        if seen == 0 && self.problems.is_empty() && self.notes.is_empty() {
            self.notes.push(
                "no tasks were discovered; the program may have none, or its runtime may be \
                 unsupported or unreachable at this snapshot"
                    .to_owned(),
            );
        }
    }
}

pub struct Snapshotter {
    pid: u32,
    program: Program,

    thread_pointers: BTreeMap<(u32, u64), u64>,

    runtimes: BTreeMap<crate::ModuleImageId, Vec<Detected>>,

    tls: crate::backend::RunningTls,

    kept: RefCell<BTreeMap<(crate::ModuleImageId, u32, u64), Kept>>,

    files: ThreadsFiles,
}

type ThreadsFiles = BTreeMap<u32, crate::backend::ThreadFiles>;

const MOST_KEPT_THREADS: usize = 256;

const EXITED: &str = "it exited";

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

#[derive(Debug)]
struct Kept {
    switches: (u64, u64),
    asleep: crate::backend::Asleep,
    frames: Vec<runtime_model::InterpretedFrame>,
}

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

fn quiet_state(
    thread: &impl ThreadState,
) -> Result<(crate::backend::Asleep, (u64, u64)), Arc<str>> {
    let io = |error: std::io::Error| Arc::<str>::from(error.to_string());
    let asleep = thread.asleep().map_err(io)?.ok_or(NOT_ASLEEP)?;
    let switches = thread.switches().map_err(io)?;
    Ok((asleep, switches))
}

type Detected = Result<Arc<dyn runtime_model::RuntimeModel>, runtime_model::Unbound>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedCode {
    pub start: u64,
    pub end: u64,
    pub image: crate::ModuleImageId,
    pub bias: crate::LoadBias,
    pub generated: bool,
}

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
    pub fn new(
        pid: u32,
        program: &Path,
        debug_files: &crate::DebugFileOptions,
    ) -> Result<Self, RecordError> {
        let _span = crate::span!("task snapshot load");
        let program = Program::load(program, debug_files)
            .map_err(|error| RecordError::Program(error.to_string()))?
            .keeping_modules();
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

        let detached = debugger.shutdown().await;
        let read = read?;
        detached?;
        Ok(read)
    }
}

fn thread_start(pid: u32, tid: u32) -> Option<u64> {
    crate::backend::thread_start_time(crate::model::Pid::new(pid), crate::model::Tid::new(tid))
}

async fn read(
    handle: &DebuggerHandle,
    stop: StopId,
    reads: Reads,
) -> Result<Snapshot, crate::Error> {
    let mut snapshot = Snapshot::default();
    if reads.tasks {
        read_tasks(handle, stop, &mut snapshot, !reads.threads).await?;
    }

    if reads.threads {
        read_threads(handle, stop, &mut snapshot).await?;
    }

    Ok(snapshot)
}

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

async fn read_tasks(
    handle: &DebuggerHandle,
    stop: StopId,
    snapshot: &mut Snapshot,
    alone: bool,
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
            None => {
                if alone {
                    snapshot.note_empty_tasks(seen);
                }
                return Ok(());
            }
        }
    }
}

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

fn label_text(labels: &[(Arc<str>, Arc<str>)]) -> Option<String> {
    (!labels.is_empty()).then(|| {
        labels
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn frames(backtrace: &crate::Backtrace, futures: Vec<String>) -> Vec<WaitFrame> {
    let mut futures = futures.into_iter();
    let mut frames = Vec::with_capacity(backtrace.frames.len() + 1);

    let mut exact = true;

    for frame in backtrace.frames.iter() {
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

const LIVE_STACK_BYTES: u64 = 1 << 20;

const LIVE_FRAMES: usize = 512;

#[derive(Debug)]
pub enum Live {
    Taken(Snapshot),

    Exited,
}

impl Snapshotter {
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
        let mut seen = 0;

        for (loaded, info) in modules.iter().filter(|(loaded, _)| loaded.window.is_none()) {
            let stop = LiveStop {
                pid: self.pid,
                bias: loaded.load_bias,
                threads: threads.clone(),
                pointers: &pointers,
                tls: Some(&self.tls),
                unlearned: RefCell::new(BTreeSet::new()),
                blocks: None,
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
                            seen += live.read(&mut snapshot);
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

        if reads.tasks && !reads.threads {
            snapshot.note_empty_tasks(seen);
        }

        if exited(self.pid) {
            Live::Exited
        } else {
            Live::Taken(snapshot)
        }
    }

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
                blocks: None,
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

        self.thread_pointers.clear();

        for (&tid, &start) in started {
            if let Some(pointer) = listed.get(&tid) {
                self.thread_pointers.insert((tid, start), *pointer);
            }
        }

        Ok(())
    }
}

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

pub fn exited(pid: u32) -> bool {
    crate::backend::process_exited(crate::model::Pid::new(pid))
}

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

pub struct LiveStop<'a> {
    pid: u32,
    bias: crate::LoadBias,
    threads: Vec<crate::ThreadId>,
    pointers: &'a BTreeMap<u32, u64>,

    tls: Option<&'a crate::backend::RunningTls>,

    unlearned: RefCell<BTreeSet<u32>>,

    blocks: Option<&'a crate::runtime_model::StopMemory>,
}

static NO_POINTERS: BTreeMap<u32, u64> = BTreeMap::new();

impl LiveStop<'static> {
    pub(super) fn for_threads(pid: u32, bias: crate::LoadBias) -> Self {
        Self {
            threads: threads(pid),
            ..Self::memory(pid, bias)
        }
    }

    pub fn for_thread(pid: u32, tid: u32, bias: crate::LoadBias) -> Self {
        Self {
            threads: vec![crate::ThreadId::new(u64::from(tid))],
            ..Self::memory(pid, bias)
        }
    }

    pub fn memory(pid: u32, bias: crate::LoadBias) -> Self {
        Self {
            pid,
            bias,
            threads: Vec::new(),
            pointers: &NO_POINTERS,
            tls: None,
            unlearned: RefCell::new(BTreeSet::new()),
            blocks: None,
        }
    }
}

impl<'a> LiveStop<'a> {
    pub fn through<'b>(self, blocks: &'b crate::runtime_model::StopMemory) -> LiveStop<'b>
    where
        'a: 'b,
    {
        let stop: LiveStop<'b> = self;
        LiveStop {
            blocks: Some(blocks),
            ..stop
        }
    }

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
            blocks: None,
        }
    }
}

impl LiveStop<'_> {
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
    fn runs(&self) -> bool {
        true
    }

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

    fn memory(&self) -> Option<&crate::runtime_model::StopMemory> {
        self.blocks
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

struct LivePages<'a> {
    stop: &'a LiveStop<'a>,

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
            *byte = *self.page(page)?.get(usize::try_from(at - page).ok()?)?;
        }

        Some(u64::from_le_bytes(word))
    }
}

struct LiveRuntime<'a> {
    image: &'a Arc<ModuleImage>,
    stop: &'a LiveStop<'a>,
    modules: &'a [crate::unwind::UnwindModule<'a>],
    code: &'a [MappedCode],
    model: &'a dyn runtime_model::RuntimeModel,

    starts: &'a BTreeMap<u32, u64>,
    kept: &'a RefCell<BTreeMap<(crate::ModuleImageId, u32, u64), Kept>>,

    files: &'a ThreadsFiles,
}

impl LiveRuntime<'_> {
    fn read(&self, snapshot: &mut Snapshot) -> usize {
        let noun = self.model.task_noun();
        let mut start = 0;
        let mut seen = 0;
        loop {
            let page = self.model.tasks(self.stop, start, 256, false);
            snapshot
                .problems
                .extend(page.gaps.iter().map(ToString::to_string));
            seen += page.value.tasks.len();

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
                    return seen;
                }
                None => return seen,
            }
        }
    }

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
            if !stacks.value.whole(thread) {
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

    /// Walks a running thread's interpreted frames without stopping it,
    /// bracketing the walk with `/proc/<pid>/task/<tid>/syscall` and
    /// context-switch counts so torn reads are discarded.
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

    /// Unwinds a parked task optimistically without stopping the process,
    /// re-reading touched 4 KiB stack pages or double-walking future chains
    /// and rechecking task context to reject torn reads.
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

    fn parked(
        &self,
        registers: &crate::unwind::RegisterFile,
        after_call: bool,
    ) -> Option<(Vec<WaitFrame>, LivePages<'_>)> {
        let mut pages = LivePages {
            stop: self.stop,
            start: registers.get(crate::unwind::STACK_POINTER)?,
            pages: BTreeMap::new(),
        };

        let registers: Vec<_> = registers.iter().collect();
        let (unwound, end) =
            crate::unwind::sample::unwind_memory(self.modules, &registers, &mut pages, LIVE_FRAMES);
        let mut frames: Vec<WaitFrame> = unwound
            .iter()
            .enumerate()
            .map(|(index, frame)| WaitFrame::Code {
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

    #[test]
    fn empty_discovery_preserves_specific_problems_and_counts_unlocated_tasks() {
        let mut snapshot = super::Snapshot::default();
        snapshot.note_empty_tasks(1);
        assert!(
            snapshot.notes.is_empty(),
            "a running task may have no object or wait"
        );

        snapshot
            .problems
            .push("tokio's context is unreadable".into());
        snapshot.note_empty_tasks(0);
        assert_eq!(snapshot.problems, ["tokio's context is unreadable"]);
        assert!(snapshot.notes.is_empty());

        snapshot.problems.clear();
        snapshot.note_empty_tasks(0);
        snapshot.note_empty_tasks(0);
        assert!(snapshot.problems.is_empty());
        assert_eq!(snapshot.notes.len(), 1);
        assert!(snapshot.notes[0].contains("no tasks were discovered"));
    }

    #[test]
    fn a_live_task_snapshot_without_a_discoverable_runtime_says_so() {
        let program =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/basic");
        let info = crate::debug_info::load_module(
            &program,
            crate::ModuleImageId::new(0),
            &crate::debug_info::DebugFileSearch::default(),
        )
        .unwrap();
        let loaded = crate::LoadedModule {
            id: crate::ModuleId::new(0),
            image: info.image.id(),
            load_bias: crate::LoadBias::default(),
            window: None,
        };
        let modules = [(loaded, info)];
        let mut snapshotter = super::Snapshotter::new(
            std::process::id(),
            &program,
            &crate::DebugFileOptions::default(),
        )
        .unwrap();
        for tasks in [false, true] {
            let super::Live::Taken(snapshot) = snapshotter.take_live(
                super::Reads {
                    tasks,
                    threads: false,
                },
                &modules,
                &[],
            ) else {
                panic!("the test process is alive");
            };
            assert!(snapshot.located.is_empty());
            assert!(snapshot.waiting.is_empty());
            assert!(snapshot.problems.is_empty(), "{snapshot:?}");
            assert_eq!(snapshot.notes.len(), usize::from(tasks), "{snapshot:?}");
            if tasks {
                assert!(snapshot.notes[0].contains("no tasks were discovered"));
            }
        }
    }

    #[test]
    fn a_stop_read_through_blocks_keeps_what_it_read() {
        use crate::runtime_model::{RuntimeStop, StopMemory};
        let mut buffer = vec![1_u8; 8192];
        let at = buffer.as_ptr() as u64 + 100;
        let pid = std::process::id();
        let bias = crate::LoadBias::default();
        let read = |stop: &super::LiveStop<'_>| {
            let mut byte = [0];
            match stop.memory() {
                Some(blocks) => {
                    let copied = blocks
                        .copy(at, &mut byte, |start, block| {
                            Ok::<_, ()>(stop.read(start, block))
                        })
                        .expect("no error");
                    assert_eq!(copied, Some(1));
                }
                None => assert!(stop.read(crate::VirtualAddress::new(at), &mut byte)),
            }
            byte[0]
        };

        let blocks = StopMemory::with_block(4096);
        let through = super::LiveStop::memory(pid, bias).through(&blocks);
        assert_eq!(read(&through), 1);
        buffer[100] = 2;
        std::hint::black_box(&buffer);
        assert_eq!(read(&through), 1, "kept from the first read");
        assert_eq!(read(&super::LiveStop::memory(pid, bias)), 2);
        let afresh = StopMemory::with_block(4096);
        assert_eq!(
            read(&super::LiveStop::memory(pid, bias).through(&afresh)),
            2
        );
    }

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

    #[test]
    fn a_thread_that_ran_between_the_checks_reads_is_seen_to_have_run() {
        use std::cell::Cell;

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

    #[test]
    fn the_files_of_a_thread_that_exited_are_dropped_and_opened_again() {
        let pid = std::process::id();
        let (thread, writer, tid) = blocked();
        let mut files = super::ThreadsFiles::new();
        super::keep_files(&mut files, pid, &[tid], super::MOST_KEPT_THREADS);
        assert!(files[&tid].switches().is_ok());

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

        super::keep_files(&mut files, pid, &[tid], super::MOST_KEPT_THREADS);
        assert!(!files.contains_key(&tid), "kept the files of a thread gone");

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

    fn zombie(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| Some(stat[stat.rfind(')')? + 2..].starts_with('Z')))
            .unwrap_or(false)
    }

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
