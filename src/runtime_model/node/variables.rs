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
    /// A variable of a context the frame's function made, by its name: its
    /// function's, a block's, or a catch clause's, that a closure captures.
    Local,
    /// A variable of a context around the function, which its closure
    /// captured where it was made.
    Captured,
    /// A top-level binding of a module, or a top-level `let`, `const`, or
    /// `class` of any script.
    Module,
    /// An interpreter register, by number.
    Register(u32),
    /// `globalThis`, the realm's global proxy.
    Global,
}

impl Kind {
    /// The kind the debugger shows the variable as.
    pub(super) const fn shown(&self) -> crate::VariableKind {
        match self {
            Self::Receiver | Self::Argument(_) => crate::VariableKind::Parameter,
            Self::Local => crate::VariableKind::Local,
            Self::Captured => crate::VariableKind::Captured,
            Self::Module => crate::VariableKind::Module,
            Self::Register(_) => crate::VariableKind::Unnamed,
            Self::Global => crate::VariableKind::Global,
        }
    }
}

/// Why an interpreter register has no name.
pub(super) const UNNAMED: &str = "V8 names only the locals a closure captures; the \
    rest are interpreter registers, which only the function's source names";

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
    let context = reader.word(at(Constant::OffFpContext)?)?;
    let context = reader
        .object(context)?
        .ok_or("the frame's context is no object")?;
    // An arrow function's `this` is the one where it was defined, which V8
    // keeps in a context as the local `this` where the arrow uses it; the
    // receiver its caller passed is not it.
    let this = if is_arrow(reader, function)? {
        lexical_this(reader, context)?.unwrap_or_else(|| {
            Err(
                "an arrow function's `this` is its definer's, which no context kept: \
                 the arrow does not use it"
                    .into(),
            )
        })
    } else {
        reader.word(first)
    };
    found.push(Variable {
        name: "this".to_owned(),
        kind: Kind::Receiver,
        word: this,
    });
    for index in 0..u32::try_from(argc).unwrap_or(0) {
        found.push(Variable {
            name: format!("arguments[{index}]"),
            kind: Kind::Argument(index),
            word: reader.word(first + 8 * (u64::from(index) + 1)),
        });
    }

    let mut seen = BTreeSet::new();
    let closure = reader.field(function, Field::JSFunctionContext)?;
    found.extend(context_variables(reader, context, closure, &mut seen)?);

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

    let native = super::access::native_context(reader, context)?;
    for context in super::access::script_contexts(reader, native)? {
        for (name, word) in locals(reader, context)? {
            if seen.insert(name.clone()) {
                found.push(Variable {
                    name,
                    kind: Kind::Module,
                    word,
                });
            }
        }
    }
    found.push(Variable {
        name: "globalThis".to_owned(),
        kind: Kind::Global,
        word: super::access::slot(reader, native, Constant::ContextGlobalProxyIndex),
    });
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
/// native context, each name shown once, as the innermost scope binds it:
/// the frame's own up to `closure`, the context its function was made in,
/// and captured from there out.
fn context_variables(
    reader: &Reader<'_>,
    mut context: Object,
    closure: u64,
    seen: &mut BTreeSet<String>,
) -> Result<Vec<Variable>, Problem> {
    let mut found = Vec::new();
    let mut own = true;
    for _ in 0..MAX_CONTEXTS {
        own &= context.0 != closure;
        let ty = reader.instance_type(context)?;
        if ty == reader.constant(Constant::NativeContextType)? {
            return Ok(found);
        }
        let first = reader.constant(Constant::FirstContextType)?;
        let last = reader.constant(Constant::LastContextType)?;
        if !(first..=last).contains(&ty) {
            return Err(format!("{:#x}, of instance type {ty}, is no context", context.0).into());
        }
        let kind = if ty == reader.constant(Constant::ScriptContextType)?
            || ty == reader.constant(Constant::ModuleContextType)?
        {
            Kind::Module
        } else if own {
            Kind::Local
        } else {
            Kind::Captured
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

/// Whether `function` is an arrow function, as V8's `IsArrowFunction`
/// says of its kind.
fn is_arrow(reader: &Reader<'_>, function: Object) -> Result<bool, Problem> {
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    let flags = i64::from(reader.int32_field(shared, Field::SharedFunctionInfoFlags)?);
    let kind = (flags & reader.constant(Constant::SfiFunctionKindMask)?)
        >> reader.constant(Constant::SfiFunctionKindShift)?;
    Ok((reader.constant(Constant::FunctionKindArrowFunction)?
        ..=reader.constant(Constant::FunctionKindAsyncArrowFunction)?)
        .contains(&kind))
}

/// The `this` the innermost context around `context`, or `context`
/// itself, keeps as a local, if any does.
fn lexical_this(
    reader: &Reader<'_>,
    mut context: Object,
) -> Result<Option<Result<u64, Problem>>, Problem> {
    for _ in 0..MAX_CONTEXTS {
        let ty = reader.instance_type(context)?;
        if ty == reader.constant(Constant::NativeContextType)? {
            return Ok(None);
        }
        if let Some((_, word)) = all_locals(reader, context)?
            .into_iter()
            .find(|(name, _)| name == "this")
        {
            return Ok(Some(word));
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
/// V8's own debugger leaves them out, and so is `this`, which is no
/// variable a name binds.
pub(super) fn locals(reader: &Reader<'_>, context: Object) -> Result<Vec<Local>, Problem> {
    let mut found = all_locals(reader, context)?;
    found.retain(|(name, _)| name != "this");
    Ok(found)
}

/// As [`locals`], with `this` where the context keeps it.
fn all_locals(reader: &Reader<'_>, context: Object) -> Result<Vec<Local>, Problem> {
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
    let names = local_names(reader, scope, start, count)?;

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
    for (index, name) in names {
        let name = reader
            .object(name)?
            .ok_or("a context local's name is no string")?;
        let name = reader.string(name)?;
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        found.push((name, reader.word(elements + 8 * (first + index))));
    }
    Ok(found)
}

/// The tagged name of each of the `count` context locals of `scope`,
/// whose untagged start is `start`, with its index, in index order: from
/// the scope's own slots, or, past V8's
/// `kScopeInfoMaxInlinedLocalNamesSize`, from the `NameToIndexHashTable`
/// it keeps instead, each name's value there being its index.
fn local_names(
    reader: &Reader<'_>,
    scope: Object,
    start: u64,
    count: i64,
) -> Result<Vec<(u64, u64)>, Problem> {
    let mut read = |offset| reader.word(start + offset).ok();
    if let Some(names) =
        scope_info_slice(reader.version, ScopeInfoSlice::ContextLocalNames, &mut read)?
            .filter(|names| i64::try_from(names.length).is_ok_and(|length| length == count))
    {
        return (0..names.length)
            .map(|index| Ok((index, reader.word(start + names.offset + 8 * index)?)))
            .collect();
    }

    let missing = || {
        format!(
            "the scope {:#x} of {count} context locals keeps no names",
            scope.0
        )
    };
    let table = scope_info_slice(
        reader.version,
        ScopeInfoSlice::ContextLocalNamesHashtable,
        &mut read,
    )?
    .filter(|table| table.length == 1)
    .ok_or_else(missing)?;
    let table = reader
        .object(reader.word(start + table.offset)?)?
        .filter(|table| {
            reader
                .is(*table, Constant::NameToIndexHashTableType)
                .unwrap_or(false)
        })
        .ok_or_else(missing)?;
    let length = reader.smi_field(table, Field::FixedArrayBaseLength)?;
    let objects = reader.field_address(table, Field::FixedArrayObjects)?;
    let word = |index: i64| -> Result<u64, Problem> {
        if !(0..length).contains(&index) {
            return Err(format!("an entry past the hash table {:#x}", table.0).into());
        }
        reader.word(objects + 8 * index.cast_unsigned())
    };
    let capacity = reader
        .smi(word(reader.constant(Constant::HashTableCapacityIndex)?)?)?
        .filter(|capacity| (0..=MAX_SLOTS).contains(capacity))
        .ok_or_else(|| format!("the hash table {:#x} has no capacity", table.0))?;
    let first = reader.constant(Constant::HashTablePrefixStartIndex)?
        + reader.constant(Constant::NameToIndexPrefixSize)?;
    let size = reader.constant(Constant::NameToIndexEntrySize)?;
    let key = reader.constant(Constant::HashTableEntryKeyIndex)?;
    let value = reader.constant(Constant::NameToIndexEntryValueIndex)?;

    let mut names = Vec::new();
    for entry in 0..capacity {
        let at = first + entry * size;
        let name = word(at + key)?;
        // An empty or deleted entry's key is an oddball, no name.
        let Some(object) = reader.object(name)? else {
            continue;
        };
        if reader.instance_type(object)? >= reader.constant(Constant::FirstNonstringType)? {
            continue;
        }
        let index = reader
            .smi(word(at + value)?)?
            .filter(|index| (0..count).contains(index))
            .ok_or_else(|| format!("a local of the hash table {:#x} has no index", table.0))?;
        names.push((index.cast_unsigned(), name));
    }
    names.sort_unstable();
    if i64::try_from(names.len()).ok() != Some(count) {
        return Err(format!(
            "the hash table {:#x} names {} of {count} context locals",
            table.0,
            names.len()
        )
        .into());
    }
    Ok(names)
}
