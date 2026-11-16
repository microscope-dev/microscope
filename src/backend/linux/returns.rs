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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Returning {
    pub(super) activation: Activation,
    pub(super) return_address: VirtualAddress,

    pub(super) module: ModuleId,
    pub(super) function: ImageAddress,
}

#[derive(Debug, Clone)]
pub(super) struct Returned {
    module: ModuleId,
    thread: Pid,
    values: Arc<[ReturnedValue]>,
}

impl<P: InspectionOps> Controller<P> {
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

        let inferior = self.session.inferior()?;
        let ActiveKind::Step { start, .. } = &inferior.run.execution()?.kind else {
            return None;
        };

        start
            .returned
            .clone()
            .or_else(|| self.read_returned(pid, start.returning?))
    }

    pub(super) fn note_returned_values(&mut self, pid: Pid) {
        let Some(returning) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
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
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.run.execution_mut())
            .map(|active| &mut active.kind)
        {
            start.returned = returned;
        }
    }

    fn read_returned(&self, pid: Pid, returning: Returning) -> Option<Returned> {
        let inferior = self.session.inferior()?;
        let registers = self.ptrace.registers(pid).ok()?;
        if registers.program_counter() != returning.return_address
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

    pub(super) fn returned_variables(
        &self,
        returned: &Returned,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>> {
        if frame != StackFrameId::INNERMOST || root.thread() != Some(returned.thread) {
            return Ok(Vec::new());
        }

        let Some(module) = self.modules.get(&returned.module) else {
            return Ok(Vec::new());
        };

        let inferior = self.session.inferior().ok_or(crate::Error::NotRunning)?;
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
                unnamed: None,
                state,
            });
        }

        Ok(variables)
    }

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
