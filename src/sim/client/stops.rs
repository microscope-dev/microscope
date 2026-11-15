use std::collections::BTreeSet;
use std::sync::Arc;

use super::breakpoints::Added;
use super::{Client, Evaluated, Observation, Purpose, protocol};
use crate::sim::choices::Stream;
use crate::sim::marks::Mark;
use crate::sim::report::Failure;
use crate::{
    Backtrace, BreakpointLocation, BreakpointOptions, BreakpointSpec, Error, ExceptionDisposition,
    ExecutionContext, Expression, FrameKind, InferiorState, LogMessage, PresentedFrame,
    ResumeScope, ScalarValue, StackFrame, StackFrameId, StateSnapshot, StepKind, StopContext,
    StopId, StopReason, ThreadState, UnwindTermination, VariableKind, VariableSnapshot,
    VariableState, VariableValue, VariableValueSource, VirtualAddress,
};

async fn assign_pc(view: &crate::StopView<'_>, pc: u64) -> Result<(), Failure> {
    let assignment = Expression::parse(&format!("$pc = {pc:#x}")).expect("an assignment");
    let assigned = view
        .evaluate_with(
            &assignment,
            crate::EvaluationMode::Assign,
            crate::InspectionLimits::default(),
        )
        .await
        .map_err(|error| protocol(format!("assigning $pc failed: {error}")))?;
    let crate::eval::Evaluation::Value { value, .. } = assigned else {
        return Err(protocol(format!("assigning $pc gave {assigned:?}")));
    };

    if !matches!(
        value.state,
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Unsigned(read)),
            ..
        } if read == u128::from(pc)
    ) {
        return Err(protocol(format!(
            "$pc read {value:?} once assigned {pc:#x}"
        )));
    }

    Ok(())
}

impl Client {
    pub(super) async fn backtrace(&self) -> Result<(), Failure> {
        let snapshot = self.snapshot().await?;
        match self.handle.backtrace().await {
            Ok(backtrace) => {
                self.note(format!(
                    "backtrace: {} frames, {:?}",
                    backtrace.frames.len(),
                    backtrace.termination
                ));
                if let Some(stop) = snapshot.stop_id {
                    self.observe(Observation::Backtrace { stop, backtrace });
                }
            }
            Err(Error::AmbiguousInlineFrame) if presented_ambiguously(&snapshot) => {
                self.note("backtrace from an ambiguous inline frame refused");
            }
            Err(error) => return Err(protocol(format!("backtrace failed: {error}"))),
        }

        Ok(())
    }

    pub(super) async fn step(&self) -> Result<(), Failure> {
        let kinds = [
            StepKind::Instruction,
            StepKind::OverInstruction,
            StepKind::IntoSource,
            StepKind::OverSource,
            StepKind::Out,
            StepKind::IntoNewTask,
        ];
        let kind = *self.choices.borrow_mut().pick(Stream::Client, &kinds);
        let mut before = self.snapshot().await?;

        let ambiguous = kind != StepKind::Instruction
            && kind != StepKind::OverInstruction
            && presented_ambiguously(&before);

        let (caller, outer, was) = if kind == StepKind::Out && !ambiguous {
            self.step_out_of(&mut before).await?
        } else {
            (Caller::Trusted, 0, None)
        };

        let result = self
            .step_from(&before, kind, ambiguous, &caller, outer)
            .await;

        if let (Some(frame), Some(stop)) = (was, before.stop_id)
            && self.snapshot().await?.stop_id == Some(stop)
        {
            self.select_frame(&frame).await?;
        }

        result
    }

