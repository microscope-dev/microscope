use std::collections::BTreeMap;
use std::sync::Arc;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::{AsyncFrame, AsyncFrameKind, AwaitChain, ChainEnd, MAX_DEPTH, walk};
use crate::runtime_model::{ImageSymbol, Member, RuntimeImage, RuntimeStop};
use crate::test_strategies::hostile_address;
use crate::{
    Accessibility, ArgumentOrigin, CoroutineInfo, CoroutineKind, CoroutineState,
    CoroutineStateKind, ImageAddress, IntegerValue, LineNumber, ModuleImageId, RecordKind,
    RecordMember, RecordMemberLayout, SourceFileId, SourceLanguage, SourceLocation, StateMember,
    ThreadId, ThreadLocal, TypeId, TypeIdentity, TypeInfo, TypeKind, TypeReference, Variant,
    VariantDiscriminant, VariantSelection, VariantSelector, VariantStorageKind, VirtualAddress,
};

const OUTER: u32 = 0;
const INNER: u32 = 1;
const SLEEP: u32 = 2;
const BOX: u32 = 3;
const PIN: u32 = 4;
const DYN: u32 = 5;
const DYN_POINTER: u32 = 6;
const VTABLE_POINTER: u32 = 7;
const DYN_BOX: u32 = 8;
const PIN_DYN: u32 = 9;
const OUTER_BOX: u32 = 10;
const PIN_OUTER: u32 = 11;
const WRAPPER: u32 = 12;
const INSTRUMENTED: u32 = 13;
const MANUALLY_DROP: u32 = 14;
const SPAN: u32 = 15;
const LOOKALIKE: u32 = 16;
const MAYBE_DANGLING: u32 = 17;
const TASK_LOCAL: u32 = 18;
const OPTION: u32 = 19;
const SOME: u32 = 20;
const NONE: u32 = 21;
const TAG: u32 = 22;
const PIN_DECLARED: u32 = 23;

const AWAITEE: u64 = 8;

const VTABLE: u64 = 0x5000;

const fn reference(id: u32) -> TypeReference {
    TypeReference {
        image: ModuleImageId::new(0),
        id: TypeId::new(id),
    }
}

fn line(line: u64) -> SourceLocation {
    SourceLocation {
        file: SourceFileId::new(0),
        line: LineNumber::new(line).expect("a line"),
        column: None,
    }
}

fn member(name: &str, ty: u32, offset: u64) -> RecordMember {
    RecordMember {
        name: Some(name.into()),
        type_ref: reference(ty),
        layout: RecordMemberLayout::ByteOffset(offset),
        accessibility: Accessibility::Public,
        artificial: false,
        embedded: false,
        declaration: None,
    }
}

fn record(id: u32, name: &str, members: Vec<RecordMember>) -> TypeInfo {
    TypeInfo {
        reference: reference(id),
        name: name.into(),
        byte_size: Some(32),
        kind: TypeKind::Record {
            kind: RecordKind::Struct,
            members: members.into(),
            bases: Arc::new([]),
            incomplete: false,
        },
        identity: None,
    }
}

fn pointer(id: u32, name: &str, target: Option<u32>) -> TypeInfo {
    TypeInfo {
        reference: reference(id),
        name: name.into(),
        byte_size: Some(8),
        kind: TypeKind::Pointer {
            target: target.map(reference),
            address_class: 0,
        },
        identity: None,
    }
}

fn named(id: u32, path: &[&str], base: &str, members: Vec<RecordMember>) -> TypeInfo {
    TypeInfo {
        identity: Some(Arc::new(TypeIdentity {
            language: SourceLanguage::Rust,
            path: path.iter().map(|&part| part.into()).collect(),
            inline_namespaces: Arc::new([]),
            base: base.into(),
            arguments: Arc::new([]),
            pack: None,
            origin: ArgumentOrigin::Dwarf,
            go: None,
        })),
        ..record(id, &format!("{base}<…>"), members)
    }
}

