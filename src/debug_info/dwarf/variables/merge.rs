//! Walking units apart, and putting what each built in order.
//!
//! Units are walked in groups: the units whose DIEs refer to one
//! another's, directly or through other units. Each group's walk reads
//! what the arena held before any unit was walked, walks the group's units
//! in order, and builds their types, locations, and files beside that
//! base; each unit's types are those built between where its walk began
//! and ended. Every unit's records are then added in unit order,
//! renumbered after the units before it, which numbers everything as one
//! walk of every unit in order does, so long as no unit reaches another
//! group's types: a walk in order would have built those once, where the
//! first unit reached them. A unit that does, or a load that runs out of
//! its budget or fails, is walked again in order instead.

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
use crate::debug_info::dwarf::{DieMap, DwarfError, is_type_unit};
use crate::image::lines::Files;
use crate::image::locations::ExpressionId;
use crate::image::variables::TypeResolution;
use crate::{SourceFileId, SourceLocation, TypeArgument, TypeId, TypeReference};

/// Where the lists and expressions one unit pooled apart are pooled.
pub(super) struct Pooled {
    expressions: Vec<ExpressionId>,
    lists: Vec<LocationListId>,
}

impl Pooled {
    pub(super) fn expression(&self, id: ExpressionId) -> ExpressionId {
        self.expressions[id.0 as usize]
    }

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

/// Where the types one group built apart are numbered, for the group's
/// units added so far.
struct Renumbered<'r> {
    /// Where the shared base ends, and the group's own types begin.
    base: usize,
    ids: &'r [TypeId],
}

impl Renumbered<'_> {
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

/// What one unit's walk built apart from other groups'.
struct UnitWalk {
    /// The unit's group, which its types are numbered within.
    group: usize,
    result: Result<(), DwarfError>,
    walked: Walked,
    types: UnitTypes,
    files: Files,
    pool: LocationsBuilder,
}

/// The types one unit's walk built, numbered within its group's past the
/// shared base.
pub(super) struct UnitTypes {
    /// Where its types begin among its group's.
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
    foreign: bool,
}

/// The arena as it was before any unit was walked, which each unit's walk
/// builds on.
struct Frozen<'a, 'data> {
    dwarf: &'a gimli::Dwarf<crate::debug_info::dwarf::Reader<'data>>,
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
    /// What every unit's walk builds on: this arena, frozen.
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
    /// An arena that builds the types of `home`, a group and each unit's
    /// group, on this base, pooling locations into `pool`.
    fn split<'b>(
        &'b self,
        home: (usize, &'b [usize]),
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
            home: Some(home),
            foreign: false,
            aliases: Vec::new(),
        }
    }
}

/// The type numbered `index`, if the number fits a type id.
fn type_id(index: usize) -> Option<TypeId> {
    u32::try_from(index).ok().map(TypeId::new)
}

/// Where one unit's walk ended in its group's arena.
struct Cut {
    /// How many types the arena held.
    end: usize,

    /// How many DIEs it had taken for another's type.
    aliases: usize,

    /// What the unit's walk spent.
    budget: Meter,
    foreign: bool,
}

impl TypeArenaBuilder<'_, '_> {
    /// What this arena, split from another, built past their base for each
    /// unit whose walk ended at `cuts`, in order. `None` if a DIE taken for
    /// another's type names no type.
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

        // The unit whose walk built a type, by its number.
        let unit_of = |id: usize| cuts.partition_point(|cut| cut.end <= id);
        let made = |made: Option<TypeId>, start: usize, end: usize| {
            made.filter(|id| (start..end).contains(&id.index()))
        };

        let mut units = cuts
            .iter()
            .enumerate()
            .map(|(index, cut)| {
                let start = index.checked_sub(1).map_or(base, |before| cuts[before].end);
                UnitTypes {
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
                    foreign: cut.foreign,
                }
            })
            .collect::<Vec<_>>();
        let mut entries = self.entries.into_own();
        debug_assert_eq!(
            base + entries.len(),
            cuts.last().map_or(base, |cut| cut.end)
        );

        for unit in units.iter_mut().rev() {
            unit.entries = entries.split_off(unit.start);
        }

        // A DIE taken for another's type goes to the unit that took it, and
        // a type's own to the unit that built it.
        let mut by_die = self.by_die.into_own();