    async fn step_from(
        &self,
        before: &StateSnapshot,
        kind: StepKind,
        ambiguous: bool,
        caller: &Caller,
        outer: usize,
    ) -> Result<(), Failure> {
        self.note(format!("step {kind:?}"));
        if let Some(ExecutionContext::Thread(thread)) = before.selected {
            self.observe(Observation::StepBegins {
                thread,
                kind,
                presentation: before.presentation.clone(),
                targets: BTreeSet::new(),
                call: None,
                outer,
            });
        }

        if !ambiguous && matches!(caller, Caller::Trusted) && self.control(2) == 0 {
            return self.start_step(before, kind).await;
        }

        let result = self.handle.step(kind).await;
        self.observe(Observation::StepEnded(result.as_ref().ok().cloned()));
        if ambiguous {
            if !matches!(result, Err(Error::AmbiguousInlineFrame)) {
                return Err(protocol(format!(
                    "step {kind:?} from an ambiguous inline frame returned {result:?}"
                )));
            }

            let after = self.snapshot().await?;
            if after != *before {
                return Err(protocol(format!(
                    "a refused step changed the state from {before:?} to {after:?}"
                )));
            }

            self.note(format!(
                "step {kind:?} from an ambiguous inline frame refused"
            ));
            return Ok(());
        }

        match (result, caller) {
            (
                Ok(reason),
                Caller::Trusted | Caller::Outermost(UnwindTermination::NoUnwindInfo { .. }),
            ) => {
                if matches!(reason, StopReason::Syscall(_)) {
                    self.mark(Mark::CatchStepStop);
                }

                if outer > 0 {
                    self.mark(Mark::SteppedOutOfCaller);
                }

                self.note(format!("stepped: {reason:?}"));
            }
            (Err(Error::EventStreamLagged(_)), _) => self.mark(Mark::ClientLagged),
            (Err(error), _) if self.refused_unarmed(&error).await? => {}
            (Err(error), Caller::Outermost(_) | Caller::Corrupt(_)) => {
                let after = self.snapshot().await?;
                if after != *before {
                    return Err(protocol(format!(
                        "a refused step out changed the state from {before:?} to {after:?}"
                    )));
                }

                self.note(format!("step out of a frame {caller} refused: {error}"));
                self.mark(Mark::StepOutRefused);
            }
            (Ok(reason), caller) => {
                return Err(protocol(format!(
                    "stepped out of a frame {caller}: {reason:?}"
                )));
            }
            (Err(error), Caller::Trusted) => {
                return Err(protocol(format!("step {kind:?} failed: {error}")));
            }
        }

        Ok(())
    }

    async fn start_step(&self, before: &StateSnapshot, kind: StepKind) -> Result<(), Failure> {
        let (Some(stop), Some(context), InferiorState::Stopped { process_id, .. }) =
            (before.stop_id, before.selected, &before.inferior)
        else {
            return Err(protocol(format!("a step began from {before:?}")));
        };

        let frame = match kind {
            StepKind::Out => before.selected_frame.unwrap_or(StackFrameId::INNERMOST),
            _ => StackFrameId::INNERMOST,
        };

        let alone = match context {
            ExecutionContext::Thread(thread) if self.draw(2) == 0 => Some(thread),
            _ => None,
        };
        if let Some(thread) = alone {
            self.observe(Observation::Alone(Some((stop, thread))));
        }

        let scope = alone.map_or(ResumeScope::Process(*process_id), ResumeScope::Thread);

        let result = self
            .handle
            .start_step(
                stop,
                context,
                frame,
                kind,
                scope,
                ExceptionDisposition::Pass,
            )
            .await;

        match result {
            Ok(execution) => {
                self.note(format!("began step {kind:?} as execution {execution}"));
                self.stepping.set(Some(execution));
                if alone.is_some() {
                    self.alone.set(Some(execution));
                    self.mark(Mark::ThreadStepped);
                }
            }
            Err(error) if self.refused_unarmed(&error).await? => {
                self.observe(Observation::Alone(None));
                self.observe(Observation::StepEnded(None));
            }
            Err(error) => {
                return Err(protocol(format!("beginning step {kind:?} failed: {error}")));
            }
        }

        Ok(())
    }

