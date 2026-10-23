//! The variables of a JavaScript frame, as V8 keeps them at any stop: its
//! receiver and the arguments its caller passed, by position, on the
//! stack above its frame pointer; the variables its closures capture, by
//! name, in its context chain, each context named by its `ScopeInfo`; and
//! the interpreter's registers, which hold its other locals, by number.
//!
//! V8 names a variable no closure captures only in its source, which only
//! its parser reads (F11), so a register is `r0`, `r1`, and so on. An
//! optimized frame keeps its locals where its translation says, which is
//! not read here, so it has no registers.

use std::collections::BTreeSet;

use super::objects::{Object, Problem, Reader};
use crate::image::v8::{Constant, Field, ScopeInfoSlice, scope_info_slice};

/// The most contexts a chain is followed through.
const MAX_CONTEXTS: usize = 256;

/// The most arguments or registers of a frame read.
const MAX_SLOTS: i64 = 1 << 16;

/// Where a frame's variable is, and which kind it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Kind {
    /// `this`.
    Receiver,
    /// The argument at this position.
    Argument(u32),
    /// A variable of a context, by its name: a function's, a block's, or a
    /// catch clause's that a closure captures.
    Closure,
    /// A top-level `let`, `const`, or `class` of a script.
    Script,
    /// An interpreter register, by number.
    Register(u32),
}

/// One variable of a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Variable {
    pub(super) name: String,
    pub(super) kind: Kind,

    /// Its tagged word, or why it cannot be read. The hole is a `let` or
    /// `const` not yet initialized.
    pub(super) word: Result<u64, Problem>,
}

/// The variables of the JavaScript frame whose frame pointer is `fp`, in
/// the order a debugger shows them: the receiver, the arguments, the
/// context's variables from the innermost scope out, each name shown once
/// as the innermost scope binds it, and the registers.
pub(super) fn variables(reader: &Reader<'_>, fp: u64) -> Result<Vec<Variable>, Problem> {
    let function = super::frames::javascript_function(reader, fp)?
        .ok_or("the frame runs no JavaScript function")?;
    let at = |constant| -> Result<u64, Problem> {
        Ok(fp.wrapping_add_signed(reader.constant(constant)?))
    };
    let mut found = Vec::new();

    // The argument count includes the receiver, kept untagged.
    let argc = reader
        .word(at(Constant::StandardFrameArgCOffset)?)?
        .cast_signed()
        - reader.constant(Constant::JSArgcReceiverSlots)?;
    if !(0..MAX_SLOTS).contains(&argc) {
        return Err(format!("the frame's argument count is {argc}").into());
    }
    let first = at(Constant::UnoptimizedFrameFirstParamFromFp)?;
    found.push(Variable {
        name: "this".to_owned(),
        kind: Kind::Receiver,
        word: reader.word(first),
    });
    for index in 0..u32::try_from(argc).unwrap_or(0) {
        found.push(Variable {
            name: format!("arguments[{index}]"),
            kind: Kind::Argument(index),
            word: reader.word(first + 8 * (u64::from(index) + 1)),
        });
    }

    let context = reader.word(at(Constant::OffFpContext)?)?;
    let context = reader
        .object(context)?
        .ok_or("the frame's context is no object")?;
    let mut seen = BTreeSet::new();
    found.extend(context_variables(reader, context, &mut seen)?);

    if let Some(count) = registers(reader, fp, function)? {
        let file = at(Constant::UnoptimizedFrameRegisterFileFromFp)?;
        for index in 0..count {
            found.push(Variable {
                name: format!("r{index}"),
                kind: Kind::Register(index),
                word: reader.word(file - 8 * u64::from(index)),
            });
        }
    }
    Ok(found)
}

/// How many registers the frame at `fp`, which runs `function`, keeps:
/// `None` for a frame that keeps no register file, an optimized one,
/// whose bytecode slot holds no bytecode of its function.
fn registers(reader: &Reader<'_>, fp: u64, function: Object) -> Result<Option<u32>, Problem> {
    let slot = fp.wrapping_add_signed(reader.constant(Constant::OffFpBytecodeArray)?);
    let Some(bytecode) = reader.object(reader.word(slot)?)? else {
        return Ok(None);
    };
    if !reader.is(bytecode, Constant::BytecodeArrayType)? {
        return Ok(None);
    }
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    if super::names::bytecode(reader, shared)? != Some(bytecode) {
        return Ok(None);
    }
    let size = reader.int32_field(bytecode, Field::BytecodeArrayFrameSize)?;
    let count = i64::from(size) / 8;
    if !(0..MAX_SLOTS).contains(&count) {
        return Err(format!("the frame keeps {count} registers").into());
    }
    Ok(u32::try_from(count).ok())
}

