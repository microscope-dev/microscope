//! Reading tokio's tasks at one stop: each runtime's list, shard by shard,
//! then the blocking pools' closures, queued and running.

use std::sync::Arc;

use super::layout::{Flavor, Pool, Queues};
use super::{
    CANCELLED, COMPLETE, Census, Cursor, Instance, List, MAX_TASKS, NOTIFIED, RUNNING, Tasks,
    TokioRuntime,
};

use crate::runtime_model::futures::{self, AsyncFrameKind, ChainEnd};
use crate::runtime_model::records;
use crate::runtime_model::{
    Partial, RuntimeStop, RuntimeTask, StartedTask, TaskPage, TaskRef, ThreadActivity,
};

use crate::{RecordedPlace, StackSegment, TaskState, ThreadId, VirtualAddress};

/// One task of a list, checked: it belongs to the list, its vtable is a
/// task's, and it links back to the task before it.
#[derive(Debug, Clone, Copy)]
struct Node {
    id: u64,
    state: u64,
    next: VirtualAddress,
}

/// Where a page resumes within the runtimes' lists: the runtime's index,
/// the shard, and the task with the task before it.
type Resume = (usize, u64, Option<(VirtualAddress, VirtualAddress)>);

impl TokioRuntime {
    /// Fills a page from `cursor`: the runtimes' lists, then the blocking
    /// pools' tasks.
    pub(super) fn page(
        &self,
        stop: &dyn RuntimeStop,
        census: &Census,
        cursor: Cursor,
        limit: usize,
        program_only: bool,
        page: &mut Partial<TaskPage>,
    ) {
        page.gaps.extend(census.gaps.iter().cloned());
        let tasks = match self.task_layout() {
            Ok(tasks) => tasks,
            Err(reason) => {
                page.gaps
                    .push(format!("tokio's tasks cannot be read: {reason}").into());
                return;
            }
        };

        let resume = match cursor {
            Cursor::Start => Some((0, 0, None)),
            Cursor::Node(header) => match self.locate(stop, census, tasks, header) {
                Ok(resume) => Some(resume),
                Err(reason) => {
                    page.gaps.push(reason);
                    return;
                }
            },
            Cursor::Blocking(_) => None,
        };

        if let Some((first, mut shard, mut node)) = resume {
            for &runtime in census.runtimes.iter().skip(first) {
                if self.list_page(stop, census, runtime, (shard, node), limit, page) {
                    return;
                }

                (shard, node) = (0, None);
            }
        }

        let start = match cursor {
            Cursor::Blocking(index) => index,
            Cursor::Start | Cursor::Node(_) => 0,
        };

        let blocking = self.blocking(stop, census, tasks, &mut page.gaps);
        for (index, task) in (0..)
            .zip(blocking)
            .skip(usize::try_from(start).unwrap_or(usize::MAX))
        {
            if program_only && task.internal {
                continue;
            }

            if page.value.tasks.len() == limit {
                page.value.next = Some(Cursor::Blocking(index).encode());
                return;
            }

            page.value.tasks.push(task);
        }
    }