fn pin(id: u32, target: u32) -> TypeInfo {
    named(
        id,
        &["core", "pin"],
        "Pin",
        vec![member("pointer", target, 0)],
    )
}

fn pin_outer(id: u32) -> TypeInfo {
    TypeInfo {
        name: "Pin<Box<outer>>".into(),
        ..pin(id, OUTER_BOX)
    }
}

fn declaration(defined: TypeInfo) -> TypeInfo {
    TypeInfo {
        byte_size: None,
        kind: TypeKind::Record {
            kind: RecordKind::Struct,
            members: Arc::new([]),
            bases: Arc::new([]),
            incomplete: true,
        },
        ..defined
    }
}

fn state(value: u64, kind: CoroutineStateKind, at: u64, awaits: Option<u32>) -> CoroutineState {
    CoroutineState {
        value,
        kind,
        location: Some(line(at)),
        saved: awaits
            .map(|ty| member("__awaitee", ty, AWAITEE))
            .into_iter()
            .collect(),
    }
}

fn coroutine(states: Vec<CoroutineState>) -> CoroutineInfo {
    CoroutineInfo {
        kind: CoroutineKind::AsyncFunction,
        state: StateMember { offset: 0, size: 1 },
        states: states.into(),
        captures: Arc::new([]),
    }
}

fn option(held: u32) -> Vec<TypeInfo> {
    let variant = |tag: u64, name: &str, ty: u32| Variant {
        name: None,
        selection: VariantSelection::Selectors(Arc::new([VariantSelector::Value(
            IntegerValue::Unsigned(tag.into()),
        )])),
        members: Arc::new([member(name, ty, 0)]),
    };

    vec![
        TypeInfo {
            reference: reference(OPTION),
            name: "Option<…>".into(),
            byte_size: Some(32),
            kind: TypeKind::Variant {
                storage: VariantStorageKind::Struct,
                common_members: Arc::new([]),
                bases: Arc::new([]),
                discriminant: Box::new(VariantDiscriminant::Stored(member("tag", TAG, 0))),
                variants: Arc::new([variant(0, "None", NONE), variant(1, "Some", SOME)]),
                incomplete: false,
            },
            identity: None,
        },
        record(SOME, "Some", vec![member("__0", held, AWAITEE)]),
        record(NONE, "None", Vec::new()),
        TypeInfo {
            byte_size: Some(1),
            ..record(TAG, "u8", Vec::new())
        },
    ]
}

const fn suspended(index: u32) -> CoroutineStateKind {
    CoroutineStateKind::Suspended { index }
}

#[derive(Debug)]
struct Types {
    types: Vec<TypeInfo>,
    coroutines: BTreeMap<TypeReference, CoroutineInfo>,
}

