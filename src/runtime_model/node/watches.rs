//! Watches of a JavaScript frame's variables. V8 keeps a variable no
//! closure captures in a slot of its frame, an interpreter register or an
//! argument, and a captured one in a slot of a context that the frame's
//! chain of contexts reaches. A collection moves contexts, young or old,
//! and rewrites each slot that names a moved object, so a watch is placed
//! by how its frame reaches its word: the frame's own slot, or how many
//! contexts out along the chain, and how far into that context.

use std::sync::Arc;

use super::objects::{Object, Problem, Reader};
use super::variables::{self, Kind};
use crate::VirtualAddress;
use crate::image::v8::{Constant, Field};
use crate::runtime_model::{InterpretedFrameRef, InterpretedPlace, InterpretedWatch};

/// Where the variable at `slot` of the frame `frame` keeps its word, to
/// watch, and the function the frame runs; or why it is not watched.
pub(super) fn watch(
    reader: &Reader<'_>,
    frame: InterpretedFrameRef,
    slot: u32,
) -> Result<Result<InterpretedWatch, Arc<str>>, Problem> {
    let variable = variables::variables(reader, frame)?
        .into_iter()
        .nth(usize::try_from(slot).unwrap_or(usize::MAX))
        .ok_or("the frame no longer has the variable")?;
    let refused = |reason: &str| Ok(Err(format!("{} {reason}", variable.name).into()));
    match variable.kind {
        Kind::Receiver => return refused("is the call's receiver, which JavaScript never changes"),
        Kind::Global => return refused("is the global object, which JavaScript never changes"),
        Kind::Optimized => {
            return refused("is kept by optimized code where microscope cannot watch it");
        }
        // Optimized code keeps a parameter it changes in a register, not
        // in the slot the call passed it in.
        Kind::Argument(_) if !frame.live_registers => {
            return refused(
                "is a parameter that optimized code keeps where microscope cannot watch it",
            );
        }
        Kind::Module if variable.at.is_none() => {
            return refused(
                "is a script's top-level binding, which V8 keeps in its realm's table of script \
                 scopes, which microscope does not watch",
            );
        }
        _ => {}
    }
    if let Err(problem) = &variable.word
        && **problem == *variables::WITH_CONTEXT
    {
        return refused("is found through a `with` statement's object, which is not watched");
    }
    let Some(at) = variable.at else {
        return refused("is kept where microscope cannot find it");
    };
    let fp = frame.address.get();
    let function = super::frames::javascript_function(reader, fp)?
        .ok_or("the frame runs no JavaScript function")?;
    // A generator's or async function's frame leaves the stack at each
    // `yield` or `await`, its slots kept in its object until it resumes.
    if matches!(
        variable.kind,
        Kind::Argument(_) | Kind::Named(_) | Kind::Register { .. }
    ) && variables::is_resumable(reader, function)?
    {
        return refused(
            "is kept in a generator's or async function's object between the runs of its \
             frame, which are not watched",
        );
    }
    // A collection finds the word again by its place, so a word whose
    // place cannot be found is not watched.
    let Some(found) = place(reader, fp, at)? else {
        return refused("is in a context its frame's chain no longer reaches");
    };
    if let InterpretedPlace::Scope { depth, .. } = found
        && let Ok(word) = &variable.word
        && in_cell(reader, context_at(reader, fp, depth)?, *word)?
    {
        return refused(
            "is kept, or is to be kept, in a context cell, whose fields V8 moves its value \
             between as its type changes, which microscope does not watch",
        );
    }
    Ok(Ok(InterpretedWatch {
        address: VirtualAddress::new(at),
        byte_size: 8,
        code: VirtualAddress::new(function.0),
    }))
}