    /// Lists one runtime's tasks from a shard, at a task or the shard's
    /// head, until the page is full, which it says.
    fn list_page(
        &self,
        stop: &dyn RuntimeStop,
        census: &Census,
        runtime: Instance,
        (mut shard, mut node): (u64, Option<(VirtualAddress, VirtualAddress)>),
        limit: usize,
        page: &mut Partial<TaskPage>,
    ) -> bool {
        let list = self
            .task_layout()
            .and_then(|tasks| Ok((tasks, self.list(stop, runtime)?)));
        let (tasks, list) = match list {
            Ok(list) => list,
            Err(reason) => {
                page.gaps.push(reason);
                return false;
            }
        };

        let flavor = runtime.flavor.describe();
        let shard_start = (shard, node.is_some());

        // A list read with no shard changing or damaged holds its count.
        let mut whole = true;
        let mut total = 0;

        while shard < list.length {
            let (head, changing) = match self.shard(stop, runtime, &list, shard) {
                Ok((head, locked)) => {
                    if locked {
                        whole = false;
                        page.gaps.push(
                            format!(
                                "shard {shard} of the {flavor}'s tasks was being \
                                 changed at the stop; its tasks may be missing or stale"
                            )
                            .into(),
                        );
                    }

                    (head, locked)
                }
                Err(reason) => {
                    whole = false;
                    page.gaps.push(reason);
                    shard += 1;
                    continue;
                }
            };

            let (mut at, mut previous) = node.take().unwrap_or((head, records::NULL));
            let mut walked = 0;

            while at != records::NULL {
                if walked >= list.count {
                    whole = false;
                    page.gaps.push(
                        format!(
                            "shard {shard} of the {flavor}'s tasks holds more than its \
                             {} tasks; it is read no further",
                            list.count
                        )
                        .into(),
                    );
                    break;
                }

                match self.node(stop, tasks, &list, at, previous) {
                    // A page ends only before a task the next can list.
                    Ok(_) if page.value.tasks.len() == limit => {
                        page.value.next = Some(Cursor::Node(at).encode());
                        return true;
                    }
                    Ok(found) => {
                        page.value.tasks.push(RuntimeTask {
                            entry: self.entry(stop, at),
                            labels: label(census, runtime, list.id),
                            ..owned_task(census, runtime, at, &found)
                        });
                        (previous, at) = (at, found.next);
                        walked += 1;
                        total += 1;
                    }

                    // A shard being changed may disagree with itself
                    // anywhere, which its gap already says.
                    Err(_) if changing => break,
                    Err(reason) => {
                        whole = false;
                        page.gaps.push(
                            format!(
                                "shard {shard} of the {flavor}'s tasks: {reason}; the \
                                 rest of the shard is not listed"
                            )
                            .into(),
                        );
                        break;
                    }
                }
            }

            shard += 1;
        }

        // A list one page reads whole holds its count. One that pages read
        // in parts is not counted, which would read it all again, so that
        // a page costs what its own tasks do.
        if whole && list.counted && shard_start == (0, false) && total != list.count {
            page.gaps.push(
                format!(
                    "the {flavor} runtime counts {} tasks, but its list holds {total}",
                    list.count
                )
                .into(),
            );
        }

        false
    }

    /// A shard's head, and whether its lock was held at the stop.
    fn shard(
        &self,
        stop: &dyn RuntimeStop,
        runtime: Instance,
        list: &List,
        index: u64,
    ) -> Result<(VirtualAddress, bool), Arc<str>> {
        // A set's one list is its thread's alone, behind no lock.
        if runtime.flavor == Flavor::Local {
            let head = records::pointer(stop, list.shards).ok_or_else(|| {
                Arc::<str>::from(format!(
                    "the local set's list at {:#x} is unreadable",
                    list.shards
                ))
            })?;
            return Ok((head, false));
        }

        let owned = self.owned(runtime.flavor)?;
        let at = list
            .shards
            .wrapping_add(index.wrapping_mul(owned.shard_size));
        let unreadable = || Arc::<str>::from(format!("the shard at {at:#x} is unreadable"));
        let lock = owned.shard_lock;
        let state =
            records::read(stop, at.wrapping_add(lock.offset), lock.size).ok_or_else(unreadable)?;
        let head = records::pointer(stop, at.wrapping_add(owned.head)).ok_or_else(unreadable)?;
        Ok((head, lock.is_held(state)))
    }