impl Types {
    fn new() -> Self {
        let types: Vec<TypeInfo> = vec![
            record(OUTER, "outer", Vec::new()),
            record(INNER, "inner", Vec::new()),
            record(
                SLEEP,
                "Sleep",
                vec![member("deadline", BOX, 0), member("entry", BOX, 8)],
            ),
            pointer(BOX, "Box<inner>", Some(INNER)),
            pin(PIN, BOX),
            record(DYN, "(dyn Future<Output = ()> + Send)", Vec::new()),
            pointer(
                DYN_POINTER,
                "*mut (dyn Future<Output = ()> + Send)",
                Some(DYN),
            ),
            pointer(VTABLE_POINTER, "&[usize; 4]", None),
            record(
                DYN_BOX,
                "Box<(dyn Future<Output = ()> + Send)>",
                vec![
                    member("pointer", DYN_POINTER, 0),
                    member("vtable", VTABLE_POINTER, 8),
                ],
            ),
            pin(PIN_DYN, DYN_BOX),
            pointer(OUTER_BOX, "Box<outer>", Some(OUTER)),
            pin_outer(PIN_OUTER),
            record(WRAPPER, "Coop<inner>", vec![member("fut", INNER, 0)]),
            named(
                INSTRUMENTED,
                &["tracing", "instrument"],
                "Instrumented",
                vec![member("inner", MANUALLY_DROP, 0), member("span", SPAN, 8)],
            ),
            named(
                MANUALLY_DROP,
                &["core", "mem", "manually_drop"],
                "ManuallyDrop",
                vec![member("value", MAYBE_DANGLING, 0)],
            ),
            record(
                SPAN,
                "Span",
                vec![member("inner", BOX, 0), member("meta", BOX, 8)],
            ),
            named(
                LOOKALIKE,
                &["lookalike"],
                "Instrumented",
                vec![member("inner", MANUALLY_DROP, 0), member("span", SPAN, 8)],
            ),
            named(
                MAYBE_DANGLING,
                &["core", "mem", "maybe_dangling"],
                "MaybeDangling",
                vec![member("__0", PIN, 0)],
            ),
            named(
                TASK_LOCAL,
                &["tokio", "task", "task_local"],
                "TaskLocalFuture",
                vec![member("future", OPTION, 0)],
            ),
        ]
        .into_iter()
        .chain(option(INNER))
        .chain([declaration(pin_outer(PIN_DECLARED))])
        .collect();
        let outer = coroutine(vec![
            state(0, CoroutineStateKind::Unresumed, 10, None),
            state(1, CoroutineStateKind::Returned, 19, None),
            state(2, CoroutineStateKind::Panicked, 19, None),
            state(3, suspended(0), 12, Some(INNER)),
            state(4, suspended(1), 13, Some(PIN)),
            state(5, suspended(2), 14, Some(PIN_DYN)),
            state(6, suspended(3), 15, None),
            state(7, suspended(4), 16, Some(PIN_OUTER)),
            state(8, suspended(5), 17, Some(WRAPPER)),
            state(10, suspended(6), 18, Some(INSTRUMENTED)),
            state(11, suspended(7), 18, Some(LOOKALIKE)),
            state(12, suspended(8), 18, Some(TASK_LOCAL)),
        ]);
        let inner = coroutine(vec![
            state(0, CoroutineStateKind::Unresumed, 20, None),
            state(3, suspended(0), 21, Some(SLEEP)),
        ]);

        Self {
            types,
            coroutines: [(reference(OUTER), outer), (reference(INNER), inner)].into(),
        }
    }
}

impl RuntimeImage for Types {
    fn producers(&self) -> Vec<&str> {
        Vec::new()
    }

    fn constant(&self, _name: &str) -> Option<IntegerValue> {
        None
    }

    fn symbol(&self, _name: &str) -> Option<ImageSymbol> {
        None
    }

    fn function_answering(&self, _name: &str) -> Option<ImageSymbol> {
        None
    }

    fn symbol_at(&self, _address: ImageAddress) -> Option<Arc<str>> {
        None
    }

    fn has_function(&self, _name: &str) -> bool {
        false
    }

    fn function_body(&self, _name: &str) -> Option<ImageAddress> {
        None
    }

    fn member(&self, _type_name: &str, _path: &[&str]) -> Option<Member> {
        None
    }

    fn function_name(&self, _address: ImageAddress) -> Option<Arc<str>> {
        None
    }

    fn thread_local(&self, _name: &str) -> Option<Result<ThreadLocal, Arc<str>>> {
        None
    }

    fn types_named(&self, name: &str) -> Vec<TypeReference> {
        self.types
            .iter()
            .filter(|info| {
                info.identity.as_ref().is_some_and(|identity| {
                    format!("{}::{}", identity.path.join("::"), info.name) == name
                })
            })
            .map(|info| info.reference)
            .collect()
    }

    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        self.types.get(ty.id.index())
    }

    fn coroutine(&self, ty: TypeReference) -> Option<Result<&CoroutineInfo, &Arc<str>>> {
        self.coroutines.get(&ty).map(Ok)
    }

    fn trait_object_type(&self, address: ImageAddress) -> Option<TypeReference> {
        (address.get() == VTABLE).then_some(reference(INNER))
    }
}