    pub(super) async fn step_into_call(&self) -> Result<(), Failure> {
        let before = self.snapshot().await?;
        let (Some(stop), Some(ExecutionContext::Thread(thread))) =
            (before.stop_id, before.selected)
        else {
            return Ok(());
        };

        let view = self.handle.at(StopContext {
            stop,
            execution: thread.into(),
            frame: StackFrameId::INNERMOST,
        });
        let targets = match view.step_targets().await {
            Ok(targets) => targets,
            Err(Error::AmbiguousInlineFrame) if presented_ambiguously(&before) => {
                self.note("step targets from an ambiguous inline frame refused");
                return Ok(());
            }
            Err(error) => return Err(protocol(format!("step targets failed: {error}"))),
        };

        if targets.is_empty() {
            self.note("no call on the line to step into");
            return Ok(());
        }

        let target = self
            .choices
            .borrow_mut()
            .pick(Stream::Control, &targets)
            .clone();
        self.note(format!(
            "step into the call at {} of {:?}",
            target.call, target.callee
        ));
        self.observe(Observation::StepBegins {
            thread,
            kind: StepKind::IntoSource,
            presentation: before.presentation.clone(),
            targets: BTreeSet::new(),
            call: Some(target.call.get()),
            outer: 0,
        });
        let result = self.handle.step_into(target.call).await;
        self.observe(Observation::StepEnded(result.as_ref().ok().cloned()));

        match result {
            Ok(reason) => {
                self.note(format!("stepped into the call: {reason:?}"));
                self.mark(Mark::SteppedIntoCall);
            }
            Err(Error::EventStreamLagged(_)) => self.mark(Mark::ClientLagged),
            Err(error) if self.refused_unarmed(&error).await? => {}
            Err(error) => {
                return Err(protocol(format!(
                    "step into the call at {} failed: {error}",
                    target.call
                )));
            }
        }

        Ok(())
    }

    pub(super) async fn advance(&self, breakpoints: &[Added]) -> Result<(), Failure> {
        let spec = self.choose_spec(Stream::Control);
        let Some(targets) = self.advance_targets(&spec, breakpoints).await? else {
            return Ok(());
        };

        let before = self.snapshot().await?;
        let ambiguous = presented_ambiguously(&before);
        let caller = if ambiguous {
            Caller::Trusted
        } else {
            let backtrace = self
                .handle
                .backtrace()
                .await
                .map_err(|error| protocol(format!("backtrace failed: {error}")))?;
            let caller = Caller::of(&backtrace, 0);
            if let Some(stop) = before.stop_id {
                self.observe(Observation::Backtrace { stop, backtrace });
            }

            caller
        };

        self.note(format!("advance to {spec}: {targets:#x?}"));
        if let Some(ExecutionContext::Thread(thread)) = before.selected {
            self.observe(Observation::StepBegins {
                thread,
                kind: StepKind::Advance,
                presentation: before.presentation.clone(),
                targets,
                call: None,
                outer: 0,
            });
        }

        let result = self.handle.advance(spec.clone()).await;
        self.observe(Observation::StepEnded(result.as_ref().ok().cloned()));

        if ambiguous {
            if !matches!(result, Err(Error::AmbiguousInlineFrame)) {
                return Err(protocol(format!(
                    "advance from an ambiguous inline frame returned {result:?}"
                )));
            }

            return self.unchanged_by_refusal(&before, "advance").await;
        }

        match (result, &caller) {
            (
                Ok(reason),
                Caller::Trusted | Caller::Outermost(UnwindTermination::NoUnwindInfo { .. }),
            ) => {
                match reason {
                    StopReason::Step {
                        kind: StepKind::Advance,
                    } => self.mark(Mark::AdvanceReached),
                    StopReason::Step {
                        kind: StepKind::Out,
                    } => self.mark(Mark::AdvanceReturned),
                    _ => {}
                }

                self.note(format!("advanced: {reason:?}"));
            }
            (Err(Error::EventStreamLagged(_)), _) => self.mark(Mark::ClientLagged),
            (Err(error), _) if self.refused_unarmed(&error).await? => {}
            (Err(error), Caller::Outermost(_) | Caller::Corrupt(_)) => {
                self.unchanged_by_refusal(&before, "advance").await?;
                self.note(format!("advance from a frame {caller} refused: {error}"));
                self.mark(Mark::StepOutRefused);
            }
            (Ok(reason), caller) => {
                return Err(protocol(format!(
                    "advanced from a frame {caller}: {reason:?}"
                )));
            }
            (Err(error), Caller::Trusted) => {
                return Err(protocol(format!("advance to {spec} failed: {error}")));
            }
        }

        Ok(())
    }

