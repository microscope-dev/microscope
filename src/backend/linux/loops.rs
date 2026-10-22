//! Steps through loop bodies that are functions of their own, such as Go's
//! range-over-func bodies, which the loop's iterator calls once a pass.
//!
//! A step treats a loop's body as its enclosing function's own code, and
//! the iterator between them as a call the loop makes:
//!
//! - a step over in the enclosing function enters the body, as it would
//!   the body of any other loop;
//! - a step over in the body goes from one pass of the body to the next,
//!   or on past the loop;
//! - a step out of the body runs the rest of the loop.
//!
//! None of them stops in the iterator. A body the compiler kept out of
//! line is followed by breakpoints at its entries, and, once a step leaves
//! the body, where the enclosing activation resumes when the loop is done.
//! A body inlined with its iterator into the enclosing function is
//! followed by single steps, which pass over the iterator's lines.

use std::collections::BTreeSet;

use nix::libc;
use nix::unistd::Pid;

use crate::unwind::{CallerProvider, CallerResult, DEFAULT_MAX_FRAMES};
use crate::{
    CodeInstanceId, CodeInstanceKind, FunctionId, ImageLocation, InlineFrameLookup, ModuleImage,
    Result, StepKind, VirtualAddress,
};

use super::activation::Activation;
use super::frames::{
    code_instance_is_active, frame_lookup_address, source_for_code_instance, source_line_changed,
    source_step_destination,
};

use super::native::LinuxTraceOps;
use super::{ActiveKind, Controller, LinuxError, StepStart, backend_error};

/// The loops a step over or out treats as its own code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StepLoops {
    /// The function whose loops they are.
    enclosing: FunctionId,

    /// The enclosing function's activation, which runs them.
    activation: Activation,

    /// The bodies whose passes complete the step: the one it began in, or
    /// every loop body of the enclosing function, when it began there.
    bodies: BTreeSet<FunctionId>,

    /// Whether the step began in a body.
    in_body: bool,

    /// For a step begun in an out-of-line body, where the enclosing
    /// activation resumes once the call that runs the loop returns.
    resume: Option<VirtualAddress>,

    /// Whether the step left the body it began in for the iterator, and
    /// goes on to the body's next pass or the end of the loop.
    left: bool,
}

impl StepLoops {
    /// Whether the step began in a body inlined with its iterator into
    /// the enclosing activation.
    pub(super) const fn in_inline_body(&self) -> bool {
        self.in_body && self.resume.is_none()
    }

    /// Whether the step left the out-of-line body it began in, and waits
    /// for the loop at its breakpoints.
    pub(super) const fn waits(&self) -> bool {
        self.in_body && self.left && self.resume.is_some()
    }
}

/// What a plan breakpoint a step through a loop reached means.
pub(super) enum LoopReach {
    /// It is no part of the loops' plan.
    Elsewhere,

    /// The step is complete: a body's next pass begins, or a step out's
    /// loop is done.
    Complete,

    /// Another activation reached it, such as another loop's body: the
    /// step goes on as it was.
    Pass,