#[derive(Default)]
struct Memory(BTreeMap<u64, u8>);

impl Memory {
    fn byte(&mut self, address: u64, value: u8) {
        self.0.insert(address, value);
    }

    fn word(&mut self, address: u64, value: u64) {
        for (index, byte) in (0..).zip(value.to_le_bytes()) {
            self.0.insert(address + index, byte);
        }
    }
}

impl RuntimeStop for Memory {
    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
        for (index, byte) in (0..).zip(bytes.iter_mut()) {
            let Some(read) = address
                .get()
                .checked_add(index)
                .and_then(|at| self.0.get(&at))
            else {
                return false;
            };
            *byte = *read;
        }

        true
    }

    fn thread_pointer(&self, _thread: ThreadId) -> Option<u64> {
        None
    }

    fn instruction(&self, _thread: ThreadId) -> Option<VirtualAddress> {
        None
    }

    fn load_bias(&self) -> crate::LoadBias {
        crate::LoadBias::default()
    }

    fn threads(&self) -> Vec<ThreadId> {
        Vec::new()
    }
}

fn coroutine_frame(
    object: u64,
    ty: u32,
    state: u64,
    kind: CoroutineStateKind,
    at: u64,
) -> AsyncFrame {
    AsyncFrame {
        object: VirtualAddress::new(object),
        ty: reference(ty),
        kind: AsyncFrameKind::Coroutine {
            state,
            kind,
            location: Some(line(at)),
        },
    }
}

fn leaf(object: u64, ty: u32) -> AsyncFrame {
    AsyncFrame {
        object: VirtualAddress::new(object),
        ty: reference(ty),
        kind: AsyncFrameKind::Leaf,
    }
}

fn walk_from(memory: &Memory, object: u64, ty: u32) -> AwaitChain {
    walk(
        &Types::new(),
        memory,
        VirtualAddress::new(object),
        reference(ty),
    )
}

#[test]
fn a_trait_object_of_an_unknown_vtable_ends_the_chain() {
    let outer = 0x1000;
    let boxed = 0x2000;
    let mut memory = Memory::default();
    memory.byte(boxed, 3);
    memory.byte(outer, 5);
    memory.word(outer + AWAITEE, boxed);
    memory.word(outer + AWAITEE + 8, VTABLE + 8);
    let unknown = walk_from(&memory, outer, OUTER);
    assert!(
        matches!(&unknown.end, ChainEnd::Broken(reason) if reason.contains("vtable")),
        "{unknown:?}"
    );
}

#[test]
fn every_end_of_a_chain_is_said() {
    let outer = 0x1000;
    let mut memory = Memory::default();
    memory.byte(outer, 9);
    let chain = walk_from(&memory, outer, OUTER);
    assert!(
        matches!(&chain.end, ChainEnd::Broken(reason) if reason.contains("no state, 9")),
        "{chain:?}"
    );
    assert!(chain.frames.is_empty());

    let chain = walk_from(&memory, 0x9000, OUTER);
    assert!(
        matches!(&chain.end, ChainEnd::Broken(reason) if reason.contains("unreadable")),
        "{chain:?}"
    );
}

