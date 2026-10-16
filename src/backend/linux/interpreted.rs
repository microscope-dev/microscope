//! Interpreted frames in a thread's backtrace. A runtime that interprets
//! code, as CPython does, names the frames each thread runs, cut into the
//! segments each activation of its interpreter's loop runs. The innermost
//! activation runs the innermost segment, and so on out: each segment shows
//! before its activation, which stays, as a driven future's chain shows
//! before the frame that polls it.
//!
//! Where the activations and segments differ in number, no segment's place
//! is known: they all show after the thread's machine frames, and a note
//! says why. A segment is never placed by its position alone.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::frames::{FrameRegisters, FrameScope, ResolvedFrame, StackRoot};
use super::native::InspectionOps;
use super::runtimes::BoundRuntime;
use super::{Controller, Inferior, debug_thread_id};

use crate::model::FrameMetadata;
use crate::protocol::PresentedFrame;
use crate::runtime_model::{Driven, InterpretedFrame, InterpretedStacks, Partial};
use crate::{
    CallFrameUnavailableReason, CodeRole, FrameKind, RecordedFrame, RuntimeId, StackFrame,
    StackFrameId, StackSegment, VariableUnavailableReason,
};

/// Where a thread's interpreted frames go in its backtrace.
#[derive(Debug, Default)]
pub(super) struct Placement {
    /// The segment each activation of an interpreter's loop runs,
    /// innermost first, by the activation's frame in the trace.
    pub(super) placed: BTreeMap<usize, Vec<InterpretedFrame>>,

    /// Why the segment the activation at each frame of the trace runs is
    /// shown in part, where it is.
    pub(super) notes: BTreeMap<usize, Arc<str>>,

    /// The frames whose place is unknown, innermost first, and why.
    pub(super) unplaced: Vec<InterpretedFrame>,
    pub(super) unplaced_reason: Option<Arc<str>>,
}

impl Placement {
    pub(super) fn is_empty(&self) -> bool {
        self.placed.is_empty() && self.notes.is_empty() && self.unplaced.is_empty()
    }

    /// Adds where another runtime's frames go. Two runtimes' frames whose
    /// place is unknown follow each other, the first's first.
    pub(super) fn merge(&mut self, other: Self) {
        self.placed.extend(other.placed);
        self.notes.extend(other.notes);
        self.unplaced.extend(other.unplaced);
        self.unplaced_reason = match (self.unplaced_reason.take(), other.unplaced_reason) {
            (Some(first), Some(second)) => Some(format!("{first}; {second}").into()),
            (first, second) => first.or(second),
        };
    }
}

/// The interpreted frames a runtime names at a stop, read once for a
/// published stop.
pub(super) type InterpretedCache =
    std::cell::RefCell<BTreeMap<RuntimeId, Option<Arc<Partial<InterpretedStacks>>>>>;

impl<P: InspectionOps> Controller<P> {
    /// The frames `runtime` interprets on the stop's threads.
    fn interpreted(
        &self,
        inferior: &Inferior,
        runtime: &BoundRuntime,
        root: &StackRoot,
    ) -> Option<Arc<Partial<InterpretedStacks>>> {
        let stop = inferior.public_stop.as_ref();
        if let Some(found) =
            stop.and_then(|stop| stop.interpreted.borrow().get(&runtime.id).cloned())
        {
            return found;
        }

        let found = self
            .with_runtime_stop(inferior, runtime, root.reader(), |stop| {
                runtime.model.interpreted(stop)
            })
            .map(Arc::new);
        if let Some(stop) = stop {
            stop.interpreted
                .borrow_mut()
                .insert(runtime.id, found.clone());
        }

        found
    }

    /// Where the frames each of `runtimes` interprets go in the trace of
    /// the stack `root` begins, given the trace's `drivers`, each with
    /// what it drives and the index of the runtime that says so. Every
    /// runtime that interprets code names its threads' frames, which show
    /// even where no activation of its loop is found.
    pub(super) fn placement(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        runtimes: &[BoundRuntime],
        drivers: &[(usize, Driven, usize)],
    ) -> Placement {
        let mut placement = Placement::default();
        for (index, runtime) in runtimes.iter().enumerate() {
            let activations: Vec<usize> = drivers
                .iter()
                .filter(|(_, driven, by)| *driven == Driven::Interpreted && *by == index)
                .map(|(frame, _, _)| *frame)
                .collect();
            placement.merge(self.place_interpreted(inferior, root, runtime, &activations));
        }

        placement
    }

