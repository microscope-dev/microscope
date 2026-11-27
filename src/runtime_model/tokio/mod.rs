//! Tokio async runtime model.
//!
//! Discovers active `MultiThread`, `CurrentThread`, and `LocalSet` runtimes
//! from thread-local contexts, walks sharded `OwnedTasks` lists and blocking
//! pools, inspects suspended task futures, and tracks task lifecycle entries.

mod future;
mod layout;
mod sets;
mod tasks;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use layout::{Context, Flavor, Layout, Locals, Owned, Storage, Tasks};

use super::records::{self, Missing};
use super::{
    Crossing, Driven, DynamicValue, Partial, RuntimeException, RuntimeHook, RuntimeImage,
    RuntimeModel, RuntimeSignals, RuntimeStop, StartedTask, StoredValue, TaskContext, TaskEnd,
    TaskEntries, TaskPage, TaskRef, TaskStarter, ThreadActivity,
};

use crate::unwind::{ARGUMENTS, RegisterFile};
use crate::{
    ImageAddress, StackSegment, TaskState, ThreadId, ThreadLocal, TypeReference, VirtualAddress,
};

pub(super) const TASK_NOUN: (&str, &str) = ("task", "tasks");

const VERIFIED: [(u64, u64); 4] = [(1, 51), (1, 52), (1, 53), (1, 54)];

const VERSIONED_SOURCES: [&str; 2] = ["src/runtime/task/raw.rs", "src/runtime/task/core.rs"];

const RUNNING: u64 = 0b1;
const COMPLETE: u64 = 0b10;
const NOTIFIED: u64 = 0b100;
const CANCELLED: u64 = 0b10_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Complete,
    Running,
    Notified,
    Idle,
}

/// Decoded view of Tokio's atomic task state bitfield.
///
/// Precedence matches Tokio's state machine: `COMPLETE` overrides `RUNNING`,
/// which overrides `NOTIFIED`.
#[derive(Debug, Clone, Copy)]
struct TaskWord {
    lifecycle: Lifecycle,
    cancelled: bool,
}

impl TaskWord {
    const fn new(bits: u64) -> Self {
        let lifecycle = match (
            bits & COMPLETE != 0,
            bits & RUNNING != 0,
            bits & NOTIFIED != 0,
        ) {
            (true, ..) => Lifecycle::Complete,
            (false, true, _) => Lifecycle::Running,
            (false, false, true) => Lifecycle::Notified,
            (false, false, false) => Lifecycle::Idle,
        };
        Self {
            lifecycle,
            cancelled: bits & CANCELLED != 0,
        }
    }
}

const MAX_SHARDS: u64 = 1 << 16;
const MAX_TASKS: u64 = 1 << 24;

const POLL: &str = "tokio::runtime::task::raw::poll";

const LAUNCH: [&str; 2] = [
    "multi_thread::worker::Launch>::launch::{closure",
    "multi_thread::worker::{impl#0}::launch::{closure_env",
];

const STARTERS: [(&str, &str, bool); 3] = [
    (
        "bind_inner<alloc::sync::Arc<tokio::runtime::scheduler::multi_thread::handle::Handle, \
         alloc::alloc::Global>>",
        "tokio::runtime::task::list::OwnedTasks<",
        false,
    ),
    (
        "bind_inner<alloc::sync::Arc<tokio::runtime::scheduler::current_thread::Handle, \
         alloc::alloc::Global>>",
        "tokio::runtime::task::list::OwnedTasks<",
        false,
    ),
    ("schedule", "tokio::task::local::Shared", true),
];

pub fn detect(
    image: &Arc<dyn RuntimeImage + Send + Sync>,
) -> Option<Result<Arc<dyn RuntimeModel>, super::Unbound>> {
    let Some(tls) = layout::thread_local_storage(image.as_ref(), layout::CONTEXT_SCOPE) else {
        if image
            .types_named("tokio::runtime::context::Context")
            .is_empty()
        {
            return None;
        }

        return Some(Err(super::Unbound {
            reason: format!(
                "tokio's context is described, but its thread-local storage is not named \
                 {} within {}",
                crate::rust_names::THREAD_LOCAL_STORAGE.join(" or "),
                layout::CONTEXT_SCOPE,
            )
            .into(),
            deferrable: &[],
        }));
    };
    if let Err(reason) = tls {
        return Some(Err(super::Unbound {
            reason: format!("tokio's context cannot be found: {reason}").into(),
            deferrable: &[],
        }));
    }

    Some(Ok(Arc::new(TokioRuntime::bind(Arc::clone(image)))))
}

