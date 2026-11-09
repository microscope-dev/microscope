//! The `LocalSet`s the futures a thread drives hold, wherever they hold
//! them: as the future itself, through the `RunUntil` that runs one, or in
//! a member of a combinator, as `join!` keeps each branch in a tuple of
//! `MaybeDone`s that its `async` block saves across its await.
//!
//! A set runs only while its future is polled, so while another branch of
//! a `join!` runs, or the thread waits, no thread's context names it, and
//! only the futures it drives lead to it. The walk reads values as their
//! types lay them out: a record's members, an enum's active variant, a
//! coroutine's members in its state, the future a `Box` or `Pin` points
//! to, and the future a `dyn Future`'s vtable names. It goes only where a
//! type may lead to a set, as [`may_hold_set`](super::may_hold_set) says,
//! and says where it cannot go on.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::{TokioRuntime, holds_pointee, local_type, records};
use crate::runtime_model::futures::{AsyncFrameKind, trait_object};
use crate::runtime_model::{DrivenFuture, Partial, RuntimeStop};
use crate::{
    CoroutineKind, CoroutineStateKind, RecordMemberLayout, TypeKind, TypeReference,
    VariantDiscriminant, VirtualAddress,
};

/// The most values the futures one thread drives are read through, so that
/// a large future, or corrupt memory, cannot make the walk run on.
const MAX_VALUES: usize = 4_096;

/// The most names and qualifiers passed to reach a type's layout.
const MAX_WRAPPERS: usize = 16;

/// One value the walk has yet to read: where, of which type, and whether a
/// `Box` or `Pin` holds it, whose pointer then leads to what they hold.
type Pending = (VirtualAddress, TypeReference, bool);

/// Which state a coroutine the walk meets is read in.
#[derive(Debug, Clone, Copy)]
enum State {
    /// The one its driven future's chain reads it in.
    Chosen(u64),

    /// None: a frame runs it, and it stores the state it last suspended
    /// in.
    Running,

    /// The one it stores, as it is suspended.
    Stored,
}

impl TokioRuntime {
    /// The sets a future a thread drives holds, by where each set's shared
    /// state is, and why a set it may hold cannot be found. A coroutine of
    /// its chain is read in the state the chain gives it; one that runs
    /// beside the chain, in no state; any other, in the state it stores.
    pub(super) fn held_sets(
        &self,
        stop: &dyn RuntimeStop,
        driven: &DrivenFuture,
    ) -> Partial<Vec<u64>> {
        let mut found = Partial {
            value: Vec::new(),
            gaps: Vec::new(),
        };
        let Ok(Some(locals)) = self.locals() else {
            return found;
        };

        let mut pending: Vec<Pending> = vec![(driven.object, driven.ty, false)];
        let mut seen = BTreeSet::new();
        let mut sets = Vec::new();
        let mut read = 0;

        while let Some((object, ty, boxed)) = pending.pop() {
            if !seen.insert((object, ty, boxed)) || !self.may_hold(ty, boxed) {
                continue;
            }

            read += 1;
            if read > MAX_VALUES {
                found.gaps.push(
                    format!("the futures it drives were read through {MAX_VALUES} values, and not further")
                        .into(),
                );
                break;
            }

            let chained = driven.chain.iter().find_map(|future| match future.kind {
                AsyncFrameKind::Coroutine { state, .. }
                    if future.object == object && future.ty == ty =>
                {
                    Some(state)
                }
                _ => None,
            });
            let state = match chained {
                Some(state) => State::Chosen(state),
                None if driven.running.contains(&ty) => State::Running,
                None => State::Stored,
            };

            match self.read_held(stop, object, ty, boxed, state, &mut pending) {
                Ok(Some(set)) if !sets.contains(&set) => sets.push(set),
                Ok(_) => {}
                Err(reason) => found.gaps.push(reason),
            }
        }

        for set in sets {
            let shared = records::pointer(stop, set.wrapping_add(locals.set))
                .ok_or_else(|| format!("the local set at {set:#x} is unreadable").into())
                .and_then(|context| Self::set_shared(stop, locals, context));
            match shared {
                Ok(shared) if !found.value.contains(&shared.get()) => {
                    found.value.push(shared.get());
                }
                Ok(_) => {}
                Err(reason) => found.gaps.push(reason),
            }
        }

        found
    }