/// Whether the frame at `fp` still runs `function` and reaches the
/// watched word at `watched`. That the frame has not returned, nor been
/// replaced by a new one, the debugger proves from the thread's stack
/// ([`crate::runtime_model::RuntimeModel::stack_frame_builders`]).
pub(super) fn runs(
    reader: &Reader<'_>,
    fp: u64,
    function: u64,
    watched: u64,
) -> Result<bool, Problem> {
    if super::frames::javascript_function(reader, fp)?.map(|running| running.0) != Some(function) {
        return Ok(false);
    }
    Ok(place(reader, fp, watched)?.is_some())
}

/// How the frame at `fp` reaches the word at `watched`: through a context
/// of its chain, or as one of its own slots that its code keeps current.
/// `None` when it reaches it no more, as when the word's block ended and
/// popped its context, or optimized code replaced the frame's.
pub(super) fn place(
    reader: &Reader<'_>,
    fp: u64,
    watched: u64,
) -> Result<Option<InterpretedPlace>, Problem> {
    let mut context = current_context(reader, fp)?;
    for depth in 0..variables::MAX_CONTEXTS {
        let Some(object) = context else {
            break;
        };
        if let Some(offset) = offset_in(reader, object, watched)? {
            let depth = u32::try_from(depth).map_err(|_| "too many contexts")?;
            return Ok(Some(InterpretedPlace::Scope { depth, offset }));
        }
        context = outer(reader, object)?;
    }
    Ok(frame_slot(reader, fp, watched)?
        .then(|| InterpretedPlace::Frame(VirtualAddress::new(watched))))
}

/// Where the frame at `fp` keeps the word it reaches by `place`, with the
/// function it runs, after a collection.
pub(super) fn placed(
    reader: &Reader<'_>,
    fp: u64,
    place: InterpretedPlace,
) -> Result<InterpretedWatch, Problem> {
    let function = super::frames::javascript_function(reader, fp)?
        .ok_or("the frame runs no JavaScript function")?;
    let address = match place {
        InterpretedPlace::Frame(address) => address.get(),
        InterpretedPlace::Scope { depth, offset } => {
            let context = context_at(reader, fp, depth)?;
            let address = untagged(reader, context)? + offset;
            if offset_in(reader, context, address)? != Some(offset) {
                return Err(
                    format!("the context {:#x} has no slot {offset} bytes in", context.0).into(),
                );
            }
            address
        }
    };
    Ok(InterpretedWatch {
        address: VirtualAddress::new(address),
        byte_size: 8,
        code: VirtualAddress::new(function.0),
    })
}

/// The context `depth` links out along the chain of the frame at `fp`.
fn context_at(reader: &Reader<'_>, fp: u64, depth: u32) -> Result<Object, Problem> {
    let mut context = current_context(reader, fp)?;
    for _ in 0..depth {
        context = match context {
            Some(object) => outer(reader, object)?,
            None => None,
        };
    }
    context.ok_or_else(|| "the frame's chain of contexts is shorter".into())
}

/// Whether the slot of `context` holding `word` keeps its value in a
/// context cell (V8 14.6), or will once initialized: a cell holds a number
/// in one field or another as the number's type changes, and a slot holds
/// the cell until a value of another type replaces it
/// (`Context::Set`, `src/objects/contexts.cc`).
fn in_cell(reader: &Reader<'_>, context: Object, word: u64) -> Result<bool, Problem> {
    // A version without cells names no cell type.
    let Ok(cell) = reader.constant(Constant::ContextCellType) else {
        return Ok(false);
    };
    if let Some(object) = reader.object(word)?
        && reader.instance_type(object)? == cell
    {
        return Ok(true);
    }
    if !super::own::is_hole(reader, word)? {
        return Ok(false);
    }
    let scope = variables::slot(reader, context, Constant::ContextScopeInfoIndex)?;
    let scope = reader
        .object(scope)?
        .ok_or("a context's scope info is not an object")?;
    let flags = super::names::scope_flags(reader, scope)?;
    Ok(flags & reader.constant(Constant::ScopeHasContextCellsMask)? != 0)
}

