//! Parallel per-unit DWARF walk coordinator and deterministic arena merger.
//!
//! Workers walk units against a frozen shared type base and merge their local
//! types, files, and location pools back in unit order with ID renumbering.

use crate::debug_info::dwarf::DwarfSet;
use std::ops::Range;
use std::sync::{Mutex, PoisonError};

use foldhash::HashMap;

use super::identity::{IdentityParts, PendingArguments};
use super::in_order::InOrder;
use super::layered::{Layered, Marks, Rows};
use super::location::{LocationListId, LocationsBuilder};
use super::types::{
    AggregateMemberDeclaration, Bits, DieBuffers, DynamicAggregateLayoutKey, TypeArenaBuilder,
    TypeEntry,
};

use super::{Metadata, ValueDescription, WalkContext, Walked, dedup, die, row, walk_unit};
use crate::debug_info::dwarf::budget::Meter;
use crate::debug_info::dwarf::{DieKey, DieMap, DwarfError, is_type_unit};
use crate::image::lines::Files;
use crate::image::locations::ExpressionId;
use crate::image::variables::TypeResolution;
use crate::{SourceFileId, SourceLocation, TypeArgument, TypeId, TypeReference};

pub(super) struct Pooled {
    expressions: Vec<ExpressionId>,
    lists: Vec<LocationListId>,
}

impl Pooled {
    #[expect(
        clippy::indexing_slicing,
        reason = "a unit's expressions are in its own pool, which `absorb` maps entry by entry"
    )]
    pub(super) fn expression(&self, id: ExpressionId) -> ExpressionId {
        self.expressions[id.0 as usize]
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "a unit's lists are in its own pool, which `absorb` maps entry by entry"
    )]
    pub(super) fn list(&self, id: LocationListId) -> LocationListId {
        self.lists[id.0 as usize]
    }

    pub(super) fn frame_base(&self, frame_base: &mut Metadata<LocationListId>) {
        if let Metadata::Value(list) = frame_base {
            *list = self.list(*list);
        }
    }

    fn value(&self, value: &mut Metadata<ValueDescription>) {
        if let Metadata::Value(ValueDescription::Location(list)) = value {
            *list = self.list(*list);
        }
    }
}

struct Renumbered<'r> {
    base: usize,
    ids: &'r [TypeId],
}

impl Renumbered<'_> {
    #[expect(
        clippy::indexing_slicing,
        reason = "`absorb` numbers each of a unit's own types in `ids` before it renumbers any"
    )]
    fn id(&self, id: TypeId) -> TypeId {
        id.index()
            .checked_sub(self.base)
            .map_or(id, |own| self.ids[own])
    }

    fn reference(&self, reference: TypeReference) -> TypeReference {
        TypeReference {
            image: reference.image,
            id: self.id(reference.id),
        }
    }

    fn resolution(&self, resolution: &mut TypeResolution) {
        if let TypeResolution::Resolved(id) = resolution {
            *id = self.id(*id);
        }
    }
}

struct UnitWalk {
    unit: usize,
    group: usize,
    result: Result<(), DwarfError>,
    walked: Walked,
    types: UnitTypes,
    files: Files,
    pool: LocationsBuilder,
}

pub(super) struct UnitTypes {
    start: usize,
    entries: Vec<TypeEntry>,
    by_die: DieMap<TypeId>,
    explicit_names: Bits,
    identity_parts: Vec<(usize, IdentityParts)>,
    go_identity_parts: HashMap<TypeId, super::identity::GoParts>,
    go_dict_indices: HashMap<TypeId, u64>,
    passed_by_value: HashMap<TypeId, bool>,
    pending_arguments: Vec<PendingArguments>,
    record_member_declarations: Vec<AggregateMemberDeclaration>,
    dynamic_record_layouts: HashMap<DynamicAggregateLayoutKey, ExpressionId>,
    void_type: Option<TypeId>,
    character_type: Option<TypeId>,
    limit_type: Option<TypeId>,
    budget: Meter,
    traced: Vec<Traced>,
    aliases: Vec<(DieKey, TypeId, Option<usize>)>,
}

/// Per-type spend and pool-range log recorded during a worker's walk so
/// duplicate types dropped during merge can refund their load budget.
#[derive(Default)]
pub(super) struct Trail {
    entries: Vec<Traced>,
    open: Vec<usize>,
    aliases: Vec<(DieKey, Option<usize>)>,
}

