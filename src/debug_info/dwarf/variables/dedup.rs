//! Types no query can tell apart become one. Each unit describes the types
//! it uses, so a program built from many units holds many copies of each:
//! tokio-server's 60,839 types are 8,313 distinct ones.
//!
//! Two types merge only when everything about them is equal, the types
//! they refer to included, however deep and however cyclic: their
//! unfoldings are the same tree, so every walk of one is a walk of the
//! other. The coarsest such partition is found by refinement, starting
//! from each type's own fields and splitting until the types it refers to
//! agree. A type that keeps its unit's provenance, or whose layout is
//! computed at run time, is never merged.

use std::sync::Arc;

use std::collections::HashMap;

use foldhash::HashSet;
use rayon::prelude::*;

use super::types::{BuiltTypes, TypeEntry, TypeResolution};
use crate::type_identity::ANONYMOUS_NAMESPACE;
use crate::{
    BaseClass, RecordMember, TypeArgument, TypeId, TypeInfo, TypeKind, TypeReference,
    VariantDiscriminant,
};

/// How many rounds refinement may take. Each round costs one pass over
/// every reference, and real programs settle in a few; a module that does
/// not keeps all its types.
const MAX_ROUNDS: usize = 64;

/// Where each type went.
#[derive(Debug)]
pub(super) struct Remap {
    ids: Vec<TypeId>,
    retained: usize,
}

impl Remap {
    /// The new identifier of the type `id` named. An identifier past the
    /// types stays past them.
    pub(super) fn id(&self, id: TypeId) -> TypeId {
        self.ids.get(id.index()).copied().unwrap_or_else(|| {
            let beyond = id.get() - super::row(self.ids.len());
            TypeId::new(super::row(self.retained) + beyond)
        })
    }

    pub(super) fn resolution(&self, resolution: &mut TypeResolution) {
        if let TypeResolution::Resolved(id) = resolution {
            *id = self.id(*id);
        }
    }

    fn reference(&self, reference: TypeReference) -> TypeReference {
        TypeReference {
            image: reference.image,
            id: self.id(reference.id),
        }
    }
}

impl BuiltTypes {
    /// Merges the types no query can tell apart, keeping the first of each
    /// in identifier order and renumbering the rest densely. Returns where
    /// each type went, or `None` when no two types merged.
    pub(super) fn deduplicate(&mut self) -> Option<Remap> {
        self.deduplicate_with(&foldhash::fast::FixedState::default())
    }

    /// [`Self::deduplicate`], hashing with `hasher`, which only chooses
    /// candidates: equal hashes never merge types that differ.
    pub(super) fn deduplicate_with<H: std::hash::BuildHasher + Clone + Sync>(
        &mut self,
        hasher: &H,
    ) -> Option<Remap> {
        let mut classes = self.classes(hasher);
        let pending = std::mem::take(&mut self.pending_arguments);
        if !pending.is_empty() {
            // Types of one class are alike to every name, so matching names
            // against the first of each finds what matching them all would.
            // Resolving can make types equal that were not, so the classes
            // are found again.
            super::identity::resolve_parsed_arguments(
                &mut self.entries,
                self.image,
                &pending,
                classes.as_deref(),
            );
            classes = self.classes(hasher);
        }

        let classes = classes?;
        let mut new_of_class = vec![u32::MAX; classes.len()];
        let mut retained = 0_u32;
        let mut kept = vec![false; classes.len()];
        let ids = classes
            .iter()
            .zip(&mut kept)
            .map(|(class, kept)| {
                let slot = &mut new_of_class[*class as usize];
                if *slot == u32::MAX {
                    *slot = retained;
                    retained += 1;
                    *kept = true;
                }

                TypeId::new(*slot)
            })
            .collect::<Vec<_>>();
        let merged = ids.len() - retained as usize;
        crate::count!("types_merged", merged);
        if merged == 0 {
            return None;
        }

        let remap = Remap {
            ids,
            retained: retained as usize,
        };

        let _phase = crate::span!("types.deduplicate.apply");
        self.apply(&remap, &kept);
        Some(remap)
    }