    /// Whether a value of `ty`, held by a box or pin when `boxed`, may lead
    /// to a set, remembered for each type.
    pub(super) fn may_hold(&self, ty: TypeReference, boxed: bool) -> bool {
        *self
            .holds_sets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry((ty, boxed))
            .or_insert_with(|| super::may_hold_set(self.image.as_ref(), ty, boxed))
    }

    /// Reads the value of `ty` at `object`: the set it is or refers to, or
    /// else what it holds, added to `pending`. A coroutine of the chains is
    /// read in `state`.
    fn read_held(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
        mut ty: TypeReference,
        boxed: bool,
        state: State,
        pending: &mut Vec<Pending>,
    ) -> Result<Option<VirtualAddress>, Arc<str>> {
        let image = self.image.as_ref();
        let within = |offset: u64| {
            object.checked_add(offset).ok_or_else(|| {
                Arc::<str>::from(format!("the future at {object:#x} holds no address"))
            })
        };

        for _ in 0..MAX_WRAPPERS {
            let Some(info) = image.type_info(ty) else {
                return Ok(None);
            };

            if image.coroutine(ty).is_some() {
                self.read_coroutine(stop, object, ty, state, pending)?;
                return Ok(None);
            }

            if local_type(info) == Some("LocalSet") {
                return Ok(Some(object));
            }

            match &info.kind {
                TypeKind::Named {
                    target: Some(target),
                    ..
                }
                | TypeKind::Modified { target, .. } => {
                    ty = *target;
                    continue;
                }

                TypeKind::Pointer {
                    target: Some(target),
                    ..
                }
                | TypeKind::Reference { target, .. } => {
                    let to_set = image
                        .type_info(*target)
                        .is_some_and(|pointee| local_type(pointee) == Some("LocalSet"));
                    if to_set || boxed {
                        let pointer = records::pointer(stop, object)
                            .ok_or_else(|| format!("the pointer at {object:#x} is unreadable"))?;
                        if to_set {
                            return Ok(Some(pointer));
                        }

                        pending.push((pointer, *target, false));
                    }
                }

                TypeKind::Record { members, bases, .. } => {
                    if let Some((data, vtable)) = self.trait_object(members) {
                        pending.push(held_object(stop, image, object, data, vtable)?);
                        return Ok(None);
                    }

                    let boxed = boxed || holds_pointee(info);
                    let members = members
                        .iter()
                        .map(|member| (member.layout, member.type_ref));
                    let bases = bases.iter().map(|base| (base.layout, base.type_ref));
                    for (layout, ty) in members.chain(bases) {
                        if let RecordMemberLayout::ByteOffset(offset) = layout {
                            pending.push((within(offset)?, ty, boxed));
                        }
                    }
                }

                // An enum with one variant that can hold a value, such as
                // `Result<T, Infallible>`, stores no tag.
                TypeKind::Variant {
                    discriminant,
                    variants,
                    ..
                } => {
                    if let (VariantDiscriminant::Absent, [only]) =
                        (discriminant.as_ref(), &variants[..])
                    {
                        for member in only.members.iter() {
                            if let RecordMemberLayout::ByteOffset(offset) = member.layout {
                                pending.push((within(offset)?, member.type_ref, boxed));
                            }
                        }
                    } else {
                        let sum = records::sum(image, ty)?;
                        let active = sum.active(stop, object)?;
                        pending.push((within(active.payload.offset)?, active.payload.ty, boxed));
                    }
                }

                TypeKind::Array { .. } => {
                    let (element, count) = records::array(image, ty)?;
                    let size = records::size(image, element)?;
                    for index in 0..count.min(MAX_VALUES as u64) {
                        let offset = index.checked_mul(size).ok_or_else(|| {
                            Arc::<str>::from("an array's element lies past memory")
                        })?;
                        pending.push((within(offset)?, element, boxed));
                    }
                }

                TypeKind::Union { .. } => {
                    return Err(format!(
                        "the {} at {object:#x} is a union, whose member in use is unknown",
                        info.name
                    )
                    .into());
                }

                _ => {}
            }

            return Ok(None);
        }

        Ok(None)
    }