#[derive(Clone, Default)]
struct Traced {
    key: Option<DieKey>,
    parent: Option<usize>,
    spent: u64,
    expressions: Range<usize>,
    lists: Range<usize>,
}

#[derive(Clone, Copy)]
pub(super) struct BuildMark {
    spent: u64,
    expressions: usize,
    lists: usize,
}

impl Trail {
    pub(super) fn open(&mut self, id: TypeId) {
        self.open.push(id.index());
    }

    pub(super) fn alias(&mut self, key: DieKey) {
        self.aliases.push((key, self.open.last().copied()));
    }
}

impl TypeArenaBuilder<'_, '_> {
    pub(super) fn push_entry(&mut self, entry: TypeEntry, key: Option<DieKey>) {
        self.entries.push(entry);
        if let Some(trail) = &mut self.trail {
            trail.entries.push(Traced {
                key,
                parent: trail.open.last().copied(),
                ..Traced::default()
            });
        }
    }

    pub(super) fn build_mark(&self) -> BuildMark {
        let (expressions, lists) = self
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .counts();
        BuildMark {
            spent: self.budget.spent(),
            expressions,
            lists,
        }
    }

    pub(super) fn close_build(&mut self, id: TypeId, mark: BuildMark) {
        let end = self.build_mark();
        let base = self.entries.base_len();
        let Some(trail) = &mut self.trail else {
            return;
        };

        let closed = trail.open.pop();
        debug_assert_eq!(
            closed,
            Some(id.index()),
            "type builds close in the order they open"
        );
        let traced = id
            .index()
            .checked_sub(base)
            .and_then(|own| trail.entries.get_mut(own));
        debug_assert!(traced.is_some(), "a type build closes an entry it pushed");
        if let Some(traced) = traced {
            traced.spent = end.spent - mark.spent;
            traced.expressions = mark.expressions..end.expressions;
            traced.lists = mark.lists..end.lists;
        }
    }
}

struct Frozen<'a, 'data> {
    dwarf: &'a DwarfSet<'data>,
    units: &'a crate::debug_info::dwarf::Units<'data>,
    type_signatures: &'a crate::debug_info::dwarf::TypeSignatures,
    image: crate::ModuleImageId,
    context: std::sync::Arc<super::types::TypeContext>,
    by_die: Layered<DieMap<TypeId>>,
    entries: Rows<TypeEntry>,
    explicit_names: Marks,
    identity_parts: Rows<Option<IdentityParts>>,
    go_identity_parts: Layered<HashMap<TypeId, super::identity::GoParts>>,
    go_dict_indices: Layered<HashMap<TypeId, u64>>,
    passed_by_value: Layered<HashMap<TypeId, bool>>,
    byte_order: crate::ByteOrder,
    void_type: Option<TypeId>,
    character_type: Option<TypeId>,
    budget: Meter,
}

impl<'a, 'data> TypeArenaBuilder<'a, 'data> {
    fn frozen(&self) -> Frozen<'a, 'data> {
        Frozen {
            dwarf: self.dwarf,
            units: self.units,
            type_signatures: self.type_signatures,
            image: self.image,
            context: std::sync::Arc::clone(&self.context),
            by_die: self.by_die.split(),
            entries: self.entries.split(),
            explicit_names: self.explicit_names.split(),
            identity_parts: self.identity_parts.split(),
            go_identity_parts: self.go_identity_parts.split(),
            go_dict_indices: self.go_dict_indices.split(),
            passed_by_value: self.passed_by_value.split(),
            byte_order: self.byte_order,
            void_type: self.void_type,
            character_type: self.character_type,
            budget: self.budget.remaining(),
        }
    }
}

impl<'data> Frozen<'_, 'data> {
    fn split<'b>(
        &'b self,
        pool: &'b Mutex<LocationsBuilder>,
        die_buffers: &'b DieBuffers<'data>,
    ) -> TypeArenaBuilder<'b, 'data> {
        TypeArenaBuilder {
            dwarf: self.dwarf,
            units: self.units,
            type_signatures: self.type_signatures,
            image: self.image,
            by_die: self.by_die.split(),
            context: std::sync::Arc::clone(&self.context),
            entries: self.entries.split(),
            explicit_names: self.explicit_names.split(),
            pending_arguments: Vec::new(),
            resolution_depth: 0,
            byte_order: self.byte_order,
            limit_type: None,
            void_type: self.void_type,
            character_type: self.character_type,
            dynamic_record_layouts: HashMap::default(),
            pool,
            die_buffers,
            record_member_declarations: Vec::new(),
            budget: self.budget.remaining(),
            identity_parts: self.identity_parts.split(),
            go_identity_parts: self.go_identity_parts.split(),
            complex_parts: HashMap::default(),
            go_dict_indices: self.go_dict_indices.split(),
            passed_by_value: self.passed_by_value.split(),
            trail: Some(Trail::default()),
        }
    }
}

