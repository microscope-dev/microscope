//! Stepping into the task a line starts.
//!
//! The step runs as a step over its line while it watches the entry of
//! each runtime's task starter. When the step's own task enters one, the
//! runtime names the task it starts and where the task begins: from the
//! starter's arguments at once, or once the starter returns on that
//! thread. The step then belongs to the new task: it waits at that entry
//! for the task to run, and goes on from there as a step in, through
//! wrappers, to the first statement of the program's own code. A task
//! whose coroutine's body is inlined, as into its runtime's poll, has no
//! entry: the step waits at the body's statements, and ends at the first
//! the task runs. A line that starts no task ends as a step over.

use std::collections::{BTreeMap, BTreeSet};

use nix::unistd::Pid;

use crate::protocol::{StepKind, StopReason};
use crate::runtime_model::StartedTask;
use crate::{
    CodeInstanceKind, Error, ImageAddress, Result, RuntimeId, TaskId, TypeReference, VirtualAddress,
};

use super::breakpoints::install_plan_breakpoint;
use super::native::{InspectionOps, LinuxTraceOps};
use super::registers::x86_64_registers;
use super::{ActiveKind, Controller, StepOwner, StepStart};

/// A runtime's task starter that a step watches.
#[derive(Debug, Clone, Copy)]
pub(super) struct Starter {
    runtime: RuntimeId,

    /// Where the starter begins in its image, by which its runtime knows
    /// it.
    entry: ImageAddress,

    /// Whether the starter names its task as it begins, rather than once
    /// it returns.
    names_at_entry: bool,
}

/// How far a step into a new task has followed the task's start.
#[derive(Debug, Clone)]
pub(super) enum NewTask {
    /// Watching for the step's task to enter a runtime's task starter, at
    /// these entries.
    Watching(BTreeMap<VirtualAddress, Starter>),

    /// The step's task entered `starter` on `thread`, which returns to
    /// `returns_to` with the stack pointer at `stack`.
    Starting {
        starters: BTreeMap<VirtualAddress, Starter>,
        starter: Starter,
        thread: Pid,
        returns_to: VirtualAddress,
        stack: u64,
    },

    /// The task started, and the step waits for it where it begins: at
    /// its function's entry, or at any statement of its coroutine's
    /// inlined body, the first of which it runs ends the step.
    Started {
        entries: BTreeSet<VirtualAddress>,
        statements: bool,
    },

    /// The task began, and the step goes on in it as a step in.
    Entered,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// The entries of the task starters of every runtime loaded, which a
    /// step into a new task watches.
    pub(super) fn task_starters(&self) -> BTreeMap<VirtualAddress, Starter> {
        let mut starters = BTreeMap::new();
        let Some(inferior) = self.inferior.as_ref() else {
            return starters;
        };

        for runtime in self.runtimes(inferior) {
            for starter in runtime.model.task_starters() {
                if let Ok(address) = runtime.module.virtual_address(starter.entry) {
                    starters.insert(
                        address,
                        Starter {
                            runtime: runtime.id,
                            entry: starter.entry,
                            names_at_entry: starter.names_at_entry,
                        },
                    );
                }
            }
        }

        starters
    }