    pub(super) async fn jump_in_place(&self) -> Result<(), Failure> {
        let before = self.snapshot().await?;
        let (Some(stop), Some(ExecutionContext::Thread(thread))) =
            (before.stop_id, before.selected)
        else {
            return Ok(());
        };

        if presented_ambiguously(&before) {
            return Ok(());
        }

        let view = self.handle.at(StopContext {
            stop,
            execution: thread.into(),
            frame: StackFrameId::INNERMOST,
        });
        let registers = view
            .registers()
            .await
            .map_err(|error| protocol(format!("registers failed: {error}")))?;
        let value = |name: &str| {
            let value = registers
                .registers
                .iter()
                .find(|value| &*value.register.name == name)?;
            Some(u64::from_le_bytes(value.bytes.as_deref()?.try_into().ok()?))
        };

        let (Some(pc), Some(call)) = (value("rip"), value("orig_rax")) else {
            return Err(protocol(format!(
                "the innermost frame lacks rip or orig_rax: {registers:?}"
            )));
        };

        if call != u64::MAX {
            self.note(format!(
                "thread {thread} is in system call {call}; not moved"
            ));
            return Ok(());
        }

        let by_jump = self.control(2) == 0;

        if by_jump {
            match self
                .handle
                .start_jump(
                    stop,
                    thread,
                    BreakpointSpec::Address(VirtualAddress::new(pc)),
                )
                .await
            {
                Ok(_) => {}
                Err(Error::JumpWithoutFunction) => {
                    self.note(format!(
                        "a jump at {pc:#x}, in no described function, refused"
                    ));
                    return self.unchanged_by_refusal(&before, "jump").await;
                }
                Err(error) => return Err(protocol(format!("a jump in place failed: {error}"))),
            }
        } else {
            assign_pc(&view, pc).await?;
        }

        let after = self.snapshot().await?;
        if after.stop_id == Some(stop)
            || !matches!(
                after.inferior,
                crate::InferiorState::Stopped {
                    reason: StopReason::Jump,
                    ..
                }
            )
        {
            return Err(protocol(format!(
                "a move in place did not publish the stop again: {after:?}"
            )));
        }

        self.note(format!(
            "moved thread {thread} in place at {pc:#x}, {}",
            if by_jump {
                "by a jump"
            } else {
                "by assigning $pc"
            }
        ));
        self.mark(Mark::JumpedInPlace);
        Ok(())
    }