    /// Reads the task at `header`, the task after `previous` in its shard,
    /// or its head when `previous` is null.
    fn node(
        &self,
        stop: &dyn RuntimeStop,
        tasks: &Tasks,
        list: &List,
        header: VirtualAddress,
        previous: VirtualAddress,
    ) -> Result<Node, Arc<str>> {
        let word = |address: VirtualAddress| {
            records::word(stop, address)
                .ok_or_else(|| Arc::<str>::from(format!("the task at {header:#x} is unreadable")))
        };

        let owner = word(header.wrapping_add(tasks.owner))?;
        if owner != list.id {
            return Err(format!(
                "the task at {header:#x} belongs to task list {owner}, not {}",
                list.id
            )
            .into());
        }

        let (vtable, trailer) = self.trailer(stop, tasks, header)?;
        let back = VirtualAddress::new(word(trailer.wrapping_add(tasks.prev))?);
        if back != previous {
            return Err(format!(
                "the task at {header:#x} links back to {back:#x}, not {previous:#x}"
            )
            .into());
        }

        let id_offset = word(vtable.wrapping_add(tasks.id_offset))?;

        Ok(Node {
            id: word(header.wrapping_add(id_offset))?,
            state: word(header.wrapping_add(tasks.state))?,
            next: VirtualAddress::new(word(trailer.wrapping_add(tasks.next))?),
        })
    }

    /// A task's checked vtable and where its trailer is.
    fn trailer(
        &self,
        stop: &dyn RuntimeStop,
        tasks: &Tasks,
        header: VirtualAddress,
    ) -> Result<(VirtualAddress, VirtualAddress), Arc<str>> {
        let word = |address: VirtualAddress| {
            records::word(stop, address)
                .ok_or_else(|| Arc::<str>::from(format!("the task at {header:#x} is unreadable")))
        };

        let vtable = VirtualAddress::new(word(header.wrapping_add(tasks.vtable))?);
        self.checked_vtable(stop, vtable)?;
        let offset = word(vtable.wrapping_add(tasks.trailer_offset))?;
        Ok((vtable, header.wrapping_add(offset)))
    }

    /// Where a page resumes at the task `header` a previous page ended
    /// before, once the task is still in its runtime's list.
    fn locate(
        &self,
        stop: &dyn RuntimeStop,
        census: &Census,
        tasks: &Tasks,
        header: VirtualAddress,
    ) -> Result<Resume, Arc<str>> {
        let stale = || Arc::<str>::from("the page's cursor names no task at this stop");
        let owner = records::word(stop, header.wrapping_add(tasks.owner)).ok_or_else(stale)?;

        for (index, &runtime) in census.runtimes.iter().enumerate() {
            let list = self.list(stop, runtime)?;
            if list.id != owner {
                continue;
            }

            let (vtable, trailer) = self.trailer(stop, tasks, header)?;
            let id_offset =
                records::word(stop, vtable.wrapping_add(tasks.id_offset)).ok_or_else(stale)?;
            let id = records::word(stop, header.wrapping_add(id_offset)).ok_or_else(stale)?;
            let shard = id & (list.length - 1);
            let previous =
                records::pointer(stop, trailer.wrapping_add(tasks.prev)).ok_or_else(stale)?;
            if self.follows(stop, tasks, runtime, &list, shard, previous)? != header {
                return Err(stale());
            }

            return Ok((index, shard, Some((header, previous))));
        }

        Err(stale())
    }

    /// The task after `previous` in a shard, or its head when `previous`
    /// is null.
    fn follows(
        &self,
        stop: &dyn RuntimeStop,
        tasks: &Tasks,
        runtime: Instance,
        list: &List,
        shard: u64,
        previous: VirtualAddress,
    ) -> Result<VirtualAddress, Arc<str>> {
        if previous == records::NULL {
            return Ok(self.shard(stop, runtime, list, shard)?.0);
        }

        let (_, trailer) = self.trailer(stop, tasks, previous)?;
        records::pointer(stop, trailer.wrapping_add(tasks.next))
            .ok_or_else(|| format!("the task at {previous:#x} is unreadable").into())
    }

