//! Physical frames whose code a runtime compiled, which the runtime names.
//!
//! A runtime such as V8 runs its functions on the native stack, in code
//! that no image's debug information describes: its own builtins, and the
//! code it generates for the program. The unwinder walks those frames by
//! the runtime's convention, and the runtime, which recorded what each
//! runs, names it: the function, its place, and the functions its code
//! inlined, innermost first. Only frames whose code the runtime is proven
//! to own are asked about, never a frame because of what its stack holds.

use std::collections::BTreeSet;

use super::frames::{Expanded, FrameOrigin, FrameSource, PhysicalStack, StackRoot};
use super::runtimes::{BoundRuntime, ProcessCode};
use super::{Controller, Inferior};
use crate::backend::linux::native::InspectionOps;
use crate::error::{Error, Result};
use crate::runtime_model::{CodeFrame, DescribedFrame};
use crate::unwind::{UnwindModule, unwind_module_for};
use crate::{FrameKind, ModuleId, RuntimeId, StackFrame};

impl<P: InspectionOps> Controller<P> {
    /// Names each frame of `expanded` whose code a runtime owns as the
    /// runtime recorded it, adding the functions its code inlined.
    pub(super) fn describe_code_frames(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        modules: &[UnwindModule<'_>],
        expanded: &mut Expanded,
    ) -> Result<()> {
        let Some(thread) = root.thread().map(super::debug_thread_id) else {
            return Ok(());
        };

        let runtimes = self.runtimes(inferior);
        if runtimes.is_empty() {
            return Ok(());
        }
        let _span = crate::span!("code_frames");
        let generated = self.generated_code_modules();
        let process_code = self.runtime_process_code(inferior, &runtimes, root.reader());
        let owners = Owned {
            runtimes: &runtimes,
            generated: &generated,
            process_code: &process_code,
        };
        let mut described = Vec::new();
        for (activation, physical) in stack.frames.iter().enumerate() {
            let Some((owner, generated)) = owner(&owners, modules, stack, activation) else {
                continue;
            };

            let frame = CodeFrame {
                thread,
                instruction: physical.context.instruction,
                after_call: activation != 0 || stack.after_call,
                generated,
                registers: &physical.registers,
            };
            let description = self.with_runtime_stop(inferior, owner, root.reader(), |stop| {
                owner.model.code_frame(stop, &frame)
            });
            if let Some(description) = description {
                described.push((activation, owner.id, description));
            }
        }

        crate::count!("code_frames_described", described.len());
        if described.is_empty() {
            return Ok(());
        }
        rewrite(expanded, &described)
    }
}

/// What the process's runtimes own at a stop.
struct Owned<'a> {
    runtimes: &'a [BoundRuntime],

    /// The modules of code a runtime generated.
    generated: &'a BTreeSet<ModuleId>,

    /// The code each runtime owns as the process says, in the order of
    /// `runtimes`.
    process_code: &'a [Vec<ProcessCode>],
}

/// The runtime that owns the code of a stack's `activation`, if one does:
/// the code is in one of the modules a runtime generated, in the image the
/// runtime was bound to, as the runtime says, through any binding of that
/// image, such as a copy of its code, or where the runtime says the
/// process keeps code it owns.
fn owner<'a>(
    owned: &Owned<'a>,
    modules: &[UnwindModule<'_>],
    stack: &PhysicalStack,
    activation: usize,
) -> Option<(&'a BoundRuntime, bool)> {
    let lookup = stack.lookup_address(activation)?;
    let in_module = unwind_module_for(modules, lookup);
    if let Some((module, _)) = in_module
        && owned.generated.contains(&module.loaded.id)
    {
        return owned
            .runtimes
            .iter()
            .find(|runtime| runtime.model.owns_generated_code())
            .map(|runtime| (runtime, true));
    }

    let by_image = in_module.and_then(|(module, address)| {
        owned.runtimes.iter().find(|runtime| {
            runtime.module.image == module.loaded.image && runtime.model.owns_code(address)
        })
    });
    let code = in_module.map(|(module, address)| (module.loaded.image, address));
    by_image
        .or_else(|| {
            owned
                .runtimes
                .iter()
                .zip(owned.process_code)
                .find(|(_, ranges)| ranges.iter().any(|range| range.holds(lookup, code)))
                .map(|(runtime, _)| runtime)
        })
        .map(|runtime| (runtime, false))
}