fn version(path: &std::path::Path, source: &str) -> Option<(u64, u64, Arc<str>)> {
    let mut components = path.components().rev();
    for _ in source.split('/') {
        components.next()?;
    }

    let directory = components.next()?.as_os_str().to_str()?;
    let release = directory.strip_prefix("tokio-")?;
    let mut parts = release.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor, release.into()))
}

#[derive(Debug)]
struct TokioRuntime {
    image: Arc<dyn RuntimeImage + Send + Sync>,
    layout: Layout,

    caveat: Option<Arc<str>>,

    vtables: Mutex<BTreeMap<ImageAddress, Option<Arc<str>>>>,

    futures: Mutex<BTreeMap<ImageAddress, Result<future::FutureLayout, Arc<str>>>>,

    bodies: Mutex<BTreeMap<crate::TypeReference, Option<ImageAddress>>>,

    holds_sets: Mutex<BTreeMap<(crate::TypeReference, bool), bool>>,

    starters: Vec<(ImageAddress, bool)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Instance {
    flavor: Flavor,
    handle: VirtualAddress,
}

#[derive(Debug, Clone, Copy)]
struct ThreadContext {
    thread: ThreadId,
    runtime: Option<Instance>,

    worker: bool,

    entered: bool,

    task: Option<u64>,

    local: Option<VirtualAddress>,
}

#[derive(Debug, Default)]
struct Census {
    threads: Vec<ThreadContext>,
    runtimes: Vec<Instance>,
    gaps: Vec<Arc<str>>,
}

#[derive(Debug, Clone, Copy)]
struct List {
    id: u64,
    shards: VirtualAddress,
    length: u64,
    count: u64,
    counted: bool,
}

/// Pagination cursor packed into a single `u64`.
///
/// Task headers are pointer-aligned (bit 0 is 0), so bit 0 = 1 distinguishes
/// blocking-pool indices from task-header addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cursor {
    Start,
    Node(VirtualAddress),
    Blocking(u64),
}

impl Cursor {
    const fn decode(position: u64) -> Self {
        if position == 0 {
            Self::Start
        } else if position & 1 == 1 {
            Self::Blocking(position >> 1)
        } else {
            Self::Node(VirtualAddress::new(position))
        }
    }

    const fn encode(self) -> u64 {
        match self {
            Self::Start => 0,
            Self::Node(header) => header.get(),
            Self::Blocking(index) => (index << 1) | 1,
        }
    }
}

impl TokioRuntime {
    fn bind(image: Arc<dyn RuntimeImage + Send + Sync>) -> Self {
        let [.., (major, minor)] = VERIFIED;
        let caveat = match VERSIONED_SOURCES.iter().find_map(|source| {
            image
                .source_path_ending(source)
                .map(|path| version(&path, source))
        }) {
            Some(Some((major, minor, _))) if VERIFIED.contains(&(major, minor)) => None,
            Some(Some((.., release))) => Some(format!(
                "tokio {release} is unverified; its runtime is read as tokio {major}.{minor}'s"
            )),
            _ => Some(format!(
                "tokio's version is unknown; its runtime is read as tokio {major}.{minor}'s"
            )),
        };

        let starters = STARTERS
            .iter()
            .flat_map(|&(name, symbol, schedules)| {
                crate::rust_names::with_each_arc(name)
                    .iter()
                    .flat_map(|name| image.function_entries(name))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .filter(|entry| {
                        image
                            .symbol_at(*entry)
                            .is_some_and(|found| found.trim_start_matches('<').starts_with(symbol))
                    })
                    .map(move |entry| (entry, schedules))
                    .collect::<Vec<_>>()
            })
            .collect();

        Self {
            layout: Layout::bind(image.as_ref()),
            starters,
            image,
            caveat: caveat.map(Arc::from),
            vtables: Mutex::new(BTreeMap::new()),
            futures: Mutex::new(BTreeMap::new()),
            bodies: Mutex::new(BTreeMap::new()),
            holds_sets: Mutex::new(BTreeMap::new()),
        }
    }

    fn partial<T>(&self, value: T) -> Partial<T> {
        Partial {
            value,
            gaps: self.caveat.iter().cloned().collect(),
        }
    }

