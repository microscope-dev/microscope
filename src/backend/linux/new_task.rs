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

#[derive(Debug, Clone, Copy)]
pub(super) struct Starter {
    runtime: RuntimeId,

    entry: ImageAddress,

    names_at_entry: bool,
}

#[derive(Debug, Clone)]
pub(super) enum NewTask {
    Watching(BTreeMap<VirtualAddress, Starter>),

    Starting {
        starters: BTreeMap<VirtualAddress, Starter>,
        starter: Starter,
        thread: Pid,
        returns_to: VirtualAddress,
        stack: u64,
    },

    Started {
        entries: BTreeSet<VirtualAddress>,
        statements: bool,
    },

    Entered,
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn task_starters(&self) -> BTreeMap<VirtualAddress, Starter> {
        let mut starters = BTreeMap::new();
        let Some(inferior) = self.session.inferior() else {
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

    pub(super) fn reach_new_task(&mut self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        let Some(state) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
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
                let starter = *starters
                    .get(&address)
                    .ok_or_else(|| Error::internal("a starter the guard found is listed"))?;
                if starter.names_at_entry {
                    self.start_at_entry(pid, address, starter)?;
                } else {
                    self.enter_starter(pid, address, starter, starters)?;
                }
            }

            NewTask::Starting {
                starter,
                thread,
                returns_to,
                stack,
                ..
            } if returns_to == address => {
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

    fn enter_starter(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        starter: Starter,
        starters: BTreeMap<VirtualAddress, Starter>,
    ) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        let returns_to = VirtualAddress::new(self.ptrace.read_word(pid, registers.rsp)?);
        record!("thread {pid} starts a task, returning to {returns_to}");
        let execution = self.active_execution()?;
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        install_plan_breakpoint(&self.ptrace, inferior, returns_to, execution)?;

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

    fn started_task(
        &self,
        pid: Pid,
        starter: Starter,
        registers: &nix::libc::user_regs_struct,
    ) -> std::result::Result<Option<StartedTask>, std::sync::Arc<str>> {
        let inferior = self.session.inferior().ok_or("the process is gone")?;
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
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        for entry in &entries {
            install_plan_breakpoint(&self.ptrace, inferior, *entry, execution)?;
        }

        if let Some(ActiveKind::Step { owner, start, .. }) =
            inferior.run.execution_mut().map(|active| &mut active.kind)
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

    fn new_task_statements(&self, coroutine: TypeReference) -> Result<BTreeSet<VirtualAddress>> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
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

    fn arrive_in_new_task(&mut self, pid: Pid) -> Result<()> {
        record!("the step's new task begins on thread {pid}");
        let (execution, kind) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
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
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { kind, .. } => Some(*kind),
                _ => None,
            })
            .ok_or(Error::NotRunning)?;

        self.go_on_without_plan(pid, address, Some(kind))
    }

    pub(super) fn requested_step_reason(&self, reason: StopReason) -> StopReason {
        let Some((running, requested)) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
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
    pub(super) fn entered_new_task(&self) -> bool {
        self.session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .is_some_and(|active| {
                matches!(
                    &active.kind,
                    ActiveKind::Step { start, .. }
                        if matches!(start.new_task, Some(NewTask::Entered))
                )
            })
    }
}
