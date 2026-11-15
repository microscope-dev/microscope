use std::sync::Arc;

use super::frames;
use super::objects::{Object, Problem, Reader};
use super::positions::Encoding;
use super::scripts::Table;
use crate::image::v8::{Constant, Field};
use crate::runtime_model::SampledCodeFrame;
use crate::{RecordedFrame, RecordedPlace};

const COMPILED: &str = "the frame runs code that is not V8's interpreter, or not a copy of it \
                        microscope proved, and the walker does not prove which compiled code it \
                        ran, so its line is unknown";

const INNERMOST: &str = "the interpreter writes a frame's bytecode offset to it only before a \
                         call, and the walk saw none made from this frame, so its line is \
                         unknown";

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
