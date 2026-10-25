//! Which JavaScript functions an optimized frame runs at its call, and at
//! which bytecode, as V8's `OptimizedJSFrame::Summarize` reads them.
//!
//! Optimized (Maglev and Turbofan) code keeps, for each call that may
//! deoptimize, how to rebuild the interpreter's frames it stands for: its
//! safepoint table maps the call's return address to a deoptimization
//! index, the index to a translation in the code's deoptimization data,
//! and the translation names each frame, outermost first, with the
//! function's `SharedFunctionInfo` and the offset of the bytecode it was
//! at. This holds where the code's own position table does not, which a
//! build may leave empty (Maglev in 13.6 collects positions only when a
//! profiler asks).

use super::objects::{Object, Problem, Reader};
use super::positions;
use crate::image::v8::{Constant, Field, TranslationOpcode, Translations, V8Layout};

/// The most bytes of a safepoint table read.
const MAX_SAFEPOINT_TABLE: u64 = 1 << 20;

/// The most frames one translation stands for.
const MAX_FRAMES: i32 = 1 << 10;

/// Where a JavaScript frame of a translation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Offset {
    /// At the bytecode at this offset in its function's bytecode, or at
    /// its entry, `kFunctionEntryBytecodeOffset`.
    Bytecode(i64),

    /// In the continuation of the JavaScript builtin with this id, which
    /// V8 summarizes as the builtin's code at offset 0: a function whose
    /// script is none.
    Builtin(i64),
}

/// A JavaScript frame an optimized frame stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct JsFrame {
    /// Its function's `SharedFunctionInfo`.
    pub(super) shared: Object,
    pub(super) offset: Offset,

    /// Whether V8 calls it as a constructor: it follows a construct
    /// stub's frame, which the translation keeps and V8's summary skips.
    pub(super) constructor: bool,
}

/// The JavaScript frames, innermost first, that the optimized `code` stands
/// for at its instruction `pc_offset` bytes from its start, the frame's pc
/// (a caller's is its return address, past its call), as V8's
/// `OptimizedJSFrame::Summarize` gives them. `shared` is the frame's
/// function's: Maglev code keeps no deoptimization index at its entry,
/// which V8 summarizes as that function at its entry.
///
/// An error for code that is neither Maglev's nor Turbofan's (V8
/// summarizes a builtin's frame as an interpreted one), for a translation
/// that names a WebAssembly function inlined into JavaScript, and for
/// anything unreadable or other than V8 writes.
pub(super) fn call_site(
    reader: &Reader<'_>,
    code: Object,
    shared: Object,
    pc_offset: u64,
) -> Result<Vec<JsFrame>, Problem> {
    let flags = i64::from(reader.int32_field(code, Field::CodeFlags)?.cast_unsigned());
    let kind = (flags & reader.constant(Constant::CodeKindFieldMask)?)
        >> reader.constant(Constant::CodeKindFieldShift)?;
    let maglev = kind == reader.constant(Constant::CodeKindMaglev)?;
    if !maglev && kind != reader.constant(Constant::CodeKindTurbofanJS)? {
        return Err(format!("code of kind {kind} keeps no deoptimization translation").into());
    }

    let format = SafepointFormat::of(reader.layout, maglev)?;
    let table = safepoint_table(reader, code)?;
    let pc_offset = i64::try_from(pc_offset).map_err(|_| "the pc offset is out of range")?;
    let Some(deopt_index) = deopt_index(&table, &format, pc_offset)? else {
        return Ok(vec![JsFrame {
            shared,
            offset: Offset::Bytecode(reader.constant(Constant::FunctionEntryBytecodeOffset)?),
            constructor: false,
        }]);
    };

    let data = reader
        .object(reader.field(code, Field::CodeDeoptimizationDataOrInterpreterData)?)?
        .ok_or("the optimized code keeps no deoptimization data")?;
    let slot = reader.constant(Constant::DeoptimizationDataFirstDeoptEntryIndex)?
        + i64::from(deopt_index) * reader.constant(Constant::DeoptimizationDataDeoptEntrySize)?
        + reader.constant(Constant::DeoptimizationDataTranslationIndexOffset)?;
    let index = reader
        .smi(reader.element(data, slot)?)?
        .ok_or("a deoptimization entry's translation index is no Smi")?;
    let translation = reader
        .object(reader.element(
            data,
            reader.constant(Constant::DeoptimizationDataFrameTranslationIndex)?,
        )?)?
        .ok_or("the deoptimization data keeps no translation")?;
    let bytes = positions::byte_array(reader, translation)?;
    let translations = Translations::of(reader.version)?;
    let first_builtin = reader.constant(Constant::FirstBuiltinContinuationId)?;
    let frames = summarize(&bytes, index, translations, first_builtin)?;

    let literals = reader
        .object(reader.element(
            data,
            reader.constant(Constant::DeoptimizationDataLiteralArrayIndex)?,
        )?)?
        .ok_or("the deoptimization data keeps no literals")?;
    frames
        .into_iter()
        .rev()
        .map(|frame| {
            Ok(JsFrame {
                shared: literal_function(reader, literals, frame.literal)?,
                offset: frame.offset,
                constructor: frame.constructor,
            })
        })
        .collect()
}