fn type_id(index: usize) -> Option<TypeId> {
    u32::try_from(index).ok().map(TypeId::new)
}

struct Cut {
    end: usize,

    aliases: usize,

    budget: Meter,
}

fn owner(units: &mut [UnitTypes], index: usize) -> Option<&mut UnitTypes> {
    let unit = units.get_mut(index);
    debug_assert!(
        unit.is_some(),
        "a cut search found unit {index}, past the last"
    );
    unit
}

impl TypeArenaBuilder<'_, '_> {
    #[expect(
        clippy::iter_over_hash_type,
        reason = "each entry of a map goes to its unit on its own, so their order cannot matter"
    )]
    fn into_unit_types(self, cuts: &[Cut]) -> Option<Vec<UnitTypes>> {
        debug_assert!(
            self.complex_parts.is_empty(),
            "complex parts are added once every unit is walked"
        );
        let base = self.entries.base_len();
        check_cut_order(cuts);
        let Some(trail) = self.trail else {
            debug_assert!(false, "only a walk apart is cut into units");
            return None;
        };
        debug_assert!(trail.open.is_empty(), "every type build closed");

        let unit_of = |id: usize| cuts.partition_point(|cut| cut.end <= id);
        let made = |made: Option<TypeId>, start: usize, end: usize| {
            made.filter(|id| (start..end).contains(&id.index()))
        };

        let starts = std::iter::once(base).chain(cuts.iter().map(|cut| cut.end));
        let mut units = cuts
            .iter()
            .zip(starts)
            .map(|(cut, start)| UnitTypes {
                start: start - base,
                entries: Vec::new(),
                by_die: DieMap::default(),
                explicit_names: Bits::default(),
                identity_parts: Vec::new(),
                go_identity_parts: HashMap::default(),
                go_dict_indices: HashMap::default(),
                passed_by_value: HashMap::default(),
                pending_arguments: Vec::new(),
                record_member_declarations: Vec::new(),
                dynamic_record_layouts: HashMap::default(),
                void_type: made(self.void_type, start, cut.end),
                character_type: made(self.character_type, start, cut.end),
                limit_type: made(self.limit_type, start, cut.end),
                budget: cut.budget.clone(),
                traced: Vec::new(),
                aliases: Vec::new(),
            })
            .collect::<Vec<_>>();
        let mut entries = self.entries.into_own();
        debug_assert_eq!(
            base + entries.len(),
            cuts.last().map_or(base, |cut| cut.end)
        );

        let mut traced = trail.entries;
        debug_assert_eq!(traced.len(), entries.len(), "every own entry is traced");

        for unit in units.iter_mut().rev() {
            unit.entries = entries.split_off(unit.start);
            unit.traced = traced.split_off(unit.start);
        }

        split_dies(
            self.by_die.into_own(),
            trail.aliases,
            cuts,
            base,
            &mut units,
        )?;

        for id in self.explicit_names.into_own().iter() {
            owner(&mut units, unit_of(id))?.explicit_names.insert(id);
        }

        let identity_start = self.identity_parts.base_len();

        for (own, parts) in self.identity_parts.into_own().into_iter().enumerate() {
            if let Some(parts) = parts {
                let id = identity_start + own;
                let unit = owner(&mut units, unit_of(id))?;
                unit.identity_parts.push((id, parts));
            }
        }

        for (id, parts) in self.go_identity_parts.into_own() {
            owner(&mut units, unit_of(id.index()))?
                .go_identity_parts
                .insert(id, parts);
        }

        for (id, index) in self.go_dict_indices.into_own() {
            let unit = owner(&mut units, unit_of(id.index()))?;
            unit.go_dict_indices.insert(id, index);
        }

        for (id, by_value) in self.passed_by_value.into_own() {
            owner(&mut units, unit_of(id.index()))?
                .passed_by_value
                .insert(id, by_value);
        }

        for pending in self.pending_arguments {
            owner(&mut units, unit_of(pending.entry))?
                .pending_arguments
                .push(pending);
        }

        for declaration in self.record_member_declarations {
            owner(&mut units, unit_of(declaration.aggregate.index()))?
                .record_member_declarations
                .push(declaration);
        }

        for (key, expression) in self.dynamic_record_layouts {
            owner(&mut units, unit_of(key.aggregate.index()))?
                .dynamic_record_layouts
                .insert(key, expression);
        }

        Some(units)
    }

    fn repeats(&self, unit: &UnitTypes, first: usize) -> Option<Repeats> {
        let mut repeats = Repeats {
            existing: Vec::with_capacity(unit.traced.len()),
            inside: Vec::with_capacity(unit.traced.len()),
        };

        for (own, traced) in unit.traced.iter().enumerate() {
            let id = type_id(first + own)?;
            let inside = match traced.parent {
                Some(parent) => repeats.dropped(parent.checked_sub(first)?)?,
                None => false,
            };
            let known = match traced.key {
                Some(key) => self.by_die.get(&key).copied(),
                None if unit.void_type == Some(id) => self.void_type,
                None if unit.character_type == Some(id) => self.character_type,
                None => None,
            };

            if inside && known.is_none() {
                return None;
            }

            repeats.existing.push(known);
            repeats.inside.push(inside);
        }

        Some(repeats)
    }

    fn same_as_existing(
        &self,
        unit: &UnitTypes,
        first: usize,
        repeats: &Repeats,
        renumbered: &Renumbered<'_>,
    ) -> bool {
        unit.entries
            .iter()
            .zip(&repeats.existing)
            .enumerate()
            .all(|(own, (entry, existing))| {
                let Some(existing) = existing else {
                    return true;
                };

                let id = TypeId::new(super::row(first + own));
                self.entries.get(existing.index()) == Some(&renumber(entry.clone(), renumbered))
                    && unit.explicit_names.contains(id.index())
                        == self.explicit_names.contains(existing.index())
                    && unit.passed_by_value.get(&id) == self.passed_by_value.get(existing)
                    && unit.go_dict_indices.get(&id) == self.go_dict_indices.get(existing)
            })
    }

    fn matches(&self, unit: &UnitTypes, ids: &mut Vec<TypeId>) -> Option<Repeats> {
        let base = self.entries.base_len();
        debug_assert!(
            unit.limit_type.is_none(),
            "a unit that exceeds the budget is walked in order"
        );
        debug_assert_eq!(ids.len(), unit.start, "a group's units are added in order");
        debug_assert_eq!(unit.traced.len(), unit.entries.len());
        let first = base + unit.start;
        let repeats = self.repeats(unit, first)?;

        let mut next = self.entries.len();
        let numbered = repeats
            .existing
            .iter()
            .map(|existing| {
                existing.or_else(|| {
                    let id = type_id(next);
                    next += 1;
                    id
                })
            })
            .collect::<Option<Vec<_>>>()?;
        ids.extend(numbered);
        let renumbered = Renumbered { base, ids };

        self.same_as_existing(unit, first, &repeats, &renumbered)
            .then_some(repeats)
    }

    #[expect(
        clippy::iter_over_hash_type,
        reason = "each entry of a map is added on its own, so their order cannot matter"
    )]
    fn absorb<'r>(
        &mut self,
        unit: UnitTypes,
        repeats: &Repeats,
        pooled: &Pooled,
        pooled_before: (usize, usize),
        ids: &'r [TypeId],
    ) -> Option<Renumbered<'r>> {
        let base = self.entries.base_len();
        let first = base + unit.start;
        debug_assert_eq!(ids.len(), unit.start + unit.entries.len());
        let renumbered = Renumbered { base, ids };
        let mut budget = unit.budget;
        budget.refund(repeats.refund(&unit.traced, pooled, pooled_before)?);
        if !self.budget.absorb(&budget) {
            return None;
        }

        let kept = |id: usize| {
            id.checked_sub(first)
                .and_then(|own| repeats.dropped(own))
                .is_none_or(|dropped| !dropped)
        };

        for (own, entry) in unit.entries.into_iter().enumerate() {
            if !repeats.dropped(own)? {
                self.entries.push(renumber(entry, &renumbered));
            }
        }

        if self.void_type.is_none() {
            self.void_type = unit.void_type.map(|id| renumbered.id(id));
        }

        if self.character_type.is_none() {
            self.character_type = unit.character_type.map(|id| renumbered.id(id));
        }

        for own in unit.explicit_names.iter() {
            if kept(own) {
                self.explicit_names
                    .insert(renumbered.id(type_id(own)?).index());
            }
        }

        for (id, mut parts) in unit.identity_parts {
            if !kept(id) {
                continue;
            }

            for argument in &mut parts.template {
                if let TypeArgument::Type(reference) = argument {
                    *reference = renumbered.reference(*reference);
                }
            }

            let index = renumbered.id(type_id(id)?).index();
            self.identity_parts.resize_with(index + 1, || None);
            *self.identity_parts.get_mut(index)? = Some(parts);
        }

        for (id, mut parts) in unit.go_identity_parts {
            if kept(id.index()) {
                parts.map_references(|reference| renumbered.reference(reference));
                self.go_identity_parts.insert(renumbered.id(id), parts);
            }
        }

        for (id, index) in unit.go_dict_indices {
            if kept(id.index()) {
                self.go_dict_indices.insert(renumbered.id(id), index);
            }
        }

        for (id, by_value) in unit.passed_by_value {
            if kept(id.index()) {
                self.passed_by_value.insert(renumbered.id(id), by_value);
            }
        }

        for mut pending in unit.pending_arguments {
            if kept(pending.entry) {
                pending.entry = renumbered.id(type_id(pending.entry)?).index();
                self.pending_arguments.push(pending);
            }
        }

        self.record_member_declarations.extend(
            unit.record_member_declarations
                .into_iter()
                .filter(|declaration| kept(declaration.aggregate.index()))
                .map(|mut declaration| {
                    declaration.aggregate = renumbered.id(declaration.aggregate);
                    declaration
                }),
        );

        for (mut key, expression) in unit.dynamic_record_layouts {
            if kept(key.aggregate.index()) {
                key.aggregate = renumbered.id(key.aggregate);
                self.dynamic_record_layouts
                    .insert(key, pooled.expression(expression));
            }
        }

        for (key, id) in unit.by_die.into_entries() {
            let id = renumbered.id(id);
            match self.by_die.get(&key) {
                Some(existing) if *existing != id => return None,
                Some(_) => {}
                None => self.by_die.insert(key, id),
            }
        }

        for (key, id, parent) in unit.aliases {
            let id = renumbered.id(id);
            match self.by_die.get(&key) {
                Some(existing) if *existing != id => return None,
                Some(_) => {}
                None if parent.is_some_and(|parent| !kept(parent)) => return None,
                None => self.by_die.insert(key, id),
            }
        }

        Some(renumbered)
    }
}

