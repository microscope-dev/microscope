//! Plants a native breakpoint on V8's addon entry trampoline during `StepKind::IntoSource`
//! so stepping into a native addon hands off to `ptrace` and stepping back out returns to V8.

use std::sync::Arc;

use nix::unistd::Pid;

use super::super::Controller;
use super::super::cdp::Session;
use super::super::native::LinuxTraceOps;
use super::steps::Stepping;
use crate::runtime_model::{RuntimeStep, inspector_protocol};
use crate::{Result, RuntimeId, StepKind, VirtualAddress};

#[derive(Debug, Clone)]
pub(super) struct Through {
    pub(super) session: Session,
    pub(super) thread: Pid,

    pub(super) depth: usize,
}

pub(super) fn returning(
    through: Option<Through>,
    step: &RuntimeStep,
) -> std::result::Result<Stepping, Arc<str>> {
    let through = through
        .filter(|through| super::super::debug_thread_id(through.thread) == step.thread)
        .ok_or("V8 runs no step back to JavaScript for this thread")?;
    Ok(Stepping {
        number: step.number,
        kind: step.kind,
        session: through.session,
        depth: None,
        command: inspector_protocol::STEP_INTO,
        sent: true,
        skipped: 0,
    })
}

impl<P: LinuxTraceOps> Controller<P> {
    fn native_entry(&self, runtime: RuntimeId) -> Option<VirtualAddress> {
        let inferior = self.session.inferior()?;
        let bound = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)?;
        bound
            .module
            .virtual_address(bound.model.native_entry()?)
            .ok()
    }

    pub(in crate::backend::linux) fn install_native_entry(
        &mut self,
        runtime: RuntimeId,
        kind: StepKind,
        execution: crate::protocol::ExecutionId,
    ) -> Result<()> {
        if kind != StepKind::IntoSource {
            return Ok(());
        }
        let Some(entry) = self.native_entry(runtime) else {
            record!("V8 enters no Node-API function microscope can find; a step steps over them");
            return Ok(());
        };
        let Some(inferior) = self.session.inferior_mut() else {
            return Ok(());
        };
        let paused = inferior
            .inspectors
            .records
            .get(&runtime)
            .and_then(|inspector| inspector.step.as_ref())
            .is_some_and(|step| step.depth.is_some());
        if !paused {
            return Ok(());
        }
        super::super::breakpoints::install_plan_breakpoint(&self.ptrace, inferior, entry, execution)
    }

    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(in crate::backend::linux) fn reach_native_entry(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> bool {
        let Some(active) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
        else {
            return false;
        };
        let super::super::ActiveKind::RuntimeStep {
            owner,
            kind: StepKind::IntoSource,
            runtime,
            number,
        } = active.kind
        else {
            return false;
        };
        if self.native_entry(runtime) != Some(address) {
            return false;
        }
        if !self.runs_step(owner, pid) {
            return true;
        }
        let target = match self.read_hook(pid, runtime, |model, stop, registers| {
            model.native_callee(stop, registers)
        }) {
            Ok(target) => target,
            Err(reason) => {
                record!("the step cannot tell what the Node-API call runs: {reason}");
                return true;
            }
        };
        if !self.step_into_native(pid, runtime, number, target) {
            return true;
        }
        let Some(inspector) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return true;
        };
        inspector.through = inspector.step.as_ref().and_then(|step| {
            Some(Through {
                session: step.session.clone(),
                thread: pid,
                depth: step.depth?,
            })
        });
        true
    }

    pub(in crate::backend::linux) fn native_step_returned_to_javascript(
        &self,
        pid: Pid,
        kind: StepKind,
    ) -> Result<bool> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(false);
        }
        let Some(inferior) = self.session.inferior() else {
            return Ok(false);
        };
        if !inferior.inspectors.records.values().any(|inspector| {
            inspector
                .through
                .as_ref()
                .is_some_and(|through| through.thread == pid)
        }) {
            return Ok(false);
        }
        self.returned_into_unlined_code(pid)
    }

    pub(in crate::backend::linux) fn returns_through(&self, pid: Pid) -> Option<RuntimeId> {
        self.session
            .inferior()?
            .inspectors
            .records
            .iter()
            .find(|(_, inspector)| {
                inspector
                    .through
                    .as_ref()
                    .is_some_and(|through| through.thread == pid)
            })
            .map(|(&runtime, _)| runtime)
    }
}