/// The `SharedFunctionInfo` the literal `index` of `literals` holds,
/// strongly or weakly.
fn literal_function(reader: &Reader<'_>, literals: Object, index: i64) -> Result<Object, Problem> {
    let word = reader.element(literals, index)?;
    let weak_tag = reader
        .constant(Constant::WeakHeapObjectTag)?
        .cast_unsigned();
    let tag_mask = reader
        .constant(Constant::HeapObjectTagMask)?
        .cast_unsigned();
    let word = if word & tag_mask == weak_tag {
        word & !reader
            .constant(Constant::WeakHeapObjectMask)?
            .cast_unsigned()
    } else {
        word
    };
    reader
        .object(word)?
        .filter(|object| {
            reader
                .is(*object, Constant::SharedFunctionInfoType)
                .unwrap_or(false)
        })
        .ok_or_else(|| format!("the translation's literal {index} is no function").into())
}

/// The bytes of `code`'s safepoint table: its metadata's first, which
/// follows its instructions, up to its handler table.
fn safepoint_table(reader: &Reader<'_>, code: Object) -> Result<Vec<u8>, Problem> {
    let start = reader.field(code, Field::CodeInstructionStart)?;
    let size = u64::from(
        reader
            .int32_field(code, Field::CodeInstructionSize)?
            .cast_unsigned(),
    );
    let length = u64::try_from(reader.int32_field(code, Field::CodeHandlerTableOffset)?)
        .map_err(|_| "the code's handler table offset is negative")?;
    if length > MAX_SAFEPOINT_TABLE {
        return Err("the code's safepoint table is implausibly long".into());
    }
    let mut bytes = vec![0; usize::try_from(length).map_err(|_| "a length is out of range")?];
    reader.bytes_into(start.wrapping_add(size), &mut bytes)?;
    Ok(bytes)
}

/// How one kind of code lays out its safepoint table.
#[derive(Debug, Clone, Copy)]
struct SafepointFormat {
    length_offset: usize,
    configuration_offset: usize,
    header_size: usize,

    /// Each field of the entry configuration, as `(shift, mask)`.
    has_deopt_data: (u32, u32),
    register_indexes_size: (u32, u32),
    pc_size: (u32, u32),
    deopt_index_size: (u32, u32),

    /// The bytes between an entry's deoptimization data and its
    /// registers: Maglev's count of extra spill slots.
    between: usize,

    /// Whether the table is Maglev's, which finds a safepoint only at its
    /// exact pc.
    maglev: bool,
}

impl SafepointFormat {
    /// Maglev code's format, or Turbofan's.
    fn of(layout: &V8Layout, maglev: bool) -> Result<Self, Problem> {
        use Constant as C;

        let constant = |wanted: Constant| {
            layout
                .constant(wanted)
                .ok_or_else(|| -> Problem { format!("the layout has no {}", wanted.name()).into() })
        };
        let unsigned = |wanted: Constant| -> Result<usize, Problem> {
            usize::try_from(constant(wanted)?)
                .map_err(|_| format!("{} is out of range", wanted.name()).into())
        };
        let field = |shift: Constant, mask: Constant| -> Result<(u32, u32), Problem> {
            let shift_value =
                u32::try_from(constant(shift)?).map_err(|_| "a shift is out of range")?;
            let mask_value =
                u32::try_from(constant(mask)?).map_err(|_| "a mask is out of range")?;
            Ok((shift_value, mask_value))
        };
        Ok(if maglev {
            Self {
                length_offset: unsigned(C::MaglevSafepointTableLengthOffset)?,
                configuration_offset: unsigned(C::MaglevSafepointTableEntryConfigurationOffset)?,
                header_size: unsigned(C::MaglevSafepointTableHeaderSize)?,
                has_deopt_data: field(
                    C::MaglevSafepointHasDeoptDataShift,
                    C::MaglevSafepointHasDeoptDataMask,
                )?,
                register_indexes_size: field(
                    C::MaglevSafepointRegisterIndexesSizeShift,
                    C::MaglevSafepointRegisterIndexesSizeMask,
                )?,
                pc_size: field(C::MaglevSafepointPcSizeShift, C::MaglevSafepointPcSizeMask)?,
                deopt_index_size: field(
                    C::MaglevSafepointDeoptIndexSizeShift,
                    C::MaglevSafepointDeoptIndexSizeMask,
                )?,
                between: unsigned(C::MaglevSafepointTableNumPushedRegistersSize)?,
                maglev: true,
            }
        } else {
            Self {
                length_offset: unsigned(C::SafepointTableLengthOffset)?,
                configuration_offset: unsigned(C::SafepointTableEntryConfigurationOffset)?,
                header_size: unsigned(C::SafepointTableHeaderSize)?,
                has_deopt_data: field(C::SafepointHasDeoptDataShift, C::SafepointHasDeoptDataMask)?,
                register_indexes_size: field(
                    C::SafepointRegisterIndexesSizeShift,
                    C::SafepointRegisterIndexesSizeMask,
                )?,
                pc_size: field(C::SafepointPcSizeShift, C::SafepointPcSizeMask)?,
                deopt_index_size: field(
                    C::SafepointDeoptIndexSizeShift,
                    C::SafepointDeoptIndexSizeMask,
                )?,
                between: 0,
                maglev: false,
            }
        })
    }
}

