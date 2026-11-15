use std::collections::HashSet;
use std::sync::Arc;

use super::code::{CodeId, CodeTable};
use super::objects::{Problem, Reader, Record, stack_reference};
use super::threads::ThreadState;
use crate::image::python::{Constant, Field};
use crate::runtime_model::{InterpretedFrame, SampledFrame};
use crate::{RecordedPlace, SourceLanguage, VirtualAddress};

pub(super) const MAX_FRAMES: usize = 1 << 13;

const EXECUTED: &str = "<string>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Code,

    Entry,
}

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

    pub(super) fn thread(
        &mut self,
        state: &ThreadState,
    ) -> (Vec<InterpretedFrame>, Option<Problem>) {
        let mut frames: Vec<InterpretedFrame> = Vec::new();
        let mut seen = HashSet::new();
        let mut next = state.frame;
        let end = loop {
            let Some(address) = next else {
                break (state.base.is_some() && !seen.is_empty())
                    .then(|| "the thread's frames end before its base frame".into());
            };

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
                Ok(Owner::Code) => frames.extend(self.frame(&frame)),
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

        let end = match frames.last_mut() {
            Some(outermost) if !outermost.outermost => {
                outermost.outermost = true;
                end.or_else(|| Some("the outermost frames have no entry frame beneath them".into()))
            }
            _ => end,
        };

        debug_assert!(
            frames.len() <= seen.len(),
            "native thread {} shows {} frames of {} walked",
            state.native,
            frames.len(),
            seen.len(),
        );
        (frames, end)
    }

    pub(super) fn frame(&mut self, frame: &Record<'_>) -> Option<InterpretedFrame> {
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

        match self.code_of(frame) {
            Ok((code, id)) => {
                if self.code.row(id).internal {
                    return None;
                }
                let line = self.line(frame, code, id);
                let function = frame.word(Field::FrameFunction).ok();
                built.function = Some(self.function_name(id, code, function));
                let row = self.code.row(id);
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
                    Err(problem) => built.problem = Some(problem),
                }
            }
            Err(problem) => built.problem = Some(problem),
        }

        Some(built)
    }

    fn function_name(
        &mut self,
        id: CodeId,
        code: VirtualAddress,
        function: Option<u64>,
    ) -> Arc<str> {
        let row = self.code.row(id);
        let own = Arc::clone(self.code.name(row.qualname));
        if **self.code.name(row.file) != *EXECUTED {
            return own;
        }

        let Some(function) = function.and_then(|bits| self.reference(bits)) else {
            return own;
        };
        let named = self.reader.head(function).and_then(|head| {
            Ok((
                head.field_word(Field::FunctionCode)?,
                head.field_word(Field::FunctionQualname)?,
            ))
        });
        match named {
            Ok((runs, qualname)) if runs == code.get() && qualname != 0 => self
                .code
                .function_name(&self.reader, id, function, VirtualAddress::new(qualname))
                .unwrap_or(own),
            _ => own,
        }
    }

    fn reference(&self, bits: u64) -> Option<VirtualAddress> {
        if self.reader.layout.constant(Constant::StackRefs) == 0 {
            Some(bits)
                .filter(|address| *address != 0)
                .map(VirtualAddress::new)
        } else {
            stack_reference(bits)
        }
    }

    pub(super) fn code_of(
        &mut self,
        frame: &Record<'_>,
    ) -> Result<(VirtualAddress, CodeId), Problem> {
        let executable = frame.word(Field::FrameExecutable)?;
        let code = self.reference(executable).ok_or_else(|| -> Problem {
            format!("the frame at {:#x} has no code", frame.address).into()
        })?;

        Ok((code, self.code.code(&self.reader, self.code_type, code)?))
    }

    fn line(&self, frame: &Record<'_>, code: VirtualAddress, id: CodeId) -> Result<u32, Problem> {
        let instruction = frame.word(Field::FrameInstruction)?;
        let bytecode = self.bytecode(frame, code)?;
        let offset = instruction.wrapping_sub(bytecode.get()).cast_signed();
        self.line_at(id, code, offset)
            .map_err(|problem| format!("the frame at {:#x} {problem}", frame.address).into())
    }

    fn line_at(&self, id: CodeId, code: VirtualAddress, offset: i64) -> Result<u32, Problem> {
        let row = self.code.row(id);
        let length = row.units.saturating_mul(super::linetable::CODE_UNIT);
        let outside =
            || -> Problem { format!("is at byte {offset}, outside its code's bytecode").into() };

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

    pub(super) fn sampled(
        &mut self,
        sampled: &[SampledFrame],
    ) -> (Vec<InterpretedFrame>, Option<Problem>) {
        let mut frames: Vec<InterpretedFrame> = Vec::new();
        let mut end = None;

        for frame in sampled {
            match self.owner_of(u64::from(frame.owner)) {
                Some(Owner::Code) => frames.extend(self.sampled_frame(frame)),
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

        debug_assert!(
            frames.len() <= sampled.len(),
            "{} frames shown of {} sampled",
            frames.len(),
            sampled.len(),
        );
        (frames, end)
    }

    fn sampled_frame(&mut self, frame: &SampledFrame) -> Option<InterpretedFrame> {
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
                return Some(built);
            }
        };

        if self.code.row(id).internal {
            return None;
        }
        built.function = Some(self.function_name(id, frame.code, frame.function));
        let row = self.code.row(id);
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
            Err(problem) => {
                built.problem =
                    Some(format!("the frame running {:#x} {problem}", frame.code).into());
            }
        }

        Some(built)
    }

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
