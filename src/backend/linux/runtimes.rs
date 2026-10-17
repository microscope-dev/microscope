//! The language runtimes in a stopped process, and the tasks they schedule.
//!
//! Each loaded image is asked once whether it carries a runtime a model
//! knows; the bound model then answers through the stopped process's memory
//! and threads, read as every other inspection reads them.

use std::cell::Cell;
use std::sync::Arc;

use nix::unistd::Pid;

use crate::protocol::StopId;
use crate::runtime_model::{
    self, CodeAddress, RuntimeModel, RuntimeStop, RuntimeTask, TaskContext, TaskRef,
};

use crate::unwind::DEFAULT_MAX_FRAMES;
use crate::{
    ColumnNumber, Error, ExecutionContext, ImageAddress, InspectionUsage, LineNumber, LoadedModule,
    ModuleImage, RecordedPlace, Result, RuntimeId, SourceLocation, StackSegment, TaskCursor,
    TaskId, TaskLocation, TaskPage, TaskSnapshot, TaskState, ThreadActivity, ThreadId,
    VirtualAddress,
};

use super::activation::TaskStack;
use super::frames::{RootOrigin, StackRoot};
use super::memory::read_logical_memory;
use super::native::InspectionOps;
use super::signals::Signal;
use super::{
    BreakpointSite, Controller, Inferior, debug_pid, debug_thread_id, validate_public_stop,
};

/// The sets of tasks the futures threads drive at a stop run, by where
/// the runtime keeps them, and why some may be missing.
pub(super) struct TaskSets {
    sets: Arc<[u64]>,
    gaps: Arc<[Arc<str>]>,
}

/// The most tasks one page holds.
pub(super) const MAX_TASK_PAGE: usize = 4096;

/// A runtime bound to the loaded module that carries it.
#[derive(Clone)]
pub(super) struct BoundRuntime {
    pub(super) id: RuntimeId,
    pub(super) module: LoadedModule,
    pub(super) model: Arc<dyn RuntimeModel>,
}

/// The models bound to each image so far, each with why binding it failed
/// if it did.
pub(super) type RuntimeCache = std::cell::RefCell<
    std::collections::BTreeMap<
        crate::ModuleImageId,
        Vec<std::result::Result<Arc<dyn RuntimeModel>, runtime_model::Unbound>>,
    >,
>;

/// The most runtimes one module carries, which numbers each runtime by its
/// module and its place among the module's runtimes, the same at every
/// stop.
const RUNTIMES_PER_MODULE: u32 = 4;

/// The id of the `index`th runtime the module `module` carries.
fn runtime_id(module: crate::ModuleId, index: usize) -> Option<RuntimeId> {
    let index = u32::try_from(index)
        .ok()
        .filter(|index| *index < RUNTIMES_PER_MODULE)?;
    module
        .get()
        .checked_mul(RUNTIMES_PER_MODULE)?
        .checked_add(index)
        .map(RuntimeId::new)
}

/// One stop of the process, as a runtime model reads it.
struct ProcessStop<'a, P> {
    ptrace: &'a P,

    /// A stopped thread, through which memory is read.
    reader: Pid,
    breakpoints: &'a std::collections::BTreeMap<VirtualAddress, BreakpointSite>,
    bias: crate::LoadBias,

    /// Where the reads made are counted, when they are.
    usage: Option<&'a Cell<InspectionUsage>>,

    /// The process's threads, in the order of their ids.
    threads: Vec<ThreadId>,

    /// The sets of tasks threads drive, once found at this stop.
    task_sets: Vec<u64>,

    /// The memory read at the published stop, if the process is at one.
    memory: Option<&'a crate::runtime_model::StopMemory>,
}

