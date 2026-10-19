use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

use foldhash::HashMap;
use proptest::prelude::*;

use super::super::types::{
    BuiltTypes, DynamicAggregateChild, DynamicAggregateLayoutKey, TypeEntry,
};

use super::*;
use crate::{
    Accessibility, ModuleImageId, RecordKind, RecordMember, RecordMemberLayout, SourceLanguage,
    TypeIdentity, TypeInfo, TypeKind, TypeReference,
};

/// Hashes everything alike, so every type collides with every other.
#[derive(Clone, Copy)]
struct Colliding;

impl BuildHasher for Colliding {
    type Hasher = Constant;

    fn build_hasher(&self) -> Constant {
        Constant
    }
}

struct Constant;

impl Hasher for Constant {
    fn finish(&self) -> u64 {
        7
    }

    fn write(&mut self, _: &[u8]) {}
}

fn reference(id: u32) -> TypeReference {
    TypeReference {
        image: ModuleImageId::new(3),
        id: TypeId::new(id),
    }
}

fn info(id: u32, name: &str, kind: TypeKind) -> TypeInfo {
    TypeInfo {
        reference: reference(id),
        name: name.into(),
        byte_size: Some(8),
        kind,
        identity: None,
    }
}

fn pointer(id: u32, target: u32) -> TypeEntry {
    TypeEntry::Resolved(info(
        id,
        "*",
        TypeKind::Pointer {
            target: Some(reference(target)),
            address_class: 0,
        },
    ))
}

fn record(id: u32, name: &str, members: &[(&str, u32)]) -> TypeEntry {
    TypeEntry::Resolved(info(
        id,
        name,
        TypeKind::Record {
            kind: RecordKind::Struct,
            members: members
                .iter()
                .enumerate()
                .map(|(index, (name, ty))| RecordMember {
                    name: Some((*name).into()),
                    type_ref: reference(*ty),
                    layout: RecordMemberLayout::ByteOffset(index as u64 * 8),
                    accessibility: Accessibility::Public,
                    artificial: false,
                    embedded: false,
                    declaration: None,
                })
                .collect(),
            bases: Arc::from([]),
            incomplete: false,
        },
    ))
}

fn built(entries: Vec<TypeEntry>) -> BuiltTypes {
    BuiltTypes {
        entries,
        dynamic_record_layouts: HashMap::default(),
        complex_parts: HashMap::default(),
        go_dict_indices: HashMap::default(),
        passed_by_value: HashMap::default(),
        image: ModuleImageId::new(3),
        pending_arguments: Vec::new(),
    }
}

fn ids(remap: &Remap) -> Vec<u32> {
    remap.ids.iter().map(|id| id.get()).collect()
}

/// Two units' copies of a linked list's node, each pointing to itself
/// through its own pointer type, and a node that differs only in a
/// member's name.
fn linked_lists() -> Vec<TypeEntry> {
    vec![
        record(0, "Node", &[("next", 1)]),
        pointer(1, 0),
        record(2, "Node", &[("next", 3)]),
        pointer(3, 2),
        record(4, "Node", &[("link", 5)]),
        pointer(5, 4),
        // A pointer to the second copy, which follows it to the first.
        pointer(6, 2),
    ]
}

#[test]
fn copies_of_a_cyclic_type_merge_and_every_reference_follows() {
    let mut types = built(linked_lists());
    types
        .complex_parts
        .insert(("float".into(), 4), TypeId::new(3));
    types.passed_by_value.insert(TypeId::new(2), true);
    types.passed_by_value.insert(TypeId::new(0), true);
    let remap = types.deduplicate().expect("the copies merge");
    assert_eq!(ids(&remap), [0, 1, 0, 1, 2, 3, 1]);
    assert_eq!(
        types
            .entries
            .iter()
            .map(|entry| match entry {
                TypeEntry::Resolved(info) => info.clone(),
                _ => panic!("every type resolved"),
            })
            .collect::<Vec<_>>(),
        built(vec![
            record(0, "Node", &[("next", 1)]),
            pointer(1, 0),
            record(2, "Node", &[("link", 3)]),
            pointer(3, 2),
        ])
        .entries
        .iter()
        .map(|entry| match entry {
            TypeEntry::Resolved(info) => info.clone(),
            _ => unreachable!(),
        })
        .collect::<Vec<_>>()
    );
    assert_eq!(types.complex_parts[&("float".into(), 4)], TypeId::new(1));
    assert_eq!(types.passed_by_value.len(), 1);
    assert!(types.passed_by_value[&TypeId::new(0)]);

    // An identifier past the types stays past them.
    assert_eq!(remap.id(TypeId::new(7)), TypeId::new(4));

    // Colliding hashes only propose candidates.
    let mut colliding = built(linked_lists());
    let collided = colliding
        .deduplicate_with(&Colliding)
        .expect("the copies merge");
    assert_eq!(ids(&collided), ids(&remap));
}

