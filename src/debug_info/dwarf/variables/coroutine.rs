//! The variables of a running async body. rustc keeps a variable in the
//! body's future only across the awaits it is live across; elsewhere the
//! variable is on the stack, or in a part of the future other states
//! reuse. While the body runs, its future's state number still says which
//! await this poll resumed from, until the body suspends again: a variable
//! last written before that await, which the await's state does not keep,
//! holds whatever other polls left in its storage.

#[cfg(target_arch = "x86_64")]
use rayon::prelude::*;

use crate::debug_info::VariableRuntime;
use crate::inspection::InspectionBudget;
use crate::model::ValueStorage;

#[cfg(target_arch = "x86_64")]
use crate::debug_info::dispatch::{DispatchImage, first_beyond, flood_all};
use crate::{
    CodeInstanceId, CoroutineState, CoroutineStateKind, ImageAddress, RecordMemberLayout, Variable,
    VariableState, VariableUnavailableReason, VariableValueSource, VirtualAddress,
};

use super::DwarfVariableInfo;
use super::codec::unsigned_value;
use super::evaluate::FrameBaseCache;
use crate::image::variables::{Object, VariableFunction};

/// The await a running async body resumed from.
pub(super) struct Resumption<'a> {
    /// Where its future begins.
    object: VirtualAddress,

    /// The future's size.
    size: u64,

    /// The suspended state the poll began in.
    state: &'a CoroutineState,

    /// Where the body runs.
    address: ImageAddress,
    resumes: crate::image::resumes::ResumeView<'a>,
    locations: super::location::LocationTables<'a>,
}

impl DwarfVariableInfo {
    /// The await the async body running at `address` resumed from, or
    /// `None` when the code there runs no async body, the poll began at the
    /// body's start, or the future cannot be read.
    pub(super) fn resumption(
        &self,
        function: VariableFunction<'_>,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        runtime: &mut dyn VariableRuntime,
        frame_base: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> Option<Resumption<'_>> {
        let (future, ty) = function.objects().find_map(|object| {
            (object.instance() == selected && self.visible_at(object, address)).then_some(())?;
            Some((object, object.coroutine()?))
        })?;
        let coroutine = self.types.coroutine(ty)?.ok()?;
        let size = self.type_info(ty).ok()?.byte_size?;
        let ValueStorage::Memory(object) = self
            .located_data_object(future, Some(address), runtime, frame_base, budget)
            .ok()?
        else {
            return None;
        };

        let state_size = usize::try_from(coroutine.state.size).ok()?;
        budget.consume_memory(state_size).ok()?;
        let raw = runtime
            .read_memory(
                VirtualAddress::new(object.get().checked_add(coroutine.state.offset)?),
                state_size,
            )
            .ok()?;

        let value = u64::try_from(unsigned_value(&raw, self.target.byte_order).ok()?).ok()?;
        let state = coroutine.states.iter().find(|state| state.value == value)?;

        matches!(state.kind, CoroutineStateKind::Suspended { .. }).then_some(Resumption {
            object,
            size,
            state,
            address,
            resumes: self.resumes(),
            locations: self.locations(),
        })
    }
}

