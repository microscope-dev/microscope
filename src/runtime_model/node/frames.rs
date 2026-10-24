//! What a physical frame in V8's code runs.
//!
//! V8 keeps its frames on the native stack. Each holds, below the word
//! that saved its caller's frame pointer, either a marker, a small integer
//! naming a frame of V8's own (entry, exit, stub), or the frame's context,
//! a heap object, followed by the JavaScript function it runs. An
//! interpreted frame also holds its bytecode and the offset of the
//! instruction it runs or called from. Each fact is read only as the type
//! it is proven to be.

use std::sync::Arc;

use super::Statics;
use super::compiled;
use super::objects::{Object, Problem, Reader};
use super::positions::{self, Encoding};
use super::scripts::Table;
use crate::image::v8::{Constant, Field};
use crate::runtime_model::{CodeFrame, DescribedFrame};
use crate::unwind::RBP;
use crate::{CodeRole, RecordedFrame, RecordedPlace, SourceLanguage};

/// The frame `frame`, in V8's code, describes; `None` where nothing proves
/// what it runs, which keeps the name its code's symbol gives it.
pub(super) fn describe(
    reader: &Reader<'_>,
    table: &mut Table,
    encoding: Encoding,
    statics: &Statics,
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
            live_registers: false,
        });
    }

    let function = javascript_function(reader, fp).ok()??;
    let (functions, live_registers) =
        match functions(reader, table, encoding, statics, frame, fp, function) {
            // A function V8's own stack traces hide is its machinery.
            Ok(None) => {
                return Some(DescribedFrame {
                    functions: Vec::new(),
                    role: CodeRole::Interpreter,
                    live_registers: false,
                });
            }
            Ok(Some(found)) => found,
            Err(problem) => (
                vec![RecordedFrame {
                    function: "<unknown>".into(),
                    language: SourceLanguage::JavaScript,
                    place: None,
                    first_line: None,
                    generated: None,
                    problem: Some(problem),
                }],
                false,
            ),
        };
    // The frame's own function, outermost, decides whose code it is.
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
    })
}

/// The functions the frame at `fp`, which runs `function`, runs, innermost
/// first: those its code inlined, then its own; and whether the frame's
/// interpreter registers are live, as they are unless its code is
/// optimized. `None` for a function V8's stack traces hide: one of no
/// script that is not exposed to the program as a native function, such
/// as the closures that resume an `await`.
fn functions(
    reader: &Reader<'_>,
    table: &mut Table,
    encoding: Encoding,
    statics: &Statics,
    frame: &CodeFrame<'_>,
    fp: u64,
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
    // The interpreter's own code is the image's, and baseline code keeps
    // its registers; optimized code, even entered on a loop, does not.
    let (places, live_registers) = if frame.generated {
        match compiled::running(reader, statics, frame, (function, shared)) {
            Ok(code) => (
                compiled::places(reader, encoding, &code, shared, frame.instruction.get()),
                code.is_baseline(reader)?,
            ),
            Err(problem) => (Err(problem), false),
        }
    } else {
        (
            offset_in_script(reader, encoding, fp, shared).map(|offset| vec![(shared, Ok(offset))]),
            true,
        )
    };
    // Only the frame's own function, outermost, is known as a function;
    // those its code inlined are known by their shared information.
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
        // The frame's function is known where its place is not.
        Err(problem) => vec![recorded(reader, table, Named::Function(own), Err(problem))?],
    };
    Ok(Some((functions, live_registers)))
}

/// The script `shared` was compiled from, if it was.
pub(super) fn script(reader: &Reader<'_>, shared: Object) -> Result<Option<Object>, Problem> {
    let script = reader.field(shared, Field::SharedFunctionInfoScript)?;
    Ok(match reader.object(script)? {
        Some(script) if reader.is(script, Constant::ScriptType)? => Some(script),
        _ => None,
    })
}

/// The function the JavaScript frame at `fp` runs, or `None` where its
/// slot holds no function, as in a frame of V8's that keeps a context.
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

/// A function a frame runs: one known as a function, with its shared
/// information, or one an optimized frame inlined, known only by its
/// shared information.
#[derive(Clone, Copy)]
enum Named {
    Function((Object, Object)),
    Shared(Object),
}

/// The function `named` as V8 records it, at `offset` in its script, or
/// with the problem that it has no place known.
fn recorded(
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
    // A name that cannot be read leaves the frame its place.
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
        function: name,
        language,
        place,
        first_line: None,
        generated: None,
        problem: problem.or(unnamed),
    })
}

/// The name of `script`, or its source URL where it has no name, as V8's
/// call sites give it; `None` where it has neither.
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

/// The path a `file:` URL names, as Node names an ES module's script, or
/// `None` for any other name or a URL that does not name a local path.
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
        // Node's fileURLToPath refuses an encoded separator or NUL.
        if decoded == b'/' || decoded == 0 {
            return None;
        }
        bytes.push(decoded);
    }
    String::from_utf8(bytes).ok()
}

/// The offset in its script of the code the frame at `fp` runs: from its
/// bytecode and the offset it keeps, for a frame the interpreter runs.
fn offset_in_script(
    reader: &Reader<'_>,
    encoding: Encoding,
    fp: u64,
    shared: Object,
) -> Result<u64, Problem> {
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
        // Not a file URL, or one no local path decodes from.
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
