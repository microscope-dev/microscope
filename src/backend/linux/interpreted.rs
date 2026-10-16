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

use crate::eval::types::UNIDENTIFIED;
use crate::inspection::InspectionBudget;
use crate::model::FrameMetadata;
use crate::model::{ValueStorage, ViewChildren};
use crate::protocol::PresentedFrame;
use crate::runtime_model::{
    Driven, InterpretedChild, InterpretedFrame, InterpretedKind, InterpretedSlot,
    InterpretedStacks, InterpretedValue, Partial,
};
use crate::{
    AddressValue, ArgumentOrigin, CallFrameUnavailableReason, CodeRole, DereferenceState, Error,
    ExecutionContext, FloatValue, FrameKind, LoadedModule, MapKey, Presentation, PresentedCount,
    PresentedShape, RecordedFrame, Result, RuntimeId, ScalarValue, SourceLanguage, StackFrame,
    StackFrameId, StackSegment, StopId, TypeIdentity, TypeInfo, TypeKind,
    UnsupportedVariableFeature, ValueChild, ValueChildPage, ValueChildQuery,
    ValueChildRelationship, ValueChildren, ValueChildrenReference, Variable, VariableKind,
    VariableQuery, VariableState, VariableUnavailableReason, VariableValue, VariableValueSource,
    ViewName, ViewProblem, VirtualAddress,
};

/// An interpreted frame of a trace, and the runtime that interprets it.
#[derive(Debug, Clone)]
pub(super) struct Placed {
    pub(super) runtime: RuntimeId,
    pub(super) frame: InterpretedFrame,
}

/// Where a thread's interpreted frames go in its backtrace.
#[derive(Debug, Default)]
pub(super) struct Placement {
    /// The segment each activation of an interpreter's loop runs,
    /// innermost first, by the activation's frame in the trace.
    pub(super) placed: BTreeMap<usize, Vec<Placed>>,

    /// Why the segment the activation at each frame of the trace runs is
    /// shown in part, where it is.
    pub(super) notes: BTreeMap<usize, Arc<str>>,

    /// The frames whose place is unknown, innermost first, and why.
    pub(super) unplaced: Vec<Placed>,
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