/// The variables of `context` and the contexts around it, up to the
/// native context, each name shown once, as the innermost scope binds it.
fn context_variables(
    reader: &Reader<'_>,
    mut context: Object,
    seen: &mut BTreeSet<String>,
) -> Result<Vec<Variable>, Problem> {
    let mut found = Vec::new();
    for _ in 0..MAX_CONTEXTS {
        let ty = reader.instance_type(context)?;
        if ty == reader.constant(Constant::NativeContextType)? {
            return Ok(found);
        }
        let first = reader.constant(Constant::FirstContextType)?;
        let last = reader.constant(Constant::LastContextType)?;
        if !(first..=last).contains(&ty) {
            return Err(format!("{:#x}, of instance type {ty}, is no context", context.0).into());
        }
        let kind = if ty == reader.constant(Constant::ScriptContextType)? {
            Kind::Script
        } else {
            Kind::Closure
        };
        for (name, word) in locals(reader, context)? {
            if seen.insert(name.clone()) {
                found.push(Variable {
                    name,
                    kind: kind.clone(),
                    word,
                });
            }
        }
        let previous = slot(reader, context, Constant::ContextPreviousIndex)?;
        context = reader
            .object(previous)?
            .ok_or("a context's previous context is no object")?;
    }
    Err("the frame's contexts are too many".into())
}

/// The tagged word in the slot `index` of `context`.
fn slot(reader: &Reader<'_>, context: Object, index: Constant) -> Result<u64, Problem> {
    let index = u64::try_from(reader.constant(index)?).map_err(|_| "a negative context slot")?;
    reader.word(reader.field_address(context, Field::ContextElements)? + 8 * index)
}

/// A context local's name and its tagged word, or why it cannot be read.
pub(super) type Local = (String, Result<u64, Problem>);

/// Each local `context` keeps, as its scope info names it, with its word;
/// those V8 makes for itself, whose names begin with `.`, are left out, as
/// V8's own debugger leaves them out.
pub(super) fn locals(reader: &Reader<'_>, context: Object) -> Result<Vec<Local>, Problem> {
    let scope = slot(reader, context, Constant::ContextScopeInfoIndex)?;
    let scope = reader
        .object(scope)?
        .filter(|scope| reader.is(*scope, Constant::ScopeInfoType).unwrap_or(false))
        .ok_or("a context's scope info is no ScopeInfo")?;
    let count = reader.smi_field(scope, Field::ScopeInfoContextLocalCount)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    let start = scope.0 - reader.constant(Constant::HeapObjectTag)?.cast_unsigned();
    let names = scope_info_slice(
        reader.version,
        ScopeInfoSlice::ContextLocalNames,
        &mut |offset| reader.word(start + offset).ok(),
    )?
    .filter(|names| i64::try_from(names.length).is_ok_and(|length| length == count))
    .ok_or_else(|| {
        format!(
            "the scope of {count} context locals keeps their names in a hash table, which \
             microscope does not yet read"
        )
    })?;

    let flags = super::names::scope_flags(reader, scope)?;
    let extension = flags & reader.constant(Constant::ScopeHasContextExtensionSlotMask)? != 0;
    let first = reader.constant(if extension {
        Constant::ContextMinContextExtendedSlots
    } else {
        Constant::ContextMinContextSlots
    })?;
    let first = u64::try_from(first).map_err(|_| "a negative context slot")?;
    let elements = reader.field_address(context, Field::ContextElements)?;

    let mut found = Vec::new();
    for index in 0..names.length {
        let name = reader
            .object(reader.word(start + names.offset + 8 * index)?)?
            .ok_or("a context local's name is no string")?;
        let name = reader.string(name)?;
        if name.is_empty() || name.starts_with('.') || name == "this" {
            continue;
        }
        found.push((name, reader.word(elements + 8 * (first + index))));
    }
    Ok(found)
}