impl<P: InspectionOps> RuntimeStop for ProcessStop<'_, P> {
    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
        if let Some(usage) = self.usage {
            let mut counted = usage.get();
            counted.memory_reads += 1;
            counted.memory_bytes += bytes.len() as u64;
            usage.set(counted);
        }

        match read_logical_memory(
            self.ptrace,
            self.reader,
            self.breakpoints,
            address,
            bytes.len(),
        ) {
            Ok(read) if read.bytes.len() == bytes.len() => {
                bytes.copy_from_slice(&read.bytes);
                true
            }
            _ => false,
        }
    }

    fn memory(&self) -> Option<&crate::runtime_model::StopMemory> {
        self.memory
    }

    fn thread_pointer(&self, thread: ThreadId) -> Option<u64> {
        let pid = debug_pid(thread).ok()?;
        self.ptrace
            .registers(pid)
            .ok()
            .map(|registers| registers.fs_base)
    }

    fn instruction(&self, thread: ThreadId) -> Option<VirtualAddress> {
        let pid = debug_pid(thread).ok()?;
        self.ptrace
            .registers(pid)
            .ok()
            .map(|registers| VirtualAddress::new(registers.rip))
    }

    fn load_bias(&self) -> crate::LoadBias {
        self.bias
    }

    fn task_sets(&self) -> Vec<u64> {
        self.task_sets.clone()
    }

    fn threads(&self) -> Vec<ThreadId> {
        self.threads.clone()
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Every loaded module carrying a runtime a model knows, with the model
    /// bound to it. A module whose runtime cannot be bound has none; see
    /// [`Self::unbound_runtimes`].
    pub(super) fn runtimes(&self, inferior: &Inferior) -> Vec<BoundRuntime> {
        self.loaded_images(inferior)
            .flat_map(|(module, image)| {
                self.bind_runtimes(image)
                    .into_iter()
                    .enumerate()
                    .filter_map(move |(index, model)| {
                        Some(BoundRuntime {
                            id: runtime_id(module.id, index)?,
                            module,
                            model: model.ok()?,
                        })
                    })
            })
            .collect()
    }

    /// Why each loaded module carrying a runtime a model knows cannot have
    /// it bound, as when its debug information is stripped.
    pub(super) fn unbound_runtimes(&self, inferior: &Inferior) -> Vec<Arc<str>> {
        self.loaded_images(inferior)
            .flat_map(|(_, image)| self.bind_runtimes(image))
            .filter_map(|runtime| Some(runtime.err()?.reason))
            .collect()
    }

    fn loaded_images<'a>(
        &'a self,
        inferior: &'a Inferior,
    ) -> impl Iterator<Item = (LoadedModule, &'a Arc<ModuleImage>)> {
        std::iter::once((inferior.loaded_module, &self.module_image)).chain(
            self.modules
                .values()
                .filter(|module| module.loaded.id != inferior.loaded_module.id)
                .map(|module| (module.loaded, &module.image)),
        )
    }

    /// The image of the loaded module whose range holds `address`, and the
    /// address within it.
    pub(super) fn module_holding<'a>(
        &'a self,
        inferior: &'a Inferior,
        address: VirtualAddress,
    ) -> Option<(&'a Arc<ModuleImage>, ImageAddress)> {
        self.loaded_images(inferior).find_map(|(module, image)| {
            module
                .image_address(address)
                .ok()
                .filter(|address| image.contains_address(*address))
                .map(|address| (image, address))
        })
    }

    fn bind_runtimes(
        &self,
        image: &Arc<ModuleImage>,
    ) -> Vec<std::result::Result<Arc<dyn RuntimeModel>, runtime_model::Unbound>> {
        self.runtime_models
            .borrow_mut()
            .entry(image.id())
            .or_insert_with(|| runtime_model::detect(&(Arc::clone(image) as _)))
            .clone()
    }

    /// Runs `read` against one runtime while the process is stopped,
    /// reading memory through the stopped thread `reader`.
    pub(super) fn with_runtime_stop<T>(
        &self,
        inferior: &Inferior,
        runtime: &BoundRuntime,
        reader: Pid,
        read: impl FnOnce(&dyn RuntimeStop) -> T,
    ) -> T {
        self.with_counted_runtime_stop(inferior, runtime, reader, None, read)
    }

    /// Runs `read` against one runtime as [`Self::with_runtime_stop`]
    /// does, counting the memory it reads in `usage`.
    fn with_counted_runtime_stop<T>(
        &self,
        inferior: &Inferior,
        runtime: &BoundRuntime,
        reader: Pid,
        usage: Option<&Cell<InspectionUsage>>,
        read: impl FnOnce(&dyn RuntimeStop) -> T,
    ) -> T {
        self.with_process_stop(inferior, &runtime.module, reader, usage, read)
    }

    /// Runs `read` against the process at a stop, as the code of `module`
    /// sees it, reading memory through the stopped thread `reader`.
    pub(super) fn with_module_stop<T>(
        &self,
        inferior: &Inferior,
        module: &LoadedModule,
        reader: Pid,
        read: impl FnOnce(&dyn RuntimeStop) -> T,
    ) -> T {
        self.with_process_stop(inferior, module, reader, None, read)
    }

    pub(super) fn with_process_stop<T>(
        &self,
        inferior: &Inferior,
        module: &LoadedModule,
        reader: Pid,
        usage: Option<&Cell<InspectionUsage>>,
        read: impl FnOnce(&dyn RuntimeStop) -> T,
    ) -> T {
        read(&ProcessStop {
            ptrace: &self.ptrace,
            reader,
            breakpoints: &inferior.breakpoints,
            bias: module.load_bias,
            usage,
            threads: inferior
                .threads
                .keys()
                .map(|pid| debug_thread_id(*pid))
                .collect(),
            task_sets: inferior
                .public_stop
                .as_ref()
                .and_then(|stop| {
                    let found = stop.task_sets.borrow();
                    found.as_ref().map(|found| found.sets.to_vec())
                })
                .unwrap_or_default(),
            memory: inferior.public_stop.as_ref().map(|stop| &stop.memory),
        })
    }

    /// The sets of tasks the futures the program's own threads drive run,
    /// which no runtime lists, found once for a published stop from the
    /// futures their stacks show, and why some may be missing.
    pub(super) fn task_sets(&self, inferior: &Inferior) -> Arc<[Arc<str>]> {
        let Some(stop) = inferior.public_stop.as_ref() else {
            return Arc::from([]);
        };

        if let Some(found) = stop.task_sets.borrow().as_ref() {
            return Arc::clone(&found.gaps);
        }

        let (sets, gaps) = self.find_task_sets(inferior, stop.triggering_thread);
        let gaps: Arc<[Arc<str>]> = gaps.into();
        *stop.task_sets.borrow_mut() = Some(TaskSets {
            sets: sets.into(),
            gaps: Arc::clone(&gaps),
        });
        gaps
    }

    fn find_task_sets(&self, inferior: &Inferior, reader: Pid) -> (Vec<u64>, Vec<Arc<str>>) {
        let runtimes = self.runtimes(inferior);

        // In the order of the threads that drive them, not of their
        // addresses, which change from run to run.
        let mut sets = Vec::new();
        let mut gaps = Vec::new();
        if runtimes.is_empty() {
            return (Vec::new(), gaps);
        }

        let modules = self.unwind_modules(inferior);
        for &pid in inferior.threads.keys() {
            // Only a thread no runtime schedules tasks on drives a future.
            if self.thread_activity(inferior, pid) != Some(ThreadActivity::Outside) {
                continue;
            }

            let root = StackRoot::of_thread(pid);
            let expanded = self.root_presentation(&root).and_then(|presentation| {
                let stack = self.physical_stack(inferior, &root, DEFAULT_MAX_FRAMES)?;
                self.expand_backtrace(
                    inferior,
                    &root,
                    &stack,
                    DEFAULT_MAX_FRAMES,
                    &modules,
                    presentation.as_ref(),
                )
            });
            let expanded = match expanded {
                Ok(expanded) => expanded,
                Err(error) => {
                    gaps.push(
                        format!(
                            "thread {}'s stack, where it may drive tasks, cannot be read: {error}",
                            debug_thread_id(pid)
                        )
                        .into(),
                    );
                    continue;
                }
            };

            // A set that a future that cannot be read runs is not found.
            if let Some(unread) = expanded.trace.unfollowed.first()
                && expanded.lost.iter().any(|ty| {
                    runtimes.iter().any(|runtime| {
                        runtime.module.image == ty.image && runtime.model.may_run_task_set(*ty)
                    })
                })
            {
                gaps.push(
                    format!(
                        "thread {} drives a future that may run a local set, whose tasks are not listed: {}",
                        debug_thread_id(pid),
                        unread.reason
                    )
                    .into(),
                );
            }

            for future in &expanded.futures {
                for runtime in runtimes
                    .iter()
                    .filter(|runtime| runtime.module.image == future.ty.image)
                {
                    let set = self.with_runtime_stop(inferior, runtime, reader, |stop| {
                        runtime.model.task_set(stop, future.object, future.ty)
                    });
                    if let Some(set) = set
                        && !sets.contains(&set)
                    {
                        sets.push(set);
                    }
                }
            }
        }

        (sets, gaps)
    }

    /// How to ask a runtime about `task`: by its number, and where the
    /// runtime said it keeps it when it listed it at this stop.
    pub(super) fn task_ref(inferior: &Inferior, task: TaskId) -> TaskRef {
        TaskRef {
            number: task.number,
            locator: inferior
                .public_stop
                .as_ref()
                .and_then(|stop| stop.task_locators.borrow().get(&task).copied()),
        }
    }

    /// One page of the tasks of every runtime at a stop.
    pub(super) fn tasks(
        &self,
        stop_id: StopId,
        from: Option<TaskCursor>,
        limit: usize,
        program_only: bool,
    ) -> Result<TaskPage> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let reader = inferior
            .public_stop
            .as_ref()
            .ok_or(Error::NotStopped)?
            .triggering_thread;
        let limit = limit.clamp(1, MAX_TASK_PAGE);
        let runtimes = self.runtimes(inferior);
        let mut cursor = from.unwrap_or(TaskCursor {
            runtime: 0,
            position: 0,
        });
        let mut tasks = Vec::new();

        // A runtime that cannot be read leaves its tasks out of every page.
        // So are the tasks of a set that cannot be found.
        let set_gaps = self.task_sets(inferior);
        let mut gaps = if from.is_none() {
            let mut gaps = self.unbound_runtimes(inferior);
            gaps.extend(set_gaps.iter().cloned());
            gaps
        } else {
            Vec::new()
        };

        let usage = Cell::new(InspectionUsage::default());
        while let Some(runtime) = runtimes.get(cursor.runtime) {
            let page =
                self.with_counted_runtime_stop(inferior, runtime, reader, Some(&usage), |stop| {
                    runtime
                        .model
                        .tasks(stop, cursor.position, limit - tasks.len(), program_only)
                });
            gaps.extend(page.gaps);
            if let Some(stop) = inferior.public_stop.as_ref() {
                stop.task_locators
                    .borrow_mut()
                    .extend(page.value.tasks.iter().map(|task| {
                        (
                            TaskId {
                                runtime: runtime.id,
                                number: task.number,
                            },
                            task.locator,
                        )
                    }));
            }

            tasks.extend(
                page.value
                    .tasks
                    .iter()
                    .map(|task| self.task_snapshot(inferior, runtime, task)),
            );
            cursor = page.value.next.map_or(
                TaskCursor {
                    runtime: cursor.runtime + 1,
                    position: 0,
                },
                |position| TaskCursor {
                    runtime: cursor.runtime,
                    position,
                },
            );
            if tasks.len() == limit {
                break;
            }
        }

        // A task suspended in its future waits for what that future does.
        for task in &mut tasks {
            if task.state == TaskState::Blocked
                && let Some(awaiting) =
                    self.task_awaiting(inferior, stop_id, task.id, reader, &usage)
            {
                task.detail = Some(awaiting);
            }
        }

        Ok(TaskPage {
            tasks: tasks.into(),
            next: (cursor.runtime < runtimes.len()).then_some(cursor),
            gaps: gaps.into(),
            usage: usage.get(),
        })
    }

    fn task_snapshot(
        &self,
        inferior: &Inferior,
        runtime: &BoundRuntime,
        task: &RuntimeTask,
    ) -> TaskSnapshot {
        let id = |number| TaskId {
            runtime: runtime.id,
            number,
        };

        let code = |code: CodeAddress| self.task_location(inferior, code);
        TaskSnapshot {
            id: id(task.number),
            noun: runtime.model.task_noun(),
            state: task.state.clone(),
            detail: task.detail.clone(),
            thread: task.thread,
            resume: task.resume.map(code),
            creation: task.creation.map(code).or_else(|| {
                task.spawned
                    .as_ref()
                    .map(|place| self.recorded_location(inferior, &runtime.module, place))
            }),
            entry: task.entry.map(|address| {
                code(CodeAddress {
                    address,
                    after_call: false,
                })
            }),
            parent: task.parent.map(id),
            internal: task.internal,
            labels: task.labels.clone().into(),
            object: (task.locator != 0).then(|| VirtualAddress::new(task.locator)),
        }
    }

    /// The function and source line of an address in a task's code.
    fn task_location(&self, inferior: &Inferior, code: CodeAddress) -> TaskLocation {
        let lookup = if code.after_call {
            VirtualAddress::new(code.address.get().saturating_sub(1))
        } else {
            code.address
        };

        let found = std::iter::once((inferior.loaded_module, &self.module_image))
            .chain(
                self.modules
                    .values()
                    .map(|module| (module.loaded, &module.image)),
            )
            .find_map(|(module, image)| {
                let address: ImageAddress = module.image_address(lookup).ok()?;
                image
                    .contains_address(address)
                    .then(|| (module, image.locate(address)))
            });
        let Some((module, location)) = found else {
            return TaskLocation {
                address: Some(code.address),
                module: None,
                function: None,
                source: None,
                recorded: None,
            };
        };

        TaskLocation {
            address: Some(code.address),
            module: Some(module.id),
            function: location
                .function
                .map(|function| function.name)
                .or_else(|| location.symbol.map(|symbol| symbol.name)),
            source: location.source,
            recorded: None,
        }
    }

    /// A place a runtime recorded in the source of its module, in the
    /// source file of the module's image that its path names, if one does.
    fn recorded_location(
        &self,
        inferior: &Inferior,
        module: &LoadedModule,
        place: &RecordedPlace,
    ) -> TaskLocation {
        let image = if module.id == inferior.loaded_module.id {
            Some(&self.module_image)
        } else {
            self.modules.get(&module.id).map(|found| &found.image)
        };

        let source = image
            .and_then(|image| {
                image
                    .source_file_matching(std::path::Path::new(&*place.path))
                    .ok()
            })
            .and_then(|file| {
                Some(SourceLocation {
                    file: file.id,
                    line: LineNumber::new(u64::from(place.line))?,
                    column: ColumnNumber::new(u64::from(place.column)),
                })
            });
        TaskLocation {
            address: None,
            module: Some(module.id),
            function: None,
            source,
            recorded: Some(place.clone()),
        }
    }
}