/// One entry of a safepoint table: its pc's offset, and with
/// deoptimization data its index and its trampoline's pc, each `None` for
/// none.
#[derive(Debug, Clone, Copy)]
struct Safepoint {
    pc: i64,
    deopt_index: Option<u32>,
    trampoline: Option<i64>,
}

/// The little-endian integer of `size` bytes at `at` of `bytes`.
fn read_le(bytes: &[u8], at: usize, size: usize) -> Result<u32, Problem> {
    let field = at
        .checked_add(size)
        .and_then(|end| bytes.get(at..end))
        .filter(|field| field.len() <= 4)
        .ok_or("a safepoint table ends inside an entry")?;
    Ok(field
        .iter()
        .rev()
        .fold(0, |value, byte| (value << 8) | u32::from(*byte)))
}

/// The entries of the safepoint table `table`, and whether they keep
/// deoptimization data.
fn safepoints(table: &[u8], format: &SafepointFormat) -> Result<(Vec<Safepoint>, bool), Problem> {
    let length = read_le(table, format.length_offset, 4)?.cast_signed();
    let configuration = read_le(table, format.configuration_offset, 4)?;
    let decode = |(shift, mask): (u32, u32)| -> usize {
        usize::try_from((configuration & mask) >> shift).unwrap_or(usize::MAX)
    };
    let has_deopt_data = decode(format.has_deopt_data) != 0;
    let pc_size = decode(format.pc_size);
    let deopt_index_size = decode(format.deopt_index_size);
    let registers = decode(format.register_indexes_size);
    if pc_size > 4 || deopt_index_size > 4 || registers > 4 {
        return Err("a safepoint table's field width exceeds four bytes".into());
    }
    let deopt_size = if has_deopt_data {
        pc_size + deopt_index_size
    } else {
        0
    };
    let entry_size = pc_size + deopt_size + format.between + registers;
    let length = usize::try_from(length).map_err(|_| "a safepoint table's length is negative")?;
    if length > 0 && entry_size == 0 {
        return Err("a nonempty safepoint table has zero-width entries".into());
    }
    if length as u64 > MAX_SAFEPOINT_TABLE {
        return Err("a safepoint table exceeds the entry count limit".into());
    }
    if length
        .checked_mul(entry_size)
        .and_then(|entries| entries.checked_add(format.header_size))
        .is_none_or(|end| end > table.len())
    {
        return Err("a safepoint table's entries overrun it".into());
    }

    let mut entries = Vec::with_capacity(length);
    for index in 0..length {
        let at = format.header_size + index * entry_size;
        let pc = i64::from(read_le(table, at, pc_size)?);
        let (deopt_index, trampoline) = if has_deopt_data {
            // Each is stored plus one, so that 0 is none.
            let deopt_index = read_le(table, at + pc_size, deopt_index_size)?.checked_sub(1);
            let trampoline =
                read_le(table, at + pc_size + deopt_index_size, pc_size)?.checked_sub(1);
            (deopt_index, trampoline.map(i64::from))
        } else {
            (None, None)
        };
        entries.push(Safepoint {
            pc,
            deopt_index,
            trampoline,
        });
    }
    Ok((entries, has_deopt_data))
}

/// The deoptimization index of the safepoint at `pc_offset` in `table`, as
/// V8's `SafepointTable::FindEntry` or `MaglevSafepointTable::FindEntry`
/// finds it, or `None` for a Maglev safepoint that keeps none, as at its
/// function's entry. An error where V8 would find no safepoint, or a
/// Turbofan one without an index.
fn deopt_index(
    table: &[u8],
    format: &SafepointFormat,
    pc_offset: i64,
) -> Result<Option<u32>, Problem> {
    let (entries, has_deopt_data) = safepoints(table, format)?;
    if format.maglev {
        return Ok(
            maglev_entry(&entries, has_deopt_data, pc_offset).and_then(|entry| entry.deopt_index)
        );
    }
    let entry = turbofan_entry(&entries, has_deopt_data, pc_offset)
        .ok_or("the Turbofan code has no safepoint at the frame's pc")?;
    entry
        .deopt_index
        .map(Some)
        .ok_or_else(|| "the Turbofan code keeps no deoptimization index at the frame's pc".into())
}