    async fn advance_targets(
        &self,
        spec: &BreakpointSpec,
        breakpoints: &[Added],
    ) -> Result<Option<BTreeSet<u64>>, Failure> {
        let probe = match self
            .handle
            .add_breakpoint_with(
                spec.clone(),
                BreakpointOptions {
                    enabled: false,
                    log_message: Some(LogMessage::parse("advance").expect("a valid message")),
                    ..BreakpointOptions::default()
                },
            )
            .await
        {
            Ok(probe) => probe,
            Err(error) if self.names_no_code(spec, &error) => return Ok(None),
            Err(error) => {
                return Err(protocol(format!("resolving {spec} failed: {error}")));
            }
        };

        self.judge_locations(&probe)?;
        if breakpoints.iter().any(|added| added.id() == probe.id) {
            return Err(protocol(format!(
                "a probe for {spec} returned the client's own breakpoint {}",
                probe.id
            )));
        }

        self.handle
            .remove_breakpoint(probe.id)
            .await
            .map_err(|error| protocol(format!("removing probe {} failed: {error}", probe.id)))?;

        let mut targets = BTreeSet::new();
        for location in probe.locations.iter() {
            match location.location {
                BreakpointLocation::Image(address) => {
                    targets.insert(address.get());
                }

                BreakpointLocation::Virtual(_) | BreakpointLocation::Managed(_) => {
                    return Ok(None);
                }
            }
        }

        Ok(Some(targets))
    }

    async fn unchanged_by_refusal(
        &self,
        before: &StateSnapshot,
        request: &str,
    ) -> Result<(), Failure> {
        let after = self.snapshot().await?;
        if after != *before {
            return Err(protocol(format!(
                "a refused {request} changed the state from {before:?} to {after:?}"
            )));
        }

        Ok(())
    }

    pub(super) async fn inspect(&self, stop: StopId) -> Result<(), Failure> {
        let snapshot = self.snapshot().await?;
        let backtrace = match self.handle.backtrace().await {
            Ok(backtrace) => backtrace,
            Err(Error::AmbiguousInlineFrame) if presented_ambiguously(&snapshot) => {
                self.note("inspecting an ambiguous inline frame refused");
                return Ok(());
            }
            Err(error) => return Err(protocol(format!("backtrace failed: {error}"))),
        };

        let outer = match &backtrace.frames[..] {
            [_, callers @ ..] if !callers.is_empty() && self.draw(3) == 0 => Some(
                self.choices
                    .borrow_mut()
                    .pick(Stream::Client, callers)
                    .clone(),
            ),
            _ => None,
        };
        if let Some(frame) = &outer {
            self.select_frame(frame).await?;
            self.mark(Mark::OuterFrameInspected);
        }

        let selected = outer
            .as_ref()
            .map(|frame| frame.id)
            .or(snapshot.selected_frame);
        let inspected = self.inspect_frame(stop, &backtrace, selected).await;

        if outer.is_some()
            && let Some(was) = backtrace
                .frames
                .iter()
                .find(|frame| Some(frame.id) == snapshot.selected_frame)
                .or_else(|| backtrace.frames.first())
        {
            self.select_frame(was).await?;
        }

        inspected?;

        if self.script.views.is_some() {
            self.present(stop).await?;
        }

        for thread in snapshot.threads.iter() {
            if snapshot.selected == Some(ExecutionContext::Thread(thread.id))
                || !matches!(thread.state, ThreadState::Stopped { .. })
            {
                continue;
            }

            let context = StopContext {
                stop,
                execution: ExecutionContext::Thread(thread.id),
                frame: StackFrameId::new(0),
            };

            match self.handle.at(context).backtrace().await {
                Ok(backtrace) => self.observe(Observation::Backtrace { stop, backtrace }),
                Err(Error::AmbiguousInlineFrame) => {
                    self.note(format!("thread {}'s inline frame is ambiguous", thread.id));
                }
                Err(error) => {
                    return Err(protocol(format!(
                        "backtrace of thread {} failed: {error}",
                        thread.id
                    )));
                }
            }
        }

        Ok(())
    }