    /// Each type's class, as [`refine`] finds them.
    ///
    /// Each type's signature is its own entry with its references zeroed in
    /// place, which are put back once refinement is done. Copying every
    /// type with its references zeroed instead cost a clone of every
    /// record's members, most of the work of finding classes, and held the
    /// copies beside the types at the load's peak: 450 MB on a large Rust
    /// program.
    fn classes<H: std::hash::BuildHasher + Clone + Sync>(
        &mut self,
        hasher: &H,
    ) -> Option<Vec<u32>> {
        let signatures_phase = crate::span!("types.deduplicate.signatures");
        let (mergeable, offsets, edges) = self.zero_references();
        let fields = self
            .entries
            .par_iter()
            .zip(&mergeable)
            .enumerate()
            .map(|(index, (entry, mergeable))| match entry {
                TypeEntry::Resolved(info) if *mergeable => {
                    let id = TypeId::new(super::row(index));
                    Some(Fields {
                        info,
                        go_dict_index: self.go_dict_indices.get(&id).copied(),
                        passed_by_value: self.passed_by_value.get(&id).copied(),
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        drop(signatures_phase);
        let refine_phase = crate::span!("types.deduplicate.refine");

        let classes = refine(
            &Signatures {
                fields,
                offsets: &offsets,
                edges: &edges,
            },
            hasher,
        );
        drop(refine_phase);

        let _phase = crate::span!("types.deduplicate.signatures");
        self.restore_references(&mergeable, &offsets, &edges);
        classes
    }

    /// Zeroes the references of every type that may merge, its own among
    /// them, and returns which types may, and their references in the order
    /// a walk of its fields meets them, by where each type's start.
    #[expect(
        clippy::disallowed_methods,
        reason = "which aggregates have run-time layouts is one set in any order"
    )]
    fn zero_references(&mut self) -> (Vec<bool>, Vec<usize>, Vec<u32>) {
        let count = self.entries.len();
        let layouts = self
            .dynamic_record_layouts
            .keys()
            .map(|key| key.aggregate)
            .collect::<HashSet<_>>();

        // Only a type's references, usually few, are gathered, and copied
        // into one list after.
        let (mergeable, edges): (Vec<bool>, Vec<Vec<u32>>) = self
            .entries
            .par_iter_mut()
            .enumerate()
            .map_init(Vec::new, |scratch, (index, entry)| {
                let id = TypeId::new(super::row(index));
                let TypeEntry::Resolved(info) = entry else {
                    return (false, Vec::new());
                };

                if layouts.contains(&id) || local(info) {
                    return (false, Vec::new());
                }

                scratch.clear();
                map_references(info, &mut |reference| {
                    scratch.push(reference);
                    zero(reference)
                });

                // A type that refers outside the graph keeps its own
                // identifier, so it never merges, and refers to nothing
                // refinement must follow.
                if scratch.iter().any(|reference| {
                    reference.image != info.reference.image || reference.id.index() >= count
                }) {
                    let mut original = scratch.iter().copied();
                    map_references(info, &mut |zeroed| {
                        let reference = original.next();
                        debug_assert!(reference.is_some(), "a type has as many references again");
                        reference.unwrap_or(zeroed)
                    });
                    return (false, Vec::new());
                }

                debug_assert_eq!(info.reference.id, id, "a type's own reference names it");
                info.reference = zero(info.reference);
                (
                    true,
                    scratch.iter().map(|reference| reference.id.get()).collect(),
                )
            })
            .unzip();
        let mut offsets = Vec::with_capacity(count + 1);
        offsets.push(0);
        let mut flat = Vec::with_capacity(edges.iter().map(Vec::len).sum());
        for own in edges {
            flat.extend_from_slice(&own);
            offsets.push(flat.len());
        }

        (mergeable, offsets, flat)
    }

    /// Puts back what [`Self::zero_references`] zeroed. Every reference of
    /// a type that may merge is to its own image.
    fn restore_references(&mut self, mergeable: &[bool], offsets: &[usize], edges: &[u32]) {
        self.entries
            .par_iter_mut()
            .enumerate()
            .for_each(|(index, entry)| {
                let TypeEntry::Resolved(info) = entry else {
                    return;
                };

                if !mergeable[index] {
                    return;
                }

                info.reference.id = TypeId::new(super::row(index));
                let image = info.reference.image;
                let mut original = edges[offsets[index]..offsets[index + 1]].iter();

                map_references(info, &mut |zeroed| {
                    let id = original.next();
                    debug_assert!(id.is_some(), "a type has as many references again");
                    id.map_or(zeroed, |id| TypeReference {
                        image,
                        id: TypeId::new(*id),
                    })
                });
            });
    }

    /// Keeps the first type of each class, pointing every reference where
    /// its type went.
    #[expect(
        clippy::disallowed_methods,
        clippy::iter_over_hash_type,
        reason = "each identifier moves on its own, so order does not matter"
    )]
    fn apply(&mut self, remap: &Remap, kept: &[bool]) {
        // An indexed collect splits alike however the work is stolen.
        let entries = std::mem::take(&mut self.entries)
            .into_par_iter()
            .enumerate()
            .map(|(index, mut entry)| {
                kept[index].then(|| {
                    if let TypeEntry::Resolved(info) = &mut entry {
                        map_references(info, &mut |reference| remap.reference(reference));
                        info.reference.id = remap.ids[index];
                    }

                    entry
                })
            })
            .collect::<Vec<_>>();
        self.entries = Vec::with_capacity(remap.retained);
        self.entries.extend(entries.into_iter().flatten());

        // Identifiers past the types begin at the retained count.
        debug_assert_eq!(
            self.entries.len(),
            remap.retained,
            "deduplication keeps one type per class, of {} classes",
            remap.retained
        );
        self.dynamic_record_layouts = std::mem::take(&mut self.dynamic_record_layouts)
            .into_iter()
            .map(|(mut key, layout)| {
                key.aggregate = remap.id(key.aggregate);
                (key, layout)
            })
            .collect();
        for id in self.complex_parts.values_mut() {
            *id = remap.id(*id);
        }

        // Merged types agree on these, which their signatures include.
        self.go_dict_indices = std::mem::take(&mut self.go_dict_indices)
            .into_iter()
            .map(|(id, index)| (remap.id(id), index))
            .collect();
        self.passed_by_value = std::mem::take(&mut self.passed_by_value)
            .into_iter()
            .map(|(id, by_value)| (remap.id(id), by_value))
            .collect();
    }
}

/// What a type is apart from the types it refers to: its entry, with its
/// references zeroed.
#[derive(PartialEq, Eq, Hash)]
struct Fields<'a> {
    info: &'a TypeInfo,
    go_dict_index: Option<u64>,
    passed_by_value: Option<bool>,
}

