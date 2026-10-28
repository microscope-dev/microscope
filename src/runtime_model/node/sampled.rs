//! Names sampled JavaScript frames after revalidating V8 object identity.
//!
//! The BPF walker captures the function, shared information, script address
//! and id, and function literal id. A later read of shared information must
//! retain the script address, script id, and literal id: script ids are never
//! reused within an isolate, and a literal id names one function in that
//! script (as in `scripts.rs`). GC movement or address reuse fails validation
//! and leaves the function unknown.
//!
//! Interpreted locations use the frame's bytecode and offset after proving
//! the bytecode belongs to its function. Handlers keep the offset in varying
//! registers and save it to the slot only before a call. A location therefore
//! requires an observed call: a nested call frame or a system call. CPU
//! samples cannot distinguish a handler's own frame from a call, so their
//! innermost frame is not treated as having called.
//! Compiled code identity is not captured, so its line remains unknown even
//! when its function is known.

use std::sync::Arc;

use super::frames;
use super::objects::{Object, Problem, Reader};
use super::positions::Encoding;
use super::scripts::Table;
use crate::image::v8::{Constant, Field};
use crate::runtime_model::SampledCodeFrame;
use crate::{RecordedFrame, RecordedPlace};

/// Why a compiled frame, or one whose code is not proven the
/// interpreter's, has no place.
const COMPILED: &str = "the frame runs code that is not V8's interpreter, or not a copy of it \
                        microscope proved, and the walker does not prove which compiled code it \
                        ran, so its line is unknown";

/// Why an interpreted frame that made no call the walk could see has no
/// place.
const INNERMOST: &str = "the interpreter writes a frame's bytecode offset to it only before a \
                         call, and the walk saw none made from this frame, so its line is \
                         unknown";

/// The functions `frame` runs; `None` for a frame of no script's function,
/// which V8's stack traces hide or a native function's.
pub(super) fn describe(
    reader: &Reader<'_>,
    table: &mut Table,
    encoding: Encoding,
    frame: &SampledCodeFrame,
) -> Option<Vec<RecordedFrame>> {
    if frame.script == 0 {
        return None;
    }
    Some(vec![match recorded(reader, table, encoding, frame) {
        Ok(recorded) => recorded,
        Err(problem) => RecordedFrame {
            function: "<unknown>".into(),
            language: crate::SourceLanguage::JavaScript,
            place: None,
            first_line: None,
            declared_in: None,
            generated: None,
            problem: Some(problem),
        },
    }])
}

/// The shared information the walker read, proven the same now.
fn proven(reader: &Reader<'_>, frame: &SampledCodeFrame) -> Result<(Object, Object), Problem> {
    let moved = || -> Problem {
        "the function's shared information moved or was freed after the sample".into()
    };
    let shared = reader.object(frame.shared)?.ok_or_else(moved)?;
    if !reader.is(shared, Constant::SharedFunctionInfoType)? {
        return Err(moved());
    }
    let script = frames::script(reader, shared)?.ok_or_else(moved)?;
    if script.0 != frame.script
        || reader.field(script, Field::ScriptId)? != frame.script_id
        || reader.int32_field(shared, Field::SharedFunctionInfoFunctionLiteralId)?
            != frame.literal_id
    {
        return Err(moved());
    }
    Ok((shared, script))
}

/// The function the walker read, if it is still a function of the shared
/// information it read: its `name` property names it where V8's
/// `JSFunction::GetDebugName` reads one, as for a method keyed by a symbol.
/// One that moved names its frame by its shared information, which differs
/// only for a function whose `name` the program redefined.
fn function(reader: &Reader<'_>, frame: &SampledCodeFrame) -> Option<Object> {
    let function = reader.object(frame.function).ok()??;
    let ty = reader.instance_type(function).ok()?;
    let first = reader.constant(Constant::FirstJSFunctionType).ok()?;
    let last = reader.constant(Constant::LastJSFunctionType).ok()?;
    ((first..=last).contains(&ty)
        && reader
            .field(function, Field::JSFunctionSharedFunctionInfo)
            .ok()?
            == frame.shared)
        .then_some(function)
}

fn recorded(
    reader: &Reader<'_>,
    table: &mut Table,
    encoding: Encoding,
    frame: &SampledCodeFrame,
) -> Result<RecordedFrame, Problem> {
    let (shared, script) = proven(reader, frame)?;
    let named = function(reader, frame).map_or(Ok(None), |function| {
        super::names::property_name(reader, function)
    });
    let named = named.and_then(|name| {
        name.map_or_else(
            || table.function_name(reader, shared),
            |name| Ok(name.into()),
        )
    });
    let (function, unnamed) = match named {
        Ok(name) => (name, None),
        Err(problem) => (Arc::from(""), Some(problem)),
    };
    let path = table.path(reader, script)?;
    let offset = if frame.generated {
        Err(Problem::from(COMPILED))
    } else if !frame.after_call {
        Err(Problem::from(INNERMOST))
    } else {
        frames::bytecode_offset_in_script(
            reader,
            encoding,
            frame.bytecode,
            frame.bytecode_offset,
            shared,
        )
    };
    let (place, problem) = match (&path, offset) {
        (Some(path), Ok(offset)) => {
            let (line, column) = table.line_column(reader, script, offset)?;
            let place = RecordedPlace {
                path: Arc::clone(path),
                line,
                column,
            };
            (Some(place), None)
        }
        (Some(_), Err(problem)) => (None, Some(problem)),
        (None, _) => (None, None),
    };
    // A function is told apart from others of its name by where it is
    // declared, and an anonymous one is shown by it; a frame without it
    // is still the function's.
    let first_line = table.first_line(reader, shared, script).ok().flatten();
    let language = frames::language(path.as_deref());
    let declared_in = path.filter(|_| place.is_none() && first_line.is_some());
    Ok(RecordedFrame {
        function,
        language,
        place,
        first_line,
        declared_in,
        generated: None,
        problem: problem.or(unnamed),
    })
}