    /// The task numbered `number` in a runtime's list, at `locator` when
    /// that still holds it, or else in the shard its number picks.
    fn listed(
        &self,
        stop: &dyn RuntimeStop,
        tasks: &Tasks,
        runtime: Instance,
        number: u64,
        locator: Option<u64>,
    ) -> Result<Option<(VirtualAddress, Node)>, Arc<str>> {
        let list = self.list(stop, runtime)?;
        let shard = number & (list.length - 1);
        if let Some(header) = locator
            .map(VirtualAddress::new)
            .filter(|header| *header != records::NULL)
            && let Ok((_, trailer)) = self.trailer(stop, tasks, header)
            && let Some(previous) = records::pointer(stop, trailer.wrapping_add(tasks.prev))
            && self.follows(stop, tasks, runtime, &list, shard, previous) == Ok(header)
            && let Ok(found) = self.node(stop, tasks, &list, header, previous)
            && found.id == number
        {
            return Ok(Some((header, found)));
        }

        let (mut at, _) = self.shard(stop, runtime, &list, shard)?;
        let mut previous = records::NULL;

        for _ in 0..=list.count {
            if at == records::NULL {
                return Ok(None);
            }

            let found = self.node(stop, tasks, &list, at, previous)?;
            if found.id == number {
                return Ok(Some((at, found)));
            }

            (previous, at) = (at, found.next);
        }

        Err(format!("a shard holds more than its runtime's {} tasks", list.count).into())
    }

    /// The task a reference names, as a page would list it.
    pub(super) fn find(
        &self,
        stop: &dyn RuntimeStop,
        census: &Census,
        task: TaskRef,
    ) -> Result<Option<RuntimeTask>, Arc<str>> {
        let tasks = self.task_layout()?;

        // A runtime whose list cannot be read may hold the task, so it is
        // absent only once every list was read.
        let mut unreadable = None;
        for &runtime in &census.runtimes {
            match self.listed(stop, tasks, runtime, task.number, task.locator) {
                Ok(Some((header, node))) => {
                    return Ok(Some(RuntimeTask {
                        entry: self.entry(stop, header),
                        labels: self
                            .list(stop, runtime)
                            .map_or_else(|_| Vec::new(), |list| label(census, runtime, list.id)),
                        ..owned_task(census, runtime, header, &node)
                    }));
                }
                Ok(None) => {}
                Err(reason) => unreadable = unreadable.or(Some(reason)),
            }
        }

        let mut gaps = Vec::new();
        if let Some(found) = self
            .blocking(stop, census, tasks, &mut gaps)
            .into_iter()
            .find(|found| found.number == task.number)
        {
            return Ok(Some(found));
        }

        unreadable
            .or_else(|| gaps.into_iter().next())
            .map_or(Ok(None), |reason| {
                Err(format!("task {} was not found, and {reason}", task.number).into())
            })
    }

