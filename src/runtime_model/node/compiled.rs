//! Resolves JIT-compiled V8 frames (`Sparkplug` baseline, `Maglev`, and `TurboFan`) to source
//! positions.
//!
//! Checks the function's current and cached `Code` objects first, then scans backward for an
//! aligned `InstructionStream` header when a frame is still running older tiered code.

use super::Statics;
use super::objects::{Object, Problem, Reader};
use super::positions::{self, Encoding};
use super::translation::{self, Offset};
use crate::image::v8::{Constant, DispatchTable, Field};
use crate::runtime_model::CodeFrame;

const MAX_FEEDBACK_SLOTS: u64 = 1 << 16;

const MAX_CODE_SEARCH: u64 = 1 << 20;

pub(super) const PREFIXES: [usize; 4] = [1, 2, 1, 2];

#[expect(
    clippy::indexing_slicing,
    reason = "a compile-time check: an index out of bounds fails the build"
)]
const _: () = {
    let mut index = 0;
    while index < PREFIXES.len() {
        assert!(PREFIXES[index] >= 1 && PREFIXES[index] <= 2);
        index += 1;
    }
};

pub(super) type Place = (Object, Result<u64, Problem>);

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

    pub(super) fn is_baseline(&self, reader: &Reader<'_>) -> Result<bool, Problem> {
        Ok(self.kind == reader.constant(Constant::CodeKindBaseline)?)
    }
}

pub(super) fn running(
    reader: &Reader<'_>,
    statics: &Statics,
    frame: &CodeFrame<'_>,
    (function, shared): (Object, Object),
) -> Result<Code, Problem> {
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
    if let Some(code) = containing_code(reader, lookup)? {
        debug_assert!(
            code.holds(lookup),
            "the instruction stream found for {lookup:#x} is code {:#x} at {:#x}, which does \
             not hold it",
            code.object.0,
            code.start
        );
        return Ok(code);
    }
    let mut reason = String::from(
        "none of its function's cached code holds the frame's instruction, and no matching \
         instruction stream was found within the preceding 1 MiB; neither its place \
         nor the functions its code inlined are known",
    );
    for problem in problems {
        reason.push_str("; ");
        reason.push_str(&problem);
    }
    Err(reason.into())
}

fn containing_code(reader: &Reader<'_>, lookup: u64) -> Result<Option<Code>, Problem> {
    let alignment = reader.constant(Constant::CodeAlignment)?.cast_unsigned();
    let header = reader.offset(Field::InstructionStreamInstructions)?;
    let tag = reader.constant(Constant::HeapObjectTag)?.cast_unsigned();
    let mut entry = lookup & !(alignment - 1);
    let first = entry.saturating_sub(MAX_CODE_SEARCH);
    while entry >= first {
        let Some(address) = entry.checked_sub(header) else {
            break;
        };
        let stream = Object(address | tag);
        if reader
            .is(stream, Constant::InstructionStreamType)
            .unwrap_or(false)
            && let Ok(object) =
                reader.typed_field(stream, Field::InstructionStreamCode, Constant::CodeType)
            && let Ok(found) = code(reader, object)
            && found.start == entry
            && found.holds(lookup)
            && reader
                .field(object, Field::CodeInstructionStream)
                .is_ok_and(|back| back == stream.0)
        {
            return Ok(Some(found));
        }
        let Some(previous) = entry.checked_sub(alignment) else {
            break;
        };
        entry = previous;
    }
    Ok(None)
}

pub(super) fn places(
    reader: &Reader<'_>,
    encoding: Encoding,
    code: &Code,
    shared: Object,
    pc: u64,
) -> Result<Vec<Place>, Problem> {
    debug_assert!(
        code.start < pc,
        "the pc {pc:#x} is not past the start {:#x} of the code {:#x} that holds it",
        code.start,
        code.object.0
    );
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

fn current_code(
    reader: &Reader<'_>,
    statics: &Statics,
    frame: &CodeFrame<'_>,
    function: Object,
) -> Result<Object, Problem> {
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

fn unwrap_code(reader: &Reader<'_>, object: Object) -> Result<Object, Problem> {
    if reader.is(object, Constant::CodeWrapperType)? {
        return reader.typed_field(object, Field::CodeWrapperCode, Constant::CodeType);
    }
    Ok(object)
}

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
        words.push(reader.word(super::objects::address_at(first, slot * 8)?)?);
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
    use super::super::objects::Object;
    use super::super::objects::tests::{Heap, layout, reader};
    use super::{bytecode_at, cached_code};
    use crate::image::v8::{Constant, Field};

    #[test]
    fn a_feedback_vector_at_the_top_of_memory_ends_there() {
        let layout = layout();
        let mut heap = Heap::default();
        let field = |field| layout.field(field).expect("the layout has the field");
        let ty = |constant| layout.constant(constant).expect("the layout has the type");
        let map = |heap: &mut Heap, at: u64, ty: i64| {
            heap.word(at, at + 1);
            heap.word(at + field(Field::MapInstanceType), ty.cast_unsigned());
            at + 1
        };

        let cell_map = map(&mut heap, 0x1000, ty(Constant::FeedbackCellType));
        let vector_map = map(&mut heap, 0x2000, ty(Constant::FeedbackVectorType));
        let (function, cell) = (0x3000, 0x4000);

        let vector = u64::MAX - 7 - field(Field::FeedbackVectorRawFeedbackSlots);
        heap.word(vector, vector_map);
        heap.word(vector + field(Field::FeedbackVectorLength), 2);
        if let Some(code) = layout.field(Field::FeedbackVectorMaybeOptimizedCode) {
            heap.word(vector + code, 0);
        }
        heap.word(u64::MAX - 7, 0);
        heap.word(function + field(Field::JSFunctionFeedbackCell), cell + 1);
        heap.word(cell, cell_map);
        heap.word(cell + field(Field::FeedbackCellValue), vector + 1);

        let reader = reader(&heap, &layout);
        assert!(cached_code(&reader, Object(function + 1)).is_err());
    }

    const SIZES: [u8; 15] = [1, 1, 1, 1, 2, 1, 1, 1, 1, 3, 1, 1, 1, 1, 5];

    #[test]
    fn baseline_bytecodes_cannot_index_another_scale_row() {
        for bytes in [&[5][..], &[0, 5][..], &[1, 5][..]] {
            assert!(bytecode_at(&[0, 1], bytes, &SIZES, 1).is_err(), "{bytes:?}");
        }
    }

    #[test]
    fn a_baseline_instruction_is_in_the_bytecode_whose_code_holds_it() {
        let bytecodes = [4, 7, 0, 4, 7, 7, 1, 4, 7, 7, 7, 7, 4, 7];
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