    /// The step goes on from here as a step over in the code here: in the
    /// enclosing function once the loop is done, or in a body a step over
    /// entered from the line its loop is on.
    Restarted,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// The loops a step over or out begun in `code_instance`, the
    /// innermost activation's selected code, treats as its own: the loop
    /// it began in the body of, or the loops of the function it began in.
    /// Adds what the plan of a step from there needs to `plan`.
    pub(super) fn step_loops(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        kind: StepKind,
        code_instance: Option<CodeInstanceId>,
        activation: Option<Activation>,
        plan: &mut BTreeSet<VirtualAddress>,
    ) -> Result<Option<StepLoops>> {
        let image = &self.main.image;
        let (Some(instance), Some(activation)) = (
            code_instance.and_then(|instance| image.code_instance(instance)),
            activation,
        ) else {
            return Ok(None);
        };

        let function = instance.function();
        let Some(enclosing) = image
            .function(function)
            .and_then(crate::image::functions::Function::enclosing)
        else {
            // A step over in the enclosing function enters its loops'
            // bodies. A plan that single steps finds inlined ones.
            if kind != StepKind::OverSource {
                return Ok(None);
            }

            let bodies = image
                .functions()
                .filter(|body| body.enclosing() == Some(function))
                .map(crate::image::functions::Function::id)
                .collect::<BTreeSet<_>>();
            if bodies.is_empty() {
                return Ok(None);
            }

            // A plan that runs to breakpoints stops at the entries of
            // out-of-line bodies, and at the statements of inlined ones.
            if !plan.is_empty() {
                plan.extend(bodies.iter().flat_map(|body| self.function_entries(*body)));
                plan.extend(self.inlined_body_statements(instance, &bodies));
            }

            return Ok(Some(StepLoops {
                enclosing: function,
                activation,
                bodies,
                in_body: false,
                resume: None,
                left: false,
            }));
        };

        let in_body = |activation, resume| StepLoops {
            enclosing,
            activation,
            bodies: BTreeSet::from([function]),
            in_body: true,
            resume,
            left: false,
        };

        // Inlined with its iterator, the body runs in the enclosing
        // function's own activation.
        if matches!(instance.kind(), CodeInstanceKind::Inline { .. }) {
            let mut parent = instance.parent();

            while let Some(outer) = parent.and_then(|id| image.code_instance(id)) {
                if outer.function() == enclosing {
                    return Ok(Some(in_body(activation, None)));
                }

                parent = outer.parent();
            }

            return Ok(None);
        }

        let Some((activation, resume)) = self.enclosing_activation(pid, registers, enclosing)?
        else {
            return Ok(None);
        };

        let mut loops = in_body(activation, Some(resume));

        // A step out runs the rest of the loop, to where the enclosing
        // activation resumes.
        if kind == StepKind::Out {
            *plan = BTreeSet::from([resume]);
            loops.left = true;
        }

        Ok(Some(loops))
    }

    /// The nearest activation above the innermost one that runs
    /// `enclosing`'s code, and its instruction: where it resumes once the
    /// call it makes returns.
    fn enclosing_activation(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
        enclosing: FunctionId,
    ) -> Result<Option<(Activation, VirtualAddress)>> {
        let inferior = self.inferior.as_ref().ok_or(crate::Error::NotRunning)?;
        let view = self.stack_view(pid);
        let mut provider = self.stack_unwinder(inferior, pid, native);
        let mut context = super::stepping::innermost_frame(native);

        for level in 0..DEFAULT_MAX_FRAMES {
            let level = u32::try_from(level)
                .map_err(|_| crate::Error::internal("the frame limit fits u32"))?;
            if level > 0
                && frame_lookup_address(level, &context)
                    .and_then(|address| inferior.loaded_module.image_address(address).ok())
                    .filter(|address| self.main.image.contains_address(*address))
                    .is_some_and(|address| {
                        runs_function(
                            &self.main.image,
                            &self.main.image.locate(address),
                            enclosing,
                        )
                    })
            {
                let cfa = provider
                    .frame_cfa(&context)
                    .map_err(|reason| backend_error(LinuxError::CallerUnavailable(reason)))?;
                let resume = self.executable_return_address(pid, context.instruction)?;

                return Ok(Some((view.activation(cfa), resume)));
            }

            context = match provider.caller(&context) {
                CallerResult::Caller(caller) => caller,
                CallerResult::Finished(_) => return Ok(None),
            };
        }

        Ok(None)
    }

    /// Once a step over's out-of-line body returns into its iterator,
    /// plans the rest of the loop: a breakpoint at each entry of the body,
    /// and one where the enclosing activation resumes. Returns whether it
    /// did.
    pub(super) fn wait_for_loop(&mut self, pid: Pid, kind: StepKind) -> Result<bool> {
        if kind != StepKind::OverSource {
            return Ok(false);
        }

        let Some((start_activation, loops)) = self.active_step().and_then(|start| {
            let loops = start
                .loops
                .as_ref()
                .filter(|loops| loops.in_body && !loops.left && loops.resume.is_some())?;
            Some((start.activation?, loops.clone()))
        }) else {
            return Ok(false);
        };

        let registers = self.ptrace.registers(pid)?;
        if !start_activation.has_returned(self.stack_position(pid, &registers)) {
            return Ok(false);
        }

        // Returned all the way to the enclosing function, the step is
        // already back in the loop's own code.
        let Ok(current) = self.top_activation(pid, &registers) else {
            return Ok(false);
        };

        if !current.is_callee_of(loops.activation) {
            return Ok(false);
        }

        record!("step over follows its loop from the body's return");

        let inferior = self.inferior.as_ref().ok_or(crate::Error::NotRunning)?;
        let mut plan = loops
            .bodies
            .iter()
            .flat_map(|body| self.function_entries(*body))
            .collect::<BTreeSet<_>>();
        plan.extend(loops.resume);

        let guards = self.panic_entries(inferior);
        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        self.install_additional_plan_breakpoints(
            execution,
            &plan.union(&guards).copied().collect(),
        )?;

        let start = self.active_step_mut().ok_or_else(|| {
            crate::Error::internal("the step remained active while it followed its loop")
        })?;
        start.plan_addresses = plan;
        start.panic_guards = guards;
        start.returned_to = None;
        start.loops = Some(StepLoops {
            left: true,
            ..loops
        });
        Ok(true)
    }