/// A case's name, its types, and what else it says about them.
type Case = (&'static str, Vec<TypeEntry>, fn(&mut BuiltTypes));

#[test]
fn types_that_keep_their_provenance_never_merge() {
    let anonymous = |id| {
        let TypeEntry::Resolved(mut info) = record(id, "Hidden", &[]) else {
            unreachable!()
        };

        info.identity = Some(Arc::new(TypeIdentity {
            language: SourceLanguage::Cpp,
            path: Arc::from([Arc::from(ANONYMOUS_NAMESPACE)]),
            inline_namespaces: Arc::from([]),
            base: "Hidden".into(),
            arguments: Arc::from([]),
            pack: None,
            origin: crate::ArgumentOrigin::Dwarf,
            go: None,
        }));
        TypeEntry::Resolved(info)
    };

    let cases: [Case; 5] = [
        (
            "an anonymous namespace's",
            vec![anonymous(0), anonymous(1)],
            |_| {},
        ),
        (
            "a run-time layout's",
            vec![record(0, "Dynamic", &[]), record(1, "Dynamic", &[])],
            |types| {
                types.dynamic_record_layouts.insert(
                    DynamicAggregateLayoutKey {
                        aggregate: TypeId::new(1),
                        child: DynamicAggregateChild::Discriminant,
                    },
                    crate::image::locations::ExpressionId(0),
                );
            },
        ),
        (
            "another dictionary entry's",
            vec![record(0, "Shape", &[]), record(1, "Shape", &[])],
            |types| {
                types.go_dict_indices.insert(TypeId::new(0), 1);
                types.go_dict_indices.insert(TypeId::new(1), 2);
            },
        ),
        (
            "a malformed type's",
            vec![
                TypeEntry::Malformed("bad".into()),
                TypeEntry::Malformed("bad".into()),
            ],
            |_| {},
        ),
        (
            "a reference outside the graph's",
            vec![pointer(0, 9), pointer(1, 9)],
            |_| {},
        ),
    ];

    for (name, entries, change) in cases {
        let mut types = built(entries.clone());
        change(&mut types);
        assert!(types.deduplicate().is_none(), "{name} copies merged");

        // Signatures zero references in place, so this proves each came
        // back.
        assert_eq!(types.entries, entries, "{name}");
    }
}

#[test]
fn refinement_that_does_not_settle_keeps_every_type() {
    // Two chains of pointers that differ only at their ends, and a copy of
    // the first: a chain's depth is how many rounds it takes to tell apart.
    let chains = |depth: u32| {
        let mut entries = Vec::new();
        for (chain, end) in ["int", "long", "int"].into_iter().enumerate() {
            let base = u32::try_from(chain).unwrap() * (depth + 1);

            for link in 0..depth {
                entries.push(pointer(base + link, base + link + 1));
            }

            entries.push(TypeEntry::Resolved(info(
                base + depth,
                end,
                TypeKind::Unspecified,
            )));
        }

        built(entries)
    };

    let mut shallow = chains(8);
    let remap = shallow.deduplicate().expect("the copy merges");
    assert_eq!(shallow.entries.len(), 18);
    assert_eq!(remap.id(TypeId::new(18)), TypeId::new(0));

    let depth = u32::try_from(MAX_ROUNDS).unwrap() + 2;
    let mut deep = chains(depth);
    let entries = deep.entries.clone();
    assert!(deep.deduplicate().is_none());
    assert_eq!(deep.entries, entries);
}

/// The greatest relation in which related types have equal fields and
/// refer, position by position, to related types, found the slow way.
fn bisimilar(labels: &[Option<u8>], edges: &[Vec<u32>]) -> Vec<Vec<bool>> {
    let count = labels.len();
    let mut related = (0..count)
        .map(|left| {
            (0..count)
                .map(|right| {
                    left == right || (labels[left].is_some() && labels[left] == labels[right])
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    loop {
        let mut changed = false;
        for left in 0..count {
            for right in 0..count {
                if related[left][right]
                    && left != right
                    && (edges[left].len() != edges[right].len()
                        || edges[left]
                            .iter()
                            .zip(&edges[right])
                            .any(|(a, b)| !related[*a as usize][*b as usize]))
                {
                    related[left][right] = false;
                    changed = true;
                }
            }
        }

        if !changed {
            return related;
        }
    }
}

fn graph() -> impl Strategy<Value = (Vec<Option<u8>>, Vec<Vec<u32>>)> {
    (1_usize..9).prop_flat_map(|count| {
        let bound = u32::try_from(count).unwrap();
        (
            proptest::collection::vec(proptest::option::weighted(0.9, 0_u8..3), count),
            proptest::collection::vec(proptest::collection::vec(0..bound, 0..3), count),
        )
    })
}

/// Refines types with these labels and edges with an ordinary hasher and
/// one under which everything collides, and checks both against
/// [`bisimilar`].
fn check_refinement(labels: &[Option<u8>], edges: &[Vec<u32>]) -> Result<(), TestCaseError> {
    let infos = labels
        .iter()
        .map(|label| label.map(|label| info(0, &label.to_string(), TypeKind::Unspecified)))
        .collect::<Vec<_>>();
    let mut offsets = vec![0];
    let mut flat = Vec::new();
    for targets in edges {
        flat.extend(targets);
        offsets.push(flat.len());
    }

    let signatures = Signatures {
        fields: infos
            .iter()
            .map(|info| {
                info.as_ref().map(|info| Fields {
                    info,
                    go_dict_index: None,
                    passed_by_value: None,
                })
            })
            .collect(),
        offsets: &offsets,
        edges: &flat,
    };

    let classes = refine(&signatures, &foldhash::fast::FixedState::default()).unwrap();
    let related = bisimilar(labels, edges);

    for left in 0..labels.len() {
        for right in 0..labels.len() {
            prop_assert_eq!(classes[left] == classes[right], related[left][right]);
        }
    }

    prop_assert_eq!(refine(&signatures, &Colliding).unwrap(), classes);
    Ok(())
}

proptest! {
    #[test]
    fn refinement_merges_exactly_the_types_with_equal_unfoldings((labels, edges) in graph()) {
        check_refinement(&labels, &edges)?;
    }
}

#[test]
fn a_class_splits_into_more_groups_than_it_compares_one_by_one() {
    // Records alike but for the leaf each refers to: two leaves of each of
    // many labels, so that one round splits the records into many groups,
    // and the records whose leaves share a label stay together.
    let mut labels = Vec::new();
    let mut edges = Vec::new();

    for label in 0..u8::try_from(LINEAR_GROUPS * 2).unwrap() {
        for _ in 0..2 {
            labels.push(Some(label + 1));
            edges.push(Vec::new());
            labels.push(Some(0));
            edges.push(vec![u32::try_from(labels.len() - 2).unwrap()]);
        }
    }

    check_refinement(&labels, &edges).unwrap();
}

/// A type named by a label, with members, and generic arguments that are
/// types or a label its name spells.
type NamedType = (u8, Vec<u32>, Vec<Result<u32, u8>>);

/// Types named by a label, with members and generic arguments from the
/// edges, and arguments their names spell, which resolve to a label.
fn named_graph() -> impl Strategy<Value = Vec<NamedType>> {
    (1_usize..9).prop_flat_map(|count| {
        let bound = u32::try_from(count).unwrap();
        proptest::collection::vec(
            (
                0_u8..3,
                proptest::collection::vec(0..bound, 0..3),
                proptest::collection::vec(
                    prop_oneof![(0..bound).prop_map(Ok), (0_u8..4).prop_map(Err)],
                    0..3,
                ),
            ),
            count,
        )
    })
}

fn named_types(
    types: &[NamedType],
) -> (
    Vec<TypeEntry>,
    Vec<super::super::identity::PendingArguments>,
) {
    let label = |label: u8| ["A", "B", "C", "D"][usize::from(label)];
    let mut pending = Vec::new();
    let entries = types
        .iter()
        .enumerate()
        .map(|(index, (name, members, arguments))| {
            let id = u32::try_from(index).unwrap();
            let members = members
                .iter()
                .map(|member| ("m", *member))
                .collect::<Vec<_>>();
            let TypeEntry::Resolved(mut info) = record(id, label(*name), &members) else {
                unreachable!("a record is resolved");
            };

            let positions = arguments
                .iter()
                .enumerate()
                .filter(|(_, argument)| argument.is_err())
                .map(|(position, _)| position)
                .collect::<Vec<_>>();
            if !positions.is_empty() {
                pending.push(super::super::identity::PendingArguments {
                    entry: index,
                    language: SourceLanguage::Rust,
                    positions,
                });
            }

            info.identity = Some(Arc::new(TypeIdentity {
                language: SourceLanguage::Rust,
                path: Arc::from([]),
                inline_namespaces: Arc::from([]),
                base: label(*name).into(),
                arguments: arguments
                    .iter()
                    .map(|argument| match argument {
                        Ok(target) => TypeArgument::Type(reference(*target)),
                        Err(text) => TypeArgument::Unknown(label(*text).into()),
                    })
                    .collect(),
                pack: None,
                origin: crate::ArgumentOrigin::ParsedName,
                go: None,
            }));

            TypeEntry::Resolved(info)
        })
        .collect();

    (entries, pending)
}

proptest! {
    /// Resolving the arguments names spell while merging, searching one
    /// type of each class, gives the types and merges that resolving them
    /// first, searching every type, gives.
    #[test]
    fn arguments_resolve_while_merging_as_they_would_before(types in named_graph()) {
        let (entries, pending) = named_types(&types);
        let mut first = entries.clone();
        super::super::identity::resolve_parsed_arguments(
            &mut first,
            ModuleImageId::new(3),
            &pending,
            None,
        );

        let mut before = built(first);
        let expected = before.deduplicate().as_ref().map(ids);
        let mut merging = BuiltTypes {
            pending_arguments: pending,
            ..built(entries)
        };

        prop_assert_eq!(merging.deduplicate().as_ref().map(ids), expected);
        prop_assert_eq!(format!("{:?}", merging.entries), format!("{:?}", before.entries));
    }
}
