//! Follows a thread's interpreter frames from innermost through `previous`.
//! Thread- and generator-owned frames run code; an evaluation-loop activation's
//! entry frame sits beneath its frames and ends their segment.
//!
//! Each frame costs one read and its code object is checked against the process's code table.
//! Lines refer to the current instruction, or a caller's call; out-of-bytecode instructions
//! have no line, never the nearest one.

use std::collections::HashSet;

use super::code::{CodeId, CodeTable};
use super::objects::{Problem, Reader, Record, stack_reference};
use super::threads::ThreadState;
use crate::image::python::{Constant, Field};
use crate::runtime_model::{InterpretedFrame, SampledFrame};
use crate::{RecordedPlace, SourceLanguage, VirtualAddress};

/// The most frames of one thread read.
pub(super) const MAX_FRAMES: usize = 1 << 13;

/// What owns a frame, as this version of CPython numbers its owners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// The thread, a generator, or a frame object: the frame runs code.
    Code,

    /// The evaluation loop's entry frame, beneath the frames it runs.
    Entry,
}

/// Where a thread's frames are read from: the layout's reader, the code
/// table, and where `PyCode_Type` is.
pub(super) struct Walk<'a, 'b> {
    pub(super) reader: Reader<'a>,
    pub(super) code: &'b mut CodeTable,
    pub(super) code_type: VirtualAddress,
}

impl Walk<'_, '_> {
    fn owner(&self, frame: &Record<'_>) -> Result<Owner, Problem> {
        let owner = frame.number(Field::FrameOwner, 1)?;
        self.owner_of(owner).ok_or_else(|| {
            format!(
                "the frame at {:#x} has owner {owner}, which this CPython gives no frame",
                frame.address
            )
            .into()
        })
    }

    /// What CPython's owner number `owner` says owns a frame, if it names
    /// an owner of one.
    fn owner_of(&self, owner: u64) -> Option<Owner> {
        let layout = self.reader.layout;
        let is = |constant| owner == u64::from(layout.constant(constant));
        if is(Constant::OwnerThread)
            || is(Constant::OwnerGenerator)
            || is(Constant::OwnerFrameObject)
        {
            Some(Owner::Code)
        } else if is(Constant::OwnerEntry) {
            Some(Owner::Entry)
        } else {
            None
        }
    }

    /// The frames of the thread whose state is `state`, innermost first,
    /// and why they end before the thread's outermost, if they do.
    pub(super) fn thread(
        &mut self,
        state: &ThreadState,
    ) -> (Vec<InterpretedFrame>, Option<Problem>) {
        let mut frames: Vec<InterpretedFrame> = Vec::new();
        let mut seen = HashSet::new();
        let mut next = state.frame;
        let end = loop {
            // From 3.15, ending before the base frame indicates a concurrent change.
            let Some(address) = next else {
                break (state.base.is_some() && !seen.is_empty())
                    .then(|| "the thread's frames end before its base frame".into());
            };

            // From 3.15, the thread's base frame ends its chain.
            if Some(address) == state.base {
                break None;
            }

            if !seen.insert(address) {
                break Some("the thread's frames loop".into());
            }

            if seen.len() > MAX_FRAMES {
                break Some(format!("the thread has more than {MAX_FRAMES} frames").into());
            }

            let frame = match self.reader.record(address, Field::FrameSize) {
                Ok(frame) => frame,
                Err(problem) => break Some(problem),
            };

            match self.owner(&frame) {
                Ok(Owner::Code) => frames.push(self.frame(&frame)),
                Ok(Owner::Entry) => {
                    if let Some(outermost) = frames.last_mut() {
                        outermost.outermost = true;
                    }
                }
                Err(problem) => break Some(problem),
            }

            next = match frame.pointer(Field::FramePrevious) {
                Ok(previous) => previous,
                Err(problem) => break Some(problem),
            };
        };

        // Each activation of the loop pushes an entry frame beneath what it
        // runs, so frames with none beneath them belong to no activation.
        let end = match frames.last_mut() {
            Some(outermost) if !outermost.outermost => {
                outermost.outermost = true;
                end.or_else(|| Some("the outermost frames have no entry frame beneath them".into()))
            }
            _ => end,
        };

        (frames, end)
    }