    fn context(&self) -> Result<&Context, Missing> {
        self.layout.context.as_ref().map_err(Arc::clone)
    }

    fn task_layout(&self) -> Result<&Tasks, Missing> {
        self.layout.tasks.as_ref().map_err(Arc::clone)
    }

    fn locals(&self) -> Result<Option<&Locals>, Missing> {
        self.layout
            .locals
            .as_ref()
            .map(Option::as_ref)
            .map_err(Arc::clone)
    }

    fn census(&self, stop: &dyn RuntimeStop) -> Result<Census, Missing> {
        let context = self.context()?;
        let locals = self.locals();
        let mut census = Census::default();
        if let Err(reason) = &locals {
            census
                .gaps
                .push(format!("tasks of local sets cannot be read: {reason}").into());
        }

        let locals = locals.ok().flatten();

        for thread in stop.threads() {
            match Self::thread_context(stop, context, locals, thread) {
                Ok(Some(found)) => {
                    if let Some(runtime) = found.runtime
                        && !census.runtimes.contains(&runtime)
                    {
                        census.runtimes.push(runtime);
                    }

                    census.threads.push(found);
                }
                Ok(None) => {}
                Err(reason) => census
                    .gaps
                    .push(format!("thread {thread}'s tokio context: {reason}").into()),
            }
        }

        let running = census.threads.iter().filter_map(|thread| thread.local);
        let sets = stop.task_sets().into_iter().map(VirtualAddress::new);

        for set in running.chain(sets) {
            let set = Instance {
                flavor: Flavor::Local,
                handle: set,
            };

            if !census.runtimes.contains(&set) {
                census.runtimes.push(set);
            }
        }

        Ok(census)
    }

    fn set_shared(
        stop: &dyn RuntimeStop,
        locals: &Locals,
        context: VirtualAddress,
    ) -> Result<VirtualAddress, Arc<str>> {
        let shared = records::pointer(
            stop,
            context
                .wrapping_add(locals.value)
                .wrapping_add(locals.shared),
        )
        .ok_or_else(|| format!("the local set's context at {context:#x} is unreadable"))?;
        Ok(shared.wrapping_add(locals.data))
    }

    fn running_set(
        stop: &dyn RuntimeStop,
        locals: &Locals,
        thread: ThreadId,
    ) -> Result<Option<VirtualAddress>, Arc<str>> {
        let Some(base) = alive(stop, &locals.storage, thread, "the local set context")? else {
            return Ok(None);
        };

        let unreadable =
            || Arc::<str>::from(format!("the local set context at {base:#x} is unreadable"));
        let context =
            records::pointer(stop, base.wrapping_add(locals.context)).ok_or_else(unreadable)?;
        if context == records::NULL {
            return Ok(None);
        }

        Self::set_shared(stop, locals, context).map(Some)
    }

    fn thread_context(
        stop: &dyn RuntimeStop,
        context: &Context,
        locals: Option<&Locals>,
        thread: ThreadId,
    ) -> Result<Option<ThreadContext>, Arc<str>> {
        let Some(base) = alive(stop, &context.storage, thread, "the context")? else {
            return Ok(None);
        };

        let unreadable = || Arc::<str>::from(format!("the context at {base:#x} is unreadable"));

        let option = base.wrapping_add(context.handle.offset);
        let runtime = if &*context.handle_option.active(stop, option)?.name == "Some" {
            let handle = option.wrapping_add(context.handle_some);
            let flavor = context.flavors.active(stop, handle)?;
            let (flavor, _, layout) = context
                .runtimes
                .iter()
                .find(|(_, name, _)| *name == flavor.name)
                .ok_or_else(|| format!("the runtime's flavor {} is unknown", flavor.name))?;
            let arc =
                records::pointer(stop, handle.wrapping_add(layout.arc)).ok_or_else(unreadable)?;
            Some(Instance {
                flavor: *flavor,
                handle: arc.wrapping_add(layout.data),
            })
        } else {
            None
        };

        let worker =
            records::word(stop, base.wrapping_add(context.scheduler)).ok_or_else(unreadable)? != 0;

        let entered = &*context
            .entered_state
            .active(stop, base.wrapping_add(context.entered.offset))?
            .name
            == "Entered";

        let option = base.wrapping_add(context.task.offset);
        let task = if &*context.task_option.active(stop, option)?.name == "Some" {
            Some(
                records::word(stop, option.wrapping_add(context.task_some))
                    .ok_or_else(unreadable)?,
            )
        } else {
            None
        };

        let local = match locals {
            Some(locals) => Self::running_set(stop, locals, thread)?,
            None => None,
        };

        Ok(Some(ThreadContext {
            thread,
            runtime,
            worker,
            entered,
            task,
            local,
        }))
    }