#[test]
fn a_chain_reads_each_coroutine_in_the_state_its_caller_chooses() {
    let outer = 0x1000;
    let mut memory = Memory::default();

    memory.byte(outer, 0);
    memory.byte(outer + AWAITEE, 3);
    assert_eq!(walk_from(&memory, outer, OUTER).end, ChainEnd::Unresumed);

    let mut asked = Vec::new();
    let chain = super::walk_choosing(
        &Types::new(),
        &memory,
        VirtualAddress::new(outer),
        reference(OUTER),
        &mut |at, _, _| {
            asked.push(at);
            if at == 0 {
                super::StateChoice::Chosen(3)
            } else {
                super::StateChoice::Stored
            }
        },
    );
    assert_eq!(
        chain,
        AwaitChain {
            frames: vec![
                leaf(outer + 2 * AWAITEE, SLEEP),
                coroutine_frame(outer + AWAITEE, INNER, 3, suspended(0), 21),
                coroutine_frame(outer, OUTER, 3, suspended(0), 12),
            ],
            end: ChainEnd::Leaf,
        }
    );
    assert_eq!(asked, [0, 1]);

    let chain = super::walk_choosing(
        &Types::new(),
        &memory,
        VirtualAddress::new(outer),
        reference(OUTER),
        &mut |_, _, _| super::StateChoice::Unknown,
    );
    assert!(chain.frames.is_empty(), "{chain:?}");
    assert!(
        matches!(&chain.end, ChainEnd::Broken(reason) if reason.contains("unknown")),
        "{chain:?}"
    );
}

#[test]
fn a_pin_declared_without_its_members_points_where_its_definition_does() {
    let types = Types::new();
    assert_eq!(
        super::pinned_target(&types, reference(PIN_DECLARED)),
        Some(reference(OUTER))
    );
    assert_eq!(
        super::pinned_target(&types, reference(PIN_OUTER)),
        Some(reference(OUTER))
    );
}

#[test]
fn a_chain_ends_at_a_cycle_and_at_its_depth() {
    let outer = 0x1000;
    let mut memory = Memory::default();
    memory.byte(outer, 7);
    memory.word(outer + AWAITEE, outer);
    let cycle = walk_from(&memory, outer, OUTER);
    assert_eq!(cycle.end, ChainEnd::Cycle);
    assert_eq!(cycle.frames.len(), 1);

    let mut memory = Memory::default();
    let count = 200_u64;
    for index in 0..count {
        let at = 0x1_0000 + index * 0x100;
        memory.byte(at, 7);
        memory.word(at + AWAITEE, at + 0x100);
    }

    let deep = walk_from(&memory, 0x1_0000, OUTER);
    assert_eq!(deep.end, ChainEnd::TooDeep);
    assert!(deep.frames.len() < MAX_DEPTH);
}

#[test]
fn a_held_future_never_wraps_past_the_top_of_memory() {
    let outer = u64::MAX - 15;
    let mut memory = Memory::default();

    memory.byte(outer, 12);
    memory.byte(outer + AWAITEE, 1);
    memory.byte(0, 0);
    let chain = walk_from(&memory, outer, OUTER);
    assert_eq!(chain.frames.len(), 1, "{chain:?}");
    assert!(matches!(chain.end, ChainEnd::Broken(_)), "{chain:?}");

    memory.byte(outer, 5);
    memory.word(outer + AWAITEE, 0x2000);
    memory.word(0, VTABLE);
    memory.byte(0x2000, 0);
    let chain = walk_from(&memory, outer, OUTER);
    assert_eq!(chain.frames.len(), 1, "{chain:?}");
    assert!(matches!(chain.end, ChainEnd::Broken(_)), "{chain:?}");
}

const SLOTS: usize = 64;

fn slot(index: usize) -> u64 {
    0x1_0000 + 0x40 * index as u64
}

#[derive(Debug, Clone, Copy)]
enum Innermost {
    Outer(u8),

    Inner { via: u8, state: u8 },

    Dropped,
}

fn innermost() -> impl Strategy<Value = Innermost> {
    prop_oneof![
        prop::sample::select(vec![0_u8, 1, 2, 6, 11]).prop_map(Innermost::Outer),
        (
            prop::sample::select(vec![3_u8, 4, 5, 8, 10, 12]),
            prop::sample::select(vec![0_u8, 3]),
        )
            .prop_map(|(via, state)| Innermost::Inner { via, state }),
        Just(Innermost::Dropped),
    ]
}

