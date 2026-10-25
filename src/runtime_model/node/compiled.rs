//! Where in its script a frame of V8's compiled code is.
//!
//! A frame of compiled code names its function, not its code. The code it
//! runs is one of the function's: its current code, through the dispatch
//! table from 13.6 or the function itself before; its baseline (Sparkplug)
//! code; or optimized code it entered on a loop (OSR), which only its
//! feedback vector keeps, weakly. The code whose instructions hold the
//! frame's is the one it runs, and no other is ever used: a frame whose
//! code is none of these has no place, never a nearby one.
//!
//! Optimized (Maglev and Turbofan) code maps an instruction to a source
//! position, which may lie in a function its code inlined; the code's
//! deoptimization data names each, as V8's `InliningStack` reads them.
//! Baseline code maps an instruction to the bytecode it compiled, whose
//! own position table gives the place.

use super::Statics;
use super::objects::{Object, Problem, Reader};
use super::positions::{self, Encoding};
use super::translation::{self, Offset};
use crate::image::v8::{Constant, DispatchTable, Field};
use crate::runtime_model::CodeFrame;

/// The most feedback slots searched for a function's OSR code.
const MAX_FEEDBACK_SLOTS: u64 = 1 << 16;

/// The prefixes that widen the operands of the bytecode after them, by
/// their bytecode number, with the operand scale's index each selects:
/// `Wide`, `ExtraWide`, `DebugBreakWide`, and `DebugBreakExtraWide`, the
/// first four bytecodes on 12.4, 13.6, and 14.6.
pub(super) const PREFIXES: [usize; 4] = [1, 2, 1, 2];

/// A function a frame runs, by its `SharedFunctionInfo`, and its offset
/// in its script, or why it has none.
pub(super) type Place = (Object, Result<u64, Problem>);

/// A `Code` object, with the facts of it every reading needs.
#[derive(Debug, Clone, Copy)]
pub(super) struct Code {
    object: Object,
    start: u64,
    size: u64,
    pub(super) kind: i64,
}

impl Code {
    const fn holds(&self, address: u64) -> bool {
        self.start <= address && address - self.start < self.size
    }

    /// Whether the code is baseline (Sparkplug) code, whose frames keep
    /// their variables in the interpreter's registers.
    pub(super) fn is_baseline(&self, reader: &Reader<'_>) -> Result<bool, Problem> {
        Ok(self.kind == reader.constant(Constant::CodeKindBaseline)?)
    }
}

/// The code of `function` (`shared` its `SharedFunctionInfo`) whose
/// instructions hold the compiled frame `frame`'s.
pub(super) fn running(
    reader: &Reader<'_>,
    statics: &Statics,
    frame: &CodeFrame<'_>,
    (function, shared): (Object, Object),
) -> Result<Code, Problem> {
    // A caller's instruction is its return address, past its call.
    let lookup = frame
        .instruction
        .get()
        .checked_sub(1)
        .ok_or("the frame's instruction is 0")?;
    let mut problems = Vec::new();
    for candidate in candidates(reader, statics, frame, function, shared) {
        match candidate.and_then(|object| code(reader, object)) {
            Ok(code) if code.holds(lookup) => return Ok(code),
            Ok(_) => {}
            Err(problem) => problems.push(problem),
        }
    }
    let mut reason = String::from(
        "none of its function's code holds the frame's instruction, so neither its place \
         nor the functions its code inlined are known",
    );
    for problem in problems {
        reason.push_str("; ");
        reason.push_str(&problem);
    }
    Err(reason.into())
}

/// The functions a frame running `code` of `shared`'s function runs at its
/// instruction `pc`, innermost first, each with the offset in its script.
pub(super) fn places(
    reader: &Reader<'_>,
    encoding: Encoding,
    code: &Code,
    shared: Object,
    pc: u64,
) -> Result<Vec<Place>, Problem> {
    if code.is_baseline(reader)? {
        let (bytecode, offset) = baseline_offset(reader, code, pc - code.start)?;
        let table = positions::decode(&positions::table_bytes(reader, bytecode)?, encoding)?;
        let position = positions::position_at(&table, offset)
            .and_then(positions::script_offset)
            .ok_or("the bytecode has no position at the frame's offset")?;
        return Ok(vec![(shared, Ok(position))]);
    }
    if code.kind == reader.constant(Constant::CodeKindMaglev)?
        || code.kind == reader.constant(Constant::CodeKindTurbofanJS)?
    {
        return optimized_places(reader, encoding, code, shared, pc - code.start);
    }
    Err(format!(
        "the frame runs code of kind {}, which is no JavaScript's",
        code.kind
    )
    .into())
}