    /// One frame that runs code, a thread's or a coroutine's.
    pub(super) fn frame(&mut self, frame: &Record<'_>) -> InterpretedFrame {
        let mut built = InterpretedFrame {
            frame: frame.address,
            language: SourceLanguage::Python,
            function: None,
            place: None,
            problem: None,
            first_line: None,
            outermost: false,
            role: crate::CodeRole::Ordinary,
        };

        match self.located(frame) {
            Ok((code, line)) => {
                let row = self.code.row(code);
                built.function = Some(std::sync::Arc::clone(self.code.name(row.qualname)));
                built.first_line = u32::try_from(row.first_line).ok();
                let path = std::sync::Arc::clone(self.code.name(row.file));
                built.role = role(&path);

                match line {
                    Ok(line) => {
                        built.place = Some(RecordedPlace {
                            path,
                            line,
                            column: 0,
                        });
                    }
                    Err(_) if row.internal => {}
                    Err(problem) => built.problem = Some(problem),
                }
            }
            Err(problem) => built.problem = Some(problem),
        }

        built
    }

    /// The frame's code, and its line or why it has none.
    fn located(&mut self, frame: &Record<'_>) -> Result<(CodeId, Result<u32, Problem>), Problem> {
        let (code, id) = self.code_of(frame)?;
        Ok((id, self.line(frame, code, id)))
    }

    /// The code object the frame runs, and its row in the code table.
    pub(super) fn code_of(
        &mut self,
        frame: &Record<'_>,
    ) -> Result<(VirtualAddress, CodeId), Problem> {
        let layout = self.reader.layout;
        let executable = frame.word(Field::FrameExecutable)?;
        let code = if layout.constant(Constant::StackRefs) == 0 {
            Some(executable)
                .filter(|address| *address != 0)
                .map(VirtualAddress::new)
        } else {
            stack_reference(executable)
        }
        .ok_or_else(|| -> Problem {
            format!("the frame at {:#x} runs no code", frame.address).into()
        })?;

        Ok((code, self.code.code(&self.reader, self.code_type, code)?))
    }

    /// The line of the frame's instruction in its code `id`, at `code`.
    fn line(&self, frame: &Record<'_>, code: VirtualAddress, id: CodeId) -> Result<u32, Problem> {
        let instruction = frame.word(Field::FrameInstruction)?;
        let bytecode = self.bytecode(frame, code)?;
        let offset = instruction.wrapping_sub(bytecode.get()).cast_signed();
        self.line_at(id, code, offset)
            .map_err(|problem| format!("the frame at {:#x} {problem}", frame.address).into())
    }

    /// The line of the instruction at byte `offset` of the bytecode of the
    /// code `id`, at `code`; or why it has none, to follow the frame it is
    /// a frame's.
    fn line_at(&self, id: CodeId, code: VirtualAddress, offset: i64) -> Result<u32, Problem> {
        let row = self.code.row(id);
        let length = row.units.saturating_mul(super::linetable::CODE_UNIT);
        let outside =
            || -> Problem { format!("is at byte {offset}, outside its code's bytecode").into() };

        // 3.12 points at the instruction before the one to run, which for
        // a frame that has run none is before the first.
        let line = if offset < 0 {
            if offset < -i64::try_from(super::linetable::CODE_UNIT).unwrap_or(0) {
                return Err(outside());
            }

            Some(i64::from(row.first_line))
        } else {
            let offset = offset.cast_unsigned();
            if offset >= length {
                return Err(outside());
            }

            self.code.line(id, offset).map_err(|_| -> Problem {
                format!("runs code at {code:#x}, whose line table is malformed").into()
            })?
        };

        line.and_then(|line| u32::try_from(line).ok())
            .filter(|line| *line != 0)
            .ok_or_else(|| "is at no line".into())
    }