/// `SafepointTable::TryFindEntry`: the last entry whose trampoline is at or
/// before `pc_offset`, or else the last whose pc is.
fn turbofan_entry(
    entries: &[Safepoint],
    has_deopt_data: bool,
    pc_offset: i64,
) -> Option<Safepoint> {
    if has_deopt_data {
        let mut candidate = None;
        for entry in entries {
            match entry.trampoline {
                Some(trampoline) if trampoline <= pc_offset => candidate = Some(*entry),
                Some(trampoline) if trampoline > pc_offset => break,
                _ => {}
            }
        }
        if candidate.is_some() {
            return candidate;
        }
    }
    // The first entry that is the last or whose successor is past the pc,
    // unless it is past the pc itself.
    let index = (0..entries.len()).find(|&index| {
        entries
            .get(index + 1)
            .is_none_or(|next| next.pc > pc_offset)
    })?;
    let entry = *entries.get(index)?;
    (entry.pc <= pc_offset).then_some(entry)
}

/// `MaglevSafepointTable::FindEntry`: the entry whose trampoline or pc is
/// exactly `pc_offset`, or `None` for the default entry, which keeps no
/// deoptimization data.
fn maglev_entry(entries: &[Safepoint], has_deopt_data: bool, pc_offset: i64) -> Option<Safepoint> {
    if has_deopt_data {
        for entry in entries {
            match entry.trampoline {
                Some(trampoline) if trampoline == pc_offset => return Some(*entry),
                Some(trampoline) if trampoline > pc_offset => break,
                _ => {}
            }
        }
    }
    entries.iter().find(|entry| entry.pc == pc_offset).copied()
}

/// A JavaScript frame as a translation names it: its function by its
/// index in the deoptimization data's literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranslatedFrame {
    literal: i64,
    offset: Offset,
    constructor: bool,
}

/// The JavaScript frames, outermost first, of the translation at `index` of
/// `bytes`, as V8's `Summarize` walks it: each frame in turn, a
/// construct stub's marking the next JavaScript frame as a constructor's.
fn summarize(
    bytes: &[u8],
    index: i64,
    opcodes: &Translations,
    first_builtin: i64,
) -> Result<Vec<TranslatedFrame>, Problem> {
    let mut translation = Translation::new(bytes, index, opcodes)?;
    let (frame_count, js_frame_count) = translation.enter_begin()?;
    if !(0..=MAX_FRAMES).contains(&frame_count) || !(0..=frame_count).contains(&js_frame_count) {
        return Err(format!(
            "a translation stands for {frame_count} frames, {js_frame_count} of them JavaScript's"
        )
        .into());
    }

    let mut frames = Vec::new();
    let mut constructor = false;
    for _ in 0..frame_count {
        let number = translation.seek_next_frame()?;
        let (opcode, operands) = opcodes
            .opcode(number)
            .ok_or("a translation opcode is out of range")?;
        let builtin = match opcode {
            TranslationOpcode::ConstructCreateStubFrame
            | TranslationOpcode::ConstructInvokeStubFrame => {
                constructor = true;
                translation.skip_operands(operands)?;
                continue;
            }
            TranslationOpcode::WasmInlinedIntoJsFrame => {
                return Err(
                    "the frame inlined a WebAssembly function, which microscope \
                            cannot name"
                        .into(),
                );
            }
            _ if !opcodes.is_js_frame(number) => {
                translation.skip_operands(operands)?;
                continue;
            }
            TranslationOpcode::InterpretedFrameWithReturn
            | TranslationOpcode::InterpretedFrameWithoutReturn => false,
            TranslationOpcode::JavaScriptBuiltinContinuationFrame
            | TranslationOpcode::JavaScriptBuiltinContinuationWithCatchFrame => true,
            other => return Err(format!("{} is no JavaScript frame", other.name()).into()),
        };
        let offset = i64::from(translation.next_operand()?);
        let literal = i64::from(translation.next_operand()?);
        translation.skip_operands(operands.saturating_sub(2))?;
        frames.push(TranslatedFrame {
            literal,
            offset: if builtin {
                Offset::Builtin(offset - first_builtin)
            } else {
                Offset::Bytecode(offset)
            },
            constructor,
        });
        constructor = false;
    }

    if i32::try_from(frames.len()).ok() != Some(js_frame_count) {
        return Err(format!(
            "a translation names {} JavaScript frames, not the {js_frame_count} it says",
            frames.len()
        )
        .into());
    }
    Ok(frames)
}