    fn owned(&self, flavor: Flavor) -> Result<&Owned, Missing> {
        self.context()?
            .runtimes
            .iter()
            .find(|(known, ..)| *known == flavor)
            .map(|(.., runtime)| &runtime.owned)
            .ok_or_else(|| format!("the {} is unknown", flavor.describe()).into())
    }

    fn list(&self, stop: &dyn RuntimeStop, runtime: Instance) -> Result<List, Arc<str>> {
        if runtime.flavor == Flavor::Local {
            let locals = self.locals()?.ok_or("the program describes no local set")?;
            let at = runtime.handle;
            let id = records::word(stop, at.wrapping_add(locals.id))
                .filter(|id| *id != 0)
                .ok_or_else(|| format!("the local set at {at:#x} has no task list"))?;
            return Ok(List {
                id,
                shards: at.wrapping_add(locals.head),
                length: 1,
                count: MAX_TASKS,
                counted: false,
            });
        }

        let owned = self.owned(runtime.flavor)?;
        let at = runtime.handle.wrapping_add(owned.at);
        let unreadable = || Arc::<str>::from(format!("the task list at {at:#x} is unreadable"));
        let word =
            |offset: u64| records::word(stop, at.wrapping_add(offset)).ok_or_else(unreadable);
        let list = List {
            id: word(owned.id)?,
            shards: VirtualAddress::new(word(owned.shards)?),
            length: word(owned.shards + 8)?,
            count: word(owned.count)?,
            counted: true,
        };

        let mask = word(owned.mask)?;
        if !list.length.is_power_of_two()
            || list.length > MAX_SHARDS
            || mask != list.length - 1
            || list.count > MAX_TASKS
        {
            return Err(format!(
                "the task list at {at:#x} has {} shards, mask {mask:#x}, and {} tasks",
                list.length, list.count
            )
            .into());
        }

        Ok(list)
    }

    /// Verifies that a task vtable's `poll` entry resolves to
    /// `tokio::runtime::task::raw::poll` before trusting offsets read from it,
    /// guarding against freed or concurrently mutated task nodes.
    fn checked_vtable(
        &self,
        stop: &dyn RuntimeStop,
        vtable: VirtualAddress,
    ) -> Result<(), Arc<str>> {
        let tasks = self.task_layout()?;
        let poll = records::pointer(stop, vtable.wrapping_add(tasks.poll))
            .ok_or_else(|| format!("the vtable at {vtable:#x} is unreadable"))?;
        let image = stop.load_bias().wrapping_unload(poll);
        let verdict = self
            .vtables
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(image)
            .or_insert_with(|| {
                let name = self.image.symbol_at(image);
                match name {
                    Some(name)
                        if name
                            .strip_prefix(POLL)
                            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::<")) =>
                    {
                        None
                    }
                    _ => Some(format!("the vtable at {vtable:#x} polls no task").into()),
                }
            })
            .clone();
        verdict.map_or(Ok(()), Err)
    }

    fn is_launch(&self, stop: &dyn RuntimeStop, vtable: VirtualAddress) -> bool {
        let Ok(tasks) = self.task_layout() else {
            return false;
        };

        let Some(poll) = records::pointer(stop, vtable.wrapping_add(tasks.poll)) else {
            return false;
        };

        let poll = stop.load_bias().wrapping_unload(poll);

        [self.image.symbol_at(poll), self.image.function_name(poll)]
            .into_iter()
            .flatten()
            .any(|name| LAUNCH.iter().any(|launch| name.contains(launch)))
    }
}

fn thread_local(
    stop: &dyn RuntimeStop,
    tls: &ThreadLocal,
    thread: ThreadId,
) -> Result<VirtualAddress, Arc<str>> {
    stop.thread_local_address(thread, *tls)
}

