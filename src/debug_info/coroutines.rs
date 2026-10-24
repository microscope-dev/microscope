//! The state machines compilers generate for code that can suspend, read
//! once from the normalized types into [`CoroutineInfo`].
//!
//! rustc describes the future of an `async fn` or `async` block as a type
//! named `{async_fn_env#N}`, `{async_block_env#N}`, or
//! `{async_closure_env#N}`: a variant part keyed on an artificial
//! `__state` member, each of whose variants holds one member, named by its
//! number, of a record named for the state. `Unresumed` holds the captures
//! alone; `Returned` and `Panicked` hold them too; `SuspendN` holds the
//! variables live across the `N`th await, the future it awaits in
//! `__awaitee`, and the captures, which come last. Each state's member
//! carries the source line the state is at: the function's header before
//! it starts, an await's line, or the closing brace once it has finished.
//!
//! A type that is named as a coroutine but laid out otherwise is refused
//! with the reason, never read by guess.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{
    CoroutineInfo, CoroutineKind, CoroutineState, CoroutineStateKind, IntegerValue,
    RecordMemberLayout, StateMember, TypeId, TypeInfo, TypeKind, TypeNode, VariantDiscriminant,
    VariantSelection, VariantSelector,
};

/// What kind of coroutine a type's name says it is, for rustc's names.
pub fn coroutine_kind(name: &str) -> Option<CoroutineKind> {
    let name = without_arguments(name)?;
    let name = name.rsplit("::").next().unwrap_or(name);
    let numbered = |prefix: &str| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix('}'))
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
    };

    if numbered("{async_fn_env#") {
        Some(CoroutineKind::AsyncFunction)
    } else if numbered("{async_block_env#") {
        Some(CoroutineKind::AsyncBlock)
    } else if numbered("{async_closure_env#") {
        Some(CoroutineKind::AsyncClosure)
    } else {
        None
    }
}

/// Every record its name says is a coroutine, with what it is or why its
/// layout cannot be read as one. A pointer to one, whose name ends as the
/// coroutine's does, is not one.
pub fn normalize(types: &[TypeNode]) -> BTreeMap<TypeId, Result<CoroutineInfo, Arc<str>>> {
    let by_id = |id: TypeId| match types.get(id.index()) {
        Some(TypeNode::Resolved(info)) => Some(info),
        _ => None,
    };

    found(types.iter().map(|node| node.reference().id), &by_id)
}

/// Every coroutine of `table`, as [`normalize`] finds them, decoding only
/// the types named as coroutines and those their states hold.
pub fn in_table(
    table: &crate::image::types::TypeTable,
) -> BTreeMap<TypeId, Result<CoroutineInfo, Arc<str>>> {
    let by_id = |id: TypeId| {
        table.info(crate::TypeReference {
            image: table.image(),
            id,
        })
    };

    found(
        table
            .view()
            .aggregates_named(|name| coroutine_kind(name).is_some()),
        &by_id,
    )
}

/// The records and variants among `ids` that their names say are
/// coroutines, with what each is.
fn found<'a>(
    ids: impl Iterator<Item = TypeId>,
    types: &impl Fn(TypeId) -> Option<&'a TypeInfo>,
) -> BTreeMap<TypeId, Result<CoroutineInfo, Arc<str>>> {
    ids.filter_map(|id| {
        let info = types(id)?;
        if !matches!(
            info.kind,
            TypeKind::Record { .. } | TypeKind::Variant { .. }
        ) {
            return None;
        }

        let kind = coroutine_kind(&info.name)?;
        Some((id, coroutine(info, kind, types)))
    })
    .collect()
}