/// The VLQ-encoded unsigned integer at `at` of `bytes`, as V8's
/// `base::VLQDecodeUnsigned` reads it: 7 bits a byte, least significant
/// first, each but the last with its top bit set.
fn vlq_unsigned(bytes: &[u8], at: &mut usize) -> Result<u32, Problem> {
    let mut value: u32 = 0;
    for shift in (0..=28).step_by(7) {
        let byte = *bytes
            .get(*at)
            .ok_or("a translation ends inside an operand")?;
        *at += 1;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("a translation's operand is too long".into())
}

/// The VLQ-encoded signed integer at `at` of `bytes`, its sign in its
/// lowest bit, as `base::VLQDecode` reads it.
fn vlq_signed(bytes: &[u8], at: &mut usize) -> Result<i32, Problem> {
    let bits = vlq_unsigned(bytes, at)?;
    let magnitude = (bits >> 1).cast_signed();
    Ok(if bits & 1 == 1 { -magnitude } else { magnitude })
}

/// A walk of a translation, as V8's `DeoptTranslationIterator`: an
/// opcode byte of at least the opcodes' count, or an explicit
/// `MATCH_PREVIOUS_TRANSLATION`, repeats opcodes and their operands from
/// the translation a `BEGIN`'s lookback distance names, past the ones
/// already matched.
struct Translation<'a> {
    bytes: &'a [u8],
    opcodes: &'a Translations,
    index: usize,
    previous: usize,
    from_previous: u32,
    since_previous: u32,
}

impl<'a> Translation<'a> {
    /// A walk from the translation at `index`, which begins with a `BEGIN`.
    /// A compressed translation, as `--turbo-compress-frame-translations`
    /// writes, does not: it is refused here.
    fn new(bytes: &'a [u8], index: i64, opcodes: &'a Translations) -> Result<Self, Problem> {
        let index = usize::try_from(index).map_err(|_| "a translation index is negative")?;
        let opcode = bytes
            .get(index)
            .and_then(|&byte| opcodes.opcode(usize::from(byte)))
            .map(|(opcode, _)| opcode);
        if !matches!(
            opcode,
            Some(TranslationOpcode::BeginWithFeedback | TranslationOpcode::BeginWithoutFeedback)
        ) {
            return Err(format!(
                "the translation at {index} does not begin with BEGIN; V8 may compress its \
                 translations (--turbo-compress-frame-translations), which microscope does \
                 not read"
            )
            .into());
        }
        Ok(Self {
            bytes,
            opcodes,
            index,
            previous: 0,
            from_previous: 0,
            since_previous: 0,
        })
    }

    /// The next operand, signed, from the previous translation while
    /// opcodes are repeated from it.
    fn next_operand(&mut self) -> Result<i32, Problem> {
        if self.from_previous == 0 {
            return vlq_signed(self.bytes, &mut self.index);
        }
        let value = vlq_signed(self.bytes, &mut self.previous)?;
        self.check_previous()?;
        Ok(value)
    }

    fn next_operand_unsigned(&mut self) -> Result<u32, Problem> {
        if self.from_previous == 0 {
            return vlq_unsigned(self.bytes, &mut self.index);
        }
        let value = vlq_unsigned(self.bytes, &mut self.previous)?;
        self.check_previous()?;
        Ok(value)
    }

    fn skip_operands(&mut self, count: u8) -> Result<(), Problem> {
        for _ in 0..count {
            self.next_operand()?;
        }
        Ok(())
    }

    /// The previous translation is before this one, as V8 writes it.
    fn check_previous(&self) -> Result<(), Problem> {
        if self.previous < self.index {
            Ok(())
        } else {
            Err("a translation repeats opcodes from past itself".into())
        }
    }

    /// The opcode at the previous translation's place, which is no
    /// repetition.
    fn opcode_at_previous(&mut self) -> Result<usize, Problem> {
        let number = usize::from(
            *self
                .bytes
                .get(self.previous)
                .ok_or("a translation repeats opcodes past its end")?,
        );
        self.previous += 1;
        self.check_previous()?;
        match self.opcodes.opcode(number) {
            Some((TranslationOpcode::MatchPreviousTranslation, _)) | None => {
                Err("a translation repeats an opcode that is no opcode".into())
            }
            Some(_) => Ok(number),
        }
    }

    fn skip_at_previous(&mut self) -> Result<(), Problem> {
        let number = self.opcode_at_previous()?;
        let (_, operands) = self
            .opcodes
            .opcode(number)
            .ok_or("a translation opcode is out of range")?;
        for _ in 0..operands {
            vlq_unsigned(self.bytes, &mut self.previous)?;
            self.check_previous()?;
        }
        Ok(())
    }

    const fn has_next_opcode(&self) -> bool {
        self.index < self.bytes.len() || self.from_previous > 1
    }