/// The function's codes a frame of it may run, each an object or why it
/// could not be found.
fn candidates(
    reader: &Reader<'_>,
    statics: &Statics,
    frame: &CodeFrame<'_>,
    function: Object,
    shared: Object,
) -> Vec<Result<Object, Problem>> {
    let mut candidates = vec![current_code(reader, statics, frame, function)];
    match super::names::function_data(reader, shared) {
        Ok(Some(data)) if reader.is(data, Constant::CodeType).unwrap_or(false) => {
            candidates.push(Ok(data));
        }
        Ok(_) => {}
        Err(problem) => candidates.push(Err(problem)),
    }
    match cached_code(reader, function) {
        Ok(cached) => candidates.extend(cached.into_iter().map(Ok)),
        Err(problem) => candidates.push(Err(problem)),
    }
    candidates
}

/// The code `function` runs when called now.
fn current_code(
    reader: &Reader<'_>,
    statics: &Statics,
    frame: &CodeFrame<'_>,
    function: Object,
) -> Result<Object, Problem> {
    // Before 13.6 a function holds its code.
    if reader.layout.has(Field::JSFunctionCode) {
        let object = reader
            .object(reader.field(function, Field::JSFunctionCode)?)?
            .ok_or("the function holds no code")?;
        return unwrap_code(reader, object);
    }

    let table = match &statics.dispatch_table {
        Ok(Some(table)) => *table,
        Ok(None) => return Err("the build has no dispatch table".into()),
        Err(reason) => return Err(format!("the dispatch table cannot be found: {reason}").into()),
    };
    let base = match table {
        DispatchTable::Isolate { offset } => {
            let place = match &statics.isolate {
                Some(Ok(place)) => *place,
                Some(Err(reason)) => {
                    return Err(format!("the thread's isolate cannot be found: {reason}").into());
                }
                None => return Err("the module names no current isolate".into()),
            };
            let slot = reader.stop.thread_local_address(frame.thread, place)?;
            let isolate = reader.word(slot.get())?;
            if isolate == 0 {
                return Err("the thread has no current isolate".into());
            }
            reader.word(isolate.wrapping_add(offset))?
        }
        DispatchTable::Group { group, offset } => {
            let pointer = reader
                .stop
                .load_bias()
                .load(group)
                .ok_or("the isolate group's pointer lies outside the process")?;
            let group = reader.word(pointer.get())?;
            reader.word(group.wrapping_add(offset))?
        }
    };

    let mut handle = [0; 4];
    reader.bytes_into(
        reader.field_address(function, Field::JSFunctionDispatchHandle)?,
        &mut handle,
    )?;
    let index = u64::from(u32::from_le_bytes(handle))
        >> reader.constant(Constant::JSDispatchHandleShift)?;
    let size = reader
        .constant(Constant::JSDispatchTableEntrySize)?
        .cast_unsigned();
    let entry = base.wrapping_add(index.wrapping_mul(size));
    let word = reader.word(
        entry.wrapping_add(
            reader
                .constant(Constant::JSDispatchEntryCodeObjectOffset)?
                .cast_unsigned(),
        ),
    )?;
    let shift = u32::try_from(reader.constant(Constant::JSDispatchEntryObjectPointerShift)?)
        .map_err(|_| "the dispatch entry's shift is out of range")?;
    let tag = reader.constant(Constant::HeapObjectTag)?.cast_unsigned();
    reader
        .object(word.checked_shr(shift).unwrap_or(0) | tag)?
        .ok_or_else(|| "the dispatch entry holds no code".into())
}

/// The code `object` is, through its wrapper where it is one.
fn unwrap_code(reader: &Reader<'_>, object: Object) -> Result<Object, Problem> {
    if reader.is(object, Constant::CodeWrapperType)? {
        return reader.typed_field(object, Field::CodeWrapperCode, Constant::CodeType);
    }
    Ok(object)
}