fn coroutine<'a>(
    info: &TypeInfo,
    kind: CoroutineKind,
    types: &impl Fn(TypeId) -> Option<&'a TypeInfo>,
) -> Result<CoroutineInfo, Arc<str>> {
    let TypeKind::Variant {
        discriminant,
        variants,
        incomplete: false,
        ..
    } = &info.kind
    else {
        return Err("the coroutine has no variant part".into());
    };

    let VariantDiscriminant::Stored(state) = discriminant.as_ref() else {
        return Err("the coroutine stores no state number".into());
    };

    if state.name.as_deref() != Some("__state") || !state.artificial {
        return Err("the coroutine's discriminant is not its `__state`".into());
    }

    let RecordMemberLayout::ByteOffset(offset) = state.layout else {
        return Err("the coroutine's state number has no byte offset".into());
    };

    let size = types(state.type_ref.id)
        .and_then(|ty| ty.byte_size)
        .filter(|size| (1..=8).contains(size))
        .ok_or("the coroutine's state number has no integer size")?;

    let mut states = variants
        .iter()
        .map(|variant| read_state(variant, types))
        .collect::<Result<Vec<_>, Arc<str>>>()?;

    // `Unresumed` holds the captures alone; every other state ends with
    // them, by name.
    let unresumed = states
        .iter_mut()
        .find(|state| state.kind == CoroutineStateKind::Unresumed)
        .ok_or("the coroutine has no `Unresumed` state")?;
    let captures = std::mem::replace(&mut unresumed.saved, Arc::from([]));

    for state in states
        .iter_mut()
        .filter(|state| state.kind != CoroutineStateKind::Unresumed)
    {
        let value = state.value;
        let tail = state
            .saved
            .len()
            .checked_sub(captures.len())
            .ok_or_else(|| format!("state {value} holds fewer members than the captures"))?;
        if state.saved[tail..]
            .iter()
            .zip(captures.iter())
            .any(|(held, captured)| held.name != captured.name)
        {
            return Err(format!("state {value} does not end with the captures").into());
        }

        state.saved = state.saved[..tail].into();
    }

    let mut numbers = states.iter().map(|state| state.value).collect::<Vec<_>>();
    numbers.sort_unstable();
    numbers.dedup();
    if numbers.len() != states.len() {
        return Err("two states share a number".into());
    }

    let mut suspended = states
        .iter()
        .filter_map(|state| match state.kind {
            CoroutineStateKind::Suspended { index } => Some(index),
            _ => None,
        })
        .collect::<Vec<_>>();
    suspended.sort_unstable();
    if suspended
        .iter()
        .enumerate()
        .any(|(at, index)| u32::try_from(at) != Ok(*index))
    {
        return Err("the suspended states are not numbered in order".into());
    }

    Ok(CoroutineInfo {
        kind,
        state: StateMember { offset, size },
        states: states.into(),
        captures,
    })
}

/// The coroutine a `Pin<&mut C>` parameter points to, as the body of an
/// `async fn` is passed its future.
pub fn pinned_coroutine(types: &[TypeNode], ty: TypeId) -> Option<TypeId> {
    let resolved = |id: TypeId| match types.get(id.index()) {
        Some(TypeNode::Resolved(info)) => Some(info),
        _ => None,
    };

    let pin = resolved(ty)?;
    if !pin.name.starts_with("Pin<") {
        return None;
    }

    let TypeKind::Record { members, .. } = &pin.kind else {
        return None;
    };

    let [pointer] = members.as_ref() else {
        return None;
    };

    let target = match &resolved(pointer.type_ref.id)?.kind {
        TypeKind::Pointer {
            target: Some(target),
            ..
        }
        | TypeKind::Reference { target, .. } => *target,
        _ => return None,
    };

    coroutine_kind(&resolved(target.id)?.name).map(|_| target.id)
}

/// One state, with every member it holds, laid out from the coroutine's
/// start.
fn read_state<'a>(
    variant: &crate::Variant,
    types: &impl Fn(TypeId) -> Option<&'a TypeInfo>,
) -> Result<CoroutineState, Arc<str>> {
    let value = match &variant.selection {
        VariantSelection::Selectors(selectors) => match selectors.as_ref() {
            [VariantSelector::Value(IntegerValue::Unsigned(value))] => {
                u64::try_from(*value).map_err(|_| "a state number is too large")?
            }
            [VariantSelector::Value(IntegerValue::Signed(value))] => {
                u64::try_from(*value).map_err(|_| "a state number is negative")?
            }
            _ => return Err("a state is not selected by one number".into()),
        },
        VariantSelection::Default => return Err("a state is the default variant".into()),
    };

    let [member] = variant.members.as_ref() else {
        return Err(format!("state {value} does not hold exactly one member").into());
    };

    let RecordMemberLayout::ByteOffset(base) = member.layout else {
        return Err(format!("state {value} has no byte offset").into());
    };

    let record = types(member.type_ref.id).ok_or("a state's type is unresolved")?;
    let kind = state_kind(&record.name)
        .ok_or_else(|| format!("state {value} is named `{}`", record.name))?;
    let TypeKind::Record { members, .. } = &record.kind else {
        return Err(format!("state {value} is not a record").into());
    };

    let members = members
        .iter()
        .map(|inner| {
            let RecordMemberLayout::ByteOffset(at) = inner.layout else {
                return Err(Arc::from(format!(
                    "a member of state {value} has no byte offset"
                )));
            };

            let mut moved = inner.clone();
            moved.layout = RecordMemberLayout::ByteOffset(
                base.checked_add(at).ok_or("a member's offset overflows")?,
            );
            Ok(moved)
        })
        .collect::<Result<Vec<_>, Arc<str>>>()?;

    Ok(CoroutineState {
        value,
        kind,
        location: member.declaration.clone(),
        saved: members.into(),
    })
}