    /// Follows a step into a new task at one of its plan's breakpoints,
    /// which the step's task reached. Returns whether the site was one of
    /// the task's start and has been handled.
    pub(super) fn reach_new_task(&mut self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        let Some(state) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => start.new_task.clone(),
                _ => None,
            })
        else {
            return Ok(false);
        };

        match state {
            NewTask::Watching(starters) | NewTask::Starting { starters, .. }
                if starters.contains_key(&address) =>
            {
                let starter = starters[&address];
                if starter.names_at_entry {
                    self.start_at_entry(pid, address, starter)?;
                } else {
                    self.enter_starter(pid, address, starters)?;
                }
            }

            NewTask::Starting {
                starter,
                thread,
                returns_to,
                stack,
                ..
            } if returns_to == address => {
                // The return of another call, as of a starter the task's
                // handler for a signal entered, is not the start's.
                if thread == pid && self.ptrace.registers(pid)?.rsp == stack {
                    self.return_from_starter(pid, address, starter)?;
                } else {
                    self.repair_when_alone(pid, address)?;
                }
            }

            NewTask::Started {
                entries,
                statements,
            } if entries.contains(&address) => {
                if statements {
                    self.arrive_in_new_task(pid)?;
                } else {
                    self.enter_new_task(pid, address)?;
                }
            }
            _ => return Ok(false),
        }

        Ok(true)
    }

    /// Follows the task a starter that names it as it begins starts, or
    /// goes on watching when this call starts none.
    fn start_at_entry(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        starter: Starter,
    ) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        match self.started_task(pid, starter, &registers) {
            Ok(Some(task)) => self.follow_new_task(pid, address, starter.runtime, &task),
            Ok(None) => self.repair_when_alone(pid, address),
            Err(reason) => {
                self.lose_new_task(pid, format!("the task started is unknown: {reason}"))
            }
        }
    }

    /// Waits for the return of the task starter the step's task entered.
    fn enter_starter(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        starters: BTreeMap<VirtualAddress, Starter>,
    ) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        let returns_to = VirtualAddress::new(self.ptrace.read_word(pid, registers.rsp)?);
        record!("thread {pid} starts a task, returning to {returns_to}");
        let execution = self.active_execution()?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        install_plan_breakpoint(&self.ptrace, inferior, returns_to, execution)?;

        let starter = starters[&address];

        if let Some(start) = self.active_step_mut() {
            start.new_task = Some(NewTask::Starting {
                starters,
                starter,
                thread: pid,
                returns_to,
                stack: registers.rsp.wrapping_add(8),
            });
        }

        self.repair_when_alone(pid, address)
    }

    /// Moves the step to the task the starter returned.
    fn return_from_starter(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        starter: Starter,
    ) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        match self.started_task(pid, starter, &registers) {
            Ok(Some(task)) => self.follow_new_task(pid, address, starter.runtime, &task),
            Ok(None) => self.lose_new_task(pid, "the task starter returned no task".into()),
            Err(reason) => {
                self.lose_new_task(pid, format!("the task started is unknown: {reason}"))
            }
        }
    }

    /// The task a starter starts, as its runtime reads it from the
    /// registers of the thread in the starter.
    fn started_task(
        &self,
        pid: Pid,
        starter: Starter,
        registers: &nix::libc::user_regs_struct,
    ) -> std::result::Result<Option<StartedTask>, std::sync::Arc<str>> {
        let inferior = self.inferior.as_ref().ok_or("the process is gone")?;
        self.runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == starter.runtime)
            .ok_or_else(|| "its runtime is gone".into())
            .and_then(|bound| {
                self.with_runtime_stop(inferior, &bound, pid, |stop| {
                    bound
                        .model
                        .started_task(stop, starter.entry, &x86_64_registers(registers))
                })
            })
    }

    /// Moves the step to the task a starter started, and waits for the
    /// task where it begins.
    fn follow_new_task(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        runtime: RuntimeId,
        started: &StartedTask,
    ) -> Result<()> {
        let (entries, statements) = match (started.task.entry, started.coroutine) {
            (Some(entry), _) => (BTreeSet::from([entry]), false),
            (None, Some(coroutine)) => (self.new_task_statements(coroutine)?, true),
            (None, None) => (BTreeSet::new(), false),
        };

        if entries.is_empty() {
            return self.lose_new_task(pid, "the task started begins nowhere".into());
        }

        let task = TaskId {
            runtime,
            number: started.task.number,
        };

        record!("the step goes on in task {task}, which begins at {entries:?}");
        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for entry in &entries {
            install_plan_breakpoint(&self.ptrace, inferior, *entry, execution)?;
        }

        if let Some(ActiveKind::Step { owner, start, .. }) =
            inferior.active.as_mut().map(|active| &mut active.kind)
        {
            *owner = StepOwner {
                thread: pid,
                task: Some(task),
            };
            **start = StepStart {
                plan_addresses: entries.clone(),
                new_task: Some(NewTask::Started {
                    entries,
                    statements,
                }),
                ..StepStart::default()
            };
        }

        self.go_on_without_plan(pid, address, None)
    }

    /// The statements of every copy of `coroutine`'s body inlined into
    /// other code, less the code that resumes it.
    fn new_task_statements(&self, coroutine: TypeReference) -> Result<BTreeSet<VirtualAddress>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let mut statements = BTreeSet::new();
        if coroutine.image != inferior.loaded_module.image {
            return Ok(statements);
        }

        for function in self.main.image.coroutine_functions(coroutine.id) {
            for instance in self.main.image.instances_for_function(function.id()) {
                if matches!(instance.kind(), CodeInstanceKind::Inline { .. }) {
                    statements.extend(self.body_statements(instance.id())?);
                }
            }
        }

        Ok(self.without_resume_code(statements))
    }

    /// Ends the step where the new task runs its first statement of its
    /// coroutine's inlined body.
    fn arrive_in_new_task(&mut self, pid: Pid) -> Result<()> {
        record!("the step's new task begins on thread {pid}");
        let (execution, kind) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { kind, .. } => Some((active.id, *kind)),
                _ => None,
            })
            .ok_or(Error::NotRunning)?;

        self.cleanup_plan_breakpoints(execution)?;
        if let Some(start) = self.active_step_mut() {
            start.new_task = Some(NewTask::Entered);
        }

        self.begin_visible_stop(pid, StopReason::Step { kind })
    }

    /// Goes on as a step in from where the new task begins, as if the
    /// task had just called its first function.
    fn enter_new_task(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        let position = self.stack_position(pid, &registers);
        let activation = self.top_activation(pid, &registers).ok();
        record!("the step's new task begins on thread {pid}");
        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        if let Some(start) = self.active_step_mut() {
            *start = StepStart {
                activation,
                stack_pointer: Some(position),
                following: true,
                new_task: Some(NewTask::Entered),
                ..StepStart::default()
            };
        }

        let kind = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { kind, .. } => Some(*kind),
                _ => None,
            })
            .ok_or(Error::NotRunning)?;

        self.go_on_without_plan(pid, address, Some(kind))
    }

    /// The stop a step reports for `reason`, naming the kind of step the
    /// client asked for when the step ran as another kind. Any other stop,
    /// such as an advance reaching its location, names itself.
    pub(super) fn requested_step_reason(&self, reason: StopReason) -> StopReason {
        let Some((running, requested)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step {
                    kind, requested, ..
                } => Some((*kind, *requested)),
                _ => None,
            })
        else {
            return reason;
        };

        match reason {
            StopReason::Step { kind } if kind == running => StopReason::Step { kind: requested },
            StopReason::StepIncomplete { kind, description } if kind == running => {
                StopReason::StepIncomplete {
                    kind: requested,
                    description,
                }
            }
            StopReason::FutureDropped { kind } if kind == running => {
                StopReason::FutureDropped { kind: requested }
            }
            StopReason::TaskEnded { kind, task, ending } if kind == running => {
                StopReason::TaskEnded {
                    kind: requested,
                    task,
                    ending,
                }
            }
            reason => reason,
        }
    }

    /// Ends a step into a new task that cannot follow it, where the step's
    /// task is.
    fn lose_new_task(&mut self, pid: Pid, description: String) -> Result<()> {
        record!("the step lost its new task: {description}");
        self.begin_visible_stop(
            pid,
            StopReason::StepIncomplete {
                kind: StepKind::IntoNewTask,
                description: description.into(),
            },
        )
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Whether the active step went on into the new task it started,
    /// whose stop then reveals inline frames as a step in's does.
    pub(super) fn entered_new_task(&self) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(|active| {
                matches!(
                    &active.kind,
                    ActiveKind::Step { start, .. }
                        if matches!(start.new_task, Some(NewTask::Entered))
                )
            })
    }
}