#[derive(Debug, Clone)]
struct Model {
    hops: usize,
    back: Option<prop::sample::Index>,
    innermost: Innermost,
    slots: Vec<usize>,

    damage: Vec<Option<u64>>,
}

fn model(damaged: bool) -> impl Strategy<Value = Model> {
    let valid: Vec<u64> = (0..SLOTS).map(slot).chain([VTABLE]).collect();
    let damage = proptest::option::weighted(0.2, hostile_address(valid));
    (
        0..20_usize,
        proptest::option::weighted(0.2, any::<prop::sample::Index>()),
        innermost(),
        Just((0..SLOTS).collect::<Vec<_>>()).prop_shuffle(),
        proptest::collection::vec(damage, if damaged { 0..24 } else { 0..1 }),
    )
        .prop_map(|(hops, back, innermost, slots, damage)| Model {
            hops,
            back,
            innermost,
            slots,
            damage,
        })
}

fn laid_out(model: &Model) -> (Memory, AwaitChain, bool) {
    let mut memory = Memory::default();
    let mut damage = model.damage.iter();
    let mut damaged = false;
    let mut pointer = |memory: &mut Memory, at: u64, value: u64| {
        let written = damage.next().copied().flatten().unwrap_or(value);
        damaged |= written != value;
        memory.word(at, written);
    };

    let outers: Vec<u64> = model.slots[..=model.hops]
        .iter()
        .map(|&index| slot(index))
        .collect();
    let boxed = slot(model.slots[model.hops + 1]);

    let mut frames = Vec::new();
    for (hop, &at) in outers[..model.hops].iter().enumerate() {
        memory.byte(at, 7);
        let next = match model.back {
            Some(back) if hop + 1 == model.hops => outers[back.index(model.hops)],
            _ => outers[hop + 1],
        };
        pointer(&mut memory, at + AWAITEE, next);
        frames.push(coroutine_frame(at, OUTER, 7, suspended(4), 16));
    }

    let end = if model.back.is_some() && model.hops > 0 {
        ChainEnd::Cycle
    } else {
        let outer = outers[model.hops];
        match model.innermost {
            Innermost::Outer(state) => {
                memory.byte(outer, state);
                let (kind, at, end) = match state {
                    0 => (CoroutineStateKind::Unresumed, 10, ChainEnd::Unresumed),
                    1 => (CoroutineStateKind::Returned, 19, ChainEnd::Finished),
                    2 => (CoroutineStateKind::Panicked, 19, ChainEnd::Finished),
                    6 => (suspended(3), 15, ChainEnd::NoAwaitee),
                    _ => (suspended(7), 18, ChainEnd::Leaf),
                };
                frames.push(coroutine_frame(outer, OUTER, state.into(), kind, at));
                if end == ChainEnd::Leaf {
                    frames.push(leaf(outer + AWAITEE, LOOKALIKE));
                }
                end
            }
            Innermost::Dropped => {
                memory.byte(outer, 12);
                memory.byte(outer + AWAITEE, 0);
                frames.push(coroutine_frame(outer, OUTER, 12, suspended(8), 18));
                ChainEnd::Broken("it is None".into())
            }
            Innermost::Inner { via, state } => {
                memory.byte(outer, via);
                let (index, at, inner) = match via {
                    3 => (0, 12, outer + AWAITEE),
                    4 => (1, 13, boxed),
                    5 => (2, 14, boxed),
                    8 => (5, 17, outer + AWAITEE),
                    10 => (6, 18, boxed),
                    _ => (8, 18, outer + 2 * AWAITEE),
                };
                match via {
                    4 | 10 => pointer(&mut memory, outer + AWAITEE, boxed),
                    5 => {
                        pointer(&mut memory, outer + AWAITEE, boxed);
                        pointer(&mut memory, outer + 2 * AWAITEE, VTABLE);
                    }
                    12 => memory.byte(outer + AWAITEE, 1),
                    _ => {}
                }
                frames.push(coroutine_frame(
                    outer,
                    OUTER,
                    via.into(),
                    suspended(index),
                    at,
                ));

                memory.byte(inner, state);
                if state == 0 {
                    frames.push(coroutine_frame(
                        inner,
                        INNER,
                        0,
                        CoroutineStateKind::Unresumed,
                        20,
                    ));
                    ChainEnd::Unresumed
                } else {
                    frames.push(coroutine_frame(inner, INNER, 3, suspended(0), 21));
                    frames.push(leaf(inner + AWAITEE, SLEEP));
                    ChainEnd::Leaf
                }
            }
        }
    };

    frames.reverse();
    (memory, AwaitChain { frames, end }, damaged)
}