struct Signatures<'a> {
    /// Each type's fields, or `None` for one that never merges.
    fields: Vec<Option<Fields<'a>>>,

    /// Where each type's references start in `edges`.
    offsets: &'a [usize],
    edges: &'a [u32],
}

/// Whether a type belongs to its unit alone, as one in an anonymous
/// namespace does.
fn local(info: &TypeInfo) -> bool {
    info.identity.as_ref().is_some_and(|identity| {
        identity
            .path
            .iter()
            .any(|segment| segment.as_ref() == ANONYMOUS_NAMESPACE)
    })
}

const fn zero(reference: TypeReference) -> TypeReference {
    TypeReference {
        image: reference.image,
        id: TypeId::new(0),
    }
}

/// The coarsest partition in which types of one class have equal fields
/// and refer, position by position, to types of one class. Returns each
/// type's class, numbered by first appearance, or `None` when refinement
/// does not settle within [`MAX_ROUNDS`].
fn refine<H: std::hash::BuildHasher + Clone + Sync>(
    signatures: &Signatures<'_>,
    hasher: &H,
) -> Option<Vec<u32>> {
    let Signatures {
        ref fields,
        offsets,
        edges,
    } = *signatures;
    let count = fields.len();
    let mut classes = classify(fields, hasher);
    let mut members = Members::new(&classes);
    let referrers = Referrers::new(count, offsets, edges);

    // Each round splits a class by what its members' references were as
    // the round began, so a class can split only once a type it refers
    // to has moved. Only those classes are looked at again: a large
    // program takes dozens of rounds, and after the first few only a few
    // types move in each. The partition each round reaches, and so the
    // round refinement settles in, is that of splitting every class in
    // every round.
    let mut dirty = (0..members.ranges.len())
        .filter(|class| members.ranges[*class].len() > 1)
        .collect::<Vec<_>>();
    let mut flagged = Vec::new();

    for round in 1..=MAX_ROUNDS {
        // An indexed collect splits alike however the work is stolen.
        let splits = dirty
            .par_iter()
            .map_init(Scratch::default, |scratch, class| {
                split(*class, &members, &classes, offsets, edges, scratch, hasher)
            })
            .flatten()
            .collect::<Vec<_>>();
        if splits.is_empty() {
            crate::count!("types_dedup_rounds", round);
            return Some(numbered_by_first_appearance(&classes));
        }

        let mut moved = Vec::new();
        for (class, groups) in splits {
            members.split(class, &groups, &mut classes, &mut moved);
        }

        flagged.resize(members.ranges.len(), false);
        dirty.clear();
        for referrer in moved.iter().flat_map(|moved| referrers.of(*moved)) {
            let class = classes[*referrer as usize] as usize;
            if !std::mem::replace(&mut flagged[class], true) {
                dirty.push(class);
            }
        }

        for class in &dirty {
            flagged[*class] = false;
        }
    }

    crate::count!("types_dedup_abandoned", 1);
    None
}