/// For each variable of an async body and each state its future resumes
/// in, the code where the variable still holds what it held before the
/// state's await: what execution reaches from where the state resumes
/// without leaving the variable's scope. Elsewhere in its scope, as when a
/// loop goes round to the variable's binding again, this poll bound it
/// anew. Where the code cannot all be followed, as past an indirect
/// branch, nothing is noted.
#[cfg(target_arch = "x86_64")]
pub(in crate::debug_info) fn held_ranges(
    code: &(dyn DispatchImage + Sync),
    variables: &crate::image::variables::Variables,
    instances: &[crate::CodeInstanceInfo],
    resume_points: &[crate::image::resumes::Decoded],
) -> Vec<crate::image::resumes::Held> {
    let Ok(functions) = variables.function_index() else {
        return Vec::new();
    };

    // Each body's resumptions are followed on their own, in parallel, and
    // kept in the order of the bodies.
    let held = resume_points
        .par_iter()
        .map(|(instance, points)| {
            let mut held = Vec::new();
            let Ok(points) = points else {
                return held;
            };

            let Some(function) = instances
                .get(instance.index())
                .and_then(|instance| instance.ranges.iter().map(|range| range.start).min())
                .and_then(|entry| functions.function_at(entry))
            else {
                return held;
            };

            for point in points.points.iter() {
                for object in function
                    .objects
                    .iter()
                    .map(|id| &variables.objects[*id as usize])
                {
                    // Go, whose locals are visible only past their declaration,
                    // has no coroutines.
                    let (Some(offset), None, None, None) = (
                        object.debug_info_offset,
                        object.instance,
                        object.coroutine,
                        &object.go_declaration,
                    ) else {
                        continue;
                    };

                    let in_scope = |address: u64| {
                        let address = ImageAddress::new(address);
                        object.ranges.iter().any(|range| range.contains(address))
                    };

                    // The dispatch leaves for the state outside every
                    // variable's scope.
                    let Some(entered) =
                        first_beyond(code, point.address, &|address| !in_scope(address))
                    else {
                        continue;
                    };

                    if let (reached, true) = flood_all(code, entered, &in_scope) {
                        held.push(((offset, point.state), reached.to_vec()));
                    }
                }
            }

            held
        })
        .collect::<Vec<_>>();

    held.into_iter().flatten().collect()
}

impl Resumption<'_> {
    /// Marks `variable` as holding no value when the body last wrote it
    /// before the await it resumed from: see [`Self::stale`].
    pub(super) fn check(&self, catalog: Object<'_>, variable: &mut Variable) {
        let (VariableState::Available { source, .. } | VariableState::Invalid { source, .. }) =
            &variable.state
        else {
            return;
        };

        let memory = match source {
            VariableValueSource::Memory(address) => Some(*address),
            _ => None,
        };

        if let Some(reason) = self.stale(catalog, memory) {
            variable.state = VariableState::Unavailable(reason);
        }
    }

    /// Why a variable stored in memory at `memory`, or elsewhere when
    /// `None`, holds no value, when the body last wrote it before the await
    /// it resumed from, and the await did not keep it: it is declared on an
    /// earlier line of the await's file, and is either in the future where
    /// the await's state holds no variable of its name, or in a stack slot
    /// whose one location covers the whole function.
    pub(super) fn stale(
        &self,
        catalog: Object<'_>,
        memory: Option<VirtualAddress>,
    ) -> Option<VariableUnavailableReason> {
        // Every poll passes the body its future anew.
        if catalog.coroutine().is_some() {
            return None;
        }

        let (Some(resumed), Some(declared)) = (&self.state.location, catalog.declaration()) else {
            return None;
        };

        if declared.file != resumed.file || declared.line >= resumed.line {
            return None;
        }

        // Bound anew since the poll resumed, as a loop's variable is.
        if let Some(mut held) = catalog
            .debug_info_offset()
            .and_then(|offset| self.resumes.held(offset, self.state.value))
            && !held.any(|range| range.contains(self.address))
        {
            return None;
        }

        let stale = match memory {
            Some(address)
                if address.get() >= self.object.get()
                    && address.get() - self.object.get() < self.size =>
            {
                let offset = address.get() - self.object.get();
                !self.state.saved.iter().any(|member| {
                    member.name.as_deref() == Some(catalog.name())
                        && member.layout == RecordMemberLayout::ByteOffset(offset)
                })
            }

            // A location list says where the variable is at each address,
            // which the compiler knows better than this does.
            _ => catalog.location().is_some_and(|location| {
                self.locations
                    .list(location)
                    .entries()
                    .map(|(range, _)| range)
                    .eq([None])
            }),
        };

        stale.then_some(VariableUnavailableReason::NotSavedAcrossAwait { line: resumed.line })
    }
}