/// What a state's record name says it is.
fn state_kind(name: &str) -> Option<CoroutineStateKind> {
    match name {
        "Unresumed" => Some(CoroutineStateKind::Unresumed),
        "Returned" => Some(CoroutineStateKind::Returned),
        "Panicked" => Some(CoroutineStateKind::Panicked),
        _ => {
            let index = name.strip_prefix("Suspend")?;
            (!index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
                .then(|| index.parse().ok())
                .flatten()
                .map(|index| CoroutineStateKind::Suspended { index })
        }
    }
}

/// The name a debugger shows for the function that runs a coroutine,
/// which rustc names `{async_fn#N}`, `{async_block#N}`, or
/// `{async_closure#N}` within the namespace of the function that wrote it:
/// an `async fn`'s body is that function, and a block or closure is
/// numbered within it. `namespace` is the enclosing names, outermost
/// first and joined by `::`, and a block within another coroutine's body
/// is named within that body's name.
pub fn body_name(name: &str, namespace: &str) -> Option<Arc<str>> {
    if !name.starts_with("{async_") {
        return None;
    }

    let name = without_arguments(name)?;
    let numbered = |prefix: &str| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix('}'))
            .filter(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
    };

    if namespace.is_empty() {
        return None;
    }

    let (outer, last) = namespace.rsplit_once("::").unwrap_or(("", namespace));
    let enclosing = body_name(last, outer).unwrap_or_else(|| last.into());

    if numbered("{async_fn#").is_some() {
        Some(enclosing)
    } else if let Some(number) = numbered("{async_block#") {
        Some(format!("{enclosing}::{{async block#{number}}}").into())
    } else {
        numbered("{async_closure#")
            .map(|number| format!("{enclosing}::{{async closure#{number}}}").into())
    }
}

/// A name without the generic arguments that end it, as rustc names an
/// instance of a generic function or its coroutine, `{async_fn#0}<u32>`;
/// `None` when the arguments are unbalanced.
pub fn without_arguments(name: &str) -> Option<&str> {
    if !name.ends_with('>') {
        return Some(name);
    }

    let mut depth = 0_usize;

    for (at, character) in name.char_indices().rev() {
        match character {
            // A function type's arrow closes nothing.
            '>' if name[..at].ends_with('-') => {}
            '>' => depth += 1,
            '<' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&name[..at]);
                }
            }
            _ => {}
        }
    }

    None
}

/// The full name of the coroutine type whose future the body named `name`
/// within `namespace` runs, by rustc's naming alone: the body
/// `{async_fn#N}<A>` of a function runs `{async_fn_env#N}<A>`, which rustc
/// declares in the same namespace, and likewise for a block's and a
/// closure's. `None` for a function that is no async body.
///
/// The debug information of an async body says which coroutine it runs
/// only through the type of the unnamed parameter that passes it its
/// future, which rustc first describes in Rust 1.93. A program built by an
/// older Rust has bodies that name no coroutine but by this name, without
/// which none of their resume points, their awaits' steps, or their
/// futures' frames can be found.
pub fn body_future(name: &str, namespace: &str) -> Option<String> {
    // Every Rust function is asked while loading, almost none of them a
    // body, so a name that is none is turned away before any allocation.
    if namespace.is_empty() || !name.starts_with("{async_") {
        return None;
    }

    let base = without_arguments(name)?;
    let arguments = &name[base.len()..];
    let (env, number) = [
        ("{async_fn#", "{async_fn_env#"),
        ("{async_block#", "{async_block_env#"),
        ("{async_closure#", "{async_closure_env#"),
    ]
    .into_iter()
    .find_map(|(body, env)| {
        let number = base.strip_prefix(body)?.strip_suffix('}')?;
        (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())).then_some((env, number))
    })?;
    Some(format!("{namespace}::{env}{number}}}{arguments}"))
}