/// What [`split`] reuses from one class to the next.
#[derive(Default)]
struct Scratch {
    /// The classes each member refers to, one member after another.
    keys: Vec<u32>,

    /// Where each member's run of `keys` ends.
    ends: Vec<usize>,
    groups: Vec<u32>,

    /// The first member of each group so far.
    firsts: Vec<usize>,
}

/// How many groups [`split`] tells apart by comparing a member with the
/// first of each before it hashes them: most classes split into few.
const LINEAR_GROUPS: usize = 8;

/// The group of each member of `class` when they are told apart by the
/// classes of the types they refer to, numbered by first appearance, or
/// `None` when they cannot be told apart.
fn split<H: std::hash::BuildHasher + Clone>(
    class: usize,
    members: &Members,
    classes: &[u32],
    offsets: &[usize],
    edges: &[u32],
    scratch: &mut Scratch,
    hasher: &H,
) -> Option<(usize, Vec<u32>)> {
    let group = members.of(class);
    if group.len() < 2 {
        return None;
    }

    let Scratch {
        keys,
        ends,
        groups,
        firsts,
    } = scratch;
    keys.clear();
    ends.clear();
    for member in group {
        let member = *member as usize;
        keys.extend(
            edges[offsets[member]..offsets[member + 1]]
                .iter()
                .map(|target| classes[*target as usize]),
        );
        ends.push(keys.len());
    }

    let key =
        |member: usize| &keys[member.checked_sub(1).map_or(0, |last| ends[last])..ends[member]];
    groups.clear();
    firsts.clear();
    let mut seen = None::<HashMap<&[u32], u32, H>>;

    for member in 0..group.len() {
        let own = key(member);
        let found = seen.as_ref().map_or_else(
            || {
                firsts
                    .iter()
                    .position(|first| key(*first) == own)
                    .map(super::row)
            },
            |seen| seen.get(own).copied(),
        );
        let found = found.unwrap_or_else(|| {
            let next = super::row(firsts.len());
            firsts.push(member);
            if let Some(seen) = &mut seen {
                seen.insert(own, next);
            } else if firsts.len() > LINEAR_GROUPS {
                let mut map = HashMap::with_capacity_and_hasher(firsts.len() * 2, hasher.clone());
                for (first, number) in firsts.iter().zip(0..) {
                    map.insert(key(*first), number);
                }

                seen = Some(map);
            }

            next
        });
        groups.push(found);
    }

    (firsts.len() > 1).then(|| (class, groups.clone()))
}

/// Each class's members, in identifier order, contiguous in one list.
struct Members {
    order: Vec<u32>,
    ranges: Vec<std::ops::Range<usize>>,
}