struct Repeats {
    existing: Vec<Option<TypeId>>,
    inside: Vec<bool>,
}

impl Repeats {
    fn dropped(&self, own: usize) -> Option<bool> {
        self.existing.get(own).map(Option::is_some)
    }

    fn refund(
        &self,
        traced: &[Traced],
        pooled: &Pooled,
        (expressions, lists): (usize, usize),
    ) -> Option<u64> {
        let mut refund = 0;

        for (own, traced) in traced.iter().enumerate() {
            if !self.dropped(own)? || *self.inside.get(own)? {
                continue;
            }

            refund += traced.spent;
            if traced.expressions.clone().any(|index| {
                pooled
                    .expressions
                    .get(index)
                    .is_none_or(|id| id.0 as usize >= expressions)
            }) || traced.lists.clone().any(|index| {
                pooled
                    .lists
                    .get(index)
                    .is_none_or(|id| id.0 as usize >= lists)
            }) {
                return None;
            }
        }

        Some(refund)
    }
}

fn renumber(entry: TypeEntry, renumbered: &Renumbered<'_>) -> TypeEntry {
    match entry {
        TypeEntry::Resolved(mut info) => {
            info.reference = renumbered.reference(info.reference);
            dedup::map_references(&mut info, &mut |reference| renumbered.reference(reference));
            TypeEntry::Resolved(info)
        }
        other => other,
    }
}