    async fn inspect_frame(
        &self,
        stop: StopId,
        backtrace: &Backtrace,
        selected: Option<StackFrameId>,
    ) -> Result<(), Failure> {
        let undescribed = backtrace
            .frames
            .iter()
            .find(|frame| Some(frame.id) == selected)
            .is_some_and(|frame| frame.function.is_none());
        let variables = match self.handle.variables().await {
            Ok(variables) => variables,
            Err(Error::VariableContextUnsupported) if undescribed => {
                self.note("variables of code without debug information refused");
                return Ok(());
            }
            Err(error) => return Err(protocol(format!("reading variables failed: {error}"))),
        };

        self.note(format!(
            "variables of frame {} in {}: {}",
            variables.stack_frame,
            variables.context,
            variables
                .variables
                .iter()
                .map(|variable| variable.name.as_ref())
                .collect::<Vec<_>>()
                .join(", ")
        ));

        self.observe(Observation::Backtrace {
            stop,
            backtrace: backtrace.clone(),
        });

        let evaluations = if self.draw(2) == 0 {
            self.evaluations(&variables, backtrace).await
        } else {
            Vec::new()
        };

        let frame = variables.stack_frame;
        self.observe(Observation::Variables {
            stop,
            variables,
            backtrace: backtrace.clone(),
            evaluations,
        });

        if self.draw(2) == 0 {
            let registers = self
                .handle
                .registers()
                .await
                .map_err(|error| protocol(format!("registers failed: {error}")))?;
            self.observe(Observation::Registers {
                stop,
                frame,
                registers,
                backtrace: backtrace.clone(),
            });
        }

        Ok(())
    }

    async fn step_out_of(
        &self,
        before: &mut StateSnapshot,
    ) -> Result<(Caller, usize, Option<StackFrame>), Failure> {
        let backtrace = self
            .handle
            .backtrace()
            .await
            .map_err(|error| protocol(format!("backtrace failed: {error}")))?;
        let chosen = if self.draw(3) == 0 {
            self.outer_frame(&backtrace)
        } else {
            None
        };

        if let Some(stop) = before.stop_id {
            self.observe(Observation::Backtrace {
                stop,
                backtrace: backtrace.clone(),
            });
        }

        let Some(frame) = chosen else {
            return Ok((Caller::of(&backtrace, 0), 0, None));
        };

        let was = backtrace
            .frames
            .iter()
            .find(|frame| Some(frame.id) == before.selected_frame)
            .or_else(|| backtrace.frames.first())
            .cloned();

        self.select_frame(&frame).await?;
        *before = self.snapshot().await?;
        let outer = usize::try_from(frame.level).expect("small");

        Ok((Caller::of(&backtrace, frame.level), outer, was))
    }

    fn outer_frame(&self, backtrace: &Backtrace) -> Option<StackFrame> {
        let candidates = backtrace
            .frames
            .iter()
            .take_while(|frame| frame.kind == FrameKind::Physical)
            .filter(|frame| {
                frame.level > 0 && matches!(Caller::of(backtrace, frame.level), Caller::Trusted)
            })
            .cloned()
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }

        Some(
            self.choices
                .borrow_mut()
                .pick(Stream::Client, &candidates)
                .clone(),
        )
    }

    async fn select_frame(&self, frame: &StackFrame) -> Result<(), Failure> {
        let selected =
            self.handle.select_frame(frame.id).await.map_err(|error| {
                protocol(format!("selecting frame {} failed: {error}", frame.id))
            })?;
        if selected != *frame {
            return Err(protocol(format!(
                "selecting {frame:?} answered {selected:?}"
            )));
        }

        let snapshot = self.snapshot().await?;
        if snapshot.selected_frame != Some(frame.id) {
            return Err(protocol(format!(
                "after selecting frame {} the snapshot selects {:?}",
                frame.id, snapshot.selected_frame
            )));
        }

        self.note(format!(
            "selected frame {} at level {}",
            frame.id, frame.level
        ));

        Ok(())
    }

    async fn present(&self, stop: StopId) -> Result<(), Failure> {
        for name in crate::sim::views::CONTAINERS {
            if !self
                .script
                .globals
                .iter()
                .any(|(global, ..)| global == name)
            {
                continue;
            }

            let expression = Expression::parse(name).expect("a container's name parses");
            let value = match self.handle.evaluate(&expression).await {
                Ok(crate::Evaluation::Value { value, .. }) => value,
                Ok(other) => {
                    return Err(protocol(format!("`{name}` evaluated to {other:?}")));
                }
                Err(error) => return Err(protocol(format!("evaluating `{name}` failed: {error}"))),
            };

            let children = match &value.state {
                VariableState::Available {
                    presentation: Some(presentation),
                    ..
                } if matches!(
                    presentation.shape,
                    crate::PresentedShape::Sequence | crate::PresentedShape::Map
                ) =>
                {
                    match &presentation.children {
                        crate::ValueChildren::Available(reference) => Some(Arc::clone(reference)),
                        _ => None,
                    }
                }
                _ => None,
            };

            let (whole, paged) = match children {
                Some(reference) => {
                    let elements = reference.elements().unwrap_or(0);
                    let whole = self.pages(&reference, elements, elements.max(1)).await;
                    let size = 1 + self.draw(3);
                    let paged = self.pages(&reference, elements, size).await;
                    (whole, paged)
                }
                None => (Ok(Vec::new()), Ok(Vec::new())),
            };

            self.note(format!("presented `{name}`"));
            self.observe(Observation::Presented {
                stop,
                name: name.to_owned(),
                value: Box::new(value),
                whole,
                paged,
            });
        }

        Ok(())
    }

    async fn pages(
        &self,
        reference: &Arc<crate::ValueChildrenReference>,
        elements: u64,
        size: u64,
    ) -> Result<Vec<crate::ValueChildPage>, String> {
        let mut pages = Vec::new();
        let mut offset = 0;

        while offset < elements {
            let limit = u32::try_from(size.min(elements - offset)).expect("small pages");
            let page = self
                .handle
                .value_children(
                    Arc::clone(reference),
                    crate::ValueChildQuery { offset, limit },
                )
                .await
                .map_err(|error| error.to_string())?;
            let empty = page.children.is_empty();
            offset += page.children.len() as u64;
            pages.push(page);

            if empty {
                break;
            }
        }

        Ok(pages)
    }

    async fn evaluations(
        &self,
        variables: &VariableSnapshot,
        backtrace: &Backtrace,
    ) -> Vec<Evaluated> {
        let mut asked = Vec::new();
        let line = backtrace
            .frames
            .iter()
            .find(|frame| frame.id == variables.stack_frame)
            .and_then(|frame| frame.source.as_ref())
            .map(|source| source.line.get());

        if let Some(marker) = line.and_then(|line| self.script.markers.get(&line)) {
            let condition = &marker.text;
            asked.push((Purpose::Marker { negated: false }, condition.clone()));
            asked.push((Purpose::Marker { negated: true }, format!("!({condition})")));
            if let Some(expected) = &marker.expect {
                asked.push((Purpose::Expected, expected.clone()));
            }
        }

        let nameable = || {
            variables
                .variables
                .iter()
                .filter(|variable| variable.kind != VariableKind::Returned)
        };

        let unique = nameable().filter(|variable| {
            nameable()
                .filter(|other| other.name == variable.name)
                .count()
                == 1
        });
        let integers = unique
            .clone()
            .filter(|variable| integer(&variable.state).is_some())
            .map(|variable| variable.name.to_string())
            .collect::<Vec<_>>();
        let named = unique.filter(|variable| {
            integer(&variable.state).is_some()
                || matches!(
                    variable.state,
                    VariableState::Unavailable(_)
                        | VariableState::Available {
                            value: VariableValue::Address(_),
                            ..
                        }
                )
        });
        let mut by_name = Vec::new();

        for variable in named.take(4) {
            let name = variable.name.to_string();
            by_name.push((Purpose::Name(name.clone()), name.clone()));
            if let VariableState::Available {
                source: VariableValueSource::Memory(_),
                ..
            } = variable.state
            {
                by_name.push((Purpose::Address(name.clone()), format!("&{name}")));
                by_name.push((Purpose::Name(name.clone()), format!("*&{name}")));
            }
        }

        let pick = |count: usize| usize::try_from(self.draw(count as u64)).expect("small");
        let computed = if integers.is_empty() {
            Vec::new()
        } else {
            self.computed(&integers)
        };

        if pick(2) == 0 {
            asked.extend(by_name.into_iter().chain(computed));
        } else {
            asked.extend(computed.into_iter().chain(by_name));
        }

        let mut evaluations = Vec::new();
        for (purpose, text) in asked {
            let result = match Expression::parse(&text) {
                Ok(expression) => self
                    .handle
                    .evaluate(&expression)
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(format!("parsing failed: {error}")),
            };

            self.note(format!("evaluated `{text}`"));
            evaluations.push(Evaluated {
                purpose,
                text,
                result,
            });
        }

        evaluations
    }

    fn computed(&self, integers: &[String]) -> Vec<(Purpose, String)> {
        let pick = |count: usize| usize::try_from(self.draw(count as u64)).expect("small");
        let mut computed = Vec::new();
        for _ in 0..2 {
            let left = integers[pick(integers.len())].clone();
            let right = integers[pick(integers.len())].clone();
            let operator = ['+', '-', '*'][pick(3)];
            let text = format!("{left} {operator} {right}");
            computed.push((
                Purpose::Arithmetic {
                    left,
                    operator,
                    right,
                },
                text,
            ));
        }

        let name = integers[pick(integers.len())].clone();
        let bits = [8, 16, 32][pick(3)];
        let signed = pick(2) == 0;
        let target = format!("{}{bits}", if signed { 'i' } else { 'u' });
        let text = if pick(2) == 0 {
            format!("({target}){name}")
        } else {
            format!("{name} as {target}")
        };

        computed.push((Purpose::Cast { name, bits, signed }, text));

        let name = &integers[pick(integers.len())];
        let text = match pick(3) {
            0 => format!("{name}.no_such_member"),
            1 => format!("{name}[0]"),
            _ => format!("*{name}"),
        };

        computed.push((Purpose::IllTyped, text));
        computed
    }
}