    /// Where the interpreted frames of the thread whose stack `root`
    /// begins go, given the frames of its trace that are activations of
    /// `runtime`'s loop, innermost first.
    fn place_interpreted(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        runtime: &BoundRuntime,
        activations: &[usize],
    ) -> Placement {
        let mut placement = Placement::default();
        let Some(thread) = root.thread().map(debug_thread_id) else {
            return placement;
        };

        let Some(stacks) = self.interpreted(inferior, runtime, root) else {
            return placement;
        };

        let threads: Vec<_> = stacks
            .value
            .threads
            .iter()
            .filter(|interpreted| interpreted.thread == thread)
            .collect();
        let gaps = || {
            stacks
                .gaps
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<&str>>()
                .join("; ")
        };

        let [interpreted] = threads.as_slice() else {
            if threads.is_empty() {
                // A thread in the loop with no frames it names: the loop
                // may be entering or leaving, or its threads unread.
                if let (Some(&innermost), false) = (activations.first(), stacks.gaps.is_empty()) {
                    placement.notes.insert(innermost, gaps().into());
                }
            } else {
                placement.unplaced = threads
                    .iter()
                    .flat_map(|interpreted| stacks.value.frames_of(interpreted).iter().cloned())
                    .collect();
                placement.unplaced_reason = Some(
                    format!(
                        "the thread runs frames in {} interpreters' states, whose order is unknown",
                        threads.len()
                    )
                    .into(),
                );
            }

            return placement;
        };

        let frames = stacks.value.frames_of(interpreted);
        let segments: Vec<&[InterpretedFrame]> =
            frames.split_inclusive(|frame| frame.outermost).collect();
        if segments.len() != activations.len() {
            placement.unplaced = frames.to_vec();
            placement.unplaced_reason = Some(
                format!(
                    "the thread runs {} of them, and {} activations of the interpreter's loop",
                    plural(segments.len(), "segment"),
                    activations.len()
                )
                .into(),
            );
            return placement;
        }

        for (&activation, segment) in activations.iter().zip(&segments) {
            if let Some(problem) = segment.iter().find_map(|frame| frame.problem.clone()) {
                placement.notes.insert(activation, problem);
            }

            placement.placed.insert(activation, segment.to_vec());
        }

        if let (Some(&outermost), Some(end)) = (activations.last(), &interpreted.end) {
            placement
                .notes
                .entry(outermost)
                .or_insert_with(|| Arc::clone(end));
        }

        placement
    }
}

/// The frame of the backtrace that shows an interpreted frame, on the
/// stack `segment`.
pub(super) fn interpreted_frame(frame: &InterpretedFrame, segment: StackSegment) -> StackFrame {
    let mut built = StackFrame::from_parts(
        0,
        FrameKind::Interpreted { frame: frame.frame },
        None,
        None,
        FrameMetadata {
            code_instance: None,
            function: None,
            source: None,
            symbol: None,
            role: CodeRole::Ordinary,
        },
    );
    built.segment = segment;
    built.recorded = frame.function.clone().map(|function| RecordedFrame {
        function,
        place: frame.place.clone(),
    });
    built
}

/// An interpreted frame, as its inspection sees it: it has no registers,
/// no place on a machine stack, and no code in an image.
pub(super) fn resolved(id: StackFrameId, frame: StackFrame, activation: usize) -> ResolvedFrame {
    ResolvedFrame {
        id,
        presented: PresentedFrame::Physical,
        frame: Some(frame),
        code: None,
        scope: FrameScope::Unavailable,
        registers: FrameRegisters::Discarded,
        cfa: Err(VariableUnavailableReason::CallFrameUnavailable(
            CallFrameUnavailableReason::Interpreted,
        )
        .into()),
        activation,
        below_stack_pointer: None,
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}