    /// The number of the next opcode, as `NextOpcode`.
    fn next_opcode(&mut self) -> Result<usize, Problem> {
        self.from_previous = self.from_previous.saturating_sub(1);
        if self.from_previous > 0 {
            return self.opcode_at_previous();
        }
        let byte = *self
            .bytes
            .get(self.index)
            .ok_or("a translation ends before its frames")?;
        self.index += 1;

        let matching = self
            .opcodes
            .number(TranslationOpcode::MatchPreviousTranslation)
            .ok_or("the version has no MATCH_PREVIOUS_TRANSLATION")?;
        let mut number = usize::from(byte);
        if number >= self.opcodes.count() {
            self.from_previous = u32::try_from(number - self.opcodes.count())
                .map_err(|_| "a repetition is out of range")?;
            number = matching;
        } else if number == matching {
            self.from_previous = self.next_operand_unsigned()?;
        }

        let (opcode, _) = self
            .opcodes
            .opcode(number)
            .ok_or("a translation opcode is out of range")?;
        match opcode {
            TranslationOpcode::BeginWithFeedback | TranslationOpcode::BeginWithoutFeedback => {
                let mut at = self.index;
                let lookback = usize::try_from(vlq_unsigned(self.bytes, &mut at)?)
                    .map_err(|_| "a lookback is out of range")?;
                if lookback != 0 {
                    self.previous = self
                        .index
                        .checked_sub(1 + lookback)
                        .ok_or("a translation looks back before the first")?;
                }
                self.since_previous = 1;
                Ok(number)
            }
            TranslationOpcode::MatchPreviousTranslation => {
                for _ in 0..self.since_previous {
                    self.skip_at_previous()?;
                }
                self.since_previous = 0;
                self.opcode_at_previous()
            }
            _ => {
                self.since_previous += 1;
                Ok(number)
            }
        }
    }

    /// The counts of frames and of JavaScript frames the translation's
    /// `BEGIN` gives, past its lookback distance.
    fn enter_begin(&mut self) -> Result<(i32, i32), Problem> {
        self.next_opcode()?;
        self.next_operand()?;
        let frames = self.next_operand()?;
        let js_frames = self.next_operand()?;
        Ok((frames, js_frames))
    }

    /// The number of the next opcode that begins a frame, past the values
    /// of the frame before it.
    fn seek_next_frame(&mut self) -> Result<usize, Problem> {
        while self.has_next_opcode() {
            let number = self.next_opcode()?;
            let (opcode, operands) = self
                .opcodes
                .opcode(number)
                .ok_or("a translation opcode is out of range")?;
            if matches!(
                opcode,
                TranslationOpcode::BeginWithFeedback | TranslationOpcode::BeginWithoutFeedback
            ) {
                return Err("a translation ends before its frames".into());
            }
            if self.opcodes.is_frame(number) {
                return Ok(number);
            }
            self.skip_operands(operands)?;
        }
        Err("a translation ends before its frames".into())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Offset, SafepointFormat, TranslatedFrame, deopt_index, safepoints, summarize, vlq_unsigned,
    };
    use crate::image::v8::{TranslationOpcode as T, Translations, V8Layout, V8Version};

    /// Each V8 version microscope reads.
    const VERSIONS: [(u32, u32); 3] = [(12, 4), (13, 6), (14, 6)];

    fn layout((major, minor): (u32, u32)) -> V8Layout {
        crate::debug_info::v8::generated_layout(major, minor).expect("a generated layout")
    }