    /// What a step through loops does at one of its plan's breakpoints.
    pub(super) fn reach_loop(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        kind: StepKind,
    ) -> Result<LoopReach> {
        let Some(loops) = self.active_step().and_then(|start| start.loops.clone()) else {
            return Ok(LoopReach::Elsewhere);
        };

        let registers = self.ptrace.registers(pid)?;
        if loops.waits() && Some(address) == loops.resume {
            if self.top_activation(pid, &registers).ok() != Some(loops.activation) {
                return Ok(LoopReach::Pass);
            }

            if kind == StepKind::Out {
                return Ok(LoopReach::Complete);
            }

            // The loop is done: the step goes on in the enclosing
            // function, to its next line.
            self.restart_step_over(pid)?;
            return Ok(LoopReach::Restarted);
        }

        if loops.in_body && !loops.left {
            return Ok(LoopReach::Elsewhere);
        }

        if !loops
            .bodies
            .iter()
            .any(|body| self.function_entries(*body).contains(&address))
        {
            return Ok(LoopReach::Elsewhere);
        }

        // A body's pass belongs to the step's loops only while their
        // activation runs it.
        let ours = self
            .location_for_activation(pid, &registers, loops.activation)?
            .is_some_and(|location| runs_function(&self.main.image, &location, loops.enclosing));
        if !ours {
            return Ok(LoopReach::Pass);
        }

        // Entered from the line its loop is on, which is where the body
        // begins, a step over goes on to the body's next line.
        let begun_on = self.active_step().and_then(|start| start.source.clone());
        let here = self
            .image_location(VirtualAddress::new(registers.rip))
            .and_then(|location| location.source);

        if !loops.in_body && !source_line_changed(begun_on.as_ref(), here.as_ref()) {
            self.restart_step_over(pid)?;
            return Ok(LoopReach::Restarted);
        }

        Ok(LoopReach::Complete)
    }

    /// Begins the active step over again from where its thread is stopped,
    /// in the logical frame innermost there.
    fn restart_step_over(&mut self, pid: Pid) -> Result<()> {
        let restarted = self.innermost_step_start(pid, StepKind::OverSource, || {
            self.presentation_for_thread(pid, None)
        })?;
        record!("step over begins again: {:?}", restarted.loops);

        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        self.install_additional_plan_breakpoints(
            execution,
            &restarted
                .plan_addresses
                .union(&restarted.panic_guards)
                .copied()
                .collect(),
        )?;

        let start = self
            .active_step_mut()
            .ok_or_else(|| crate::Error::internal("the step remained active as it began again"))?;
        *start = restarted;
        Ok(())
    }

    /// Notes when a step through a body inlined with its iterator leaves
    /// the body, so that the body's next pass completes it at its first
    /// statement, even on the line the step began on.
    pub(super) fn note_loop_progress(&mut self, pid: Pid) -> Result<()> {
        let Some((loops, activation)) = self.active_step().and_then(|start| {
            let loops = start
                .loops
                .as_ref()
                .filter(|loops| loops.in_inline_body() && !loops.left)?;
            Some((loops.clone(), start.activation?))
        }) else {
            return Ok(());
        };

        let registers = self.ptrace.registers(pid)?;
        let Some(location) = self.location_for_activation(pid, &registers, activation)? else {
            return Ok(());
        };

        let elsewhere = innermost_function(&self.main.image, &location).is_some_and(|function| {
            !loops.bodies.contains(&function) && function != loops.enclosing
        });
        if elsewhere && let Some(start) = self.active_step_mut() {
            start.loops = Some(StepLoops {
                left: true,
                ..loops
            });
        }

        Ok(())
    }