impl<P: InspectionOps> Controller<P> {
    /// What a stopped thread runs for the process's runtimes: the first
    /// task one names, or why one could not tell; `None` without a runtime.
    /// A published stop remembers each thread's; run control, which asks
    /// at a breakpoint's hit before any stop is published, reads afresh.
    pub(super) fn thread_activity(&self, inferior: &Inferior, pid: Pid) -> Option<ThreadActivity> {
        let Some(stop) = inferior.public_stop.as_ref() else {
            return self.read_thread_activity(inferior, pid);
        };

        if let Some(known) = stop.activities.borrow().get(&pid) {
            return known.clone();
        }

        let activity = self.read_thread_activity(inferior, pid);
        stop.activities.borrow_mut().insert(pid, activity.clone());
        activity
    }

    /// The task a stopped thread runs, as its runtime reads it from the
    /// thread's own state, without the runtime's shared state: what run
    /// control asks at a hit, before the threads still running, which may
    /// be changing that state, have stopped. `None` without a runtime.
    pub(super) fn current_task(
        &self,
        inferior: &Inferior,
        pid: Pid,
    ) -> Option<std::result::Result<Option<TaskId>, Arc<str>>> {
        let runtimes = self.runtimes(inferior);
        let mut found = self
            .unbound_runtimes(inferior)
            .into_iter()
            .next()
            .map_or_else(
                || (!runtimes.is_empty()).then_some(Ok(None)),
                |reason| Some(Err(reason)),
            );
        for runtime in &runtimes {
            let current = self.with_runtime_stop(inferior, runtime, pid, |stop| {
                runtime.model.current_task(stop, debug_thread_id(pid))
            });
            match current {
                Ok(Some(number)) => {
                    return Some(Ok(Some(TaskId {
                        runtime: runtime.id,
                        number,
                    })));
                }
                Ok(None) => {}
                Err(reason) => found = Some(Err(reason)),
            }
        }

        found
    }