impl Members {
    fn new(classes: &[u32]) -> Self {
        let distinct = classes
            .iter()
            .copied()
            .max()
            .map_or(0, |last| last as usize + 1);
        let mut sizes = vec![0_usize; distinct];
        for class in classes {
            sizes[*class as usize] += 1;
        }

        let mut ranges = Vec::with_capacity(distinct);
        let mut start = 0;
        for size in sizes {
            ranges.push(start..start);
            start += size;
        }

        let mut order = vec![0_u32; classes.len()];
        for (index, class) in classes.iter().enumerate() {
            let range = &mut ranges[*class as usize];
            order[range.end] = super::row(index);
            range.end += 1;
        }

        Self { order, ranges }
    }

    fn of(&self, class: usize) -> &[u32] {
        &self.order[self.ranges[class].clone()]
    }

    /// Splits `class` into `groups`, its members' groups in order: the
    /// first group keeps the class, and each other becomes a new one.
    /// Records the members that moved.
    fn split(&mut self, class: usize, groups: &[u32], classes: &mut [u32], moved: &mut Vec<u32>) {
        let range = self.ranges[class].clone();

        // The members are zipped with their groups below.
        debug_assert_eq!(
            groups.len(),
            range.len(),
            "class {class} splits into groups for a different number of members"
        );
        let count = groups
            .iter()
            .copied()
            .max()
            .map_or(0, |last| last as usize + 1);
        let mut starts = vec![0_usize; count];
        for group in groups {
            starts[*group as usize] += 1;
        }

        let mut start = range.start;
        for size in &mut starts {
            let own = *size;
            *size = start;
            start += own;
        }

        let first_new = self.ranges.len();
        for (group, group_start) in starts.iter().enumerate() {
            let end = starts.get(group + 1).copied().unwrap_or(range.end);
            if group == 0 {
                self.ranges[class] = *group_start..end;
            } else {
                self.ranges.push(*group_start..end);
            }
        }

        let segment = self.order[range].to_vec();
        for (member, group) in segment.into_iter().zip(groups) {
            let group = *group as usize;
            self.order[starts[group]] = member;
            starts[group] += 1;
            if group > 0 {
                classes[member as usize] = super::row(first_new + group - 1);
                moved.push(member);
            }
        }
    }
}

/// The types that refer to each type.
struct Referrers {
    offsets: Vec<usize>,
    referrers: Vec<u32>,
}

impl Referrers {
    fn new(count: usize, offsets: &[usize], edges: &[u32]) -> Self {
        let mut starts = vec![0_usize; count + 1];
        for edge in edges {
            starts[*edge as usize + 1] += 1;
        }

        for index in 0..count {
            starts[index + 1] += starts[index];
        }

        let mut cursor = starts.clone();
        let mut referrers = vec![0_u32; edges.len()];
        for referrer in 0..count {
            for edge in &edges[offsets[referrer]..offsets[referrer + 1]] {
                referrers[cursor[*edge as usize]] = super::row(referrer);
                cursor[*edge as usize] += 1;
            }
        }

        Self {
            offsets: starts,
            referrers,
        }
    }

    fn of(&self, target: u32) -> &[u32] {
        let target = target as usize;
        &self.referrers[self.offsets[target]..self.offsets[target + 1]]
    }
}

/// `classes` renumbered in order of each class's first member.
fn numbered_by_first_appearance(classes: &[u32]) -> Vec<u32> {
    let mut numbers = vec![u32::MAX; classes.len()];
    let mut next = 0;
    classes
        .iter()
        .map(|class| {
            let number = &mut numbers[*class as usize];
            if *number == u32::MAX {
                *number = next;
                next += 1;
            }
            *number
        })
        .collect()
}