        for (position, key) in self.aliases.into_iter().enumerate() {
            let Some(id) = by_die.remove(&key) else {
                debug_assert!(false, "an alias names its type");
                return None;
            };

            units[cuts.partition_point(|cut| cut.aliases <= position)]
                .by_die
                .insert(key, id);
        }

        for (key, id) in by_die.into_entries() {
            debug_assert!(
                id.index() >= base,
                "a walk's own DIE names type {}, below the shared {base}",
                id.index()
            );
            units[unit_of(id.index())].by_die.insert(key, id);
        }

        for id in self.explicit_names.into_own().iter() {
            units[unit_of(id)].explicit_names.insert(id);
        }

        let identity_start = self.identity_parts.base_len();

        for (own, parts) in self.identity_parts.into_own().into_iter().enumerate() {
            if let Some(parts) = parts {
                let id = identity_start + own;
                units[unit_of(id)].identity_parts.push((id, parts));
            }
        }

        for (id, parts) in self.go_identity_parts.into_own() {
            units[unit_of(id.index())]
                .go_identity_parts
                .insert(id, parts);
        }

        for (id, index) in self.go_dict_indices.into_own() {
            units[unit_of(id.index())].go_dict_indices.insert(id, index);
        }

        for (id, by_value) in self.passed_by_value.into_own() {
            units[unit_of(id.index())]
                .passed_by_value
                .insert(id, by_value);
        }

        for pending in self.pending_arguments {
            units[unit_of(pending.entry)]
                .pending_arguments
                .push(pending);
        }

        for declaration in self.record_member_declarations {
            units[unit_of(declaration.aggregate.index())]
                .record_member_declarations
                .push(declaration);
        }

        for (key, expression) in self.dynamic_record_layouts {
            units[unit_of(key.aggregate.index())]
                .dynamic_record_layouts
                .insert(key, expression);
        }

        Some(units)
    }

    /// The types an arena makes once, `void` and the character of strings
    /// that name none, that `unit` made, each with the one a unit before it
    /// made since, if any: the unit made one only when its group had none.
    fn made_once(&self, unit: &UnitTypes) -> [(Option<TypeId>, Option<TypeId>); 2] {
        [
            (unit.void_type, unit.void_type.and(self.void_type)),
            (
                unit.character_type,
                unit.character_type.and(self.character_type),
            ),
        ]
    }

    /// Adds the types a unit built apart after these, as building them
    /// here after the units before it would have numbered them, and
    /// numbers them among `ids`, its group's.
    #[expect(
        clippy::iter_over_hash_type,
        reason = "each entry of a map is added on its own, so their order cannot matter"
    )]
    fn absorb<'r>(
        &mut self,
        unit: UnitTypes,
        pooled: &Pooled,
        ids: &'r mut Vec<TypeId>,
    ) -> Option<Renumbered<'r>> {
        let base = self.entries.base_len();
        debug_assert!(
            unit.limit_type.is_none(),
            "a unit that exceeds the budget is walked in order"
        );
        debug_assert_eq!(ids.len(), unit.start, "a group's units are added in order");
        let first = base + unit.start;
        let made = self.made_once(&unit);
        let [(own_void, _), (own_character, _)] = made;
        let shared = |own: usize| {
            made.into_iter().find_map(|(made, before)| {
                before.filter(|_| made.is_some_and(|id| id.index() == first + own))
            })
        };

        let mut next = self.entries.len();
        let numbered = (0..unit.entries.len())
            .map(|own| {
                shared(own).map_or_else(
                    || {
                        let id = type_id(next);
                        next += 1;
                        id
                    },
                    Some,
                )
            })
            .collect::<Option<Vec<_>>>()?;
        ids.extend(numbered);
        let renumbered = Renumbered { base, ids };

        for (own, entry) in unit.entries.into_iter().enumerate() {
            if shared(own).is_some() {
                continue;
            }

            self.entries.push(match entry {
                TypeEntry::Resolved(mut info) => {
                    info.reference = renumbered.reference(info.reference);
                    dedup::map_references(&mut info, &mut |reference| {
                        renumbered.reference(reference)
                    });
                    TypeEntry::Resolved(info)
                }
                other => other,
            });
        }

        if self.void_type.is_none() {
            self.void_type = own_void.map(|id| renumbered.id(id));
        }

        if self.character_type.is_none() {
            self.character_type = own_character.map(|id| renumbered.id(id));
        }

        for own in unit.explicit_names.iter() {
            let id = type_id(own)?;
            self.explicit_names.insert(renumbered.id(id).index());
        }

        for (id, mut parts) in unit.identity_parts {
            for argument in &mut parts.template {
                if let TypeArgument::Type(reference) = argument {
                    *reference = renumbered.reference(*reference);
                }
            }

            let id = type_id(id)?;
            let index = renumbered.id(id).index();
            self.identity_parts.resize_with(index + 1, || None);
            self.identity_parts[index] = Some(parts);
        }

        for (id, mut parts) in unit.go_identity_parts {
            parts.map_references(|reference| renumbered.reference(reference));
            self.go_identity_parts.insert(renumbered.id(id), parts);
        }

        for (id, index) in unit.go_dict_indices {
            self.go_dict_indices.insert(renumbered.id(id), index);
        }

        for (id, by_value) in unit.passed_by_value {
            self.passed_by_value.insert(renumbered.id(id), by_value);
        }

        for mut pending in unit.pending_arguments {
            let id = type_id(pending.entry)?;
            pending.entry = renumbered.id(id).index();
            self.pending_arguments.push(pending);
        }

        self.record_member_declarations
            .extend(
                unit.record_member_declarations
                    .into_iter()
                    .map(|mut declaration| {
                        declaration.aggregate = renumbered.id(declaration.aggregate);
                        declaration
                    }),
            );

        for (mut key, expression) in unit.dynamic_record_layouts {
            key.aggregate = renumbered.id(key.aggregate);
            self.dynamic_record_layouts
                .insert(key, pooled.expression(expression));
        }

        let mut by_die = unit.by_die;
        by_die.map_values(|id| renumbered.id(id));
        self.by_die.extend_own(by_die);
        let absorbed = self.budget.absorb(&unit.budget);
        debug_assert!(absorbed, "units are absorbed within the budget");

        Some(renumbered)
    }
}