    /// Reads the coroutine of `ty` at `object` in `state`, adding what that
    /// state holds to `pending`.
    fn read_coroutine(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
        ty: TypeReference,
        state: State,
        pending: &mut Vec<Pending>,
    ) -> Result<(), Arc<str>> {
        let within = |offset: u64| {
            object.checked_add(offset).ok_or_else(|| {
                Arc::<str>::from(format!("the future at {object:#x} holds no address"))
            })
        };

        let Some(coroutine) = self.image.coroutine(ty) else {
            return Ok(());
        };
        let coroutine = coroutine.map_err(Arc::clone)?;
        let number = match state {
            State::Chosen(number) => number,
            State::Running => {
                return Err(format!(
                    "the future at {object:#x} is running, in a state it does not store"
                )
                .into());
            }
            State::Stored => {
                let size = usize::try_from(coroutine.state.size)
                    .map_err(|_| Arc::<str>::from("a coroutine's state is too large"))?;
                within(coroutine.state.offset)
                    .ok()
                    .and_then(|at| records::read(stop, at, size))
                    .ok_or_else(|| format!("the future at {object:#x} is unreadable"))?
            }
        };
        let state = coroutine
            .state(number)
            .ok_or_else(|| format!("the future at {object:#x} is in no state, {number}"))?;
        if matches!(
            state.kind,
            CoroutineStateKind::Returned | CoroutineStateKind::Panicked
        ) {
            return Ok(());
        }

        // An async function's arguments move into its body as it
        // starts; an async block's captures stay its own.
        let own = state.kind == CoroutineStateKind::Unresumed
            || coroutine.kind != CoroutineKind::AsyncFunction;
        let captures = coroutine.captures.iter().filter(|_| own);
        for member in state.saved.iter().chain(captures) {
            if let RecordMemberLayout::ByteOffset(offset) = member.layout {
                pending.push((within(offset)?, member.type_ref, false));
            }
        }

        Ok(())
    }

    /// Where a record's data pointer and vtable lie, if it is a pointer to
    /// a `dyn Future`: a `Box` or reference to one.
    fn trait_object(&self, members: &[crate::RecordMember]) -> Option<(u64, u64)> {
        let member = |name: &str| {
            members
                .iter()
                .find(|member| member.name.as_deref() == Some(name))
        };
        let ([_, _], Some(pointer), Some(vtable)) = (members, member("pointer"), member("vtable"))
        else {
            return None;
        };

        let image = self.image.as_ref();
        let TypeKind::Pointer {
            target: Some(target),
            ..
        } = &image.type_info(pointer.type_ref)?.kind
        else {
            return None;
        };

        let future = image.type_info(*target).is_some_and(|target| {
            trait_object(&target.name)
                .is_some_and(|name| name.starts_with("dyn core::future::future::Future"))
        });
        match (future, pointer.layout, vtable.layout) {
            (
                true,
                RecordMemberLayout::ByteOffset(data),
                RecordMemberLayout::ByteOffset(vtable),
            ) => Some((data, vtable)),
            _ => None,
        }
    }
}

/// The future the trait object at `object` points to, with its data
/// pointer and vtable at those offsets, and its type, which the vtable
/// says.
fn held_object(
    stop: &dyn RuntimeStop,
    image: &dyn crate::runtime_model::RuntimeImage,
    object: VirtualAddress,
    data: u64,
    vtable: u64,
) -> Result<Pending, Arc<str>> {
    let word = |offset| {
        object
            .checked_add(offset)
            .and_then(|at| records::pointer(stop, at))
    };
    let (Some(data), Some(vtable)) = (word(data), word(vtable)) else {
        return Err(format!("the trait object at {object:#x} is unreadable").into());
    };

    let held = image
        .trait_object_type(stop.load_bias().wrapping_unload(vtable))
        .ok_or_else(|| format!("the vtable at {vtable:#x} is no future's the program describes"))?;
    Ok((data, held, false))
}
