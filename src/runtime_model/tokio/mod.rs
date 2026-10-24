//! tokio's runtime: tasks are the futures its schedulers poll, each in a
//! heap cell whose header begins with its state and vtable.
//!
//! No list names every runtime, so the model finds them through the
//! threads that entered one: each thread's `CONTEXT` holds the handle of
//! its runtime, its scheduler when it is a worker, and the task it polls.
//! A runtime's spawned tasks hang from its `OwnedTasks`, sharded linked
//! lists through each task's trailer; its blocking pool queues the
//! closures no thread runs yet. A `LocalSet` keeps its tasks in a list of
//! its own, which a thread's `CURRENT` names while the thread runs the
//! set, and which the future a thread drives names while it waits. Every
//! node is checked before it is read, so a corrupted list is reported,
//! never followed.

mod future;
mod layout;
mod tasks;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use layout::{Context, Flavor, Layout, Locals, Owned, Tasks};

use super::records::{self, Missing};
use super::{
    Crossing, Driven, DynamicValue, Partial, RuntimeException, RuntimeHook, RuntimeImage,
    RuntimeModel, RuntimeSignals, RuntimeStop, StartedTask, StoredValue, TaskContext, TaskEnd,
    TaskEntries, TaskPage, TaskRef, TaskStarter, ThreadActivity,
};

use crate::unwind::{RDI, RSI, RegisterFile};
use crate::{ImageAddress, StackSegment, TaskState, ThreadId, ThreadLocal, VirtualAddress};

/// What tokio calls its tasks.
pub(super) const TASK_NOUN: (&str, &str) = ("task", "tasks");

/// The releases the contract was checked against, oldest first. Another is
/// read as the newest of them wherever its debug information binds, and
/// says it is unverified.
///
/// The test fixtures build with tokio 1.52 and 1.53 on every run. 1.51 and
/// 1.54 were checked by hand, by building the fixtures against them and
/// running the tokio suite (`dev/runtime-models.md` says how); 1.54 moved
/// the blocking pool's queue, which the layout binds in either shape. (One
/// 1.54 check fails by the fixture alone: its optimized build puts the
/// breakpoint of the line before a spawn after the spawn's call, so the
/// task exists before `step task` begins.)
/// tokio 1.47 binds too, but its optimized steps over `yield_now` were not
/// all found, so it stays unverified.
const VERIFIED: [(u64, u64); 4] = [(1, 51), (1, 52), (1, 53), (1, 54)];

/// A source file every build of tokio compiles, by which its version is
/// read from the directory Cargo unpacked it in.
const VERSIONED_SOURCE: &str = "src/runtime/task/raw.rs";

/// The task state's bits (`runtime/task/state.rs`), which tokio keeps in
/// constants its debug information does not describe.
const RUNNING: u64 = 0b1;
const COMPLETE: u64 = 0b10;
const NOTIFIED: u64 = 0b100;
const CANCELLED: u64 = 0b10_0000;

/// The most shards one list may have, and the most tasks one list or
/// queue is read for, so corrupted memory cannot make the debugger read
/// without end.
const MAX_SHARDS: u64 = 1 << 16;
const MAX_TASKS: u64 = 1 << 24;

/// The function each task's vtable polls it through, which a symbol
/// mangled in v0 names with its generic arguments and a legacy one
/// without them.
const POLL: &str = "tokio::runtime::task::raw::poll";

/// The closure a multi-thread runtime's workers run in the blocking pool,
/// as a v0 symbol names it and as debug information does.
const LAUNCH: [&str; 2] = [
    "multi_thread::worker::Launch>::launch::{closure",
    "multi_thread::worker::{impl#0}::launch::{closure_env",
];

/// The functions that start tasks, by the name debug information gives
/// them and the start of their symbol's, and whether each also schedules
/// woken tasks. A runtime binds each task it spawns to its list in
/// `OwnedTasks::bind_inner`, the part of `bind` that is the same for every
/// type of future; a `LocalSet` binds its tasks in a generic function, and
/// schedules each one it spawns, as each it wakes, in `Shared::schedule`.
/// Each takes the task as its second argument. The names spell `Arc` as
/// Rust 1.97 does; [`crate::rust_names::with_each_arc`] spells them as
/// every release does, since debug information names the type where std
/// defines it, which later releases moved.
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

