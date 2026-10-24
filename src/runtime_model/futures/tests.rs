//! The walk over futures the tests lay out by hand: coroutines awaiting
//! one another directly, through a pinned box, and through a trait object,
//! every way a chain can end, and memory of any content.

use std::collections::BTreeMap;
use std::sync::Arc;

use proptest::prelude::*;

use super::{AsyncFrame, AsyncFrameKind, AwaitChain, ChainEnd, MAX_DEPTH, walk};
use crate::runtime_model::{ImageSymbol, Member, RuntimeImage, RuntimeStop};
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

/// Where the awaited future lies in each coroutine.
const AWAITEE: u64 = 8;

/// The vtable whose trait object holds an `INNER`.
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

/// A Rust record named `base` within `path`.
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

/// Rust's `Pin` around a pointer of type `target`.
fn pin(id: u32, target: u32) -> TypeInfo {
    named(
        id,
        &["core", "pin"],
        "Pin",
        vec![member("pointer", target, 0)],
    )
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

/// An `Option` of the future of type `held`, its one-byte tag first:
/// `None` at 0 and `Some` at 1, with the future after the tag.
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

/// The types and coroutines of the tests' futures.
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
            // rustc names a trait object of several traits in parentheses.
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
            pin(PIN_OUTER, OUTER_BOX),
            record(WRAPPER, "Coop<inner>", vec![member("fut", INNER, 0)]),
            // tracing's `Instrumented`, its future in a `ManuallyDrop`
            // beside its span, as recent releases of Rust lay it out, and a
            // record of the same shape that is not it.
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

/// Memory the tests write.
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

/// A chain is read innermost first, to the leaf it waits on, whether a
/// coroutine holds the future it awaits, a pinned box points to it, a
/// trait object does, or a record holds it as its only member.
#[test]
fn a_chain_reaches_its_leaf_through_every_kind_of_future() {
    let outer = 0x1000;
    let mut memory = Memory::default();

    memory.byte(outer, 3);
    memory.byte(outer + AWAITEE, 3);
    let held = walk_from(&memory, outer, OUTER);
    assert_eq!(
        held,
        AwaitChain {
            frames: vec![
                leaf(outer + 2 * AWAITEE, SLEEP),
                coroutine_frame(outer + AWAITEE, INNER, 3, suspended(0), 21),
                coroutine_frame(outer, OUTER, 3, suspended(0), 12),
            ],
            end: ChainEnd::Leaf,
        }
    );

    let boxed = 0x2000;
    memory.byte(outer, 4);
    memory.word(outer + AWAITEE, boxed);
    memory.byte(boxed, 3);
    let pinned = walk_from(&memory, outer, OUTER);
    assert_eq!(
        pinned.frames,
        [
            leaf(boxed + AWAITEE, SLEEP),
            coroutine_frame(boxed, INNER, 3, suspended(0), 21),
            coroutine_frame(outer, OUTER, 4, suspended(1), 13),
        ]
    );
    assert_eq!(pinned.end, ChainEnd::Leaf);

    memory.byte(outer, 5);
    memory.word(outer + AWAITEE, boxed);
    memory.word(outer + AWAITEE + 8, VTABLE);
    let dynamic = walk_from(&memory, outer, OUTER);
    assert_eq!(
        dynamic.frames[1],
        coroutine_frame(boxed, INNER, 3, suspended(0), 21)
    );
    assert_eq!(dynamic.end, ChainEnd::Leaf);

    memory.byte(outer, 8);
    memory.byte(outer + AWAITEE, 3);
    let wrapped = walk_from(&memory, outer, OUTER);
    assert_eq!(
        wrapped.frames,
        [
            leaf(outer + 2 * AWAITEE, SLEEP),
            coroutine_frame(outer + AWAITEE, INNER, 3, suspended(0), 21),
            coroutine_frame(outer, OUTER, 8, suspended(5), 17),
        ]
    );

    // tracing's `Instrumented` is passed through to the future it holds,
    // here a pinned box in a `ManuallyDrop`'s `MaybeDangling`; a record of the
    // same shape that is not tracing's is a leaf.
    memory.byte(outer, 10);
    memory.word(outer + AWAITEE, boxed);
    let instrumented = walk_from(&memory, outer, OUTER);
    assert_eq!(
        instrumented.frames,
        [
            leaf(boxed + AWAITEE, SLEEP),
            coroutine_frame(boxed, INNER, 3, suspended(0), 21),
            coroutine_frame(outer, OUTER, 10, suspended(6), 18),
        ]
    );
    assert_eq!(instrumented.end, ChainEnd::Leaf);

    memory.byte(outer, 11);
    let lookalike = walk_from(&memory, outer, OUTER);
    assert_eq!(
        lookalike.frames,
        [
            leaf(outer + AWAITEE, LOOKALIKE),
            coroutine_frame(outer, OUTER, 11, suspended(7), 18),
        ]
    );

    // tokio's task-local scope holds its future in an `Option`, which the
    // walk passes through while it is `Some`; once the scope has dropped
    // its future, the chain ends, saying so.
    memory.byte(outer, 12);
    memory.byte(outer + AWAITEE, 1);
    memory.byte(outer + 2 * AWAITEE, 3);
    let scoped = walk_from(&memory, outer, OUTER);
    assert_eq!(
        scoped.frames,
        [
            leaf(outer + 3 * AWAITEE, SLEEP),
            coroutine_frame(outer + 2 * AWAITEE, INNER, 3, suspended(0), 21),
            coroutine_frame(outer, OUTER, 12, suspended(8), 18),
        ]
    );
    memory.byte(outer + AWAITEE, 0);
    let dropped = walk_from(&memory, outer, OUTER);
    assert!(
        matches!(&dropped.end, ChainEnd::Broken(reason) if reason.contains("it is None")),
        "{dropped:?}"
    );

    // A trait object of a vtable no type names ends the chain, saying so.
    memory.byte(outer, 5);
    memory.word(outer + AWAITEE, boxed);
    memory.word(outer + AWAITEE + 8, VTABLE + 8);
    let unknown = walk_from(&memory, outer, OUTER);
    assert!(
        matches!(&unknown.end, ChainEnd::Broken(reason) if reason.contains("vtable")),
        "{unknown:?}"
    );
}

/// A chain ends where its innermost coroutine has not begun, has ended,
/// or names nothing it awaits, and where its memory or state cannot be
/// read, each saying so, with the futures it passed.
#[test]
fn every_end_of_a_chain_is_said() {
    let outer = 0x1000;
    let mut memory = Memory::default();
    for (state, end) in [
        (0, ChainEnd::Unresumed),
        (1, ChainEnd::Finished),
        (2, ChainEnd::Finished),
        (6, ChainEnd::NoAwaitee),
    ] {
        memory.byte(outer, state);
        let chain = walk_from(&memory, outer, OUTER);
        assert_eq!(chain.end, end, "{state}");
        assert_eq!(chain.frames.len(), 1, "{state}");
    }

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

/// A running coroutine stores the state it last suspended in, not the
/// await it has gone on to: a caller that knows each coroutine's state,
/// outermost first, has the walk read it in that one, reads the rest as
/// stored, and ends the chain where it cannot say.
#[test]
fn a_chain_reads_each_coroutine_in_the_state_its_caller_chooses() {
    let outer = 0x1000;
    let mut memory = Memory::default();

    // The outer future has never suspended, so it stores `Unresumed`,
    // though it now runs in its first await.
    memory.byte(outer, 0);
    memory.byte(outer + AWAITEE, 3);
    assert_eq!(walk_from(&memory, outer, OUTER).end, ChainEnd::Unresumed);

    let mut asked = Vec::new();
    let chain = super::walk_choosing(
        &Types::new(),
        &memory,
        VirtualAddress::new(outer),
        reference(OUTER),
        &mut |at, _| {
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

    // A coroutine whose state is unknown ends the chain before it.
    let chain = super::walk_choosing(
        &Types::new(),
        &memory,
        VirtualAddress::new(outer),
        reference(OUTER),
        &mut |_, _| super::StateChoice::Unknown,
    );
    assert!(chain.frames.is_empty(), "{chain:?}");
    assert!(
        matches!(&chain.end, ChainEnd::Broken(reason) if reason.contains("unknown")),
        "{chain:?}"
    );
}

/// A chain that comes back to a future it passed ends there, as does one
/// that goes on past the most futures followed.
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

proptest! {
    /// Over memory of any content, the walk ends, within its depth, and
    /// says why.
    #[test]
    fn any_memory_ends_the_walk(
        words in proptest::collection::vec((any::<bool>(), any::<u8>()), 0..64),
        root in prop_oneof![Just(OUTER), Just(PIN_OUTER), Just(PIN_DYN), Just(INNER)],
    ) {
        let mut memory = Memory::default();
        for (index, (pointer, value)) in (0..).zip(&words) {
            // Each word is a pointer back into this memory, or a small
            // number, which a coroutine may read as its state.
            let word = if *pointer {
                0x1000 + u64::from(*value % 64) * 8
            } else {
                u64::from(*value % 12)
            };

            memory.word(0x1000 + index * 8, word);
        }

        let chain = walk_from(&memory, 0x1000, root);
        prop_assert!(chain.frames.len() <= MAX_DEPTH);
    }
}
