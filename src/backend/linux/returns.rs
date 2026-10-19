//! The values a function returned when a step out finished it. They are
//! read the instant its call returns, where the step stops, as the
//! function's calling convention left them, and kept with the stop, which
//! lists them among the variables of the frame they returned to.

use std::sync::Arc;

use nix::unistd::Pid;

use crate::debug_info::ReturnedValue;
use crate::inspection::InspectionBudget;
use crate::protocol::StopId;
use crate::{
    ImageAddress, ModuleId, Result, StackFrameId, StepKind, StopReason, Variable, VariableKind,
    VirtualAddress,
};

use super::activation::Activation;
use super::callers::FrameAt;
use super::frames::{FrameRegisters, StackRoot};
use super::inspection::{LinuxVariableRuntime, variable_context};
use super::native::InspectionOps;
use super::{ActiveKind, Controller, Inferior, RuntimeModule};

/// A function a step out is finishing: where its activation is, where its
/// call returns, and an address of its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Returning {
    pub(super) activation: Activation,
    pub(super) return_address: VirtualAddress,

    /// The loaded module holding the function, and an address of its code
    /// in that module's image.
    pub(super) module: ModuleId,
    pub(super) function: ImageAddress,
}

/// What a finished function returned, read in the module that describes
/// it.
#[derive(Debug, Clone)]
pub(super) struct Returned {
    module: ModuleId,
    thread: Pid,
    values: Arc<[ReturnedValue]>,
}

impl<P: InspectionOps> Controller<P> {
    /// What the function a step out finished returned, which the step read
    /// as `pid` returned from its call, or reads now. A step that stopped
    /// without its call returning, as where a loop's body finished or a
    /// panic unwound, returned nothing to show.
    pub(super) fn capture_returned(&self, pid: Pid, reason: &StopReason) -> Option<Returned> {
        if !matches!(
            reason,
            StopReason::Step {
                kind: StepKind::Out
            } | StopReason::TaskEnded {
                kind: StepKind::Out,
                ..
            }
        ) {
            return None;
        }

        let inferior = self.inferior.as_ref()?;
        let ActiveKind::Step { start, .. } = &inferior.active.as_ref()?.kind else {
            return None;
        };

        start
            .returned
            .clone()
            .or_else(|| self.read_returned(pid, start.returning?))
    }

    /// Reads what a step out's function returned when its call has just
    /// returned to `pid`, which the step may then go on from, through code
    /// no line describes, to where it stops.
    pub(super) fn note_returned_values(&mut self, pid: Pid) {
        let Some(returning) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step {
                    owner,
                    kind: StepKind::Out,
                    start,
                    ..
                } if self.runs_step(*owner, pid) && start.returned.is_none() => start.returning,
                _ => None,
            })
        else {
            return;
        };

        let returned = self.read_returned(pid, returning);
        if let Some(ActiveKind::Step { start, .. }) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .map(|active| &mut active.kind)
        {
            start.returned = returned;
        }
    }

    /// What a function returned, when `pid` is the instant its call
    /// returned: at its return address, with its activation's stack just
    /// popped.
    fn read_returned(&self, pid: Pid, returning: Returning) -> Option<Returned> {
        let inferior = self.inferior.as_ref()?;
        let registers = self.ptrace.registers(pid).ok()?;
        if VirtualAddress::new(registers.rip) != returning.return_address
            || !returning
                .activation
                .just_returned(self.stack_position(pid, &registers))
        {
            return None;
        }

        let module = self.modules.get(&returning.module)?;
        let frame = FrameRegisters::Thread(registers);
        let mut runtime = self.thread_runtime(inferior, module, pid, &frame);
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        match module
            .variables
            .returned(returning.function, &mut runtime, &mut budget)
        {
            Ok(Some(mut values)) => {
                // An async body's result is named for the function its
                // programmer wrote, as the body is.
                if let Some(function) = module
                    .image
                    .locate(returning.function)
                    .physical_instance
                    .and_then(|instance| module.image.code_instance(instance))
                    .and_then(|instance| module.image.function(instance.function()))
                    .filter(|function| function.coroutine().is_some())
                {
                    for value in &mut values {
                        value.name = Arc::from(function.name());
                    }
                }

                Some(Returned {
                    module: module.loaded.id,
                    thread: pid,
                    values: values.into(),
                })
            }
            Ok(None) => None,

            #[cfg_attr(
                not(debug_assertions),
                expect(unused_variables, reason = "only recorded")
            )]
            Err(error) => {
                record!("the returned values are unreadable: {error}");
                None
            }
        }
    }

    /// The values a step out's function returned, as variables of the
    /// innermost frame of the thread the step stopped, presented at the
    /// stop `stop_id`.
    pub(super) fn returned_variables(
        &self,
        returned: &Returned,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>> {
        // The thread, or the task it runs, whose innermost frame the
        // function returned to.
        if frame != StackFrameId::INNERMOST || root.thread() != Some(returned.thread) {
            return Ok(Vec::new());
        }

        let Some(module) = self.modules.get(&returned.module) else {
            return Ok(Vec::new());
        };

        let inferior = self.inferior.as_ref().ok_or(crate::Error::NotRunning)?;
        let registers = FrameRegisters::Thread(self.ptrace.registers(returned.thread)?);
        let mut runtime = self.thread_runtime(inferior, module, returned.thread, &registers);
        let context = variable_context(stop_id, root.context, frame, module, None);
        let mut variables = Vec::with_capacity(returned.values.len());
        for value in returned.values.iter() {
            let (type_info, state) = match &value.value {
                Ok(located) => {
                    let inspected =
                        module
                            .variables
                            .materialize(located, context, &mut runtime, budget)?;
                    (inspected.type_info, inspected.state)
                }
                Err(state) => (
                    value.ty.and_then(|id| {
                        module
                            .image
                            .type_info(crate::TypeReference {
                                image: module.loaded.image,
                                id,
                            })
                            .cloned()
                    }),
                    state.clone(),
                ),
            };

            variables.push(Variable {
                kind: VariableKind::Returned,
                global: None,
                name: Arc::clone(&value.name),
                declaration: None,
                type_info,
                unresolved_shape: value.unresolved_shape.clone(),
                state,
            });
        }

        Ok(variables)
    }

    /// Reads a module's values with a stopped thread's own registers. No
    /// returned value needs a call-frame address: its function's frame is
    /// gone.
    fn thread_runtime<'a>(
        &'a self,
        inferior: &'a Inferior,
        module: &'a RuntimeModule,
        pid: Pid,
        registers: &'a FrameRegisters,
    ) -> LinuxVariableRuntime<'a, P> {
        LinuxVariableRuntime {
            ptrace: &self.ptrace,
            pid,
            thread: Some(pid),
            loaded_module: module.loaded,
            image_range: module.image.address_range(),
            breakpoints: &inferior.breakpoints,
            registers,
            floating: None,
            cfa: Err(crate::debug_info::VariableRuntimeError::Unavailable(
                crate::VariableUnavailableReason::CallFrameUnavailable(
                    crate::CallFrameUnavailableReason::NoInstructionContext,
                ),
            )),
            tls: module.tls,
            frame: FrameAt {
                activation: 0,
                code: None,
                depth: 0,
            },
            callers: None,
            below_stack_pointer: None,
        }
    }
}