fn alive(
    stop: &dyn RuntimeStop,
    storage: &Storage,
    thread: ThreadId,
    what: &str,
) -> Result<Option<VirtualAddress>, Arc<str>> {
    let base = thread_local(stop, &storage.tls, thread)?;
    let at = base.wrapping_add(storage.state);
    let state = records::read(stop, at, 1)
        .ok_or_else(|| format!("the state of {what} at {at:#x} is unreadable"))?;
    Ok((state == storage.alive).then_some(base))
}

impl RuntimeModel for TokioRuntime {
    fn tasks(
        &self,
        stop: &dyn RuntimeStop,
        start: u64,
        limit: usize,
        program_only: bool,
    ) -> Partial<TaskPage> {
        let _span = crate::span!("tokio tasks");
        let mut page = self.partial(TaskPage {
            tasks: Vec::new(),
            next: None,
        });
        match self.census(stop) {
            Ok(census) => self.page(
                stop,
                &census,
                Cursor::decode(start),
                limit,
                program_only,
                &mut page,
            ),
            Err(reason) => page
                .gaps
                .push(format!("tokio's tasks cannot be read: {reason}").into()),
        }

        for task in page.value.tasks.iter_mut().filter(|task| task.locator != 0) {
            task.spawned = self.spawned_at(stop, VirtualAddress::new(task.locator));
        }

        page
    }