/// Replaces each described activation's frames with the functions its
/// runtime recorded: those inlined as inline frames, then its own as the
/// physical frame, which keeps its instruction, module, and symbol. Each
/// described frame's source is its runtime. A description with no function
/// only sets the role.
fn rewrite(
    expanded: &mut Expanded,
    described: &[(usize, RuntimeId, DescribedFrame)],
) -> Result<()> {
    let mut frames = Vec::with_capacity(expanded.trace.frames.len());
    let mut origins = Vec::with_capacity(expanded.origins.len());
    let mut next = described.iter().peekable();
    let mut index = 0;
    let old = expanded.trace.frames.to_vec();

    while index < old.len() {
        let origin = expanded.origins[index];
        let description = next
            .peek()
            .filter(|(activation, ..)| {
                *activation == origin.activation && origin.source == FrameSource::Activation
            })
            .map(|(_, runtime, description)| (*runtime, description));
        let Some((runtime, description)) = description else {
            frames.push(old[index].clone());
            origins.push(origin);
            index += 1;
            continue;
        };
        next.next();

        // The activation's own frames: its function and any the image's
        // debug information inlined into it, the physical frame last.
        let end = (index..old.len())
            .find(|&at| !matches!(old[at].kind, FrameKind::Inline))
            .filter(|&end| expanded.origins[end] == origin)
            .ok_or_else(|| Error::internal("an activation's frames end in its physical frame"))?;
        let first = frames.len();
        push_described(&mut frames, &old[index..=end], description);
        let count = frames.len() - first;
        origins.extend((0..count).map(|at| FrameOrigin {
            activation: origin.activation,
            source: if description.functions.is_empty() {
                origin.source
            } else {
                FrameSource::Described {
                    runtime,
                    inlined: at + 1 < count,
                    live_registers: description.live_registers,
                }
            },
        }));
        index = end + 1;
    }

    for (level, frame) in frames.iter_mut().enumerate() {
        let level = u32::try_from(level).map_err(|_| Error::internal("frame count fits in u32"))?;
        frame.level = level;
        frame.id = crate::StackFrameId::new(level);
    }

    expanded.trace.frames = frames.into();
    expanded.origins = origins;
    Ok(())
}