/// The optimized code `function`'s feedback vector caches: its OSR code,
/// held weakly in its slots, and on 12.4 its optimized code.
fn cached_code(reader: &Reader<'_>, function: Object) -> Result<Vec<Object>, Problem> {
    let cell = reader.typed_field(
        function,
        Field::JSFunctionFeedbackCell,
        Constant::FeedbackCellType,
    )?;
    let Some(vector) = reader
        .object(reader.field(cell, Field::FeedbackCellValue)?)?
        .filter(|vector| {
            reader
                .is(*vector, Constant::FeedbackVectorType)
                .unwrap_or(false)
        })
    else {
        return Ok(Vec::new());
    };

    let mut words = Vec::new();
    if reader.layout.has(Field::FeedbackVectorMaybeOptimizedCode) {
        words.push(reader.field(vector, Field::FeedbackVectorMaybeOptimizedCode)?);
    }
    let length = u64::try_from(reader.int32_field(vector, Field::FeedbackVectorLength)?)
        .map_err(|_| "a feedback vector has a negative length")?;
    if length > MAX_FEEDBACK_SLOTS {
        return Err("a feedback vector is implausibly long".into());
    }
    let first = reader.field_address(vector, Field::FeedbackVectorRawFeedbackSlots)?;
    for slot in 0..length {
        words.push(reader.word(first + slot * 8)?);
    }

    let mut found = Vec::new();
    for word in words {
        if let Some(object) = weak(reader, word)?
            && reader.is(object, Constant::CodeWrapperType)?
        {
            found.push(unwrap_code(reader, object)?);
        }
    }
    Ok(found)
}

/// The object a weak reference holds, or `None` for a word that is none
/// or one cleared.
fn weak(reader: &Reader<'_>, word: u64) -> Result<Option<Object>, Problem> {
    let tag_mask = reader
        .constant(Constant::HeapObjectTagMask)?
        .cast_unsigned();
    let weak_tag = reader
        .constant(Constant::WeakHeapObjectTag)?
        .cast_unsigned();
    let cleared = reader
        .constant(Constant::ClearedWeakHeapObjectLower32)?
        .cast_unsigned();
    if word & tag_mask != weak_tag || word & 0xffff_ffff == cleared {
        return Ok(None);
    }
    let mask = reader
        .constant(Constant::WeakHeapObjectMask)?
        .cast_unsigned();
    reader.object(word & !mask)
}

/// The facts of the `Code` object `object`, proven one.
pub(super) fn code(reader: &Reader<'_>, object: Object) -> Result<Code, Problem> {
    if !reader.is(object, Constant::CodeType)? {
        return Err(format!("{:#x} is no Code", object.0).into());
    }
    let start = reader.field(object, Field::CodeInstructionStart)?;
    let size = u64::from(
        reader
            .int32_field(object, Field::CodeInstructionSize)?
            .cast_unsigned(),
    );
    let flags = i64::from(
        reader
            .int32_field(object, Field::CodeFlags)?
            .cast_unsigned(),
    );
    let kind = (flags & reader.constant(Constant::CodeKindFieldMask)?)
        >> reader.constant(Constant::CodeKindFieldShift)?;
    Ok(Code {
        object,
        start,
        size,
        kind,
    })
}

/// The functions optimized `code` runs at the frame's pc, `pc_offset`
/// bytes into it, innermost first, with their script offsets: the frames
/// its deoptimization translation stands for there, each at its bytecode's
/// position, as V8's `OptimizedJSFrame::Summarize` gives them.
fn optimized_places(
    reader: &Reader<'_>,
    encoding: Encoding,
    code: &Code,
    shared: Object,
    pc_offset: u64,
) -> Result<Vec<Place>, Problem> {
    let frames = translation::call_site(reader, code.object, shared, pc_offset)?;
    frames
        .into_iter()
        .map(|frame| {
            let offset = match frame.offset {
                Offset::Bytecode(offset) => offset,
                // V8 places a builtin's continuation in the builtin's code,
                // which no script holds.
                Offset::Builtin(_) => {
                    return Ok((
                        frame.shared,
                        Err("a builtin's continuation is in no script".into()),
                    ));
                }
            };
            let bytecode = super::names::bytecode(reader, frame.shared)?
                .ok_or("an optimized frame's function keeps no bytecode")?;
            let table = positions::decode(&positions::table_bytes(reader, bytecode)?, encoding)?;
            let position = positions::position_at(&table, offset)
                .and_then(positions::script_offset)
                .ok_or("the bytecode has no position at the frame's offset");
            Ok((frame.shared, position.map_err(Problem::from)))
        })
        .collect()
}