fn absorb_walked(
    into: &mut Walked,
    unit: &mut Walked,
    types: &Renumbered<'_>,
    pooled: &Pooled,
    files: &[SourceFileId],
) -> Option<()> {
    let objects = into.objects.len();
    let functions = into.functions.len();
    let order = into.order;
    let file = |location: &mut SourceLocation| {
        location.file = *files.get(location.file.index())?;
        Some(())
    };

    for mut object in unit.objects.drain(..) {
        types.resolution(&mut object.type_info);
        for id in [&mut object.escaped, &mut object.coroutine]
            .into_iter()
            .flatten()
        {
            *id = types.id(*id);
        }

        if let Some(declaration) = &mut object.declaration {
            file(declaration)?;
        }

        if let Some(declaration) = &mut object.go_declaration {
            file(&mut declaration.location)?;
        }

        pooled.value(&mut object.value);
        pooled.frame_base(&mut object.frame_base);
        object.order = order.checked_add(object.order)?;
        into.objects.push(object);
    }

    into.functions
        .extend(unit.functions.drain(..).map(|mut function| {
            for object in &mut function.objects {
                *object += row(objects);
            }

            if let Ok(captures) = &mut function.captures {
                for capture in captures {
                    types.resolution(&mut capture.type_info);
                }
            }

            if let Some(super::returns::ReturnConvention::SystemV(convention)) =
                &mut function.returns
            {
                types.resolution(&mut convention.ty);
            }

            function
        }));
    into.calls.absorb(std::mem::take(&mut unit.calls), pooled);
    into.procedures
        .extend(unit.procedures.drain(..).map(|(offset, mut location)| {
            pooled.frame_base(&mut location);
            (offset, location)
        }));
    into.vtables.extend(
        unit.vtables
            .drain(..)
            .map(|(address, id)| (address, types.id(id))),
    );
    into.go_function_entries.extend(
        unit.go_function_entries
            .drain(..)
            .map(|(address, function)| (address, function + row(functions))),
    );
    into.unnamed_parameters
        .extend(
            unit.unnamed_parameters
                .drain(..)
                .map(|(instance, id, object)| {
                    (
                        instance,
                        types.id(id),
                        object.map(|object| object + objects),
                    )
                }),
        );
    into.abstract_bodies.extend(
        unit.abstract_bodies
            .drain(..)
            .map(|(instance, id)| (instance, types.id(id))),
    );
    for (instance, generics) in std::mem::take(&mut unit.function_generics) {
        let generics = generics
            .iter()
            .map(|(name, id)| (std::sync::Arc::clone(name), types.id(*id)))
            .collect();
        into.function_generics.insert(instance, generics);
    }

    into.order = order.checked_add(unit.order)?;
    Some(())
}