/// Pushes the frames `description` gives the activation whose frames are
/// `activation`, its physical frame last.
fn push_described(
    frames: &mut Vec<StackFrame>,
    activation: &[StackFrame],
    description: &DescribedFrame,
) {
    let Some(physical) = activation.last() else {
        return;
    };
    let Some((own, inlined)) = description.functions.split_last() else {
        frames.extend(activation.iter().map(|frame| StackFrame {
            role: description.role,
            ..frame.clone()
        }));
        return;
    };

    for function in inlined {
        let mut frame = physical.clone();
        frame.kind = FrameKind::Inline;
        frame.symbol = None;
        frame.code_instance = None;
        frame.function = None;
        frame.source = None;
        frame.role = description.role;
        frame.recorded = Some(function.clone());
        frames.push(frame);
    }

    let mut frame = physical.clone();
    frame.code_instance = None;
    frame.function = None;
    frame.source = None;
    frame.role = description.role;
    frame.recorded = Some(own.clone());
    frames.push(frame);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::frames::{Expanded, FrameOrigin, FrameSource};
    use super::rewrite;
    use crate::runtime_model::DescribedFrame;
    use crate::{
        Backtrace, CodeRole, FrameKind, RecordedFrame, RuntimeId, SourceLanguage, StackFrame,
        ThreadId, UnwindTermination, VirtualAddress,
    };

    fn frame(kind: FrameKind, instruction: u64) -> StackFrame {
        StackFrame::new(0, kind, None, VirtualAddress::new(instruction))
    }

    fn recorded(function: &str) -> RecordedFrame {
        RecordedFrame {
            function: function.into(),
            language: SourceLanguage::JavaScript,
            place: None,
            first_line: None,
            generated: None,
            problem: None,
        }
    }

    /// Three activations: the first and last plain, the middle one with a
    /// function its image's debug information inlined into it.
    fn expanded() -> Expanded {
        let frames = [
            frame(FrameKind::Physical, 0x10),
            frame(FrameKind::Inline, 0x20),
            frame(FrameKind::Physical, 0x20),
            frame(FrameKind::Physical, 0x30),
        ];
        let origins = [0, 1, 1, 2].map(|activation| FrameOrigin {
            activation,
            source: FrameSource::Activation,
        });
        Expanded {
            trace: Backtrace {
                context: ThreadId::new(1).into(),
                frames: frames.into(),
                termination: UnwindTermination::Complete,
                unfollowed: Arc::from([]),
            },
            origins: origins.into(),
            futures: Vec::new(),
            interpreted: Vec::new(),
            lost: Vec::new(),
        }
    }

    /// A described activation's frames, its image's inlined ones included,
    /// become the runtime's: the functions its code inlined as inline
    /// frames, innermost first, then its own on the physical frame, which
    /// keeps its instruction. Levels and ids are renumbered, and each frame
    /// keeps its activation.
    #[test]
    fn a_described_activation_becomes_the_functions_its_runtime_recorded() {
        let mut expanded = expanded();
        let description = DescribedFrame {
            functions: vec![recorded("inner"), recorded("middle"), recorded("outer")],
            role: CodeRole::Ordinary,
            live_registers: true,
        };
        rewrite(&mut expanded, &[(1, RuntimeId::new(7), description)]).unwrap();

        let shape = expanded
            .trace
            .frames
            .iter()
            .map(|frame| {
                (
                    frame.level,
                    frame.id.get(),
                    frame.kind,
                    frame.instruction.map(VirtualAddress::get),
                    frame.recorded.as_ref().map(|recorded| &*recorded.function),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            [
                (0, 0, FrameKind::Physical, Some(0x10), None),
                (1, 1, FrameKind::Inline, Some(0x20), Some("inner")),
                (2, 2, FrameKind::Inline, Some(0x20), Some("middle")),
                (3, 3, FrameKind::Physical, Some(0x20), Some("outer")),
                (4, 4, FrameKind::Physical, Some(0x30), None),
            ]
        );
        let activations = expanded
            .origins
            .iter()
            .map(|origin| origin.activation)
            .collect::<Vec<_>>();
        assert_eq!(activations, [0, 1, 1, 1, 2]);

        // Each described frame names its runtime; only the last is not inlined.
        let runtime = RuntimeId::new(7);
        let sources = expanded
            .origins
            .iter()
            .map(|origin| origin.source)
            .collect::<Vec<_>>();
        assert_eq!(
            sources,
            [
                FrameSource::Activation,
                FrameSource::Described {
                    runtime,
                    inlined: true,
                    live_registers: true,
                },
                FrameSource::Described {
                    runtime,
                    inlined: true,
                    live_registers: true,
                },
                FrameSource::Described {
                    runtime,
                    inlined: false,
                    live_registers: true,
                },
                FrameSource::Activation,
            ]
        );
    }

    /// A description with no function, as of V8's own machinery, keeps the
    /// activation's frames and gives them its role.
    #[test]
    fn a_description_without_functions_only_sets_the_role() {
        let mut expanded = expanded();
        let description = DescribedFrame {
            functions: Vec::new(),
            role: CodeRole::Interpreter,
            live_registers: false,
        };
        rewrite(&mut expanded, &[(1, RuntimeId::new(7), description)]).unwrap();

        let roles = expanded
            .trace
            .frames
            .iter()
            .map(|frame| (frame.kind, frame.role))
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            [
                (FrameKind::Physical, CodeRole::Ordinary),
                (FrameKind::Inline, CodeRole::Interpreter),
                (FrameKind::Physical, CodeRole::Interpreter),
                (FrameKind::Physical, CodeRole::Ordinary),
            ]
        );
    }
}