/// The bytecode baseline `code` compiled, and the offset in it of the
/// bytecode whose code holds the instruction `offset` bytes into `code`.
pub(super) fn baseline_offset(
    reader: &Reader<'_>,
    code: &Code,
    offset: u64,
) -> Result<(Object, i64), Problem> {
    let data = reader
        .object(reader.field(code.object, Field::CodeDeoptimizationDataOrInterpreterData)?)?
        .ok_or("the baseline code names no bytecode")?;
    let bytecode = if reader.is(data, Constant::InterpreterDataType)? {
        reader.typed_field(
            data,
            Field::InterpreterDataBytecodeArray,
            Constant::BytecodeArrayType,
        )?
    } else if reader.is(data, Constant::BytecodeArrayType)? {
        data
    } else {
        return Err("the baseline code names no bytecode".into());
    };
    let table = reader
        .object(reader.field(code.object, Field::CodePositionTable)?)?
        .ok_or("the baseline code keeps no offset table")?;
    let table = positions::byte_array(reader, table)?;
    let length = usize::try_from(reader.smi_field(bytecode, Field::BytecodeArrayLength)?)
        .map_err(|_| "a bytecode array has a negative length")?;
    if length > 1 << 24 {
        return Err("a bytecode array is implausibly long".into());
    }
    let mut bytes = vec![0; length];
    reader.bytes_into(
        reader.field_address(bytecode, Field::BytecodeArrayBytes)?,
        &mut bytes,
    )?;
    let sizes = crate::image::v8::bytecode_sizes(reader.version)?;
    Ok((bytecode, bytecode_at(&table, &bytes, sizes, offset)?))
}

/// The offset of the bytecode whose baseline code holds the instruction
/// `offset` bytes into it, as V8's `BytecodeOffsetIterator` walks
/// `table`: each entry the size of one bytecode's code, in the order of
/// `bytecodes`, after the function's entry.
fn bytecode_at(table: &[u8], bytecodes: &[u8], sizes: &[u8], offset: u64) -> Result<i64, Problem> {
    let count = sizes.len() / 3;
    let mut at = 0;
    let mut end = positions::unsigned(table, &mut at)?;
    let mut current = positions::FUNCTION_ENTRY;
    let mut next = 0_usize;
    while end < offset {
        if at >= table.len() {
            return Err("the instruction is past the baseline code's offset table".into());
        }
        end = end
            .checked_add(positions::unsigned(table, &mut at)?)
            .ok_or("a baseline offset table overflows")?;
        current = i64::try_from(next).map_err(|_| "a bytecode offset is out of range")?;
        let byte = usize::from(
            *bytecodes
                .get(next)
                .ok_or("the offset table outlasts its bytecode")?,
        );
        if byte >= count {
            return Err("an unknown bytecode".into());
        }
        let size = match PREFIXES.get(byte) {
            Some(scale) => {
                let widened = usize::from(
                    *bytecodes
                        .get(next + 1)
                        .ok_or("a bytecode prefix ends its bytecode")?,
                );
                if widened >= count {
                    return Err("an unknown bytecode".into());
                }
                1 + usize::from(
                    *sizes
                        .get(scale * count + widened)
                        .ok_or("an unknown bytecode")?,
                )
            }
            None => usize::from(*sizes.get(byte).ok_or("an unknown bytecode")?),
        };
        if size == 0 {
            return Err(format!("bytecode {byte} has no size").into());
        }
        next += size;
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::bytecode_at;

    /// Sizes for a made-up set of five bytecodes at each scale: the four
    /// prefixes, then one whose operand widens from 1 byte to 2 and 4.
    const SIZES: [u8; 15] = [1, 1, 1, 1, 2, 1, 1, 1, 1, 3, 1, 1, 1, 1, 5];

    #[test]
    fn baseline_bytecodes_cannot_index_another_scale_row() {
        for bytes in [&[5][..], &[0, 5][..], &[1, 5][..]] {
            assert!(bytecode_at(&[0, 1], bytes, &SIZES, 1).is_err(), "{bytes:?}");
        }
    }

    /// The bytecode whose code holds an instruction is the one whose
    /// entry's end is at or past it, after the function's entry, with a
    /// widened bytecode counted with its prefix.
    #[test]
    fn a_baseline_instruction_is_in_the_bytecode_whose_code_holds_it() {
        // Bytecodes: 4 at 0 (2 bytes), Wide 4 at 2 (1 + 3), ExtraWide 4
        // at 6 (1 + 5), and 4 at 12.
        let bytecodes = [4, 7, 0, 4, 7, 7, 1, 4, 7, 7, 7, 7, 4, 7];
        // The entry's code is 10 bytes, then each bytecode's: 4, 6, 8, 3.
        let table = [10, 4, 6, 8, 3];
        let at = |offset| bytecode_at(&table, &bytecodes, &SIZES, offset).unwrap();
        assert_eq!(at(5), -1);
        assert_eq!(at(10), -1);
        assert_eq!(at(11), 0);
        assert_eq!(at(14), 0);
        assert_eq!(at(15), 2);
        assert_eq!(at(20), 2);
        assert_eq!(at(21), 6);
        assert_eq!(at(28), 6);
        assert_eq!(at(29), 12);
        assert_eq!(at(31), 12);
        assert!(bytecode_at(&table, &bytecodes, &SIZES, 32).is_err());
    }
}