fn check_shape(chain: &AwaitChain) -> Result<(), TestCaseError> {
    prop_assert!(chain.frames.len() <= MAX_DEPTH);

    let mut seen = std::collections::BTreeSet::new();
    for frame in &chain.frames {
        prop_assert!(seen.insert((frame.object, frame.ty)), "{:?}", chain);
    }

    let awaiting = |frame: &AsyncFrame| {
        matches!(
            frame.kind,
            AsyncFrameKind::Coroutine {
                kind: CoroutineStateKind::Suspended { .. },
                ..
            }
        )
    };
    for frame in chain.frames.iter().skip(1) {
        prop_assert!(awaiting(frame), "{:?}", chain);
    }

    let innermost = chain.frames.first().map(|frame| &frame.kind);
    match (&chain.end, innermost) {
        (ChainEnd::Leaf, Some(AsyncFrameKind::Leaf))
        | (
            ChainEnd::Unresumed,
            Some(AsyncFrameKind::Coroutine {
                kind: CoroutineStateKind::Unresumed,
                ..
            }),
        )
        | (
            ChainEnd::Finished,
            Some(AsyncFrameKind::Coroutine {
                kind: CoroutineStateKind::Returned | CoroutineStateKind::Panicked,
                ..
            }),
        ) => {}
        (ChainEnd::NoAwaitee, Some(_)) => prop_assert!(awaiting(&chain.frames[0])),
        (ChainEnd::Broken(_) | ChainEnd::TooDeep | ChainEnd::Cycle, _) => {
            prop_assert!(chain.frames.iter().all(awaiting), "{:?}", chain);
        }
        _ => prop_assert!(
            false,
            "the end disagrees with the innermost future: {:?}",
            chain
        ),
    }

    Ok(())
}

proptest! {
    /// A chain laid out from a model reads as exactly the model's
    /// futures, innermost first, and ends where the model does.
    #[test]
    fn a_laid_out_chain_reads_as_its_model(model in model(false)) {
        let (memory, expected, _) = laid_out(&model);
        let chain = walk_from(&memory, slot(model.slots[0]), OUTER);
        check_shape(&chain)?;

        prop_assert_eq!(&chain.frames, &expected.frames);
        match (&chain.end, &expected.end) {
            (ChainEnd::Broken(reason), ChainEnd::Broken(part)) => {
                prop_assert!(reason.contains(&**part), "{}", reason);
            }
            (end, expected) => prop_assert_eq!(end, expected),
        }
    }

    /// Wherever a pointer of a chain leads instead, the walk reads a
    /// chain of the same shape, and exactly the model's where it leads
    /// where the model says.
    #[test]
    fn a_damaged_chain_keeps_its_shape(model in model(true)) {
        let (memory, expected, damaged) = laid_out(&model);
        let chain = walk_from(&memory, slot(model.slots[0]), OUTER);
        check_shape(&chain)?;
        if !damaged && !matches!(expected.end, ChainEnd::Broken(_)) {
            prop_assert_eq!(chain, expected);
        }
    }
}