/// Numbers the types by their fields alone, in order of first appearance;
/// a type that never merges has a class of its own. Hashes are computed in
/// parallel and only propose the earlier type a type might equal; the
/// fields are then compared.
fn classify<H: std::hash::BuildHasher + Sync>(
    fields: &[Option<Fields<'_>>],
    hasher: &H,
) -> Vec<u32> {
    let digests = fields
        .par_iter()
        .map(|fields| fields.as_ref().map(|fields| hasher.hash_one(fields)))
        .collect::<Vec<_>>();
    let mut first_of_hash = foldhash::HashMap::default();
    let candidates = digests
        .iter()
        .enumerate()
        .map(|(index, hash)| hash.map_or(index, |hash| *first_of_hash.entry(hash).or_insert(index)))
        .collect::<Vec<_>>();
    let equal = candidates
        .par_iter()
        .enumerate()
        .map(|(index, candidate)| *candidate == index || fields[*candidate] == fields[index])
        .collect::<Vec<_>>();

    // A type unequal to the first of its hash equals only types that are
    // unequal to it too, so those are compared among themselves.
    let mut collided = HashMap::with_hasher(foldhash::fast::FixedState::default());
    let mut classes = Vec::<u32>::with_capacity(fields.len());
    let mut next = 0_u32;
    let mut fresh = || {
        next += 1;
        next - 1
    };

    for (index, candidate) in candidates.iter().enumerate() {
        let class = match &fields[index] {
            None => fresh(),
            Some(_) if *candidate == index => fresh(),
            Some(_) if equal[index] => classes[*candidate],
            Some(own) => *collided.entry(own).or_insert_with(&mut fresh),
        };

        classes.push(class);
    }

    classes
}

/// Replaces every reference of `info` to a type, its own aside, with `map`
/// of it, met in one fixed order. Shared lists are copied first, but a
/// type's lists are its own once its declarations are recorded.
pub(super) fn map_references(
    info: &mut TypeInfo,
    map: &mut impl FnMut(TypeReference) -> TypeReference,
) {
    match &mut info.kind {
        TypeKind::Enumeration { underlying, .. } => map_optional(underlying, map),
        TypeKind::Pointer { target, .. } | TypeKind::Named { target, .. } => {
            map_optional(target, map);
        }
        TypeKind::Reference { target, .. } | TypeKind::Modified { target, .. } => {
            *target = map(*target);
        }

        TypeKind::Array { element, .. }
        | TypeKind::RuntimeArray { element, .. }
        | TypeKind::Slice { element, .. } => {
            *element = map(*element);
        }
        TypeKind::Record { members, bases, .. } => {
            map_members(members, map);
            map_bases(bases, map);
        }
        TypeKind::Union { members, .. } => map_members(members, map),
        TypeKind::Variant {
            common_members,
            bases,
            discriminant,
            variants,
            ..
        } => {
            map_members(common_members, map);
            map_bases(bases, map);
            match discriminant.as_mut() {
                VariantDiscriminant::Stored(member) => member.type_ref = map(member.type_ref),
                VariantDiscriminant::TagType(target) => *target = map(*target),
                VariantDiscriminant::Absent => {}
            }

            if !variants.is_empty() {
                for variant in Arc::make_mut(variants) {
                    map_members(&mut variant.members, map);
                }
            }
        }

        TypeKind::Signature {
            returns,
            parameters,
            ..
        } => {
            map_optional(returns, map);
            if !parameters.is_empty() {
                for parameter in Arc::make_mut(parameters) {
                    *parameter = map(*parameter);
                }
            }
        }

        TypeKind::Base(_)
        | TypeKind::Unspecified
        | TypeKind::Function
        | TypeKind::Opaque { .. } => {}
    }

    if let Some(identity) = &mut info.identity
        && identity
            .arguments
            .iter()
            .any(|argument| matches!(argument, TypeArgument::Type(_)))
    {
        for argument in Arc::make_mut(&mut Arc::make_mut(identity).arguments) {
            if let TypeArgument::Type(reference) = argument {
                *reference = map(*reference);
            }
        }
    }
}

fn map_optional(
    reference: &mut Option<TypeReference>,
    map: &mut impl FnMut(TypeReference) -> TypeReference,
) {
    if let Some(reference) = reference {
        *reference = map(*reference);
    }
}

/// Maps each member's reference; an empty list, which types share, stays
/// itself.
fn map_members(
    members: &mut Arc<[RecordMember]>,
    map: &mut impl FnMut(TypeReference) -> TypeReference,
) {
    if !members.is_empty() {
        for member in Arc::make_mut(members) {
            member.type_ref = map(member.type_ref);
        }
    }
}

fn map_bases(bases: &mut Arc<[BaseClass]>, map: &mut impl FnMut(TypeReference) -> TypeReference) {
    if !bases.is_empty() {
        for base in Arc::make_mut(bases) {
            base.type_ref = map(base.type_ref);
        }
    }
}

#[cfg(test)]
mod tests;