/// Whether rustc names a function as the body of an `async fn`.
pub fn is_async_fn_body(name: &str) -> bool {
    without_arguments(name)
        .and_then(|name| name.strip_prefix("{async_fn#"))
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_are_named_for_the_functions_that_wrote_them() {
        let namespace = "steps::leaf";
        assert_eq!(
            body_name("{async_fn#0}", namespace).as_deref(),
            Some("leaf")
        );
        assert_eq!(
            body_name("{async_block#2}", namespace).as_deref(),
            Some("leaf::{async block#2}")
        );
        assert_eq!(
            body_name("{async_closure#0}", namespace).as_deref(),
            Some("leaf::{async closure#0}")
        );

        // A block in an async function's body, or in another block, is
        // named within the function that wrote it.
        let within = "steps::leaf::{async_fn#0}";
        assert_eq!(
            body_name("{async_block#1}", within).as_deref(),
            Some("leaf::{async block#1}")
        );
        let within = "steps::leaf::{async_fn#0}::{async_block#1}";
        assert_eq!(
            body_name("{async_block#0}", within).as_deref(),
            Some("leaf::{async block#1}::{async block#0}")
        );
        assert_eq!(body_name("{closure#0}", namespace), None);
        assert_eq!(body_name("{async_fn#}", namespace), None);
        assert_eq!(body_name("{async_fn#0}", ""), None);
        assert_eq!(
            coroutine_kind("{async_block_env#1}"),
            Some(CoroutineKind::AsyncBlock)
        );
        assert_eq!(coroutine_kind("{closure_env#1}"), None);

        // A generic function's names end in its arguments.
        assert_eq!(
            body_name("{async_fn#0}<alloc::sync::Arc<u32>>", namespace).as_deref(),
            Some("leaf")
        );
        assert!(is_async_fn_body("{async_fn#0}<u32>"));
        assert_eq!(
            coroutine_kind("{async_fn_env#0}<usize, a::{impl#0}::b::{closure_env#0}>"),
            Some(CoroutineKind::AsyncFunction)
        );
        assert_eq!(
            coroutine_kind("{async_block_env#3}<fn(u32) -> u32>"),
            Some(CoroutineKind::AsyncBlock)
        );
        assert_eq!(coroutine_kind("{async_fn_env#0}<u32"), None);
    }

    /// Before Rust 1.93 a body names its coroutine only by rustc's naming:
    /// the same number, the same namespace, and the same arguments, with
    /// `_env` added. A body of one kind never names a coroutine of
    /// another, and a number never changes on the way.
    #[test]
    fn a_body_names_its_future_by_rustcs_naming() {
        let namespace = "steps::leaf";
        assert_eq!(
            body_future("{async_fn#0}", namespace).as_deref(),
            Some("steps::leaf::{async_fn_env#0}")
        );
        assert_eq!(
            body_future("{async_block#12}", namespace).as_deref(),
            Some("steps::leaf::{async_block_env#12}")
        );
        assert_eq!(
            body_future("{async_closure#3}", namespace).as_deref(),
            Some("steps::leaf::{async_closure_env#3}")
        );

        // A generic body's arguments are its coroutine's, nested ones too.
        assert_eq!(
            body_future("{async_fn#1}<alloc::sync::Arc<u32>, u8>", "a::b").as_deref(),
            Some("a::b::{async_fn_env#1}<alloc::sync::Arc<u32>, u8>")
        );

        // A block within a body is declared within the body's namespace.
        assert_eq!(
            body_future("{async_block#0}", "steps::leaf::{async_fn#0}").as_deref(),
            Some("steps::leaf::{async_fn#0}::{async_block_env#0}")
        );

        // Neither a closure, a coroutine's own type, a malformed number,
        // unbalanced arguments, nor a body outside any namespace.
        for name in [
            "{closure#0}",
            "{async_fn_env#0}",
            "{async_fn#}",
            "{async_fn#x}",
            "{async_fn#0}<u32",
            "leaf",
        ] {
            assert_eq!(body_future(name, namespace), None, "{name}");
        }
        assert_eq!(body_future("{async_fn#0}", ""), None);
    }
}