    /// The frames a walker in the kernel read of one thread, innermost
    /// first, as [`Self::thread`] reads a thread's own, and why they end
    /// before the thread's outermost, if they do. A frame's code object
    /// is read now, and proved to be the one the walker read.
    pub(super) fn sampled(
        &mut self,
        sampled: &[SampledFrame],
    ) -> (Vec<InterpretedFrame>, Option<Problem>) {
        let mut frames: Vec<InterpretedFrame> = Vec::new();
        let mut end = None;

        for frame in sampled {
            match self.owner_of(u64::from(frame.owner)) {
                Some(Owner::Code) => frames.push(self.sampled_frame(frame)),
                Some(Owner::Entry) => {
                    if let Some(outermost) = frames.last_mut() {
                        outermost.outermost = true;
                    }
                }
                None => {
                    end = Some(
                        format!(
                            "a frame has owner {}, which this CPython gives no frame",
                            frame.owner
                        )
                        .into(),
                    );
                    break;
                }
            }
        }

        let end = match frames.last_mut() {
            Some(outermost) if !outermost.outermost => {
                outermost.outermost = true;
                end.or_else(|| Some("the outermost frames have no entry frame beneath them".into()))
            }
            _ => end,
        };

        (frames, end)
    }

    /// One frame a walker in the kernel read.
    fn sampled_frame(&mut self, frame: &SampledFrame) -> InterpretedFrame {
        let mut built = InterpretedFrame {
            frame: VirtualAddress::new(0),
            language: SourceLanguage::Python,
            function: None,
            place: None,
            problem: None,
            first_line: None,
            outermost: false,
            role: crate::CodeRole::Ordinary,
        };

        let located = self
            .code
            .code(&self.reader, self.code_type, frame.code)
            .and_then(|id| {
                if self.code.proves(id, frame.proof) {
                    Ok(id)
                } else {
                    Err(
                        format!("the code object at {:#x} the frame ran is gone", frame.code)
                            .into(),
                    )
                }
            });

        let id = match located {
            Ok(id) => id,
            Err(problem) => {
                built.problem = Some(problem);
                return built;
            }
        };

        let row = self.code.row(id);
        built.function = Some(std::sync::Arc::clone(self.code.name(row.qualname)));
        built.first_line = u32::try_from(row.first_line).ok();
        let path = std::sync::Arc::clone(self.code.name(row.file));
        built.role = role(&path);
        let line = frame
            .offset
            .ok_or_else(|| -> Problem { "its instruction could not be read".into() })
            .and_then(|offset| self.line_at(id, frame.code, offset));
        match line {
            Ok(line) => {
                built.place = Some(RecordedPlace {
                    path,
                    line,
                    column: 0,
                });
            }
            Err(_) if row.internal => {}
            Err(problem) => {
                built.problem =
                    Some(format!("the frame running {:#x} {problem}", frame.code).into());
            }
        }

        built
    }

    /// Where the bytecode the frame runs begins: the code object's own,
    /// or on a free-threaded build the thread's copy of it.
    fn bytecode(
        &self,
        frame: &Record<'_>,
        code: VirtualAddress,
    ) -> Result<VirtualAddress, Problem> {
        let reader = &self.reader;
        if !reader.layout.has(Field::FrameTlbcIndex) {
            return Ok(code.wrapping_add(reader.offset(Field::CodeCodeAdaptive)?));
        }

        let index = u64::try_from(frame.int(Field::FrameTlbcIndex)?).map_err(|_| -> Problem {
            format!(
                "the frame at {:#x} names no copy of its bytecode",
                frame.address
            )
            .into()
        })?;
        let copies = VirtualAddress::new(reader.field_word(code, Field::CodeTlbc)?);
        let count = reader.field_word(copies, Field::CodeArraySize)?;
        if index >= count {
            return Err(format!(
                "the frame at {:#x} runs copy {index} of {count} of its bytecode",
                frame.address
            )
            .into());
        }

        let entries = copies.wrapping_add(reader.offset(Field::CodeArrayEntries)?);
        Some(reader.word(entries.wrapping_add(index * 8))?)
            .filter(|address| *address != 0)
            .map(VirtualAddress::new)
            .ok_or_else(|| {
                format!(
                    "the frame at {:#x} runs a copy of its bytecode that is gone",
                    frame.address
                )
                .into()
            })
    }
}

/// The part code compiled under `path` plays: only the agent's code and a
/// launch bootstrap's are the debugger's machinery.
fn role(path: &str) -> crate::CodeRole {
    if super::agent::is_agent_filename(path)
        || super::agent::is_bootstrap_filename(path)
        || super::agent::is_script_filename(path)
    {
        crate::CodeRole::Interpreter
    } else {
        crate::CodeRole::Ordinary
    }
}