    /// Whether a runtime interprets frames on the thread whose stack
    /// `root` begins.
    pub(super) fn interprets_on(&self, inferior: &Inferior, root: &StackRoot) -> bool {
        let Some(thread) = root.thread().map(debug_thread_id) else {
            return false;
        };

        self.runtimes(inferior).iter().any(|runtime| {
            self.interpreted(inferior, runtime, root)
                .is_some_and(|stacks| stacks.value.threads.iter().any(|on| on.thread == thread))
        })
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

        let placed = |frames: &[InterpretedFrame]| -> Vec<Placed> {
            frames
                .iter()
                .map(|frame| Placed {
                    runtime: runtime.id,
                    frame: frame.clone(),
                })
                .collect()
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
                    .flat_map(|interpreted| placed(stacks.value.frames_of(interpreted)))
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
            placement.unplaced = placed(frames);
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

            placement.placed.insert(activation, placed(segment));
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
/// no place on a machine stack, and no code in an image; its interpreter
/// keeps its variables.
pub(super) fn resolved(
    id: StackFrameId,
    frame: StackFrame,
    activation: usize,
    placed: &Placed,
) -> ResolvedFrame {
    ResolvedFrame {
        id,
        presented: PresentedFrame::Physical,
        frame: Some(frame),
        code: None,
        scope: FrameScope::Interpreted {
            runtime: placed.runtime,
            frame: placed.frame.frame,
        },
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

/// The children of an interpreter's object, which a value's capability
/// pages: only this backend reads them, through the runtime that
/// interprets the object.
#[derive(Debug)]
pub(super) struct InterpretedChildren {
    runtime: RuntimeId,
}

/// Where an interpreted frame's values are read, for their children's
/// capabilities.
struct Origin {
    stop: StopId,
    context: ExecutionContext,
    frame: StackFrameId,
    runtime: RuntimeId,
    module: LoadedModule,
}

impl<P: InspectionOps> Controller<P> {
    fn bound_runtime(&self, inferior: &Inferior, id: RuntimeId) -> Result<BoundRuntime> {
        self.runtimes(inferior)
            .into_iter()
            .find(|runtime| runtime.id == id)
            .ok_or_else(|| {
                Error::VariableRuntime(format!("runtime {id} is not bound to the process").into())
            })
    }

    /// The variables of the interpreted frame `resolved`, which `runtime`
    /// keeps at `frame`, that `query` names.
    #[allow(
        clippy::too_many_arguments,
        reason = "the stop, the frame, and what the runtime names it"
    )]
    pub(super) fn interpreted_variables(
        &self,
        inferior: &Inferior,
        stop: StopId,
        root: &StackRoot,
        resolved: StackFrameId,
        (runtime, frame): (RuntimeId, VirtualAddress),
        query: &VariableQuery,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>> {
        let bound = self.bound_runtime(inferior, runtime)?;
        let locals = self
            .with_runtime_stop(inferior, &bound, root.reader(), |runtime_stop| {
                bound.model.interpreted_locals(runtime_stop, frame, budget)
            })
            .map_err(Error::VariableRuntime)?;
        let origin = Origin {
            stop,
            context: root.context,
            frame: resolved,
            runtime,
            module: bound.module,
        };

        let mut variables = Vec::new();
        for local in locals {
            if let VariableQuery::Name(name) = query
                && *local.name != **name
            {
                continue;
            }

            // Once a read has exhausted the budget, the rest of the locals
            // show that they were not read, as they were not.
            if budget.exhaustion().is_none() && budget.consume_variable_value().is_err() {
                break;
            }

            let (type_info, state) = origin.slot(local.slot);
            variables.push(Variable {
                kind: if local.parameter {
                    VariableKind::Parameter
                } else {
                    VariableKind::Local
                },
                global: None,
                name: local.name,
                declaration: None,
                type_info,
                unresolved_shape: None,
                state,
            });
        }

        if let VariableQuery::Name(name) = query
            && variables.is_empty()
        {
            return Err(Error::VariableNotFound(name.clone()));
        }

        Ok(variables)
    }

    /// The page `query` asks for of the children of the interpreter's
    /// object `reference` names.
    pub(super) fn interpreted_children(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        reference: &ValueChildrenReference,
        children: &InterpretedChildren,
        query: &ValueChildQuery,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChildPage> {
        let ValueStorage::Memory(object) = reference.storage else {
            return Err(Error::VariableContextUnsupported);
        };

        let bound = self.bound_runtime(inferior, children.runtime)?;
        let end = query
            .offset
            .saturating_add(u64::from(query.limit))
            .min(reference.total);
        let start = query.offset.min(end);
        let read = self
            .with_runtime_stop(inferior, &bound, root.reader(), |runtime_stop| {
                bound
                    .model
                    .interpreted_children(runtime_stop, object, start..end, budget)
            })
            .map_err(Error::VariableRuntime)?;
        let origin = Origin {
            stop: reference.stop_id,
            context: reference.context,
            frame: reference.frame,
            runtime: children.runtime,
            module: bound.module,
        };

        let mut page = Vec::new();
        for (index, child) in (start..).zip(read) {
            if budget.exhaustion().is_none() && budget.consume_value_nodes(1).is_err() {
                break;
            }

            page.push(match child {
                InterpretedChild::Element(slot) => {
                    let (type_info, state) = origin.slot(slot);
                    ValueChild {
                        relationship: ValueChildRelationship::Element { index },
                        type_info: type_info.unwrap_or_else(untyped),
                        state,
                    }
                }
                InterpretedChild::Entry { key, value } => {
                    let (key_type, key_state) = origin.slot(key);
                    let (type_info, state) = origin.slot(value);
                    ValueChild {
                        relationship: ValueChildRelationship::Entry {
                            index,
                            key: Arc::new(MapKey {
                                type_info: key_type.unwrap_or_else(untyped),
                                state: key_state,
                            }),
                        },
                        type_info: type_info.unwrap_or_else(untyped),
                        state,
                    }
                }
            });
        }

        Ok(ValueChildPage {
            stop_id: reference.stop_id,
            offset: start,
            total: reference.total,
            children: page.into(),
            completion: budget.completion(),
            usage: budget.usage(),
        })
    }
}

impl Origin {
    /// The type and state the debugger presents a slot's value as.
    fn slot(&self, slot: InterpretedSlot) -> (Option<TypeInfo>, VariableState) {
        let value = match slot {
            InterpretedSlot::Value(value) => value,
            InterpretedSlot::Unbound => {
                return (
                    None,
                    VariableState::Unavailable(VariableUnavailableReason::Unbound),
                );
            }
            InterpretedSlot::Cell => {
                return (
                    None,
                    VariableState::Unavailable(VariableUnavailableReason::Unsupported(
                        UnsupportedVariableFeature::SharedCell,
                    )),
                );
            }
            InterpretedSlot::Limit(exhaustion) => {
                return (
                    None,
                    VariableState::Unavailable(VariableUnavailableReason::InspectionLimit(
                        exhaustion,
                    )),
                );
            }
            InterpretedSlot::Unreadable(reason) => {
                return (
                    None,
                    VariableState::Unavailable(VariableUnavailableReason::Interpreted(reason)),
                );
            }
        };

        let (scalar, shape) = match value.kind {
            InterpretedKind::Nothing => (None, PresentedShape::Empty),
            InterpretedKind::Bool(value) => {
                (Some(ScalarValue::Boolean(value)), PresentedShape::Value)
            }
            InterpretedKind::Int(value) => (value.map(ScalarValue::Signed), PresentedShape::Value),
            InterpretedKind::Float(value) => (
                Some(ScalarValue::Floating(FloatValue::Binary64(value.to_bits()))),
                PresentedShape::Value,
            ),
            InterpretedKind::Text => (None, PresentedShape::Text),
            InterpretedKind::Bytes | InterpretedKind::Other => (None, PresentedShape::Value),
            InterpretedKind::Sequence { .. } => (None, PresentedShape::Sequence),
            InterpretedKind::Map { .. } => (None, PresentedShape::Map),
        };

        let count = value.children();
        let children = if count == 0 {
            ValueChildren::NotApplicable
        } else {
            ValueChildren::Available(Arc::new(self.children(value.object, count)))
        };

        let presentation = Presentation {
            view: Arc::new(ViewName {
                source: "microscope".into(),
                line: 0,
                header: "Python objects".into(),
                extend: false,
            }),
            shape,
            count: matches!(
                value.kind,
                InterpretedKind::Sequence { .. } | InterpretedKind::Map { .. }
            )
            .then_some(PresentedCount::Exact(count)),
            summary: Arc::clone(&value.summary),
            children,
            problem: value.problem.clone().map(|problem| {
                ViewProblem::Unavailable(VariableUnavailableReason::Interpreted(problem))
            }),
            number: None,
            presented: None,
            visualizers: Arc::from([]),
        };

        let state = VariableState::Available {
            source: VariableValueSource::Memory(value.object),
            raw: None,
            value: scalar.map_or(
                VariableValue::Address(AddressValue {
                    address: value.object,
                    function: None,
                }),
                VariableValue::Scalar,
            ),
            dereference: DereferenceState::NotApplicable,
            children: ValueChildren::NotApplicable,
            text: None,
            presentation: Some(Arc::new(presentation)),
        };

        (Some(python_type(&value)), state)
    }

    /// The capability of the `count` children of the object at `object`.
    fn children(&self, object: VirtualAddress, count: u64) -> ValueChildrenReference {
        ValueChildrenReference {
            stop_id: self.stop,
            context: self.context,
            frame: self.frame,
            module: self.module.id,
            image: self.module.image,
            context_address: None,
            target_type: UNIDENTIFIED.id,
            storage: ValueStorage::Memory(object),
            total: count,
            active_variant: None,
            view: Some(ViewChildren {
                bound: Some(Arc::new(InterpretedChildren {
                    runtime: self.runtime,
                })),
                elements: count,
                fields: 0,
                inner: None,
                picked: None,
            }),
            placement: None,
        }
    }
}

/// The type of an interpreter's object: a Python type, named by its module
/// and its name there, with no layout the debugger computes with.
fn python_type(value: &InterpretedValue) -> TypeInfo {
    let name = match value.module.as_deref() {
        Some(module) if module != "builtins" => format!("{module}.{}", value.type_name),
        _ => value.type_name.to_string(),
    };

    TypeInfo {
        reference: UNIDENTIFIED,
        name: name.into(),
        byte_size: None,
        kind: TypeKind::Opaque {
            description: "a Python object".into(),
        },
        identity: Some(Arc::new(TypeIdentity {
            language: SourceLanguage::Python,
            path: value.module.iter().cloned().collect(),
            inline_namespaces: Arc::from([]),
            base: Arc::clone(&value.type_name),
            arguments: Arc::from([]),
            pack: None,
            origin: ArgumentOrigin::None,
            go: None,
        })),
    }
}

/// The type of a child whose slot holds no object.
fn untyped() -> TypeInfo {
    TypeInfo {
        reference: UNIDENTIFIED,
        name: "<unknown type>".into(),
        byte_size: None,
        kind: TypeKind::Unspecified,
        identity: None,
    }
}