    /// The statements of `instance`'s code that loop bodies inlined into it
    /// begin.
    fn inlined_body_statements(
        &self,
        instance: crate::CodeInstance<'_>,
        bodies: &BTreeSet<FunctionId>,
    ) -> BTreeSet<VirtualAddress> {
        let Some(inferior) = self.inferior.as_ref() else {
            return BTreeSet::new();
        };

        let image = &self.main.image;
        image
            .line_entries_in(instance)
            .filter(|line| line.statement)
            .filter(|line| {
                innermost_function(image, &image.locate(line.range.start))
                    .is_some_and(|function| bodies.contains(&function))
            })
            .filter_map(|line| {
                inferior
                    .loaded_module
                    .virtual_address(line.range.start)
                    .ok()
            })
            .collect()
    }

    /// The entries of every out-of-line instance of a function, where a
    /// function breakpoint would stop.
    fn function_entries(&self, function: FunctionId) -> BTreeSet<VirtualAddress> {
        let Some(inferior) = self.inferior.as_ref() else {
            return BTreeSet::new();
        };

        self.main
            .image
            .instances_for_function(function)
            .filter(|instance| matches!(instance.kind(), CodeInstanceKind::OutOfLine))
            .filter_map(crate::image::functions::CodeInstance::breakpoint_entry)
            .filter_map(|entry| inferior.loaded_module.virtual_address(entry.address).ok())
            .collect()
    }

    pub(super) fn active_step(&self) -> Option<&StepStart> {
        match &self.inferior.as_ref()?.active.as_ref()?.kind {
            ActiveKind::Step { start, .. } => Some(start),
            _ => None,
        }
    }
}

/// Whether a step through inlined loops is complete at `location`, in the
/// step's activation, or `None` when the loops do not decide.
///
/// Begun in a body inlined with its iterator, it is complete in the body
/// at a statement on another line, or at any statement of a later pass,
/// and in the enclosing function once the loop is done; never in the
/// iterator. Begun in the enclosing function, a step over is complete at
/// the first statement of a loop body it enters.
pub(super) fn inline_loop_step_is_complete(
    image: &ModuleImage,
    location: &ImageLocation,
    start: &StepStart,
    kind: StepKind,
) -> Option<bool> {
    let loops = start.loops.as_ref()?;
    let innermost = innermost_function(image, location);
    let in_a_body = innermost.is_some_and(|function| loops.bodies.contains(&function));

    if !loops.in_body {
        return (in_a_body
            && kind == StepKind::OverSource
            && source_step_destination(image, location, kind)
            && source_line_changed(start.source.as_ref(), location.source.as_ref()))
        .then_some(true);
    }

    if !loops.in_inline_body() {
        return None;
    }

    Some(if in_a_body {
        kind == StepKind::OverSource
            && source_step_destination(image, location, kind)
            && (loops.left
                || start.code_instance.is_none_or(|instance| {
                    !code_instance_is_active(location, instance)
                        || source_line_changed(
                            start.source.as_ref(),
                            source_for_code_instance(image, location, instance).as_ref(),
                        )
                }))
    } else {
        innermost == Some(loops.enclosing) && source_step_destination(image, location, kind)
    })
}

/// Whether an inline instance is a loop body, which a stop where it
/// begins shows, as the enclosing function's own code.
pub(super) fn is_loop_body(image: &ModuleImage, instance: CodeInstanceId) -> bool {
    image
        .code_instance(instance)
        .and_then(|instance| image.function(instance.function()))
        .is_some_and(|function| function.enclosing().is_some())
}

/// The function whose code is innermost at a location: the innermost
/// inline instance's, or the physical one's.
fn innermost_function(image: &ModuleImage, location: &ImageLocation) -> Option<FunctionId> {
    let instance = match &location.inline_frames {
        InlineFrameLookup::Unique(chain) => chain.instances.last().copied(),
        InlineFrameLookup::None => location.physical_instance,
        InlineFrameLookup::Ambiguous(_) => None,
    }?;
    image
        .code_instance(instance)
        .map(crate::image::functions::CodeInstance::function)
}

/// Whether a location runs `function`'s code, physically or inlined.
fn runs_function(image: &ModuleImage, location: &ImageLocation, function: FunctionId) -> bool {
    let runs = |instance: &CodeInstanceId| {
        image
            .code_instance(*instance)
            .is_some_and(|instance| instance.function() == function)
    };

    location.physical_instance.as_ref().is_some_and(runs)
        || match &location.inline_frames {
            InlineFrameLookup::Unique(chain) => chain.instances.iter().any(runs),
            _ => false,
        }
}