struct Checkpoint {
    objects: usize,
    functions: usize,
    order: u64,
    pending_arguments: usize,
    record_member_declarations: usize,
    dynamic_record_layouts: HashMap<DynamicAggregateLayoutKey, ExpressionId>,
    void_type: Option<TypeId>,
    character_type: Option<TypeId>,
    budget: Meter,
    files: Files,
    pool: LocationsBuilder,
}

impl Checkpoint {
    fn take(
        walked: &Walked,
        types: &TypeArenaBuilder<'_, '_>,
        files: &Files,
        pool: &LocationsBuilder,
    ) -> Self {
        Self {
            objects: walked.objects.len(),
            functions: walked.functions.len(),
            order: walked.order,
            pending_arguments: types.pending_arguments.len(),
            record_member_declarations: types.record_member_declarations.len(),
            dynamic_record_layouts: types.dynamic_record_layouts.clone(),
            void_type: types.void_type,
            character_type: types.character_type,
            budget: types.budget.clone(),
            files: files.clone(),
            pool: pool.clone(),
        }
    }

    fn restore(
        self,
        walked: &mut Walked,
        types: &mut TypeArenaBuilder<'_, '_>,
        files: &mut Files,
        pool: &mut LocationsBuilder,
    ) {
        walked.objects.truncate(self.objects);
        walked.functions.truncate(self.functions);
        walked.calls = super::call_sites::CallSiteBuilder::default();
        walked.procedures.clear();
        walked.vtables.clear();
        walked.go_function_entries.clear();
        walked.unnamed_parameters.clear();
        walked.abstract_bodies.clear();
        walked.function_generics.clear();
        walked.order = self.order;
        types.by_die.clear_own();
        types.entries.clear_own();
        types.explicit_names.clear_own();
        types.identity_parts.clear_own();
        types.go_identity_parts.clear_own();
        types.go_dict_indices.clear_own();
        types.passed_by_value.clear_own();
        types.pending_arguments.truncate(self.pending_arguments);
        types
            .record_member_declarations
            .truncate(self.record_member_declarations);
        types.dynamic_record_layouts = self.dynamic_record_layouts;
        types.void_type = self.void_type;
        types.character_type = self.character_type;
        types.budget = self.budget;
        *files = self.files;
        *pool = self.pool;
    }
}

struct Merge<'m, 'a, 'data> {
    cx: &'m WalkContext<'m, 'data>,
    walked: &'m mut Walked,
    types: &'m mut TypeArenaBuilder<'a, 'data>,
    files: &'m mut Files,
    pool: &'m Mutex<LocationsBuilder>,
    declared_files: die::DeclaredFiles,

    groups: HashMap<usize, Vec<TypeId>>,
    in_order: foldhash::HashSet<usize>,

    kept: bool,
}