/// The context the frame at `fp` runs in, which begins its chain.
fn current_context(reader: &Reader<'_>, fp: u64) -> Result<Option<Object>, Problem> {
    let slot = fp.wrapping_add_signed(reader.constant(Constant::OffFpContext)?);
    let context = reader
        .object(reader.word(slot)?)?
        .ok_or("the frame's context is not an object")?;
    Ok(is_scope(reader, context)?.then_some(context))
}

/// The context outside `context`, `None` past the last a function's
/// variables are in.
fn outer(reader: &Reader<'_>, context: Object) -> Result<Option<Object>, Problem> {
    let previous = variables::slot(reader, context, Constant::ContextPreviousIndex)?;
    let previous = reader
        .object(previous)?
        .ok_or("a context's previous context is not an object")?;
    Ok(is_scope(reader, previous)?.then_some(previous))
}

/// Whether `object` is a context that holds variables: any but the
/// native context, which ends a chain.
fn is_scope(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    let ty = reader.instance_type(object)?;
    if ty == reader.constant(Constant::NativeContextType)? {
        return Ok(false);
    }
    let first = reader.constant(Constant::FirstContextType)?;
    let last = reader.constant(Constant::LastContextType)?;
    if !(first..=last).contains(&ty) {
        return Err(format!("{:#x}, of instance type {ty}, is not a context", object.0).into());
    }
    Ok(true)
}

/// How far into `context` its slot at `address` is, if it has one there.
fn offset_in(reader: &Reader<'_>, context: Object, address: u64) -> Result<Option<u64>, Problem> {
    let elements = reader.field_address(context, Field::ContextElements)?;
    let length = reader.smi_field(context, Field::ContextLength)?;
    let length = u64::try_from(length).map_err(|_| "a context's length is negative")?;
    let end = elements
        .checked_add(
            length
                .checked_mul(8)
                .ok_or("a context's length overflows")?,
        )
        .ok_or("a context's end overflows")?;
    if !(elements..end).contains(&address) || !(address - elements).is_multiple_of(8) {
        return Ok(None);
    }
    Ok(Some(address - untagged(reader, context)?))
}

/// Where `object` begins.
fn untagged(reader: &Reader<'_>, object: Object) -> Result<u64, Problem> {
    Ok(object.0 - reader.constant(Constant::HeapObjectTag)?.cast_unsigned())
}

/// Whether `address` is an argument slot of the frame at `fp`, or one of
/// its interpreter registers while its code keeps them, as the
/// interpreter's and Sparkplug's do, and optimized code does not.
fn frame_slot(reader: &Reader<'_>, fp: u64, address: u64) -> Result<bool, Problem> {
    let at = |constant| -> Result<u64, Problem> {
        Ok(fp.wrapping_add_signed(reader.constant(constant)?))
    };
    let function = super::frames::javascript_function(reader, fp)?
        .ok_or("the frame runs no JavaScript function")?;
    if let Some(count) = variables::registers(reader, fp, function)? {
        let file = at(Constant::UnoptimizedFrameRegisterFileFromFp)?;
        let lowest = file - 8 * u64::from(count.saturating_sub(1));
        if count > 0 && (lowest..=file).contains(&address) && (file - address).is_multiple_of(8) {
            return Ok(true);
        }
        // The arguments: as many as the call passed, or as the function
        // declares, whose slots V8 fills with undefined.
        let argc = reader
            .word(at(Constant::StandardFrameArgCOffset)?)?
            .cast_signed()
            - reader.constant(Constant::JSArgcReceiverSlots)?;
        let declared = i64::try_from(variables::parameters(reader, function)?)
            .map_err(|_| "too many parameters")?;
        let count = u64::try_from(argc.max(declared)).unwrap_or(0);
        let receiver = at(Constant::UnoptimizedFrameFirstParamFromFp)?;
        if address > receiver
            && address <= receiver + 8 * count
            && (address - receiver).is_multiple_of(8)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
