//! Steps into a Node-API addon's C function and back.
//!
//! V8 steps over a call to a native function. A JavaScript step into
//! begun where V8 paused the thread therefore has a trap in its plan where
//! V8 enters Node-API callbacks (`RuntimeModel::native_entry`). When the
//! step's own thread reaches it, the callback is read from the call's info
//! and the step ends at the callback's first statement, as any step into
//! native code does.
//!
//! V8 still runs its `stepInto` then: it pauses at the next statement of
//! JavaScript the thread runs, which is the caller's once the addon
//! returns, or a function the addon calls. `Through` records that. A
//! native step from the addon that returns into V8 becomes a JavaScript
//! step that ends at that pause, in the isolate the step began in. A pause
//! no one claims deeper than the step began, in a function the addon
//! calls, continues with `stepOut`, which V8 ends in the caller once the
//! addon returns, so the step back stays armed.

use std::sync::Arc;

use nix::unistd::Pid;

use super::super::Controller;
use super::super::cdp::Session;
use super::super::native::LinuxTraceOps;
use super::steps::Stepping;
use crate::runtime_model::{RuntimeStep, inspector_protocol};
use crate::{Result, RuntimeId, StepKind, VirtualAddress};

/// A `stepInto` V8 still runs for a thread that a JavaScript step left in
/// an addon's C function.
#[derive(Debug, Clone)]
pub(super) struct Through {
    /// The isolate whose pause the step began at.
    pub(super) session: Session,
    pub(super) thread: Pid,

    /// How many frames the paused thread had where the step began.
    pub(super) depth: usize,
}

/// The step V8 runs back from an addon for `step`, which a native step
/// returning into V8 became, by the `stepInto` `through` records.
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
        // V8's next pause in the isolate ends it.
        depth: None,
        command: inspector_protocol::STEP_INTO,
        sent: true,
        skipped: 0,
    })
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Where `runtime`, loaded, enters a native function a step into a
    /// call of one may end in.
    fn native_entry(&self, runtime: RuntimeId) -> Option<VirtualAddress> {
        let inferior = self.inferior.as_ref()?;
        let bound = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)?;
        bound
            .module
            .virtual_address(bound.model.native_entry()?)
            .ok()
    }

    /// Plants, in the plan of execution `execution`, a trap where
    /// `runtime` enters native functions, for a JavaScript step into that
    /// begins where V8 holds the thread paused.
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
        let Some(inferior) = self.inferior.as_mut() else {
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

    /// Whether thread `pid`, at `address`, entered a native function for
    /// the active JavaScript step into it runs: the step then ends at the
    /// function's first statement, where V8's own step still runs.
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
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
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
            // Another thread's call is no step's.
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
            .inferior
            .as_mut()
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

    /// Whether the active native step of `pid`, of `kind`, has returned
    /// from the addon function a JavaScript step left the thread in, into
    /// V8, which still runs that step's `stepInto`.
    pub(in crate::backend::linux) fn native_step_returned_to_javascript(
        &self,
        pid: Pid,
        kind: StepKind,
    ) -> Result<bool> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(false);
        }
        let Some(inferior) = self.inferior.as_ref() else {
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

    /// The runtime whose inspector runs a step back to JavaScript for
    /// `pid`, if one does.
    pub(in crate::backend::linux) fn returns_through(&self, pid: Pid) -> Option<RuntimeId> {
        self.inferior
            .as_ref()?
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