impl super::in_order::Add<UnitWalk> for Merge<'_, '_, '_> {
    fn add(&mut self, unit: Option<UnitWalk>) -> bool {
        self.absorb(unit);
        self.kept
    }
}

impl Merge<'_, '_, '_> {
    fn absorb(&mut self, unit: Option<UnitWalk>) {
        let Some(mut unit) = unit else {
            self.kept = false;
            return;
        };

        if !self.kept {
            return;
        }

        if self.in_order.contains(&unit.group) {
            self.walk_in_order(unit.unit);
            return;
        }

        if unit.result.is_err() || unit.types.budget.check().is_err() {
            self.kept = false;
            return;
        }

        let ids = self.groups.entry(unit.group).or_default();
        let Some(repeats) = self.types.matches(&unit.types, ids) else {
            self.in_order.insert(unit.group);
            self.walk_in_order(unit.unit);
            return;
        };

        let pool = &mut *self.pool.lock().unwrap_or_else(PoisonError::into_inner);
        if u32::try_from(
            pool.largest_table()
                .saturating_add(unit.pool.largest_table()),
        )
        .is_err()
        {
            self.kept = false;
            return;
        }

        let pooled_before = pool.counts();
        let Ok((expressions, lists)) = pool.absorb(&unit.pool) else {
            self.kept = false;
            return;
        };

        let pooled = Pooled { expressions, lists };
        let files = unit
            .files
            .paths()
            .iter()
            .map(|path| self.files.intern(path.clone()))
            .collect::<Vec<_>>();
        let Some(renumbered) = self
            .types
            .absorb(unit.types, &repeats, &pooled, pooled_before, ids)
        else {
            self.kept = false;
            return;
        };

        if absorb_walked(self.walked, &mut unit.walked, &renumbered, &pooled, &files).is_none() {
            self.kept = false;
        }
    }

    fn walk_in_order(&mut self, unit: usize) {
        crate::count!("units_rewalked_in_order", 1);
        let result = walk_unit(
            self.cx,
            unit,
            self.walked,
            self.types,
            self.files,
            &mut self.declared_files,
        );

        if result.is_err() || self.types.limit_type.is_some() || self.types.budget.check().is_err()
        {
            self.kept = false;
        }
    }
}

const PIECES_PER_WORKER: usize = 128;

pub(super) fn walk_units_apart<'data>(
    cx: &WalkContext<'_, 'data>,
    walked: &mut Walked,
    types: &mut TypeArenaBuilder<'_, 'data>,
    files: &mut Files,
    pool: &Mutex<LocationsBuilder>,
    walk_ahead: usize,
) -> bool {
    let units = &cx.catalog.units;

    if rayon::current_num_threads() < 2
        || units.iter().any(is_type_unit)
        || types.limit_type.is_some()
    {
        return false;
    }

    let _phase = crate::span!("variables.walk_apart");

    let Ok(groups) = units.groups() else {
        return false;
    };

    for (unit, &group) in groups.iter().enumerate() {
        debug_assert!(
            group <= unit && groups.get(group) == Some(&group),
            "unit {unit}'s group {group} is not named by the group's first unit"
        );
    }

    debug_assert!(walk_ahead > 0, "a walk window of no units waits forever");
    let mut connected = vec![Vec::new(); units.len()];

    for (unit, group) in groups.iter().enumerate() {
        let Some(group) = connected.get_mut(*group) else {
            return false;
        };

        group.push(unit);
    }

    let workers = rayon::current_num_threads();
    let share = units.len().div_ceil(workers);
    let piece = units
        .len()
        .div_ceil(workers.saturating_mul(PIECES_PER_WORKER))
        .max(1);
    let mut members = vec![Vec::new(); units.len()];

    for piece in connected.iter().flat_map(|group| {
        group.chunks(if group.len() > share {
            piece
        } else {
            group.len().max(1)
        })
    }) {
        if let Some(&first) = piece.first()
            && let Some(members) = members.get_mut(first)
        {
            members.extend_from_slice(piece);
        }
    }

    crate::count!(
        "unit_groups",
        members.iter().filter(|units| !units.is_empty()).count()
    );
    let frozen = types.frozen();
    debug_assert!(walked.calls.is_empty(), "only the walk records calls");
    let checkpoint = Checkpoint::take(
        walked,
        types,
        files,
        &pool.lock().unwrap_or_else(PoisonError::into_inner),
    );
    let merge = Merge {
        cx,
        walked,
        types,
        files,
        pool,
        declared_files: die::DeclaredFiles::default(),
        groups: HashMap::default(),
        in_order: foldhash::HashSet::default(),
        kept: true,
    };

    let order = members
        .iter()
        .enumerate()
        .filter_map(|(group, units)| (!units.is_empty()).then_some(group))
        .collect::<Vec<_>>();
    let in_order = InOrder::new(&order, &members, merge, walk_ahead.saturating_mul(workers));
    rayon::in_place_scope(|scope| {
        for _ in 0..workers {
            let (frozen, in_order) = (&frozen, &in_order);
            scope.spawn(move |_| {
                in_order.work(|group, units| walk_group(cx, frozen, group, units));
            });
        }
    });
    let (added, merge) = in_order.into_parts();
    debug_assert!(!merge.kept || added == units.len());
    if merge.kept {
        return true;
    }

    checkpoint.restore(
        merge.walked,
        merge.types,
        merge.files,
        &mut merge.pool.lock().unwrap_or_else(PoisonError::into_inner),
    );
    false
}