    fn thread_activity(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> ThreadActivity {
        match self.activity(stop, thread) {
            Ok(activity) => activity,
            Err(reason) => ThreadActivity::Unknown(
                format!("what the thread does for tokio cannot be read: {reason}").into(),
            ),
        }
    }

    fn current_task(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<Option<u64>, Arc<str>> {
        let context = self.context()?;
        Ok(Self::thread_context(stop, context, None, thread)?.and_then(|found| found.task))
    }

    fn task_context(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>> {
        let census = self.census(stop)?;
        let Some(found) = self.find(stop, &census, task)? else {
            return Ok(None);
        };

        match (found.state, found.thread) {
            (TaskState::Running, Some(thread)) => Ok(Some(TaskContext::OnThread(thread))),
            (TaskState::Running, None) => {
                Err(format!("the thread running task {} is unknown", task.number).into())
            }
            (TaskState::Unknown(reason), _) => Err(reason),
            _ => {
                let (future, ty) = self.future(stop, VirtualAddress::new(found.locator))?;
                Ok(Some(TaskContext::Suspended { future, ty }))
            }
        }
    }

    fn running_future(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<Option<(VirtualAddress, TypeReference)>, Arc<str>> {
        let Some(number) = self.current_task(stop, thread)? else {
            return Ok(None);
        };

        let census = self.census(stop)?;
        let task = TaskRef {
            number,
            locator: None,
        };
        let Some(found) = self.find(stop, &census, task)? else {
            return Ok(None);
        };

        self.future(stop, VirtualAddress::new(found.locator))
            .map(Some)
    }

    fn thread_stacks(
        &self,
        _stop: &dyn RuntimeStop,
        _thread: ThreadId,
    ) -> Result<Vec<(std::ops::Range<u64>, StackSegment)>, Arc<str>> {
        Ok(Vec::new())
    }

    fn cross(
        &self,
        _stop: &dyn RuntimeStop,
        _thread: ThreadId,
        _frame: &RegisterFile,
        _after_call: bool,
    ) -> Result<Crossing, Arc<str>> {
        Ok(Crossing::Stay)
    }

    fn signals(&self) -> RuntimeSignals {
        RuntimeSignals::default()
    }

    fn hooks(&self) -> &[RuntimeHook] {
        &[]
    }

    fn exception(
        &self,
        _stop: &dyn RuntimeStop,
        _hook: ImageAddress,
        _registers: &RegisterFile,
    ) -> Result<RuntimeException, Arc<str>> {
        Err("tokio reports no exception of its own".into())
    }

    fn dynamic_value(
        &self,
        _stop: &dyn RuntimeStop,
        _representation: &str,
        _value: StoredValue<'_>,
    ) -> Option<Result<DynamicValue, Arc<str>>> {
        None
    }

    fn stack_mover(&self) -> Option<ImageAddress> {
        None
    }

    fn moving_task(
        &self,
        _stop: &dyn RuntimeStop,
        _registers: &RegisterFile,
    ) -> Result<u64, Arc<str>> {
        Err("tokio moves no task's stack".into())
    }

    fn task_stack(
        &self,
        _stop: &dyn RuntimeStop,
        _task: TaskRef,
    ) -> Result<Option<std::ops::Range<u64>>, Arc<str>> {
        Ok(None)
    }

    fn call_out(
        &self,
        _entry: ImageAddress,
        _registers: &RegisterFile,
    ) -> Option<Result<VirtualAddress, Arc<str>>> {
        None
    }

    fn task_starters(&self) -> Vec<TaskStarter> {
        self.starters
            .iter()
            .map(|&(entry, _)| TaskStarter {
                entry,
                names_at_entry: true,
            })
            .collect()
    }

    fn started_task(
        &self,
        stop: &dyn RuntimeStop,
        starter: ImageAddress,
        registers: &RegisterFile,
    ) -> Result<Option<StartedTask>, Arc<str>> {
        let &(_, schedules) = self
            .starters
            .iter()
            .find(|(entry, _)| *entry == starter)
            .ok_or("tokio starts no task there")?;
        let header = registers
            .get(ARGUMENTS[1])
            .map(VirtualAddress::new)
            .ok_or("the task being started is unavailable")?;
        self.started(stop, header, schedules)
    }

    fn task_noun(&self) -> &'static str {
        TASK_NOUN.0
    }

    fn held_task_sets(
        &self,
        stop: &dyn RuntimeStop,
        driven: &super::DrivenFuture,
    ) -> Partial<Vec<u64>> {
        self.held_sets(stop, driven)
    }

    fn may_run_task_set(&self, ty: crate::TypeReference) -> bool {
        self.has_task_sets() && self.may_hold(ty, false)
    }

    fn has_task_sets(&self) -> bool {
        self.locals().is_ok_and(|locals| locals.is_some())
    }

    fn task_entries(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<TaskEntries>, Arc<str>> {
        let tasks = self.task_layout()?;
        let census = self.census(stop)?;
        let Some(found) = self.find(stop, &census, task)? else {
            return Ok(None);
        };

        let header = VirtualAddress::new(found.locator);
        if header == records::NULL {
            return Ok(None);
        }

        let unreadable = || Arc::<str>::from(format!("task {} is unreadable", task.number));
        let vtable =
            records::pointer(stop, header.wrapping_add(tasks.vtable)).ok_or_else(unreadable)?;
        let entry = |offset: u64| {
            records::pointer(stop, vtable.wrapping_add(offset)).ok_or_else(unreadable)
        };

        let (poll, shutdown) = (entry(tasks.poll)?, entry(tasks.shutdown)?);

        let frees = self
            .image
            .function_name(stop.load_bias().wrapping_unload(poll))
            .and_then(|name| {
                let arguments = name.strip_prefix("poll")?.to_owned();
                let boxed = format!(
                    "alloc::boxed::Box<tokio::runtime::task::core::Cell{arguments}, \
                     alloc::alloc::Global>"
                );
                Some(
                    crate::rust_names::drop_glue_of(&boxed)
                        .iter()
                        .flat_map(|glue| self.image.function_entries(glue))
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();

        Ok(Some(TaskEntries {
            task: TaskRef {
                number: task.number,
                locator: Some(header.get()),
            },
            runs: vec![poll, shutdown],
            frees: frees
                .into_iter()
                .map(|entry| stop.load_bias().wrapping_load(entry))
                .collect(),
        }))
    }

    fn takes_up(
        &self,
        stop: &dyn RuntimeStop,
        task: &TaskEntries,
        entry: VirtualAddress,
        registers: &RegisterFile,
    ) -> bool {
        let (Some(header), Some(argument)) = (task.task.locator, registers.get(ARGUMENTS[0]))
        else {
            return false;
        };

        if task.frees.contains(&entry) {
            records::word(stop, VirtualAddress::new(argument)) == Some(header)
        } else {
            argument == header
        }
    }

    fn task_end(
        &self,
        stop: &dyn RuntimeStop,
        task: &TaskEntries,
    ) -> Result<Option<TaskEnd>, Arc<str>> {
        let tasks = self.task_layout()?;
        let header = task
            .task
            .locator
            .map(VirtualAddress::new)
            .ok_or("the task is not located")?;
        let unreadable = || Arc::<str>::from(format!("task {} is unreadable", task.task.number));
        let word = |address: VirtualAddress| records::word(stop, address).ok_or_else(unreadable);

        let vtable = VirtualAddress::new(word(header.wrapping_add(tasks.vtable))?);
        let id = word(header.wrapping_add(word(vtable.wrapping_add(tasks.id_offset))?))?;
        if task.runs.first().map(|poll| poll.get()) != Some(word(vtable.wrapping_add(tasks.poll))?)
            || id != task.task.number
        {
            return Err(format!("task {} is no longer where it was", task.task.number).into());
        }

        let state = TaskWord::new(word(header.wrapping_add(tasks.state))?);
        Ok(match (state.cancelled, state.lifecycle) {
            (true, _) => Some(TaskEnd::Cancelled),
            (false, Lifecycle::Complete) => Some(TaskEnd::Finished),
            (false, Lifecycle::Running | Lifecycle::Notified | Lifecycle::Idle) => None,
        })
    }

    fn driver_role(&self) -> Option<crate::CodeRole> {
        Some(crate::CodeRole::RuntimeInternal)
    }

    fn driven(
        &self,
        frame: &crate::StackFrame,
        _inner: Option<&crate::StackFrame>,
    ) -> Option<Driven> {
        future::driven_future(frame).map(|variable| Driven::Future { variable })
    }

    fn polling(&self, frame: &crate::StackFrame) -> Option<&'static str> {
        future::polled_future(frame)
    }
}

fn local_type(info: &crate::TypeInfo) -> Option<&str> {
    let identity = info.identity.as_ref()?;
    identity
        .path
        .iter()
        .map(AsRef::as_ref)
        .eq(["tokio", "task", "local"])
        .then_some(&*identity.base)
}

fn holds_pointee(info: &crate::TypeInfo) -> bool {
    info.identity.as_ref().is_some_and(|identity| {
        let path = identity.path.iter().map(AsRef::as_ref);
        match &*identity.base {
            "Box" => path.eq(["alloc", "boxed"]),
            "Pin" => path.eq(["core", "pin"]),
            _ => false,
        }
    })
}

/// Static DWARF reachability check for whether `ty` can transitively hold a
/// `LocalSet`, `RunUntil`, or `dyn Future` (which might erase one).
///
/// Used by `sets` to prune irrelevant fields before reading target memory.
fn may_hold_set(image: &dyn RuntimeImage, ty: crate::TypeReference, boxed: bool) -> bool {
    use crate::TypeKind;
    fn members(
        fields: &[crate::RecordMember],
        boxed: bool,
    ) -> impl Iterator<Item = (crate::TypeReference, bool)> + '_ {
        fields.iter().map(move |member| (member.type_ref, boxed))
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut pending = vec![(ty, boxed)];
    while let Some((ty, boxed)) = pending.pop() {
        if !seen.insert((ty.id, boxed)) {
            continue;
        }

        let Some(info) = image.type_info(ty) else {
            continue;
        };

        if matches!(local_type(info), Some("LocalSet" | "RunUntil")) {
            return true;
        }

        match &info.kind {
            TypeKind::Pointer {
                target: Some(target),
                ..
            }
            | TypeKind::Reference { target, .. } => {
                let Some(pointee) = image.type_info(*target) else {
                    continue;
                };

                if super::futures::trait_object(&pointee.name)
                    .is_some_and(|name| name.starts_with("dyn core::future::future::Future"))
                    || local_type(pointee) == Some("LocalSet")
                {
                    return true;
                }

                if boxed {
                    pending.push((*target, false));
                }
            }

            TypeKind::Record {
                members: fields,
                bases,
                ..
            } => {
                let boxed = boxed || holds_pointee(info);
                pending.extend(members(fields, boxed));
                pending.extend(bases.iter().map(|base| (base.type_ref, boxed)));
            }

            TypeKind::Union {
                members: fields, ..
            } => pending.extend(members(fields, boxed)),
            TypeKind::Variant {
                common_members,
                bases,
                variants,
                ..
            } => {
                pending.extend(members(common_members, boxed));
                pending.extend(bases.iter().map(|base| (base.type_ref, boxed)));
                for variant in variants.iter() {
                    pending.extend(members(&variant.members, boxed));
                }
            }
            TypeKind::Array { element, .. } => pending.push((*element, boxed)),
            TypeKind::Modified { target, .. }
            | TypeKind::Named {
                target: Some(target),
                ..
            } => pending.push((*target, boxed)),
            _ => {}
        }
    }

    false
}

#[cfg(test)]
mod tests;