    fn opcodes((major, minor): (u32, u32)) -> &'static Translations {
        Translations::of(V8Version {
            major,
            minor,
            ..V8Version::default()
        })
        .expect("the version's opcodes")
    }

    /// `value` as V8's `VLQEncodeUnsigned` writes it.
    fn unsigned(bytes: &mut Vec<u8>, mut value: u32) {
        loop {
            let low = u8::try_from(value & 0x7f).expect("seven bits");
            value >>= 7;
            if value == 0 {
                bytes.push(low);
                return;
            }
            bytes.push(low | 0x80);
        }
    }

    /// `value` as `VLQEncode` writes it, its sign in its lowest bit.
    fn signed(bytes: &mut Vec<u8>, value: i32) {
        unsigned(bytes, (value.unsigned_abs() << 1) | u32::from(value < 0));
    }

    /// Appends `opcode` and its operands, as many as the version gives it.
    fn op(bytes: &mut Vec<u8>, opcodes: &Translations, opcode: T, operands: &[i32]) {
        let number = opcodes.number(opcode).expect("an opcode of the version");
        let (_, count) = opcodes.opcode(number).expect("an opcode");
        assert_eq!(operands.len(), usize::from(count), "{opcode:?}'s operands");
        bytes.push(u8::try_from(number).expect("an opcode byte"));
        for &operand in operands {
            signed(bytes, operand);
        }
    }

    /// An interpreted frame's operands for the version: its bytecode
    /// offset and function, then the rest, which are not read.
    fn interpreted(
        opcodes: &Translations,
        with_return: bool,
        offset: i32,
        literal: i32,
    ) -> (T, Vec<i32>) {
        let opcode = if with_return {
            T::InterpretedFrameWithReturn
        } else {
            T::InterpretedFrameWithoutReturn
        };
        let (_, count) = opcodes
            .opcode(opcodes.number(opcode).expect("interpreted frames"))
            .expect("an opcode");
        let mut operands = vec![offset, literal];
        operands.resize(usize::from(count), 7);
        (opcode, operands)
    }

    #[test]
    fn vlq_operands_decode_as_v8_encodes_them() {
        for value in [0, 1, 0x7f, 0x80, 0x3fff, 0x4000, 0x0fff_ffff, u32::MAX] {
            let mut bytes = Vec::new();
            unsigned(&mut bytes, value);
            let mut at = 0;
            assert_eq!(vlq_unsigned(&bytes, &mut at).unwrap(), value);
            assert_eq!(at, bytes.len());
        }
        let mut at = 0;
        assert!(vlq_unsigned(&[0x80], &mut at).is_err());
    }

    /// A safepoint table of `entries`, each `(pc, deoptimization index,
    /// trampoline)`, as V8's builders emit one: 1-byte pcs and indices, no
    /// registers, deoptimization data when any entry has an index.
    fn safepoint_table(
        format: &SafepointFormat,
        entries: &[(u8, Option<u8>, Option<u8>)],
    ) -> Vec<u8> {
        let has_deopt = entries.iter().any(|(_, index, _)| index.is_some());
        let field = |(shift, _): (u32, u32), value: u32| value << shift;
        let configuration = field(format.has_deopt_data, u32::from(has_deopt))
            | field(format.pc_size, 1)
            | field(format.deopt_index_size, u32::from(has_deopt));
        let mut table = vec![0xee; format.header_size];
        let length = u32::try_from(entries.len()).unwrap();
        table[format.length_offset..format.length_offset + 4]
            .copy_from_slice(&length.to_le_bytes());
        table[format.configuration_offset..format.configuration_offset + 4]
            .copy_from_slice(&configuration.to_le_bytes());
        for &(pc, index, trampoline) in entries {
            table.push(pc);
            if has_deopt {
                table.push(index.map_or(0, |index| index + 1));
                table.push(trampoline.map_or(0, |trampoline| trampoline + 1));
            }
            table.extend(std::iter::repeat_n(0x55, format.between));
        }
        // The tagged slots' bits follow the entries.
        table.extend([0xff; 4]);
        table
    }

    /// A Turbofan call finds the last safepoint at or before its pc, or
    /// its trampoline's; a Maglev one only its own, or none, as at its
    /// function's entry.
    #[test]
    fn a_call_finds_its_deoptimization_index_as_v8_does() {
        for version in VERSIONS {
            let layout = layout(version);
            let turbofan = SafepointFormat::of(&layout, false).unwrap();
            let entries = [
                (10, Some(0), None),
                (20, None, None),
                (30, Some(2), Some(40)),
            ];
            let table = safepoint_table(&turbofan, &entries);
            let at = |pc| deopt_index(&table, &turbofan, pc);
            assert!(at(5).is_err(), "{version:?}: no safepoint before the first");
            assert_eq!(at(10).unwrap(), Some(0), "{version:?}");
            assert_eq!(at(15).unwrap(), Some(0), "{version:?}");
            assert!(at(20).is_err(), "{version:?}: a safepoint without an index");
            assert_eq!(at(30).unwrap(), Some(2), "{version:?}");
            assert_eq!(at(40).unwrap(), Some(2), "{version:?}: its trampoline");
            assert_eq!(at(45).unwrap(), Some(2), "{version:?}: past its trampoline");

            let maglev = SafepointFormat::of(&layout, true).unwrap();
            let table = safepoint_table(&maglev, &entries);
            let at = |pc| deopt_index(&table, &maglev, pc).unwrap();
            assert_eq!(at(5), None, "{version:?}: the default entry");
            assert_eq!(at(10), Some(0), "{version:?}");
            assert_eq!(at(15), None, "{version:?}: only an exact pc");
            assert_eq!(at(20), None, "{version:?}");
            assert_eq!(at(30), Some(2), "{version:?}");
            assert_eq!(at(40), Some(2), "{version:?}: its trampoline");

            let short = &table[..table.len() - 6];
            assert!(deopt_index(short, &maglev, 10).is_err(), "{version:?}");
        }
    }

    /// The headers differ: 12.4's keep no stack slots' count.
    #[test]
    fn safepoint_headers_are_each_versions_own() {
        let headers: Vec<_> = VERSIONS
            .iter()
            .map(|&version| {
                let layout = layout(version);
                let turbofan = SafepointFormat::of(&layout, false).unwrap();
                let maglev = SafepointFormat::of(&layout, true).unwrap();
                (
                    (turbofan.length_offset, turbofan.header_size),
                    (maglev.length_offset, maglev.header_size, maglev.between),
                )
            })
            .collect();
        assert_eq!(
            headers,
            [
                ((0, 8), (0, 16, 1)),
                ((4, 12), (4, 16, 1)),
                ((4, 12), (4, 16, 1)),
            ]
        );
    }

    #[test]
    fn safepoints_refuse_nonempty_tables_with_zero_width_entries() {
        for version in VERSIONS {
            let format = SafepointFormat::of(&layout(version), false).unwrap();
            let mut table = vec![0; format.header_size];
            table[format.length_offset..format.length_offset + 4]
                .copy_from_slice(&2_i32.to_le_bytes());
            assert!(safepoints(&table, &format).is_err(), "{version:?}");
        }
    }

    /// Two translations, the second repeating the first's frame from it:
    /// a caller at bytecode 5 that called a constructor, inlined, at 12,
    /// and then a builtin's continuation.
    fn translations(opcodes: &Translations) -> (Vec<u8>, usize) {
        let mut bytes = Vec::new();
        op(&mut bytes, opcodes, T::BeginWithoutFeedback, &[0, 4, 2]);
        let (opcode, operands) = interpreted(opcodes, false, 5, 0);
        op(&mut bytes, opcodes, opcode, &operands);
        op(&mut bytes, opcodes, T::Literal, &[3]);
        op(&mut bytes, opcodes, T::Register, &[1]);
        op(&mut bytes, opcodes, T::ArgumentsElements, &[0]);
        op(&mut bytes, opcodes, T::ConstructCreateStubFrame, &[1, 2]);
        op(&mut bytes, opcodes, T::TaggedStackSlot, &[-3]);
        op(&mut bytes, opcodes, T::BuiltinContinuationFrame, &[9, 1, 0]);
        let (opcode, operands) = interpreted(opcodes, true, 12, 1);
        op(&mut bytes, opcodes, opcode, &operands);
        op(&mut bytes, opcodes, T::OptimizedOut, &[]);

        let second = bytes.len();
        // The distance back from the opcode to the first's BEGIN, less one.
        let mut lookback = Vec::new();
        unsigned(&mut lookback, u32::try_from(second).unwrap());
        bytes.push(u8::try_from(opcodes.number(T::BeginWithFeedback).unwrap()).unwrap());
        bytes.extend(&lookback);
        signed(&mut bytes, 2);
        signed(&mut bytes, 2);
        // Implicitly, the first's interpreted frame and its literal.
        bytes.push(u8::try_from(opcodes.count() + 2).unwrap());
        op(
            &mut bytes,
            opcodes,
            T::JavaScriptBuiltinContinuationFrame,
            &[1 + 44, 2, 0],
        );
        op(&mut bytes, opcodes, T::Literal, &[4]);
        (bytes, second)
    }

    /// A translation names its JavaScript frames outermost first, each
    /// with its function's literal and its bytecode, a constructor's after
    /// a construct stub, and a builtin's continuation by its builtin;
    /// repeated opcodes are read from the translation they repeat.
    #[test]
    fn translations_name_their_javascript_frames() {
        for version in VERSIONS {
            let opcodes = opcodes(version);
            let (bytes, second) = translations(opcodes);
            let frame = |literal, offset, constructor| TranslatedFrame {
                literal,
                offset,
                constructor,
            };
            assert_eq!(
                summarize(&bytes, 0, opcodes, 1).unwrap(),
                [
                    frame(0, Offset::Bytecode(5), false),
                    frame(1, Offset::Bytecode(12), true),
                ],
                "{version:?}"
            );
            assert_eq!(
                summarize(&bytes, i64::try_from(second).unwrap(), opcodes, 1).unwrap(),
                [
                    frame(0, Offset::Bytecode(5), false),
                    frame(2, Offset::Builtin(44), false),
                ],
                "{version:?}"
            );
        }
    }

    /// What V8 would not have written is refused, never read past.
    #[test]
    fn translations_refuse_what_v8_does_not_write() {
        for version in VERSIONS {
            let opcodes = opcodes(version);
            let (bytes, second) = translations(opcodes);
            // Not at a BEGIN, as in a compressed translation.
            assert!(
                summarize(&bytes, 1, opcodes, 1)
                    .unwrap_err()
                    .contains("compress")
            );
            assert!(summarize(&bytes, -1, opcodes, 1).is_err());
            assert!(summarize(&bytes[..second - 3], 0, opcodes, 1).is_err());

            let mut wasm = Vec::new();
            op(&mut wasm, opcodes, T::BeginWithoutFeedback, &[0, 1, 0]);
            op(&mut wasm, opcodes, T::WasmInlinedIntoJsFrame, &[0, 0, 0]);
            assert!(
                summarize(&wasm, 0, opcodes, 1)
                    .unwrap_err()
                    .contains("WebAssembly")
            );

            let mut miscounted = Vec::new();
            op(
                &mut miscounted,
                opcodes,
                T::BeginWithoutFeedback,
                &[0, 1, 2],
            );
            let (opcode, operands) = interpreted(opcodes, false, 5, 0);
            op(&mut miscounted, opcodes, opcode, &operands);
            assert!(
                summarize(&miscounted, 0, opcodes, 1).is_err(),
                "{version:?}"
            );

            // A repetition from a translation that is not before it.
            let mut forward = Vec::new();
            op(&mut forward, opcodes, T::BeginWithoutFeedback, &[0, 1, 1]);
            forward.push(u8::try_from(opcodes.count() + 3).unwrap());
            assert!(summarize(&forward, 0, opcodes, 1).is_err(), "{version:?}");
        }
    }
}
