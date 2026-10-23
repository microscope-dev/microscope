//! What a physical frame in V8's code runs.
//!
//! V8 keeps its frames on the native stack. Each holds, below the word
//! that saved its caller's frame pointer, either a marker, a small integer
//! naming a frame of V8's own (entry, exit, stub), or the frame's context,
//! a heap object, followed by the JavaScript function it runs. An
//! interpreted frame also holds its bytecode and the offset of the
//! instruction it runs or called from. Each fact is read only as the type
//! it is proven to be.

use super::objects::{Object, Problem, Reader};
use super::positions::{self, Encoding};
use crate::image::v8::{Constant, Field};
use crate::runtime_model::{CodeFrame, DescribedFrame};
use crate::unwind::RBP;
use crate::{CodeRole, RecordedFrame, RecordedPlace, SourceLanguage};

/// The frame `frame`, in V8's code, describes; `None` where nothing proves
/// what it runs, which keeps the name its code's symbol gives it.
pub(super) fn describe(
    reader: &Reader<'_>,
    encoding: Encoding,
    frame: &CodeFrame<'_>,
) -> Option<DescribedFrame> {
    // A frame stopped inside its code may not have entered its frame yet,
    // or may have left it; only a call is made from inside one.
    if !frame.after_call {
        return None;
    }
    let fp = frame.registers.get(RBP)?;
    let marker = reader
        .word(fp.wrapping_add_signed(reader.constant(Constant::OffFpContextOrFrameType).ok()?))
        .ok()?;
    // A frame of V8's own keeps a marker, a small integer with the Smi
    // tag, where a JavaScript frame keeps its context.
    if reader.object(marker).ok()?.is_none() {
        return Some(DescribedFrame {
            functions: Vec::new(),
            role: CodeRole::Interpreter,
        });
    }

    let function = javascript_function(reader, fp).ok()??;
    let recorded = match recorded(reader, encoding, frame, fp, function) {
        Ok(recorded) => recorded,
        Err(problem) => RecordedFrame {
            function: "<unknown>".into(),
            language: SourceLanguage::JavaScript,
            place: None,
            first_line: None,
            problem: Some(problem),
        },
    };
    let role = if recorded
        .place
        .as_ref()
        .is_none_or(|place| place.path.starts_with("node:"))
    {
        CodeRole::RuntimeInternal
    } else {
        CodeRole::Ordinary
    };
    Some(DescribedFrame {
        functions: vec![recorded],
        role,
    })
}

/// The function the JavaScript frame at `fp` runs, or `None` where its
/// slot holds no function, as in a frame of V8's that keeps a context.
fn javascript_function(reader: &Reader<'_>, fp: u64) -> Result<Option<Object>, Problem> {
    let slot = fp.wrapping_add_signed(reader.constant(Constant::OffFpFunction)?);
    let Some(function) = reader.object(reader.word(slot)?)? else {
        return Ok(None);
    };
    let ty = reader.instance_type(function)?;
    let first = reader.constant(Constant::FirstJSFunctionType)?;
    let last = reader.constant(Constant::LastJSFunctionType)?;
    Ok((first..=last).contains(&ty).then_some(function))
}

/// What the frame at `fp` that runs `function` records.
fn recorded(
    reader: &Reader<'_>,
    encoding: Encoding,
    frame: &CodeFrame<'_>,
    fp: u64,
    function: Object,
) -> Result<RecordedFrame, Problem> {
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    let name = super::names::function_name(reader, shared)?;
    let script = reader.field(shared, Field::SharedFunctionInfoScript)?;
    let script = match reader.object(script)? {
        Some(script) if reader.is(script, Constant::ScriptType)? => Some(script),
        _ => None,
    };
    let path = script
        .map(|script| script_name(reader, script))
        .transpose()?
        .flatten();

    let (place, problem) = match (script, &path) {
        (Some(script), Some(path)) => match offset_in_script(reader, encoding, frame, fp, shared) {
            Ok(offset) => {
                let (line, column) = positions::line_column(reader, script, offset)?;
                let place = RecordedPlace {
                    path: path.as_str().into(),
                    line,
                    column,
                };
                (Some(place), None)
            }
            Err(problem) => (None, Some(problem)),
        },
        _ => (None, None),
    };

    let language = match &path {
        Some(path)
            if [".ts", ".mts", ".cts"]
                .iter()
                .any(|end| path.ends_with(end)) =>
        {
            SourceLanguage::TypeScript
        }
        _ => SourceLanguage::JavaScript,
    };
    Ok(RecordedFrame {
        function: name.into(),
        language,
        place,
        first_line: None,
        problem,
    })
}

/// The name of `script`, or its source URL where it has no name, as V8's
/// call sites give it; `None` where it has neither.
fn script_name(reader: &Reader<'_>, script: Object) -> Result<Option<String>, Problem> {
    for field in [Field::ScriptName, Field::ScriptSourceUrl] {
        let word = reader.field(script, field)?;
        if let Some(name) = reader.object(word)?
            && reader.instance_type(name)? < reader.constant(Constant::FirstNonstringType)?
        {
            let name = reader.string(name)?;
            if !name.is_empty() {
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

/// The offset in its script of the code the frame at `fp` runs: from its
/// bytecode and the offset it keeps, for a frame the interpreter runs.
fn offset_in_script(
    reader: &Reader<'_>,
    encoding: Encoding,
    frame: &CodeFrame<'_>,
    fp: u64,
    shared: Object,
) -> Result<u64, Problem> {
    if frame.generated {
        return Err("the place of compiled code is not read yet".into());
    }
    let slot = |constant| -> Result<u64, Problem> {
        reader.word(fp.wrapping_add_signed(reader.constant(constant)?))
    };
    let bytecode = reader
        .object(slot(Constant::OffFpBytecodeArray)?)?
        .filter(|bytecode| {
            reader
                .is(*bytecode, Constant::BytecodeArrayType)
                .unwrap_or(false)
        })
        .ok_or("the frame keeps no bytecode")?;
    if let Some(own) = super::names::bytecode(reader, shared)?
        && own != bytecode
    {
        return Err("the frame's bytecode is not its function's".into());
    }

    // The frame keeps the offset from the bytecode array's tagged address,
    // past its header.
    let raw = reader
        .smi(slot(Constant::OffFpBytecodeOffset)?)?
        .ok_or("the frame's bytecode offset is no Smi")?;
    let header = i64::try_from(reader.offset(Field::BytecodeArrayBytes)?)
        .map_err(|_| "a bytecode header is out of range")?;
    let offset = raw - header + reader.constant(Constant::HeapObjectTag)?;
    let length = reader.smi_field(bytecode, Field::BytecodeArrayLength)?;
    if !(0..length).contains(&offset) {
        return Err(
            format!("the frame's bytecode offset {offset} is outside its {length} bytes").into(),
        );
    }

    let table = positions::decode(&positions::table_bytes(reader, bytecode)?, encoding)?;
    positions::position_at(&table, offset)
        .and_then(positions::script_offset)
        .ok_or_else(|| "the bytecode has no position at the frame's offset".into())
}
