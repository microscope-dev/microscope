use std::sync::Arc;

use super::Statics;
use super::compiled;
use super::objects::{Object, Problem, Reader};
use super::positions::{self, Encoding};
use super::scripts::Table;
use crate::image::v8::{Constant, Field};
use crate::runtime_model::{CodeFrame, DescribedFrame};
use crate::unwind::FRAME_POINTER;
use crate::{CodeRole, RecordedFrame, RecordedPlace, SourceLanguage};

pub(super) fn describe(
    reader: &Reader<'_>,
    table: &mut Table,
    encoding: Encoding,
    statics: &Statics,
    frame: &CodeFrame<'_>,
) -> Option<DescribedFrame> {
    let running = if frame.after_call {
        Running::AfterCall
    } else if !frame.entered {
        return None;
    } else if frame.generated {
        Running::Generated
    } else if frame
        .image_instruction
        .is_some_and(|address| statics.handlers.iter().any(|range| range.contains(address)))
    {
        Running::Handler
    } else {
        return None;
    };
    let fp = frame.registers.get(FRAME_POINTER)?;
    let marker = reader
        .word(fp.wrapping_add_signed(reader.constant(Constant::OffFpContextOrFrameType).ok()?))
        .ok()?;
    // A non-heap-object marker (Smi frame type) identifies an internal/stub V8 frame.
    if reader.object(marker).ok()?.is_none() {
        return Some(DescribedFrame {
            functions: Vec::new(),
            role: CodeRole::Interpreter,
            live_registers: false,
            awaiting: None,
        });
    }

    let function = javascript_function(reader, fp).ok()??;
    let (functions, live_registers) = match functions(
        reader,
        table,
        encoding,
        statics,
        frame,
        (fp, running),
        function,
    ) {
        Ok(None) => {
            return Some(DescribedFrame {
                functions: Vec::new(),
                role: CodeRole::Interpreter,
                live_registers: false,
                awaiting: super::awaits::awaiting(
                    reader,
                    table,
                    encoding,
                    &statics.awaits,
                    function,
                ),
            });
        }
        Ok(Some(found)) => found,
        Err(problem) => (
            vec![RecordedFrame {
                function: "<unknown>".into(),
                language: SourceLanguage::JavaScript,
                place: None,
                first_line: None,
                declared_in: None,
                generated: None,
                problem: Some(problem),
            }],
            false,
        ),
    };
    let role = if functions
        .last()
        .and_then(|own| own.place.as_ref())
        .is_none_or(|place| place.path.starts_with("node:"))
    {
        CodeRole::RuntimeInternal
    } else {
        CodeRole::Ordinary
    };
    Some(DescribedFrame {
        functions,
        role,
        live_registers,
        awaiting: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Running {
    AfterCall,

    Generated,

    Handler,
}

const IN_HANDLER: &str = "the frame runs a bytecode handler, which keeps the offset of its \
     bytecode in a register, not in the frame, so its statement is not known";

const BETWEEN_CALLS: &str = "optimized code records the statement it runs only at its calls, and this frame is between them";

fn functions(
    reader: &Reader<'_>,
    table: &mut Table,
    encoding: Encoding,
    statics: &Statics,
    frame: &CodeFrame<'_>,
    (fp, running): (u64, Running),
    function: Object,
) -> Result<Option<(Vec<RecordedFrame>, bool)>, Problem> {
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    if script(reader, shared)?.is_none() && !super::names::is_native(reader, shared)? {
        return Ok(None);
    }
    let (places, live_registers) = if frame.generated {
        match compiled::running(reader, statics, frame, (function, shared)) {
            Ok(code) if running == Running::Generated => {
                let baseline = code.is_baseline(reader)?;
                let places = if baseline {
                    compiled::places(reader, encoding, &code, shared, frame.instruction.get() + 1)
                } else {
                    Err(BETWEEN_CALLS.into())
                };
                (places, baseline)
            }
            Ok(code) => (
                compiled::places(reader, encoding, &code, shared, frame.instruction.get()),
                code.is_baseline(reader)?,
            ),
            Err(problem) => (Err(problem), false),
        }
    } else if running == Running::Handler {
        (Err(IN_HANDLER.into()), true)
    } else {
        (
            offset_in_script(reader, encoding, fp, shared).map(|offset| vec![(shared, Ok(offset))]),
            true,
        )
    };
    let own = (function, shared);
    let functions = match places {
        Ok(places) => {
            let count = places.len();
            places
                .into_iter()
                .enumerate()
                .map(|(index, (inlined, offset))| {
                    let named = if index + 1 == count && inlined == shared {
                        Named::Function(own)
                    } else {
                        Named::Shared(inlined)
                    };
                    recorded(reader, table, named, offset)
                })
                .collect::<Result<_, _>>()?
        }
        Err(problem) => vec![recorded(reader, table, Named::Function(own), Err(problem))?],
    };
    Ok(Some((functions, live_registers)))
}

pub(super) fn script(reader: &Reader<'_>, shared: Object) -> Result<Option<Object>, Problem> {
    let script = reader.field(shared, Field::SharedFunctionInfoScript)?;
    Ok(match reader.object(script)? {
        Some(script) if reader.is(script, Constant::ScriptType)? => Some(script),
        _ => None,
    })
}

pub(super) fn javascript_function(reader: &Reader<'_>, fp: u64) -> Result<Option<Object>, Problem> {
    let slot = fp.wrapping_add_signed(reader.constant(Constant::OffFpFunction)?);
    let Some(function) = reader.object(reader.word(slot)?)? else {
        return Ok(None);
    };
    let ty = reader.instance_type(function)?;
    let first = reader.constant(Constant::FirstJSFunctionType)?;
    let last = reader.constant(Constant::LastJSFunctionType)?;
    Ok((first..=last).contains(&ty).then_some(function))
}

#[derive(Clone, Copy)]
pub(super) enum Named {
    Function((Object, Object)),
    Shared(Object),
}

pub(super) fn recorded(
    reader: &Reader<'_>,
    table: &mut Table,
    named: Named,
    offset: Result<u64, Problem>,
) -> Result<RecordedFrame, Problem> {
    let (name, shared) = match named {
        Named::Function((function, shared)) => {
            let name = match super::names::property_name(reader, function) {
                Ok(Some(name)) => Ok(name.into()),
                Ok(None) => table.function_name(reader, shared),
                Err(problem) => Err(problem),
            };
            (name, shared)
        }
        Named::Shared(shared) => (table.function_name(reader, shared), shared),
    };
    let (name, unnamed) = match name {
        Ok(name) => (name, None),
        Err(problem) => (Arc::from(""), Some(problem)),
    };
    let script = script(reader, shared)?;
    let path = script
        .map(|script| table.path(reader, script))
        .transpose()?
        .flatten();

    let (place, problem) = match (script, &path) {
        (Some(script), Some(path)) => match offset {
            Ok(offset) => {
                let (line, column) = table.line_column(reader, script, offset)?;
                let place = RecordedPlace {
                    path: Arc::clone(path),
                    line,
                    column,
                };
                (Some(place), None)
            }
            Err(problem) => (None, Some(problem)),
        },
        _ => (None, None),
    };

    let first_line = script
        .and_then(|script| table.first_line(reader, shared, script).ok())
        .flatten();
    let language = language(path.as_deref());
    let declared_in = path.filter(|_| place.is_none() && first_line.is_some());
    Ok(RecordedFrame {
        function: name,
        language,
        place,
        first_line,
        declared_in,
        generated: None,
        problem: problem.or(unnamed),
    })
}

pub(super) fn script_name(reader: &Reader<'_>, script: Object) -> Result<Option<String>, Problem> {
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

pub(super) fn file_url_path(name: &str) -> Option<String> {
    let rest = name.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut chars = path.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'?' || byte == b'#' {
            return None;
        }
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = char::from(chars.next()?).to_digit(16)?;
        let low = char::from(chars.next()?).to_digit(16)?;
        let decoded = u8::try_from(high * 16 + low).ok()?;
        if decoded == b'/' || decoded == 0 {
            return None;
        }
        bytes.push(decoded);
    }
    String::from_utf8(bytes).ok()
}

fn offset_in_script(
    reader: &Reader<'_>,
    encoding: Encoding,
    fp: u64,
    shared: Object,
) -> Result<u64, Problem> {
    let slot = |constant| -> Result<u64, Problem> {
        reader.word(fp.wrapping_add_signed(reader.constant(constant)?))
    };
    bytecode_offset_in_script(
        reader,
        encoding,
        slot(Constant::OffFpBytecodeArray)?,
        slot(Constant::OffFpBytecodeOffset)?,
        shared,
    )
}

pub(super) fn bytecode_offset_in_script(
    reader: &Reader<'_>,
    encoding: Encoding,
    bytecode: u64,
    offset: u64,
    shared: Object,
) -> Result<u64, Problem> {
    let bytecode = reader
        .object(bytecode)?
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

    let raw = reader
        .smi(offset)?
        .ok_or("the frame's bytecode offset is no Smi")?;
    let offset = bytecode_offset(reader, bytecode, raw)?;
    script_offset_at(reader, encoding, bytecode, offset)
}

pub(super) fn bytecode_offset(
    reader: &Reader<'_>,
    bytecode: Object,
    raw: i64,
) -> Result<i64, Problem> {
    let header = i64::try_from(reader.offset(Field::BytecodeArrayBytes)?)
        .map_err(|_| "a bytecode header is out of range")?;
    // V8 stores the bytecode offset relative to the tagged `BytecodeArray` pointer.
    let offset = raw - header + reader.constant(Constant::HeapObjectTag)?;
    let length = reader.smi_field(bytecode, Field::BytecodeArrayLength)?;
    if !(0..length).contains(&offset) {
        return Err(format!("the bytecode offset {offset} is outside its {length} bytes").into());
    }
    Ok(offset)
}

pub(super) fn script_offset_at(
    reader: &Reader<'_>,
    encoding: Encoding,
    bytecode: Object,
    offset: i64,
) -> Result<u64, Problem> {
    let table = positions::decode(&positions::table_bytes(reader, bytecode)?, encoding)?;
    positions::position_at(&table, offset)
        .and_then(positions::script_offset)
        .ok_or_else(|| "the bytecode has no position at its offset".into())
}

pub(super) fn language(path: Option<&str>) -> SourceLanguage {
    match path {
        Some(path)
            if [".ts", ".mts", ".cts"]
                .iter()
                .any(|end| path.ends_with(end)) =>
        {
            SourceLanguage::TypeScript
        }
        _ => SourceLanguage::JavaScript,
    }
}

#[cfg(test)]
mod tests {
    use super::file_url_path;

    #[test]
    fn a_file_url_names_the_path_node_encoded_into_it() {
        let path = |url: &str| file_url_path(url);
        assert_eq!(
            path("file:///srv/app/a.mjs").as_deref(),
            Some("/srv/app/a.mjs")
        );
        assert_eq!(
            path("file:///srv/my%20app/%C3%A9t%C3%A9.mjs").as_deref(),
            Some("/srv/my app/été.mjs")
        );
        assert_eq!(path("file://localhost/a.mjs").as_deref(), Some("/a.mjs"));
        for url in [
            "node:fs",
            "/srv/app/a.js",
            "file://host/a.mjs",
            "file:///a%2Fb.mjs",
            "file:///a%00.mjs",
            "file:///a%2.mjs",
            "file:///a%zz.mjs",
            "file:///a%ff.mjs",
            "file:///a.mjs?x=1",
        ] {
            assert_eq!(path(url), None, "{url}");
        }
    }
}