/// Adds what a later unit's walk recorded, renumbered as `types`,
/// `pooled`, and `files` say.
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
    let file = |location: &mut SourceLocation| location.file = files[location.file.index()];

    for mut object in unit.objects.drain(..) {
        types.resolution(&mut object.type_info);
        for id in [&mut object.escaped, &mut object.coroutine]
            .into_iter()
            .flatten()
        {
            *id = types.id(*id);
        }

        if let Some(declaration) = &mut object.declaration {
            file(declaration);
        }

        if let Some(declaration) = &mut object.go_declaration {
            file(&mut declaration.location);
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

/// What loading had recorded before any unit was walked, to go back to
/// when the units must be walked in order after all.
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

/// Where units' walks are added, in unit order, as they finish.
struct Merge<'m, 'a, 'data> {
    walked: &'m mut Walked,
    types: &'m mut TypeArenaBuilder<'a, 'data>,
    files: &'m mut Files,
    pool: &'m mut LocationsBuilder,

    /// Where each group's types added so far are numbered, by the group.
    groups: HashMap<usize, Vec<TypeId>>,

    /// Whether every unit added so far kept to its own types, the budget,
    /// and the tables' numbering.
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

        if !self.kept
            || unit.result.is_err()
            || unit.types.foreign
            || unit.types.budget.check().is_err()
            || unit.types.budget.spent() > self.types.budget.left()
            || u32::try_from(
                self.pool
                    .largest_table()
                    .saturating_add(unit.pool.largest_table()),
            )
            .is_err()
        {
            self.kept = false;
            return;
        }

        let Ok((expressions, lists)) = self.pool.absorb(&unit.pool) else {
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
        let ids = self.groups.entry(unit.group).or_default();
        let Some(renumbered) = self.types.absorb(unit.types, &pooled, ids) else {
            self.kept = false;
            return;
        };

        if absorb_walked(self.walked, &mut unit.walked, &renumbered, &pooled, &files).is_none() {
            self.kept = false;
        }
    }
}

/// Walks every group of units at once and adds what each unit recorded to
/// `walked`, `types`, `files`, and `pool` in unit order as soon as the
/// units before it are added, as walking them in order would have. Each
/// worker walks at most `walk_ahead` units past those added.
/// Returns `false`, having changed nothing, when walking them apart could
/// differ from walking them in order.
pub(super) fn walk_units_apart<'data>(
    cx: &WalkContext<'_, 'data>,
    walked: &mut Walked,
    types: &mut TypeArenaBuilder<'_, 'data>,
    files: &mut Files,
    pool: &Mutex<LocationsBuilder>,
    walk_ahead: usize,
) -> bool {
    let units = &cx.catalog.units;

    // One worker would only add copying each unit's records, and a type
    // unit's declarations name files by suffixes, which name the files
    // interned before them.
    if rayon::current_num_threads() < 2
        || units.iter().any(is_type_unit)
        || types.limit_type.is_some()
    {
        return false;
    }

    let _phase = crate::span!("variables.walk_apart");

    // A unit whose references cannot be read fails the walk in order.
    let Ok(groups) = units.groups() else {
        return false;
    };

    // Claiming in order needs each group named by its first unit.
    for (unit, &group) in groups.iter().enumerate() {
        debug_assert!(
            group <= unit && groups[group] == group,
            "unit {unit}'s group {group} is not named by the group's first unit"
        );
    }

    debug_assert!(walk_ahead > 0, "a walk window of no units waits forever");
    let mut members = vec![Vec::new(); units.len()];

    for (unit, group) in groups.iter().enumerate() {
        members[*group].push(unit);
    }

    crate::count!(
        "unit_groups",
        members.iter().filter(|units| !units.is_empty()).count()
    );
    let frozen = types.frozen();
    let pool = &mut *pool.lock().unwrap_or_else(PoisonError::into_inner);
    debug_assert!(walked.calls.is_empty(), "only the walk records calls");
    let checkpoint = Checkpoint::take(walked, types, files, pool);
    let merge = Merge {
        walked,
        types,
        files,
        pool,
        groups: HashMap::default(),
        kept: true,
    };

    let order = members
        .iter()
        .enumerate()
        .filter_map(|(group, units)| (!units.is_empty()).then_some(group))
        .collect::<Vec<_>>();
    let workers = rayon::current_num_threads();
    let in_order = InOrder::new(&order, &members, merge, walk_ahead.saturating_mul(workers));
    rayon::in_place_scope(|scope| {
        for _ in 0..workers {
            let (frozen, groups, in_order) = (&frozen, &groups, &in_order);
            scope.spawn(move |_| {
                in_order.work(|group, units| walk_group(cx, frozen, (group, groups), units));
            });
        }
    });
    let (added, merge) = in_order.into_parts();
    debug_assert!(!merge.kept || added == units.len());
    if merge.kept {
        return true;
    }

    checkpoint.restore(merge.walked, merge.types, merge.files, merge.pool);
    false
}