fn walk_group<'data>(
    cx: &WalkContext<'_, 'data>,
    frozen: &Frozen<'_, 'data>,
    group: usize,
    units: &[usize],
) -> Vec<(usize, Option<UnitWalk>)> {
    let pools = units
        .iter()
        .map(|_| Mutex::new(LocationsBuilder::default()))
        .collect::<Vec<_>>();
    let die_buffers = DieBuffers::default();
    let Some(first_pool) = pools.first() else {
        return Vec::new();
    };

    let mut types = frozen.split(first_pool, &die_buffers);
    let mut walks = Vec::with_capacity(units.len());
    let mut cuts = Vec::with_capacity(units.len());

    let mut names = super::Names::default();

    for (&unit_index, pool) in units.iter().zip(&pools) {
        types.pool = pool;
        let mark = types.budget.spent();
        let mut walked = Walked {
            names: std::mem::take(&mut names),
            ..Walked::default()
        };

        let mut files = Files::default();
        let result = walk_unit(
            cx,
            unit_index,
            &mut walked,
            &mut types,
            &mut files,
            &mut die::DeclaredFiles::default(),
        );
        names = std::mem::take(&mut walked.names);
        let left = result.is_err() || types.budget.check().is_err();
        cuts.push(Cut {
            end: types.entries.len(),
            aliases: types.trail.as_ref().map_or(0, |trail| trail.aliases.len()),
            budget: types.budget.since(mark),
        });
        walks.push((result, walked, files));
        if left {
            break;
        }
    }

    let Some(built) = types.into_unit_types(&cuts) else {
        return units.iter().map(|&unit| (unit, None)).collect();
    };

    let mut built = built.into_iter();
    let mut walks = walks.into_iter();

    units
        .iter()
        .zip(pools)
        .map(|(&unit, pool)| {
            let walk = walks
                .next()
                .zip(built.next())
                .map(|((result, walked, files), types)| UnitWalk {
                    unit,
                    group,
                    result,
                    walked,
                    types,
                    files,
                    pool: pool.into_inner().unwrap_or_else(PoisonError::into_inner),
                });
            (unit, walk)
        })
        .collect()
}

fn split_dies(
    mut by_die: DieMap<TypeId>,
    aliases: Vec<(DieKey, Option<usize>)>,
    cuts: &[Cut],
    base: usize,
    units: &mut [UnitTypes],
) -> Option<()> {
    for (position, (key, parent)) in aliases.into_iter().enumerate() {
        let Some(id) = by_die.remove(&key) else {
            debug_assert!(false, "an alias names its type");
            return None;
        };

        let unit = cuts.partition_point(|cut| cut.aliases <= position);
        owner(units, unit)?.aliases.push((key, id, parent));
    }

    for (key, id) in by_die.into_entries() {
        debug_assert!(
            id.index() >= base,
            "a walk's own DIE names type {}, below the shared {base}",
            id.index()
        );
        let unit = cuts.partition_point(|cut| cut.end <= id.index());
        owner(units, unit)?.by_die.insert(key, id);
    }

    Some(())
}

fn check_cut_order(cuts: &[Cut]) {
    debug_assert!(
        cuts.windows(2)
            .all(|pair| matches!(pair, [first, second] if first.end <= second.end && first.aliases <= second.aliases)),
        "a unit's walk ends before the walk of the unit before it"
    );
}
