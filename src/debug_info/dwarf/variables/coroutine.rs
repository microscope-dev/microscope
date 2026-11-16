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
use crate::TypeId;
use crate::image::functions::FunctionView;
use crate::image::variables::TypeResolution;
use crate::image::variables::{Object, VariableFunction};
use std::collections::BTreeSet;

pub(super) struct Resumption<'a> {
    object: VirtualAddress,

    size: u64,

    state: &'a CoroutineState,

    address: ImageAddress,
    resumes: crate::image::resumes::ResumeView<'a>,
    locations: super::location::LocationTables<'a>,
}

impl DwarfVariableInfo {
    pub(super) fn resumption(
        &self,
        function: VariableFunction<'_>,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        runtime: &mut dyn VariableRuntime,
        frame_base: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> Option<Resumption<'_>> {
        let parameter = function.objects().find_map(|object| {
            (object.instance() == selected && self.visible_at(object, address)).then_some(())?;
            Some((object, object.coroutine()?))
        });
        let (object, ty) = match parameter {
            Some((future, ty)) => match self
                .located_data_object(future, Some(address), runtime, frame_base, budget)
                .ok()?
            {
                ValueStorage::Memory(object) => (object, ty),
                _ => return None,
            },
            None => self.captured_future(address, selected, runtime, frame_base, budget)?,
        };
        let coroutine = self.types.coroutine(ty)?.ok()?;
        let size = self.type_info(ty).ok()?.byte_size?;

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

    pub(super) fn captured_future(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        runtime: &mut dyn VariableRuntime,
        frame_base: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> Option<(VirtualAddress, TypeId)> {
        let functions = FunctionView::new(self.types.tables());
        let instance = match selected {
            Some(instance) => instance,
            None => functions
                .instances_containing(address)
                .filter(|instance| instance.is_out_of_line())
                .min_by_key(|instance| instance.id())?
                .id(),
        };
        let ty = functions
            .function(functions.instance(instance)?.function())?
            .coroutine()?;
        let coroutine = self.types.coroutine(ty)?.ok()?;
        let types = self.types.view();
        let mut futures = BTreeSet::new();

        for capture in coroutine.captures.iter() {
            let (Some(name), RecordMemberLayout::ByteOffset(offset)) =
                (capture.name.as_deref(), capture.layout)
            else {
                continue;
            };

            let Ok(object) = self.visible_object(address, selected, name, true) else {
                continue;
            };

            let TypeResolution::Resolved(object_ty) = object.type_info() else {
                continue;
            };

            let same_type = object_ty == capture.type_ref.id
                || types
                    .class(object_ty)
                    .zip(types.class(capture.type_ref.id))
                    .is_some_and(|(left, right)| left == right);
            if same_type
                && let Ok(ValueStorage::Memory(at)) =
                    self.located_data_object(object, Some(address), runtime, frame_base, budget)
                && let Some(future) = at.get().checked_sub(offset)
            {
                futures.insert(future);
            }
        }

        let [future] = futures.into_iter().collect::<Vec<_>>()[..] else {
            return None;
        };

        Some((VirtualAddress::new(future), ty))
    }
}

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
                for &id in &function.objects {
                    #[expect(
                        clippy::indexing_slicing,
                        reason = "the variables were just built from DWARF, and a function's \
                                  objects index its variables' objects"
                    )]
                    let object = &variables.objects[id as usize];

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

    pub(super) fn stale(
        &self,
        catalog: Object<'_>,
        memory: Option<VirtualAddress>,
    ) -> Option<VariableUnavailableReason> {
        if catalog.coroutine().is_some() {
            return None;
        }

        let (Some(resumed), Some(declared)) = (&self.state.location, catalog.declaration()) else {
            return None;
        };

        if declared.file != resumed.file || declared.line >= resumed.line {
            return None;
        }

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