fn integer(state: &VariableState) -> Option<i128> {
    match state {
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Signed(value)),
            ..
        } => Some(*value),
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Unsigned(value)),
            ..
        } => i128::try_from(*value).ok(),
        _ => None,
    }
}

#[derive(Debug, Clone)]
enum Caller {
    Trusted,

    Outermost(UnwindTermination),

    Corrupt(VirtualAddress),
}

impl Caller {
    fn of(backtrace: &Backtrace, level: u32) -> Self {
        let level = usize::try_from(level).unwrap_or(usize::MAX);
        let Some(frame) = backtrace.frames.get(level) else {
            return Self::Outermost(backtrace.termination.clone());
        };

        if frame.kind == FrameKind::Inline {
            return Self::Trusted;
        }

        match backtrace.frames.get(level + 1) {
            None => Self::Outermost(backtrace.termination.clone()),
            Some(caller) if caller.module.is_none() => {
                caller.instruction.map_or(Self::Trusted, Self::Corrupt)
            }
            Some(_) => Self::Trusted,
        }
    }
}

impl std::fmt::Display for Caller {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trusted => formatter.write_str("with a caller"),
            Self::Outermost(termination) => write!(formatter, "without a caller ({termination})"),
            Self::Corrupt(address) => {
                write!(formatter, "whose caller {address} is outside every module")
            }
        }
    }
}

fn presented_ambiguously(snapshot: &StateSnapshot) -> bool {
    snapshot
        .presentation
        .as_ref()
        .is_some_and(|presentation| matches!(presentation.frame, PresentedFrame::Ambiguous(_)))
}