    fn read_thread_activity(&self, inferior: &Inferior, pid: Pid) -> Option<ThreadActivity> {
        let runtimes = self.runtimes(inferior);

        // A thread may run the tasks of a runtime that cannot be read. One
        // runtime's idle thread is idle whatever another says of it.
        let mut found = self
            .unbound_runtimes(inferior)
            .into_iter()
            .next()
            .map_or_else(
                || (!runtimes.is_empty()).then_some(ThreadActivity::Outside),
                |reason| Some(ThreadActivity::Unknown(reason)),
            );
        for runtime in &runtimes {
            let activity = self.with_runtime_stop(inferior, runtime, pid, |stop| {
                runtime.model.thread_activity(stop, debug_thread_id(pid))
            });
            match activity {
                runtime_model::ThreadActivity::Task { number, stack } => {
                    return Some(ThreadActivity::Task {
                        task: TaskId {
                            runtime: runtime.id,
                            number,
                        },
                        stack,
                    });
                }
                runtime_model::ThreadActivity::Idle => {
                    if found == Some(ThreadActivity::Outside) {
                        found = Some(ThreadActivity::Idle);
                    }
                }
                runtime_model::ThreadActivity::Outside => {}
                runtime_model::ThreadActivity::Unknown(reason) => {
                    found = Some(ThreadActivity::Unknown(reason));
                }
            }
        }

        found
    }
}