/// tokio, in an image that has its thread-local context.
pub fn detect(
    image: &Arc<dyn RuntimeImage + Send + Sync>,
) -> Option<Result<Arc<dyn RuntimeModel>, super::Unbound>> {
    let Some(tls) = layout::thread_local_storage(image.as_ref(), layout::CONTEXT_SCOPE) else {
        // A program stripped of its symbols shows no sign of tokio. One
        // whose debug information describes tokio's context but whose
        // storage has a name no release this model knows gives it, as a
        // later Rust may, says so: finding no tasks there would look like
        // a program that has none.
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

/// The release a source path names, such as `tokio-1.53.2/src/…`.
fn version(path: &std::path::Path) -> Option<(u64, u64, Arc<str>)> {
    let mut components = path.components().rev();
    for _ in VERSIONED_SOURCE.split('/') {
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

    /// Why every result may be wrong: a release the contract was not
    /// checked against, or one the program does not name.
    caveat: Option<Arc<str>>,

    /// Whether each vtable, by image address, is a task's.
    vtables: Mutex<BTreeMap<ImageAddress, Option<Arc<str>>>>,

    /// Where the future is in the cells of each poll function.
    futures: Mutex<BTreeMap<ImageAddress, Result<future::FutureLayout, Arc<str>>>>,

    /// Where the function that runs each coroutine type begins.
    bodies: Mutex<BTreeMap<crate::TypeReference, Option<ImageAddress>>>,

    /// Where each function that starts tasks begins, and whether it also
    /// schedules woken tasks.
    starters: Vec<(ImageAddress, bool)>,
}

/// One runtime the stop's threads entered, or a `LocalSet` one runs or
/// drives: its flavor, and its handle's address, or the set's `Shared`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Instance {
    flavor: Flavor,
    handle: VirtualAddress,
}

/// What one thread's `CONTEXT` says.
#[derive(Debug, Clone, Copy)]
struct ThreadContext {
    thread: ThreadId,
    runtime: Option<Instance>,

    /// Whether the thread runs a scheduler: a worker, or the thread a
    /// current-thread runtime blocks on.
    worker: bool,

    /// Whether the thread entered its runtime: a worker, or a thread that
    /// blocks on it.
    entered: bool,

    /// The task the thread polls.
    task: Option<u64>,

    /// The `LocalSet` the thread runs, by its `Shared`.
    local: Option<VirtualAddress>,
}

/// The runtimes at one stop and what each thread does for them.
#[derive(Debug, Default)]
struct Census {
    threads: Vec<ThreadContext>,
    runtimes: Vec<Instance>,
    gaps: Vec<Arc<str>>,
}

/// One runtime's or set's task list, as read at a stop: its shards, or a
/// set's one head, and the count of a runtime's, which a set keeps none
/// of.
#[derive(Debug, Clone, Copy)]
struct List {
    id: u64,
    shards: VirtualAddress,
    length: u64,
    count: u64,
    counted: bool,
}

/// Where a page of tasks begins: at the start, at a task of a runtime's
/// list, or at an index of the blocking pools' tasks. A task's header is
/// 128-aligned, which leaves the low bit for the index.
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
        let (major, minor) = VERIFIED[VERIFIED.len() - 1];
        let caveat = match image
            .source_path_ending(VERSIONED_SOURCE)
            .as_deref()
            .map(version)
        {
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

    /// Every stopped thread's context, and the runtimes they entered in
    /// the order of their threads, then the sets they run or drive.
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

    /// The `Shared` of the set whose `Rc<Context>` points to `context`.
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

    /// The set a thread's `CURRENT` says it runs, or `None`.
    fn running_set(
        stop: &dyn RuntimeStop,
        locals: &Locals,
        thread: ThreadId,
    ) -> Result<Option<VirtualAddress>, Arc<str>> {
        let base = thread_local(stop, &locals.tls, thread)?;
        let unreadable =
            || Arc::<str>::from(format!("the local set context at {base:#x} is unreadable"));
        let state =
            records::read(stop, base.wrapping_add(locals.state), 1).ok_or_else(unreadable)?;
        if state != locals.alive {
            return Ok(None);
        }

        let context =
            records::pointer(stop, base.wrapping_add(locals.context)).ok_or_else(unreadable)?;
        if context == records::NULL {
            return Ok(None);
        }

        Self::set_shared(stop, locals, context).map(Some)
    }

    /// What a thread's `CONTEXT` says, or `None` when the thread never
    /// made it.
    fn thread_context(
        stop: &dyn RuntimeStop,
        context: &Context,
        locals: Option<&Locals>,
        thread: ThreadId,
    ) -> Result<Option<ThreadContext>, Arc<str>> {
        let base = thread_local(stop, &context.tls, thread)?;
        let unreadable = || Arc::<str>::from(format!("the context at {base:#x} is unreadable"));
        let state =
            records::read(stop, base.wrapping_add(context.state), 1).ok_or_else(unreadable)?;
        if state != context.alive {
            return Ok(None);
        }

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

    /// A runtime's or set's task list.
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

    /// Whether the vtable at `vtable` is a task's: its poll function is
    /// tokio's `raw::poll`. The answer for each vtable is kept, as code
    /// does not change.
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

    /// Whether a task polls the blocking closure a worker runs on.
    fn is_launch(&self, stop: &dyn RuntimeStop, vtable: VirtualAddress) -> bool {
        let Ok(tasks) = self.task_layout() else {
            return false;
        };

        let Some(poll) = records::pointer(stop, vtable.wrapping_add(tasks.poll)) else {
            return false;
        };

        let poll = stop.load_bias().wrapping_unload(poll);

        // A legacy symbol names no generic arguments, while the debug
        // information names them under either mangling.
        [self.image.symbol_at(poll), self.image.function_name(poll)]
            .into_iter()
            .flatten()
            .any(|name| LAUNCH.iter().any(|launch| name.contains(launch)))
    }
}

/// Where a thread's copy of a thread-local variable is.
fn thread_local(
    stop: &dyn RuntimeStop,
    tls: &ThreadLocal,
    thread: ThreadId,
) -> Result<VirtualAddress, Arc<str>> {
    stop.thread_local_address(thread, *tls)
}

impl RuntimeModel for TokioRuntime {
    fn tasks(
        &self,
        stop: &dyn RuntimeStop,
        start: u64,
        limit: usize,
        program_only: bool,
    ) -> Partial<TaskPage> {
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

        // A running blocking closure's task is known only by its number,
        // with no header to read where it was spawned from.
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

    /// The task the thread's `CONTEXT` names, which tokio sets for each
    /// poll of a task, its own launches' and blocking closures' too.
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
            .get(RSI)
            .map(VirtualAddress::new)
            .ok_or("the task being started is unavailable")?;
        self.started(stop, header, schedules)
    }

    fn task_noun(&self) -> &'static str {
        TASK_NOUN.0
    }

    /// A `LocalSet` a thread drives, as `LocalSet::block_on` and
    /// `run_until` do through `RunUntil`, or as the set itself is awaited.
    fn task_set(
        &self,
        stop: &dyn RuntimeStop,
        future: VirtualAddress,
        ty: crate::TypeReference,
    ) -> Option<u64> {
        let locals = self.locals().ok().flatten()?;
        let info = self.image.type_info(ty)?;

        let set = match local_type(info)? {
            "LocalSet" => future,
            "RunUntil" => {
                let crate::TypeKind::Record { members, .. } = &info.kind else {
                    return None;
                };

                let member = members
                    .iter()
                    .find(|member| member.name.as_deref() == Some("local_set"))?;
                let crate::RecordMemberLayout::ByteOffset(offset) = member.layout else {
                    return None;
                };

                records::pointer(stop, future.wrapping_add(offset))?
            }
            _ => return None,
        };

        let context = records::pointer(stop, set.wrapping_add(locals.set))?;
        Self::set_shared(stop, locals, context)
            .ok()
            .map(VirtualAddress::get)
    }

    fn may_run_task_set(&self, ty: crate::TypeReference) -> bool {
        self.locals().is_ok_and(|locals| locals.is_some()) && may_hold_set(self.image.as_ref(), ty)
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

        // The task's cell is freed as the box that holds it is dropped,
        // which the glue for `Box<Cell<T, S>>` does for the task's `T` and
        // `S`, as its poll function's name spells them. The glue has the
        // name its compiler gives it, which changed in Rust 1.97.
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
        let (Some(header), Some(argument)) = (task.task.locator, registers.get(RDI)) else {
            return false;
        };

        // The functions that run the task are passed its header; the glue
        // that frees it, the box that holds it.
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

        // A freed task's place may hold another task, or nothing.
        let vtable = VirtualAddress::new(word(header.wrapping_add(tasks.vtable))?);
        let id = word(header.wrapping_add(word(vtable.wrapping_add(tasks.id_offset))?))?;
        if task.runs.first().map(|poll| poll.get()) != Some(word(vtable.wrapping_add(tasks.poll))?)
            || id != task.task.number
        {
            return Err(format!("task {} is no longer where it was", task.task.number).into());
        }

        let state = word(header.wrapping_add(tasks.state))?;

        // tokio never takes back a cancellation, nor cancels a task that
        // completed.
        Ok(if state & CANCELLED != 0 {
            Some(TaskEnd::Cancelled)
        } else {
            (state & COMPLETE != 0).then_some(TaskEnd::Finished)
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

/// The name of a type of `tokio::task::local`, such as `LocalSet`.
fn local_type(info: &crate::TypeInfo) -> Option<&str> {
    let identity = info.identity.as_ref()?;
    identity
        .path
        .iter()
        .map(AsRef::as_ref)
        .eq(["tokio", "task", "local"])
        .then_some(&*identity.base)
}

/// Whether a type is a `Box` or a `Pin`, which hold the future they point
/// to.
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

/// Whether a future of type `ty` may run a `LocalSet` when polled, as its
/// type says without its value: whether it holds a set, a `RunUntil`, or
/// a reference to a set, in place or through a box or pin, or a future
/// whose type only its value says.
fn may_hold_set(image: &dyn RuntimeImage, ty: crate::TypeReference) -> bool {
    use crate::TypeKind;
    fn members(
        fields: &[crate::RecordMember],
        boxed: bool,
    ) -> impl Iterator<Item = (crate::TypeReference, bool)> + '_ {
        fields.iter().map(move |member| (member.type_ref, boxed))
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut pending = vec![(ty, false)];
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

                // A box or pin holds the future it points to; another
                // pointer's target is some other future's, or no future.
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
