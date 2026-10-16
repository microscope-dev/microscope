//! A thread's chain of interpreter frames, from its innermost by each
//! frame's `previous`. Each frame says who owns it: the thread or a
//! generator owns a frame that runs Python code, and an entry frame is
//! the one each activation of the evaluation loop pushes beneath the
//! frames it runs, so it ends their segment.
//!
//! A frame is one read. Its code object is proved against the process's
//! code table, and its line is the line table's for its instruction: the
//! one it runs, or for a caller the call it is in. An instruction outside
//! its code's bytecode has no line, never the nearest one.

use std::collections::HashSet;

use super::code::{CodeId, CodeTable};
use super::objects::{Problem, Reader, Record, stack_reference};
use super::threads::ThreadState;
use crate::image::python::{Constant, Field};
use crate::runtime_model::InterpretedFrame;
use crate::{RecordedPlace, VirtualAddress};

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
        let layout = self.reader.layout;
        let is = |constant| owner == u64::from(layout.constant(constant));
        if is(Constant::OwnerThread)
            || is(Constant::OwnerGenerator)
            || is(Constant::OwnerFrameObject)
        {
            Ok(Owner::Code)
        } else if is(Constant::OwnerEntry) {
            Ok(Owner::Entry)
        } else {
            Err(format!(
                "the frame at {:#x} has owner {owner}, which this CPython gives no frame",
                frame.address
            )
            .into())
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
            let Some(address) = next else {
                break None;
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

    /// One frame that runs code.
    fn frame(&mut self, frame: &Record<'_>) -> InterpretedFrame {
        let mut built = InterpretedFrame {
            frame: frame.address,
            function: None,
            place: None,
            problem: None,
            outermost: false,
        };

        match self.located(frame) {
            Ok((code, line)) => {
                let row = self.code.row(code);
                built.function = Some(std::sync::Arc::clone(self.code.name(row.qualname)));
                let path = std::sync::Arc::clone(self.code.name(row.file));
                match line {
                    Ok(line) => {
                        built.place = Some(RecordedPlace {
                            path,
                            line,
                            column: 0,
                        });
                    }
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
        let row = self.code.row(id);
        let length = row.units.saturating_mul(super::linetable::CODE_UNIT);
        let offset = instruction.wrapping_sub(bytecode.get()).cast_signed();
        let outside = || -> Problem {
            format!(
                "the frame at {:#x} is at {instruction:#x}, outside its code's bytecode",
                frame.address
            )
            .into()
        };

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
                format!("the line table of the code at {code:#x} is malformed").into()
            })?
        };

        line.and_then(|line| u32::try_from(line).ok())
            .filter(|line| *line != 0)
            .ok_or_else(|| format!("the frame at {:#x} is at no line", frame.address).into())
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