impl<P: InspectionOps> Controller<P> {
    /// The stack of the task a stopped thread runs, as its runtime bounds it
    /// now, or `None` when the thread runs no task or its stack is unknown.
    pub(super) fn task_stack(&self, inferior: &Inferior, pid: Pid) -> Option<TaskStack> {
        let Some(ThreadActivity::Task { task, .. }) = self.thread_activity(inferior, pid) else {
            return None;
        };

        let runtime = self
            .runtimes(inferior)
            .into_iter()
            .find(|runtime| runtime.id == task.runtime)?;
        let stacks = self.with_runtime_stop(inferior, &runtime, pid, |stop| {
            runtime.model.thread_stacks(stop, debug_thread_id(pid))
        });
        stacks
            .ok()?
            .into_iter()
            .find(|(_, segment)| *segment == StackSegment::Task)
            .map(|(bounds, _)| TaskStack {
                task,
                low: bounds.start,
                high: bounds.end,
            })
    }

    /// The bounds of a task's stack now, or `None` when its runtime has no
    /// such task.
    pub(super) fn task_stack_bounds(
        &self,
        inferior: &Inferior,
        task: TaskId,
    ) -> std::result::Result<Option<std::ops::Range<u64>>, Arc<str>> {
        let runtime = self
            .runtimes(inferior)
            .into_iter()
            .find(|runtime| runtime.id == task.runtime)
            .ok_or("the task's runtime is no longer loaded")?;
        self.with_runtime_stop(inferior, &runtime, inferior.memory_thread(), |stop| {
            runtime
                .model
                .task_stack(stop, Self::task_ref(inferior, task))
        })
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Whether a runtime in the process tolerates `signal` arriving late.
    pub(super) fn defers(&self, signal: Signal) -> bool {
        // A runtime that cannot be bound uses signals as it does bound.
        self.inferior.as_ref().is_some_and(|inferior| {
            self.loaded_images(inferior)
                .flat_map(|(_, image)| self.bind_runtimes(image))
                .any(|runtime| {
                    let deferrable = match &runtime {
                        Ok(model) => model.signals().deferrable,
                        Err(unbound) => unbound.deferrable,
                    };

                    deferrable.contains(&signal.number())
                })
        })
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Where the frames of a thread or task begin at a stop: a task
    /// running on a thread begins in that thread's registers, and a parked
    /// one in the registers its runtime saved.
    pub(super) fn stack_root(
        &self,
        stop_id: StopId,
        context: ExecutionContext,
    ) -> Result<StackRoot> {
        let task = match context {
            ExecutionContext::Thread(thread) => {
                return Ok(StackRoot::of_thread(debug_pid(thread)?));
            }
            ExecutionContext::Task(task) => task,
        };

        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let reader = inferior
            .public_stop
            .as_ref()
            .ok_or(Error::NotStopped)?
            .triggering_thread;
        self.task_root(inferior, task, reader)?
            .ok_or(Error::UnknownTask(task))
    }

    /// Where a task's frames begin, its memory read through the stopped
    /// thread `reader`, or `None` when its runtime has no such task.
    pub(super) fn task_root(
        &self,
        inferior: &Inferior,
        task: TaskId,
        reader: Pid,
    ) -> Result<Option<StackRoot>> {
        let Some(runtime) = self
            .runtimes(inferior)
            .into_iter()
            .find(|runtime| runtime.id == task.runtime)
        else {
            return Ok(None);
        };

        self.task_sets(inferior);
        let found = self.with_runtime_stop(inferior, &runtime, reader, |stop| {
            runtime
                .model
                .task_context(stop, Self::task_ref(inferior, task))
        });
        let origin = match found {
            Ok(Some(TaskContext::OnThread(thread))) => RootOrigin::Thread(debug_pid(thread)?),
            Ok(Some(TaskContext::Saved {
                registers,
                after_call,
            })) => RootOrigin::Saved {
                registers,
                after_call,
                reader,
            },
            Ok(Some(TaskContext::Suspended { future, ty })) => RootOrigin::Suspended {
                future,
                ty,
                module: runtime.module,
                reader,
            },
            Ok(None) => return Ok(None),
            Err(reason) => return Err(Error::TaskUnavailable { task, reason }),
        };

        Ok(Some(StackRoot {
            context: ExecutionContext::Task(task),
            origin,
        }))
    }
}

impl<P: InspectionOps> Controller<P> {
    /// The thread a thread or task runs on at a stop, for requests that
    /// act on a thread, such as stepping.
    pub(super) fn context_thread(&self, stop_id: StopId, context: ExecutionContext) -> Result<Pid> {
        let root = self.stack_root(stop_id, context)?;
        match (root.thread(), context) {
            (Some(pid), _) => Ok(pid),
            (None, ExecutionContext::Task(task)) => Err(Error::TaskParked(task)),
            (None, ExecutionContext::Thread(thread)) => Err(Error::UnknownThread(thread)),
        }
    }
}