/// Walks the units of one group in order on the frozen arena, apart from
/// the other groups, and returns what each recorded, by unit. `home` is the
/// group and each unit's group. Once a unit fails, reaches another group's
/// types, or exceeds the budget, which leaves it to the walk in order, the
/// units after it go unwalked.
fn walk_group<'data>(
    cx: &WalkContext<'_, 'data>,
    frozen: &Frozen<'_, 'data>,
    home: (usize, &[usize]),
    units: &[usize],
) -> Vec<(usize, Option<UnitWalk>)> {
    // Each unit pools its locations apart, as they are pooled in unit order.
    let pools = units
        .iter()
        .map(|_| Mutex::new(LocationsBuilder::default()))
        .collect::<Vec<_>>();
    let die_buffers = DieBuffers::default();
    let mut types = frozen.split(home, &pools[0], &die_buffers);
    let mut walks = Vec::with_capacity(units.len());
    let mut cuts = Vec::with_capacity(units.len());

    // The group's units share the names they hold.
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
        let left = result.is_err() || types.foreign || types.budget.check().is_err();
        cuts.push(Cut {
            end: types.entries.len(),
            aliases: types.aliases.len(),
            budget: types.budget.since(mark),
            foreign: types.foreign,
        });
        walks.push((result, walked, files));
        if left {
            break;
        }
    }

    let Some(built) = types.into_unit_types(&cuts) else {
        // The units' types cannot be taken apart, so none of them is kept:
        // each is walked in order instead.
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
                    group: home.0,
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

/// Panics unless each unit's walk ends at or after the one before it, as
/// the searches of the cuts need.
fn check_cut_order(cuts: &[Cut]) {
    debug_assert!(
        cuts.windows(2)
            .all(|pair| pair[0].end <= pair[1].end && pair[0].aliases <= pair[1].aliases),
        "a unit's walk ends before the walk of the unit before it"
    );
}