    /// What a thread does for a runtime: a worker polling a listed task
    /// runs it, as a thread running a set runs the set's task it polls,
    /// and a pool thread runs its closure's task. A worker with no task,
    /// polling only its own launch, and a pool thread waiting for a
    /// closure, are the runtime's idle threads. A thread that never
    /// entered a runtime, or blocks on one, is the program's own; so is
    /// the thread a current-thread runtime blocks on, between its tasks.
    pub(super) fn activity(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<ThreadActivity, Arc<str>> {
        let context = self.context()?;

        // A program whose sets cannot be read says so in its pages of tasks.
        let locals = self.locals().ok().flatten();
        let found = Self::thread_context(stop, context, locals, thread)?;
        if let Some(found) = found
            && let (Some(set), Some(number)) = (found.local, found.task)
        {
            let set = Instance {
                flavor: Flavor::Local,
                handle: set,
            };

            if self
                .listed(stop, self.task_layout()?, set, number, None)?
                .is_some()
            {
                return Ok(ThreadActivity::Task {
                    number,
                    stack: StackSegment::Task,
                });
            }
        }

        let Some((found, runtime)) = found.and_then(|found| Some((found, found.runtime?))) else {
            return Ok(ThreadActivity::Outside);
        };

        let between = match (found.worker, runtime.flavor, found.entered) {
            (true, Flavor::MultiThread, _) | (false, _, false) => ThreadActivity::Idle,
            _ => ThreadActivity::Outside,
        };

        let Some(number) = found.task else {
            return Ok(between);
        };

        let running = ThreadActivity::Task {
            number,
            stack: StackSegment::Task,
        };

        // A pool thread runs its closure's task, as the blocking tasks list
        // it. One that entered the runtime blocks on it, or is a worker
        // whose launch has not yet set its scheduler.
        if !found.worker {
            return Ok(if found.entered { between } else { running });
        }

        Ok(
            match self.listed(stop, self.task_layout()?, runtime, number, None)? {
                Some(_) => running,
                None => between,
            },
        )
    }

    /// The blocking pools' tasks: each runtime's queued closures, then
    /// those threads run.
    fn blocking(
        &self,
        stop: &dyn RuntimeStop,
        census: &Census,
        tasks: &Tasks,
        gaps: &mut Vec<Arc<str>>,
    ) -> Vec<RuntimeTask> {
        let mut found = Vec::new();
        let pooled = census
            .runtimes
            .iter()
            .filter(|runtime| runtime.flavor != Flavor::Local);

        for &runtime in pooled {
            match self.queued(stop, tasks, runtime) {
                Ok((queued, locked)) => {
                    if locked {
                        gaps.push(
                            format!(
                                "the {}'s blocking pool was being changed at the stop; \
                                 its queue may be stale",
                                runtime.flavor.describe()
                            )
                            .into(),
                        );
                    }

                    let labels = self
                        .list(stop, runtime)
                        .map_or_else(|_| Vec::new(), |list| label(census, runtime, list.id));
                    found.extend(queued.into_iter().map(|task| RuntimeTask {
                        labels: labels.clone(),
                        ..task
                    }));
                }
                Err(reason) => gaps.push(format!("queued blocking tasks: {reason}").into()),
            }
        }

        found.extend(
            census
                .threads
                .iter()
                // A pool's threads neither run a scheduler nor enter the
                // runtime, as a thread blocking on it does.
                .filter(|thread| thread.runtime.is_some() && !thread.worker && !thread.entered)
                .filter_map(|thread| {
                    Some(RuntimeTask {
                        state: TaskState::Running,
                        detail: Some("running a blocking closure".into()),
                        thread: Some(thread.thread),
                        ..task(thread.task?, 0)
                    })
                }),
        );
        found
    }

    /// A runtime's queued blocking tasks, and whether the pool's lock was
    /// held at the stop.
    fn queued(
        &self,
        stop: &dyn RuntimeStop,
        tasks: &Tasks,
        runtime: Instance,
    ) -> Result<(Vec<RuntimeTask>, bool), Arc<str>> {
        let pool: &Pool = self.layout.pool.as_ref().map_err(Arc::clone)?;
        let context = self.context()?;
        let spawner = context
            .runtimes
            .iter()
            .find(|(flavor, ..)| *flavor == runtime.flavor)
            .map(|(.., layout)| layout.spawner)
            .ok_or("the runtime's flavor is unknown")?;
        let unreadable = || Arc::<str>::from("the blocking pool is unreadable");
        let word = |address: VirtualAddress| records::word(stop, address).ok_or_else(unreadable);
        let pointer =
            |address: VirtualAddress| records::pointer(stop, address).ok_or_else(unreadable);
        let inner = pointer(runtime.handle.wrapping_add(spawner))?.wrapping_add(pool.data);

        let guards = match &pool.queues {
            Queues::One(one) => vec![*one],
            Queues::Chosen {
                choice,
                at,
                locked,
                sharded,
                stride,
                shards,
            } => match &*choice.active(stop, inner.wrapping_add(*at))?.name {
                "Locked" => vec![*locked],
                // A sharded pool's workers take from every shard in turn,
                // so the pool has no one order: its tasks are listed
                // shard by shard, each shard in its own order.
                "Sharded" => (0..*shards)
                    .map(|index| sharded.shifted(index.wrapping_mul(*stride)))
                    .collect::<Result<_, _>>()?,
                other => return Err(format!("the blocking pool is {other}").into()),
            },
        };

        let mut queued = Vec::new();
        let mut locked = false;
        for guarded in guards {
            let lock = records::read(
                stop,
                inner.wrapping_add(guarded.lock.offset),
                guarded.lock.size,
            )
            .ok_or_else(unreadable)?;
            let changing = guarded.lock.is_held(lock);
            locked |= changing;

            let deque = inner.wrapping_add(guarded.queue);
            let head = word(deque.wrapping_add(pool.head))?;
            let length = word(deque.wrapping_add(pool.len))?;
            let buffer = pointer(deque.wrapping_add(pool.buffer))?;
            let capacity = word(deque.wrapping_add(pool.capacity))?;
            if length > capacity || capacity > MAX_TASKS || (capacity > 0 && head >= capacity) {
                return Err(
                    format!("its queue holds {length} of {capacity} from slot {head}").into(),
                );
            }

            for index in 0..length {
                let slot = (head + index) % capacity;
                let at = buffer.wrapping_add(slot.wrapping_mul(pool.task_size));
                let entry = pointer(at.wrapping_add(pool.task)).and_then(|header| {
                    let (vtable, _) = self.trailer(stop, tasks, header)?;
                    let id_offset = word(vtable.wrapping_add(tasks.id_offset))?;
                    Ok(RuntimeTask {
                        state: TaskState::Runnable,
                        detail: Some("queued in the blocking pool".into()),
                        internal: self.is_launch(stop, vtable),
                        ..task(word(header.wrapping_add(id_offset))?, header.get())
                    })
                });
                match entry {
                    Ok(entry) => queued.push(entry),

                    // A thread stopped inside a push may have counted a
                    // slot it has not yet written: the change the held
                    // lock's gap reports.
                    Err(_) if changing => break,
                    Err(reason) => return Err(reason),
                }
            }
        }

        Ok((queued, locked))
    }
}

/// The longest path of a spawn location read.
const MAX_PATH: u64 = 4096;

impl TokioRuntime {
    /// Where the program spawned the task whose header is at `header`, for
    /// a build that records it.
    pub(super) fn spawned_at(
        &self,
        stop: &dyn RuntimeStop,
        header: VirtualAddress,
    ) -> Option<RecordedPlace> {
        let spawns = self.layout.spawns.as_ref().ok()?;
        let tasks = self.task_layout().ok()?;
        let vtable = records::pointer(stop, header.wrapping_add(tasks.vtable))?;
        let offset = records::word(stop, vtable.wrapping_add(spawns.offset))?;
        let location = records::pointer(stop, header.wrapping_add(offset))?;
        let file = records::pointer(stop, location.wrapping_add(spawns.file))?;
        let length = records::word(stop, location.wrapping_add(spawns.length))?;
        if length > MAX_PATH {
            return None;
        }

        let mut path = vec![0; usize::try_from(length).ok()?];
        if !stop.read(file, &mut path) {
            return None;
        }

        Some(RecordedPlace {
            path: String::from_utf8(path).ok()?.into(),
            line: u32::try_from(records::read(stop, location.wrapping_add(spawns.line), 4)?)
                .ok()?,
            column: u32::try_from(records::read(
                stop,
                location.wrapping_add(spawns.column),
                4,
            )?)
            .ok()?,
        })
    }
}

impl TokioRuntime {
    /// The task at `header` that a starter is starting, where it begins;
    /// `None` for a task a starter that also schedules woken tasks has
    /// polled before, which it is not starting.
    pub(super) fn started(
        &self,
        stop: &dyn RuntimeStop,
        header: VirtualAddress,
        schedules: bool,
    ) -> Result<Option<StartedTask>, Arc<str>> {
        let tasks = self.task_layout()?;
        let unreadable = || Arc::<str>::from(format!("the task at {header:#x} is unreadable"));
        let (vtable, _) = self.trailer(stop, tasks, header)?;
        let id_offset =
            records::word(stop, vtable.wrapping_add(tasks.id_offset)).ok_or_else(unreadable)?;
        let number = records::word(stop, header.wrapping_add(id_offset)).ok_or_else(unreadable)?;
        let (future, ty) = self.future(stop, header)?;
        let chain = futures::walk(self.image.as_ref(), stop, future, ty);

        // A task that has not begun has a root coroutine that has not
        // either; a woken one has run.
        if schedules && chain.end != ChainEnd::Unresumed {
            return Ok(None);
        }

        let coroutine = chain.frames.last().and_then(|root| match root.kind {
            AsyncFrameKind::Coroutine { .. } => Some(root.ty),
            AsyncFrameKind::Leaf => None,
        });
        Ok(Some(StartedTask {
            task: RuntimeTask {
                state: TaskState::Runnable,
                detail: Some("being spawned".into()),
                entry: self.entry(stop, header),
                ..task(number, header.get())
            },
            coroutine,
        }))
    }
}

/// A task as a page lists it, with nothing known of it but its number
/// and where it is.
const fn task(number: u64, locator: u64) -> RuntimeTask {
    RuntimeTask {
        number,
        locator,
        state: TaskState::Blocked,
        detail: None,
        thread: None,
        resume: None,
        creation: None,
        spawned: None,
        entry: None,
        parent: None,
        internal: false,
        labels: Vec::new(),
        callback: None,
        stackless: false,
    }
}

/// The runtime or set that holds a task, where the process has several,
/// by its flavor and its list's number.
fn label(census: &Census, runtime: Instance, list: u64) -> crate::runtime_model::TaskLabels {
    if census.runtimes.len() < 2 {
        return Vec::new();
    }

    vec![(
        "runtime".into(),
        format!("{} {list}", runtime.flavor.describe()).into(),
    )]
}

/// A task of a runtime's list, by its state: running on the thread that
/// polls it, ready to be polled, suspended, or finished.
fn owned_task(
    census: &Census,
    runtime: Instance,
    header: VirtualAddress,
    node: &Node,
) -> RuntimeTask {
    let polls = |thread: &&super::ThreadContext| match runtime.flavor {
        Flavor::Local => thread.local == Some(runtime.handle),
        _ => thread.runtime == Some(runtime) && thread.worker,
    };

    let thread = census
        .threads
        .iter()
        .filter(polls)
        .find(|thread| thread.task == Some(node.id))
        .map(|thread| thread.thread);
    let cancelled = node.state & CANCELLED != 0;
    let (state, detail) = if node.state & COMPLETE != 0 {
        (
            TaskState::Exited,
            if cancelled { "cancelled" } else { "completed" },
        )
    } else if node.state & RUNNING != 0 {
        match thread {
            Some(_) => (TaskState::Running, "running"),
            None => (
                TaskState::Unknown("a poll of the task begins or ends on some thread".into()),
                "running",
            ),
        }
    } else if node.state & NOTIFIED != 0 {
        (TaskState::Runnable, "runnable")
    } else {
        (TaskState::Blocked, "suspended")
    };

    let detail = if cancelled && node.state & COMPLETE == 0 {
        format!("{detail}, cancelled")
    } else {
        detail.to_owned()
    };

    RuntimeTask {
        thread: thread.filter(|_| state == TaskState::Running),
        state,
        detail: Some(detail.into()),
        ..task(node.id, header.get())
    }
}
