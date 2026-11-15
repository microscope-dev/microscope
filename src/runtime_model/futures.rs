use std::collections::BTreeSet;
use std::sync::Arc;

use super::{RuntimeImage, RuntimeStop, records};
use crate::{
    CoroutineStateKind, RecordMemberLayout, SourceLanguage, SourceLocation, TypeKind,
    TypeReference, VirtualAddress,
};

pub const MAX_DEPTH: usize = 256;

const MAX_WRAPPERS: usize = 16;

const HOLDERS: [(&[&str], &str, &[&str]); 9] = [
    (&["tracing", "instrument"], "Instrumented", &["inner"]),
    (&["tracing", "instrument"], "WithDispatch", &["inner"]),
    (
        &["core", "mem", "manually_drop"],
        "ManuallyDrop",
        &["value"],
    ),
    (
        &["core", "mem", "maybe_dangling"],
        "MaybeDangling",
        &["__0"],
    ),
    (
        &["tokio", "util", "trace"],
        "InstrumentedAsyncOp",
        &["inner"],
    ),
    (
        &["tokio", "task", "task_local"],
        "TaskLocalFuture",
        &["future", "Some", "__0"],
    ),
    (&["futures_util", "future", "future"], "Map", &["inner"]),
    (
        &["futures_util", "future", "future", "map"],
        "Map",
        &["Incomplete", "future"],
    ),
    (
        &["futures_util", "future", "future", "catch_unwind"],
        "CatchUnwind",
        &["future"],
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncFrame {
    pub object: VirtualAddress,
    pub ty: TypeReference,
    pub kind: AsyncFrameKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncFrameKind {
    Coroutine {
        state: u64,
        kind: CoroutineStateKind,
        location: Option<SourceLocation>,
    },

    Leaf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitChain {
    pub frames: Vec<AsyncFrame>,
    pub end: ChainEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainEnd {
    Leaf,

    Unresumed,

    Finished,

    NoAwaitee,

    Broken(Arc<str>),

    TooDeep,

    Cycle,
}

pub fn walk(
    image: &dyn RuntimeImage,
    stop: &dyn RuntimeStop,
    future: VirtualAddress,
    ty: TypeReference,
) -> AwaitChain {
    walk_choosing(image, stop, future, ty, &mut |_, _, _| StateChoice::Stored)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateChoice {
    Stored,

    Chosen(u64),

    Unknown,
}

pub fn walk_choosing(
    image: &dyn RuntimeImage,
    stop: &dyn RuntimeStop,
    future: VirtualAddress,
    ty: TypeReference,
    choose: &mut dyn FnMut(usize, VirtualAddress, &crate::CoroutineInfo) -> StateChoice,
) -> AwaitChain {
    let mut frames = Vec::new();
    let end = follow(image, stop, future, ty, &mut frames, choose);
    frames.reverse();
    AwaitChain { frames, end }
}

fn follow(
    image: &dyn RuntimeImage,
    stop: &dyn RuntimeStop,
    mut object: VirtualAddress,
    mut ty: TypeReference,
    frames: &mut Vec<AsyncFrame>,
    choose: &mut dyn FnMut(usize, VirtualAddress, &crate::CoroutineInfo) -> StateChoice,
) -> ChainEnd {
    let mut seen = BTreeSet::new();
    let mut coroutines = 0;

    for _ in 0..MAX_DEPTH {
        if !seen.insert((object, ty)) {
            return ChainEnd::Cycle;
        }

        let Some((representation, kind)) = representation(image, ty) else {
            return ChainEnd::Broken(format!("type {:?} has no layout", ty.id).into());
        };

        if let Some(coroutine) = image.coroutine(representation) {
            let coroutine = match coroutine {
                Ok(coroutine) => coroutine,
                Err(reason) => return ChainEnd::Broken(Arc::clone(reason)),
            };

            let choice = choose(coroutines, object, coroutine);
            let number = match state_number(stop, object, coroutine, choice) {
                Ok(number) => number,
                Err(end) => return end,
            };
            coroutines += 1;

            let Some(state) = coroutine.state(number) else {
                return ChainEnd::Broken(
                    format!("the future at {object:#x} is in no state, {number}").into(),
                );
            };

            frames.push(AsyncFrame {
                object,
                ty,
                kind: AsyncFrameKind::Coroutine {
                    state: number,
                    kind: state.kind,
                    location: state.location.clone(),
                },
            });

            match state.kind {
                CoroutineStateKind::Unresumed => return ChainEnd::Unresumed,
                CoroutineStateKind::Returned | CoroutineStateKind::Panicked => {
                    return ChainEnd::Finished;
                }
                CoroutineStateKind::Suspended { .. } => {}
            }

            let Some(awaitee) = state.awaitee() else {
                return ChainEnd::NoAwaitee;
            };

            let RecordMemberLayout::ByteOffset(offset) = awaitee.layout else {
                return ChainEnd::Broken("an awaited future is not at a byte offset".into());
            };

            let Some(inner) = object.checked_add(offset) else {
                return ChainEnd::Broken(
                    format!("the future at {object:#x} awaits no address").into(),
                );
            };

            (object, ty) = (inner, awaitee.type_ref);
            continue;
        }

        match kind {
            Shape::Pointer(target) => {
                let Some(pointer) = records::pointer(stop, object) else {
                    return ChainEnd::Broken(
                        format!("the pointer at {object:#x} is unreadable").into(),
                    );
                };

                (object, ty) = (pointer, target);
            }
            Shape::Pinned { pointer, layout } => {
                let RecordMemberLayout::ByteOffset(offset) = layout else {
                    return ChainEnd::Broken("a pinned pointer is not at a byte offset".into());
                };

                let Some(inner) = object.checked_add(offset) else {
                    return past_the_end(object);
                };

                (object, ty) = (inner, pointer);
            }
            Shape::Wrapper { future, offset } => {
                let Some(inner) = object.checked_add(offset) else {
                    return past_the_end(object);
                };

                (object, ty) = (inner, future);
            }
            Shape::Holder(steps) => match held_within(image, stop, object, ty, steps) {
                Ok(held) => (object, ty) = held,
                Err(reason) => return ChainEnd::Broken(reason),
            },
            Shape::TraitObject { data, vtable } => match held(image, stop, object, data, vtable) {
                Ok(held) => (object, ty) = held,
                Err(end) => return end,
            },
            Shape::Other => {
                frames.push(AsyncFrame {
                    object,
                    ty,
                    kind: AsyncFrameKind::Leaf,
                });
                return ChainEnd::Leaf;
            }
        }
    }

    ChainEnd::TooDeep
}

fn past_the_end(object: VirtualAddress) -> ChainEnd {
    ChainEnd::Broken(format!("the future at {object:#x} holds no address").into())
}

fn state_number(
    stop: &dyn RuntimeStop,
    object: VirtualAddress,
    coroutine: &crate::CoroutineInfo,
    choice: StateChoice,
) -> Result<u64, ChainEnd> {
    match choice {
        StateChoice::Chosen(number) => Ok(number),
        StateChoice::Unknown => Err(ChainEnd::Broken(
            format!("the await the future at {object:#x} runs is unknown").into(),
        )),
        StateChoice::Stored => {
            let size = usize::try_from(coroutine.state.size)
                .map_err(|_| ChainEnd::Broken("a coroutine's state is too large".into()))?;
            object
                .checked_add(coroutine.state.offset)
                .and_then(|state| records::read(stop, state, size))
                .ok_or_else(|| {
                    ChainEnd::Broken(format!("the future at {object:#x} is unreadable").into())
                })
        }
    }
}

fn held(
    image: &dyn RuntimeImage,
    stop: &dyn RuntimeStop,
    object: VirtualAddress,
    data: u64,
    vtable: u64,
) -> Result<(VirtualAddress, TypeReference), ChainEnd> {
    let word = |offset| {
        object
            .checked_add(offset)
            .and_then(|at| records::pointer(stop, at))
    };
    let (Some(pointer), Some(table)) = (word(data), word(vtable)) else {
        return Err(ChainEnd::Broken(
            format!("the trait object at {object:#x} is unreadable").into(),
        ));
    };

    let held = stop.load_bias().wrapping_unload(table);
    let Some(held) = image.trait_object_type(held) else {
        return Err(ChainEnd::Broken(
            format!("the vtable at {table:#x} is no future's the program describes").into(),
        ));
    };

    Ok((pointer, held))
}

fn held_within(
    image: &dyn RuntimeImage,
    stop: &dyn RuntimeStop,
    mut object: VirtualAddress,
    mut ty: TypeReference,
    steps: &[&str],
) -> Result<(VirtualAddress, TypeReference), Arc<str>> {
    for step in steps {
        let found = if records::is_sum(image, ty) {
            let sum = records::sum(image, ty)?;
            let active = sum.active(stop, object)?;
            if &*active.name != *step {
                return Err(format!(
                    "the future at {object:#x} holds no future: it is {}",
                    active.name
                )
                .into());
            }

            active.payload
        } else {
            records::field(image, ty, &[step])?
        };

        let held = object.checked_add(found.offset).ok_or_else(|| {
            Arc::<str>::from(format!("the future at {object:#x} holds no address"))
        })?;
        (object, ty) = (held, found.ty);
    }

    Ok((object, ty))
}

enum Shape {
    /// A pointer to the future of the target type.
    Pointer(TypeReference),

    Pinned {
        pointer: TypeReference,
        layout: RecordMemberLayout,
    },

    /// A trait object: where its data pointer and its vtable lie.
    TraitObject { data: u64, vtable: u64 },

    /// A record that holds only a coroutine, of this type at this offset.
    Wrapper { future: TypeReference, offset: u64 },

    /// One of [`HOLDERS`], with the way to its future.
    Holder(&'static [&'static str]),

    /// Anything else, which is a leaf unless it is a coroutine.
    Other,
}

fn representation(
    image: &dyn RuntimeImage,
    mut ty: TypeReference,
) -> Option<(TypeReference, Shape)> {
    for _ in 0..MAX_WRAPPERS {
        let info = image.type_info(ty)?;
        let shape = match &info.kind {
            TypeKind::Named { target, .. } => {
                ty = (*target)?;
                continue;
            }
            TypeKind::Modified { target, .. } => {
                ty = *target;
                continue;
            }

            TypeKind::Pointer {
                target: Some(target),
                ..
            }
            | TypeKind::Reference { target, .. } => Shape::Pointer(*target),
            TypeKind::Record { .. } | TypeKind::Variant { .. } if holder(info).is_some() => {
                holder(info).map_or(Shape::Other, Shape::Holder)
            }
            TypeKind::Record { members, .. } => {
                let pinned = is_pin(info);
                let member = |name: &str| {
                    members
                        .iter()
                        .find(|member| member.name.as_deref() == Some(name))
                };

                match (pinned, &members[..], member("pointer"), member("vtable")) {
                    (true, [only], ..) => Shape::Pinned {
                        pointer: only.type_ref,
                        layout: only.layout,
                    },
                    (false, [only], ..) => match only.layout {
                        RecordMemberLayout::ByteOffset(offset)
                            if is_coroutine(image, only.type_ref) =>
                        {
                            Shape::Wrapper {
                                future: only.type_ref,
                                offset,
                            }
                        }
                        _ => Shape::Other,
                    },
                    (false, [_, _], Some(pointer), Some(vtable)) => {
                        match (
                            pointer.layout,
                            vtable.layout,
                            points_to_dyn(image, pointer.type_ref),
                        ) {
                            (
                                RecordMemberLayout::ByteOffset(data),
                                RecordMemberLayout::ByteOffset(vtable),
                                true,
                            ) => Shape::TraitObject { data, vtable },
                            _ => Shape::Other,
                        }
                    }
                    _ => Shape::Other,
                }
            }
            _ => Shape::Other,
        };

        return Some((ty, shape));
    }

    None
}

fn is_coroutine(image: &dyn RuntimeImage, mut ty: TypeReference) -> bool {
    for _ in 0..MAX_WRAPPERS {
        match image.type_info(ty).map(|info| &info.kind) {
            Some(
                TypeKind::Named {
                    target: Some(target),
                    ..
                }
                | TypeKind::Modified { target, .. },
            ) => ty = *target,
            Some(_) => return image.coroutine(ty).is_some(),
            None => return false,
        }
    }

    false
}

pub fn pinned(image: &dyn RuntimeImage, ty: TypeReference) -> bool {
    representation(image, ty)
        .and_then(|(ty, _)| image.type_info(ty))
        .is_some_and(is_pin)
}

fn is_pin(info: &crate::TypeInfo) -> bool {
    info.identity.as_ref().is_some_and(|identity| {
        identity.language == SourceLanguage::Rust
            && identity.base.as_ref() == "Pin"
            && identity.path.iter().map(AsRef::as_ref).eq(["core", "pin"])
    })
}

fn holder(info: &crate::TypeInfo) -> Option<&'static [&'static str]> {
    let identity = info.identity.as_ref()?;
    if identity.language != SourceLanguage::Rust {
        return None;
    }

    let path = identity.path.iter().map(AsRef::as_ref);
    HOLDERS.iter().find_map(|&(within, base, steps)| {
        (identity.base.as_ref() == base && path.clone().eq(within.iter().copied())).then_some(steps)
    })
}

fn points_to_dyn(image: &dyn RuntimeImage, ty: TypeReference) -> bool {
    let Some(TypeKind::Pointer {
        target: Some(target),
        ..
    }) = image.type_info(ty).map(|info| &info.kind)
    else {
        return false;
    };

    image
        .type_info(*target)
        .is_some_and(|target| trait_object(&target.name).is_some())
}

pub fn trait_object(name: &str) -> Option<&str> {
    let name = name
        .strip_prefix('(')
        .and_then(|name| name.strip_suffix(')'))
        .unwrap_or(name);
    name.starts_with("dyn ").then_some(name)
}

#[cfg(test)]
mod tests;
