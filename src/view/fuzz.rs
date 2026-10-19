//! The hostile view harness: every built-in view, and views made of
//! arbitrary text, run over arbitrary memory. Whatever the bytes, a
//! presentation ends in a value or a typed problem within its budget, never
//! panics, and allocates only what its limits allow.
#![allow(clippy::panic, reason = "a fuzz harness reports a bug by panicking")]

use super::run::{Child, Failure, children, element_place, length, present};
use super::scan::Checkpoints;
use super::{ViewSet, choose};
use crate::eval::fake::{Place, World};
use crate::{BaseTypeEncoding as E, IntegerValue, SourceLanguage, TypeArgument, TypeReference};

/// Where the harness maps the heap the containers may point into.
const HEAP: u64 = 0x10_0000;
const HEAP_SIZE: usize = 4096;

/// The most units of evaluation work one run may take.
const MAX_WORK: u64 = 65_536;

/// The fuzzer's bytes for memory, read round and round, or zeros when
/// there are none, so a short input still makes a whole world.
struct Input<'a> {
    data: &'a [u8],
    position: usize,
}

impl Input<'_> {
    const fn byte(&mut self) -> u8 {
        let byte = if self.data.is_empty() {
            0
        } else {
            self.data[self.position % self.data.len()]
        };

        self.position = self.position.wrapping_add(1);
        byte
    }

    fn word(&mut self) -> u64 {
        u64::from_le_bytes(std::array::from_fn(|_| self.byte()))
    }

    /// A word a container holds: often a pointer into the heap or a small
    /// count, so that views get past their checks, and otherwise anything.
    fn field(&mut self) -> u64 {
        match self.byte() % 4 {
            0 => HEAP + self.word() % HEAP_SIZE as u64,
            1 => u64::from(self.byte() % 32),
            2 => 0,
            _ => self.word(),
        }
    }

    fn object(&mut self, words: usize) -> Vec<u8> {
        (0..words)
            .flat_map(|_| self.field().to_le_bytes())
            .collect()
    }
}

/// A world of the containers the built-in views present, laid out as their
/// libraries lay them out, each a variable whose bytes come from `input`.
#[expect(clippy::too_many_lines, reason = "one type graph per library")]
fn containers(input: &mut Input<'_>) -> (World, Vec<(String, TypeReference)>) {
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let char = world.base("char", E::SignedCharacter, 1);
    let uchar = world.base("unsigned char", E::UnsignedCharacter, 1);
    let size = world.base("unsigned long", E::Unsigned, 8);
    let i32 = world.base("i32", E::Signed, 4);
    let u8 = world.base("u8", E::Unsigned, 1);
    let usize = world.base("usize", E::Unsigned, 8);
    let int_pointer = world.pointer(Some(int));
    let char_pointer = world.pointer(Some(char));
    let byte_pointer = world.pointer(Some(u8));
    let unknown = |text: &str| TypeArgument::Unknown(text.into());
    let mut variables = Vec::new();

    // libstdc++
    let implementation = world.record(
        "_Vector_impl",
        24,
        &[
            ("_M_start", int_pointer, 0),
            ("_M_finish", int_pointer, 8),
            ("_M_end_of_storage", int_pointer, 16),
        ],
    );
    let vector = world.record("vector<int>", 24, &[("_M_impl", implementation, 0)]);
    world.identify(
        vector,
        SourceLanguage::Cpp,
        &["std"],
        "vector",
        vec![TypeArgument::Type(int), unknown("allocator")],
    );
    variables.push(("libstdc++ vector".to_owned(), vector));
    let hider = world.record("_Alloc_hider", 8, &[("_M_p", char_pointer, 0)]);
    let buffer = world.array(char, &[16]);
    let string = world.record(
        "string",
        32,
        &[
            ("_M_dataplus", hider, 0),
            ("_M_string_length", size, 8),
            ("_M_local_buf", buffer, 16),
            ("_M_allocated_capacity", size, 16),
        ],
    );
    world.identify(
        string,
        SourceLanguage::Cpp,
        &["std"],
        "basic_string",
        vec![
            TypeArgument::Type(char),
            unknown("traits"),
            unknown("allocator"),
        ],
    );
    variables.push(("libstdc++ string".to_owned(), string));
    let old_string = world.record("string", 8, &[("_M_dataplus", hider, 0)]);
    world.identify(
        old_string,
        SourceLanguage::Cpp,
        &["std"],
        "basic_string",
        vec![
            TypeArgument::Type(char),
            unknown("traits"),
            unknown("allocator"),
        ],
    );
    variables.push(("libstdc++ copy-on-write string".to_owned(), old_string));
    let elements = world.array(int, &[4]);
    let array = world.record("array<int, 4>", 16, &[("_M_elems", elements, 0)]);
    world.identify(
        array,
        SourceLanguage::Cpp,
        &["std"],
        "array",
        vec![
            TypeArgument::Type(int),
            TypeArgument::Value(IntegerValue::Unsigned(4)),
        ],
    );
    variables.push(("std::array".to_owned(), array));
    let extent = world.record("__extent_storage", 8, &[("_M_extent_value", size, 0)]);
    let span = world.record(
        "span<int>",
        16,
        &[("_M_ptr", int_pointer, 0), ("_M_extent", extent, 8)],
    );
    world.identify(
        span,
        SourceLanguage::Cpp,
        &["std"],
        "span",
        vec![
            TypeArgument::Type(int),
            TypeArgument::Value(IntegerValue::Unsigned(u128::from(u64::MAX))),
        ],
    );
    variables.push(("libstdc++ span".to_owned(), span));

    // libc++, whose bit-fields this world lays out as whole bytes.
    let short = world.array(char, &[23]);
    let short_string = world.record(
        "__short",
        24,
        &[
            ("__is_long_", uchar, 0),
            ("__size_", uchar, 0),
            ("__data_", short, 1),
        ],
    );
    let long_string = world.record(
        "__long",
        24,
        &[
            ("__is_long_", uchar, 0),
            ("__cap_", size, 0),
            ("__size_", size, 8),
            ("__data_", char_pointer, 16),
        ],
    );
    let representation = world.record(
        "__rep",
        24,
        &[("__s", short_string, 0), ("__l", long_string, 0)],
    );
    let libcxx_string = world.record("string", 24, &[("__rep_", representation, 0)]);
    world.identify(
        libcxx_string,
        SourceLanguage::Cpp,
        &["std"],
        "basic_string",
        vec![
            TypeArgument::Type(char),
            unknown("traits"),
            unknown("allocator"),
        ],
    );
    variables.push(("libc++ string".to_owned(), libcxx_string));
    let libcxx_vector = world.record(
        "vector<int>",
        24,
        &[
            ("__begin_", int_pointer, 0),
            ("__end_", int_pointer, 8),
            ("__cap_", int_pointer, 16),
        ],
    );
    world.identify(
        libcxx_vector,
        SourceLanguage::Cpp,
        &["std"],
        "vector",
        vec![TypeArgument::Type(int), unknown("allocator")],
    );
    variables.push(("libc++ vector".to_owned(), libcxx_vector));

    // Rust
    let marker = world.record("PhantomData", 0, &[]);
    let global = world.record("Global", 0, &[]);
    let non_null = world.record("NonNull<u8>", 8, &[("pointer", byte_pointer, 0)]);
    let unique = world.record(
        "Unique<u8>",
        8,
        &[("pointer", non_null, 0), ("_marker", marker, 0)],
    );
    let cap = world.record("Cap", 8, &[("__0", usize, 0)]);
    let raw_inner = world.record(
        "RawVecInner",
        16,
        &[("ptr", unique, 0), ("cap", cap, 8), ("alloc", global, 0)],
    );
    let raw = world.record(
        "RawVec",
        16,
        &[("inner", raw_inner, 0), ("_marker", marker, 0)],
    );
    let rust_vector = |world: &mut World, element| {
        let vector = world.record("Vec", 24, &[("buf", raw, 0), ("len", usize, 16)]);
        world.identify(
            vector,
            SourceLanguage::Rust,
            &["alloc", "vec"],
            "Vec",
            vec![TypeArgument::Type(element), TypeArgument::Type(global)],
        );
        vector
    };

    let ints = rust_vector(&mut world, i32);
    variables.push(("Vec<i32>".to_owned(), ints));
    let bytes = rust_vector(&mut world, u8);
    let rust_string = world.record("String", 24, &[("vec", bytes, 0)]);
    world.identify(
        rust_string,
        SourceLanguage::Rust,
        &["alloc", "string"],
        "String",
        Vec::new(),
    );
    variables.push(("String".to_owned(), rust_string));
    let deque = world.record(
        "VecDeque<i32>",
        32,
        &[("head", usize, 0), ("len", usize, 8), ("buf", raw, 16)],
    );
    world.identify(
        deque,
        SourceLanguage::Rust,
        &["alloc", "collections", "vec_deque"],
        "VecDeque",
        vec![TypeArgument::Type(i32), TypeArgument::Type(global)],
    );
    variables.push(("VecDeque<i32>".to_owned(), deque));

    // Zig
    let items = world.slice(i32);
    let list = world.record(
        "array_list.Aligned(i32,null)",
        24,
        &[("items", items, 0), ("capacity", usize, 16)],
    );
    world.identify(
        list,
        SourceLanguage::Zig,
        &["array_list"],
        "Aligned",
        vec![TypeArgument::Type(i32), unknown("null")],
    );
    variables.push(("ArrayList(i32)".to_owned(), list));

    // Linked and hashed containers, whose views scan.
    linked_containers(&mut world, &mut variables);
    sums_and_tuples(&mut world, &mut variables);
    btree_map(&mut world, &mut variables);
    go_library(&mut world, &mut variables);

    let heap = (0..HEAP_SIZE).map(|_| input.byte()).collect::<Vec<_>>();
    world.map(HEAP, &heap);

    for (name, ty) in &variables {
        let words = usize::try_from(world_size(&world, *ty).div_ceil(8)).unwrap_or(0);
        let object = input.object(words.max(1));
        world.variable(name, *ty, &object);
    }

    (world, variables)
}

/// The containers whose built-in views scan: libstdc++'s list and map,
/// Rust's `HashMap`, a Go map, and Zig's hash map, laid out as their
/// libraries lay them out (bases flattened into their members).
#[expect(clippy::too_many_lines, reason = "one type graph per library")]
fn linked_containers(world: &mut World, variables: &mut Vec<(String, TypeReference)>) {
    let int = world.base("int", E::Signed, 4);
    let size = world.base("unsigned long", E::Unsigned, 8);
    let i32 = world.base("i32", E::Signed, 4);
    let u8 = world.base("u8", E::Unsigned, 1);
    let usize = world.base("usize", E::Unsigned, 8);
    let u32 = world.base("u32", E::Unsigned, 4);
    let unknown = |text: &str| TypeArgument::Unknown(text.into());

    // libstdc++ std::list<int>
    let link = world.record("_List_node_base", 16, &[]);
    let link_pointer = world.pointer(Some(link));
    world.set_members(
        link,
        &[("_M_next", link_pointer, 0), ("_M_prev", link_pointer, 8)],
    );
    let header = world.record(
        "_List_node_header",
        24,
        &[
            ("_M_next", link_pointer, 0),
            ("_M_prev", link_pointer, 8),
            ("_M_size", size, 16),
        ],
    );
    let list_impl = world.record("_List_impl", 24, &[("_M_node", header, 0)]);
    let list = world.record("list<int>", 24, &[("_M_impl", list_impl, 0)]);
    world.identify(
        list,
        SourceLanguage::Cpp,
        &["std"],
        "list",
        vec![TypeArgument::Type(int), unknown("allocator")],
    );
    let list_node = world.record(
        "_List_node<int>",
        24,
        &[
            ("_M_next", link_pointer, 0),
            ("_M_prev", link_pointer, 8),
            ("_M_storage", int, 16),
        ],
    );
    world.identify(
        list_node,
        SourceLanguage::Cpp,
        &["std"],
        "_List_node",
        vec![TypeArgument::Type(int)],
    );
    variables.push(("libstdc++ list".to_owned(), list));

    // libstdc++ std::map<int, int>
    let tree_node_base = world.record("_Rb_tree_node_base", 32, &[]);
    let base_pointer = world.pointer(Some(tree_node_base));
    world.set_members(
        tree_node_base,
        &[
            ("_M_color", int, 0),
            ("_M_parent", base_pointer, 8),
            ("_M_left", base_pointer, 16),
            ("_M_right", base_pointer, 24),
        ],
    );
    let pair = world.record(
        "pair<int const, int>",
        8,
        &[("first", int, 0), ("second", int, 4)],
    );
    world.identify(
        pair,
        SourceLanguage::Cpp,
        &["std"],
        "pair",
        vec![TypeArgument::Type(int), TypeArgument::Type(int)],
    );
    let tree_impl = world.record(
        "_Rb_tree_impl",
        40,
        &[
            ("_M_header", tree_node_base, 0),
            ("_M_node_count", size, 32),
        ],
    );
    let tree = world.record("_Rb_tree<int, pair>", 40, &[("_M_impl", tree_impl, 0)]);
    world.identify(
        tree,
        SourceLanguage::Cpp,
        &["std"],
        "_Rb_tree",
        vec![TypeArgument::Type(int), TypeArgument::Type(pair)],
    );
    let map = world.record("map<int, int>", 40, &[("_M_t", tree, 0)]);
    world.identify(
        map,
        SourceLanguage::Cpp,
        &["std"],
        "map",
        vec![
            TypeArgument::Type(int),
            TypeArgument::Type(int),
            unknown("less"),
            unknown("allocator"),
        ],
    );
    let tree_node = world.record(
        "_Rb_tree_node<pair>",
        40,
        &[
            ("_M_color", int, 0),
            ("_M_parent", base_pointer, 8),
            ("_M_left", base_pointer, 16),
            ("_M_right", base_pointer, 24),
            ("_M_storage", pair, 32),
        ],
    );
    world.identify(
        tree_node,
        SourceLanguage::Cpp,
        &["std"],
        "_Rb_tree_node",
        vec![TypeArgument::Type(pair)],
    );
    variables.push(("libstdc++ map".to_owned(), map));

    // Rust HashMap<i32, i32>
    let byte_pointer = world.pointer(Some(u8));
    let non_null = world.record("NonNull<u8>", 8, &[("pointer", byte_pointer, 0)]);
    let inner = world.record(
        "RawTableInner",
        32,
        &[
            ("bucket_mask", usize, 0),
            ("ctrl", non_null, 8),
            ("growth_left", usize, 16),
            ("items", usize, 24),
        ],
    );
    let tuple = world.record("(i32, i32)", 8, &[("__0", i32, 0), ("__1", i32, 4)]);
    let raw_table = world.record("RawTable<(i32, i32)>", 32, &[("table", inner, 0)]);
    world.identify(
        raw_table,
        SourceLanguage::Rust,
        &["hashbrown", "raw"],
        "RawTable",
        vec![TypeArgument::Type(tuple), unknown("Global")],
    );
    let brown = world.record("HashMap<i32, i32>", 32, &[("table", raw_table, 0)]);
    let hash_map = world.record("HashMap<i32, i32>", 32, &[("base", brown, 0)]);
    world.identify(
        hash_map,
        SourceLanguage::Rust,
        &["std", "collections", "hash", "map"],
        "HashMap",
        vec![
            TypeArgument::Type(i32),
            TypeArgument::Type(i32),
            unknown("RandomState"),
        ],
    );
    variables.push(("Rust HashMap".to_owned(), hash_map));

    // A Go map[int]int: a pointer to a swiss table.
    let slot = world.record(
        "struct { key int; elem int }",
        16,
        &[("key", size, 0), ("elem", size, 8)],
    );
    let slots = world.array(slot, &[8]);
    let group = world.record(
        "noalg.map.group[int]int",
        136,
        &[("ctrl", size, 0), ("slots", slots, 8)],
    );
    let group_pointer = world.pointer(Some(group));
    let reference = world.record(
        "groupReference<int,int>",
        16,
        &[("data", group_pointer, 0), ("lengthMask", size, 8)],
    );
    let table = world.record(
        "table<int,int>",
        32,
        &[
            ("used", u32, 0),
            ("capacity", u32, 4),
            ("localDepth", u8, 8),
            ("index", size, 16),
            ("groups", reference, 24),
        ],
    );
    let table_pointer = world.pointer(Some(table));
    let directory = world.pointer(Some(table_pointer));
    let header = world.record(
        "map<int,int>",
        48,
        &[
            ("used", size, 0),
            ("seed", size, 8),
            ("dirPtr", directory, 16),
            ("dirLen", size, 24),
            ("globalDepth", u8, 32),
        ],
    );
    let header_pointer = world.pointer(Some(header));
    let go_map = world.typedef("map[int]int", header_pointer);
    world.identify(
        go_map,
        SourceLanguage::Go,
        &[],
        "map[int]int",
        vec![TypeArgument::Type(size), TypeArgument::Type(size)],
    );
    world.edit_identity(go_map, |identity| {
        identity.go = Some(crate::GoTypeAttributes {
            kind: crate::GoKind::Map,
            runtime_type: None,
        });
    });
    world.container(go_map);
    variables.push(("Go map".to_owned(), go_map));

    // Zig HashMapUnmanaged(u32, u32, …): metadata, with its header before it.
    let name = "hash_map.HashMapUnmanaged(u32,u32,hash_map.AutoContext(u32),80)";
    let u32_pointer = world.pointer(Some(u32));
    world.record(
        &format!("{name}.Header"),
        24,
        &[
            ("values", u32_pointer, 0),
            ("keys", u32_pointer, 8),
            ("capacity", u32, 16),
        ],
    );
    let metadata = world.pointer(Some(u8));
    let zig_map = world.record(
        name,
        16,
        &[
            ("metadata", metadata, 0),
            ("size", u32, 8),
            ("available", u32, 12),
        ],
    );
    world.identify(
        zig_map,
        SourceLanguage::Zig,
        &["hash_map"],
        "HashMapUnmanaged",
        vec![
            TypeArgument::Type(u32),
            TypeArgument::Type(u32),
            unknown("hash_map.AutoContext(u32)"),
            TypeArgument::Value(IntegerValue::Unsigned(80)),
        ],
    );
    variables.push(("Zig HashMapUnmanaged".to_owned(), zig_map));
}

/// The values whose built-in views choose among alternatives or name
/// members: libstdc++'s `optional`, `variant`, and `tuple`, and a Go
/// channel.
fn sums_and_tuples(world: &mut World, variables: &mut Vec<(String, TypeReference)>) {
    let int = world.base("int", E::Signed, 4);
    let long = world.base("long", E::Signed, 8);
    let uchar = world.base("unsigned char", E::UnsignedCharacter, 1);
    let boolean = world.base("bool", E::Boolean, 1);
    let size = world.base("unsigned long", E::Unsigned, 8);
    let integer = |value| TypeArgument::Value(IntegerValue::Unsigned(value));

    // std::optional<int>: _M_payload, of _Optional_base, holds the value
    // and whether there is one.
    let storage = world.record("_Storage<int>", 4, &[("_M_value", int, 0)]);
    let payload = world.record(
        "_Optional_payload<int>",
        8,
        &[("_M_payload", storage, 0), ("_M_engaged", boolean, 4)],
    );
    let optional = world.record("optional<int>", 8, &[("_M_payload", payload, 0)]);
    world.identify(
        optional,
        SourceLanguage::Cpp,
        &["std"],
        "optional",
        vec![TypeArgument::Type(int)],
    );
    variables.push(("libstdc++ optional".to_owned(), optional));

    // std::variant<int, long>: the alternatives share _M_u, and _M_index
    // says which one it holds.
    let union = world.record("_Variadic_union<int, long>", 8, &[("_M_first", long, 0)]);
    let variant = world.record(
        "variant<int, long>",
        16,
        &[("_M_u", union, 0), ("_M_index", uchar, 8)],
    );
    world.identify(
        variant,
        SourceLanguage::Cpp,
        &["std"],
        "variant",
        vec![TypeArgument::Type(int), TypeArgument::Type(long)],
    );
    world.pack(variant, 0);
    variables.push(("libstdc++ variant".to_owned(), variant));

    // std::tuple<int, long>: each element in a _Head_base<N, T> base.
    let heads = [(0, int, 8), (1, long, 0)].map(|(index, ty, offset)| {
        let head = world.record(
            &format!("_Head_base<{index}, …>"),
            world_size(world, ty),
            &[("_M_head_impl", ty, 0)],
        );
        world.identify(
            head,
            SourceLanguage::Cpp,
            &["std"],
            "_Head_base",
            vec![integer(index), TypeArgument::Type(ty), integer(0)],
        );
        (head, offset)
    });
    let tuple = world.record("tuple<int, long>", 16, &[]);
    world.set_bases(tuple, &heads);
    world.identify(
        tuple,
        SourceLanguage::Cpp,
        &["std"],
        "tuple",
        vec![TypeArgument::Type(int), TypeArgument::Type(long)],
    );
    world.pack(tuple, 0);
    variables.push(("libstdc++ tuple".to_owned(), tuple));

    // A Go chan int: a pointer to its hchan, whose buffer is a ring.
    let buffer = world.pointer(None);
    let hchan = world.record(
        "hchan<int>",
        48,
        &[
            ("qcount", size, 0),
            ("dataqsiz", size, 8),
            ("buf", buffer, 16),
            ("closed", int, 24),
            ("sendx", size, 32),
            ("recvx", size, 40),
        ],
    );
    let hchan_pointer = world.pointer(Some(hchan));
    let channel = world.typedef("chan int", hchan_pointer);
    world.identify(
        channel,
        SourceLanguage::Go,
        &[],
        "chan int",
        vec![TypeArgument::Type(size)],
    );
    world.edit_identity(channel, |identity| {
        identity.go = Some(crate::GoTypeAttributes {
            kind: crate::GoKind::Chan,
            runtime_type: None,
        });
    });
    world.container(channel);
    variables.push(("Go channel".to_owned(), channel));
}

/// Rust's `BTreeMap<i32, i32>`, whose view a kernel walks. The harness has
/// no sums, so its root's `Some` is a plain member here: what the harness
/// exercises is the kernel's walk over whatever the nodes hold.
fn btree_map(world: &mut World, variables: &mut Vec<(String, TypeReference)>) {
    let i32 = world.base("i32", E::Signed, 4);
    let u16 = world.base("u16", E::Unsigned, 2);
    let usize = world.base("usize", E::Unsigned, 8);
    let arguments = vec![TypeArgument::Type(i32), TypeArgument::Type(i32)];
    let path = ["alloc", "collections", "btree", "node"];
    let entries = world.array(i32, &[11]);
    let leaf = world.record("LeafNode<i32, i32>", 104, &[]);
    let leaf_pointer = world.pointer(Some(leaf));
    world.set_members(
        leaf,
        &[
            ("parent", leaf_pointer, 0),
            ("keys", entries, 8),
            ("vals", entries, 52),
            ("parent_idx", u16, 96),
            ("len", u16, 98),
        ],
    );
    world.identify(
        leaf,
        SourceLanguage::Rust,
        &path,
        "LeafNode",
        arguments.clone(),
    );
    let edges = world.array(leaf_pointer, &[12]);
    let internal = world.record(
        "InternalNode<i32, i32>",
        200,
        &[("data", leaf, 0), ("edges", edges, 104)],
    );
    world.identify(
        internal,
        SourceLanguage::Rust,
        &path,
        "InternalNode",
        arguments.clone(),
    );
    let non_null = world.record(
        "NonNull<LeafNode<i32, i32>>",
        8,
        &[("pointer", leaf_pointer, 0)],
    );
    let node = world.record(
        "NodeRef",
        16,
        &[("height", usize, 0), ("node", non_null, 8)],
    );
    let some = world.record("Some", 16, &[("__0", node, 0)]);
    let root = world.record("Option<NodeRef>", 16, &[("Some", some, 0)]);
    let map = world.record(
        "BTreeMap<i32, i32>",
        24,
        &[("root", root, 0), ("length", usize, 16)],
    );
    world.identify(
        map,
        SourceLanguage::Rust,
        &["alloc", "collections", "btree", "map"],
        "BTreeMap",
        [arguments, vec![TypeArgument::Unknown("Global".into())]].concat(),
    );
    variables.push(("Rust BTreeMap".to_owned(), map));
}

/// Go's byte slices, times, mutexes, and buffers, whose views decode words,
/// read text, and look up globals.
fn go_library(world: &mut World, variables: &mut Vec<(String, TypeReference)>) {
    let go = |world: &mut World, ty, path: &[&str], base: &str| {
        world.identify(ty, SourceLanguage::Go, path, base, Vec::new());
    };

    let uint8 = world.base("uint8", E::Unsigned, 1);
    let int32 = world.base("int32", E::Signed, 4);
    let uint32 = world.base("uint32", E::Unsigned, 4);
    let int64 = world.base("int64", E::Signed, 8);
    let uint64 = world.base("uint64", E::Unsigned, 8);
    let bytes = world.slice(uint8);
    go(world, bytes, &[], "[]uint8");
    variables.push(("Go []byte".to_owned(), bytes));

    let duration = world.typedef("time.Duration", int64);
    go(world, duration, &["time"], "Duration");
    variables.push(("Go time.Duration".to_owned(), duration));
    let location = world.record("time.Location", 16, &[("name", bytes, 0)]);
    go(world, location, &["time"], "Location");
    variables.push(("time.utcLoc".to_owned(), location));
    variables.push(("time.localLoc".to_owned(), location));
    let location_pointer = world.pointer(Some(location));
    let time = world.record(
        "time.Time",
        24,
        &[
            ("wall", uint64, 0),
            ("ext", int64, 8),
            ("loc", location_pointer, 16),
        ],
    );
    go(world, time, &["time"], "Time");
    variables.push(("Go time.Time".to_owned(), time));

    let state = world.record(
        "internal/sync.Mutex",
        8,
        &[("state", int32, 0), ("sema", uint32, 4)],
    );
    let mutex = world.record("sync.Mutex", 8, &[("mu", state, 0)]);
    go(world, mutex, &["sync"], "Mutex");
    variables.push(("Go sync.Mutex".to_owned(), mutex));
    let count = world.record("sync/atomic.Int32", 4, &[("v", int32, 0)]);
    let rw = world.record(
        "sync.RWMutex",
        24,
        &[
            ("w", mutex, 0),
            ("writerSem", uint32, 8),
            ("readerSem", uint32, 12),
            ("readerCount", count, 16),
            ("readerWait", count, 20),
        ],
    );
    go(world, rw, &["sync"], "RWMutex");
    variables.push(("Go sync.RWMutex".to_owned(), rw));

    let buffer = world.record("bytes.Buffer", 24, &[("buf", bytes, 0), ("off", int64, 16)]);
    go(world, buffer, &["bytes"], "Buffer");
    variables.push(("Go bytes.Buffer".to_owned(), buffer));
}

fn world_size(world: &World, ty: TypeReference) -> u64 {
    use crate::eval::types::TypeSource as _;
    world
        .type_info(ty)
        .and_then(|info| info.byte_size)
        .unwrap_or(8)
}

/// Runs every view that binds each container over memory from `data`.
pub fn hostile(data: &[u8]) {
    // The same bytes, read as a module's embedded records, are refused or
    // read, never more than they hold.
    let embedded = super::embedded::view_set("fuzz", data);
    assert!(embedded.views().len() <= data.len());

    // A budget, the length of a view file of the input's own, which is
    // tried first, the file, and then memory.
    let (&budget, data) = data.split_first().unwrap_or((&0, &[]));
    let (&text_length, data) = data.split_first().unwrap_or((&0, &[]));
    let (text, memory) = data.split_at((usize::from(text_length) * 4).min(data.len()));
    let mut input = Input {
        data: memory,
        position: 0,
    };

    let work = u64::from(budget) * 256 + 16;
    let (mut world, variables) = containers(&mut input);
    let text = String::from_utf8_lossy(text).into_owned();
    let text = if text.starts_with("microscope-views") {
        text
    } else {
        format!("microscope-views 1\n{text}")
    };

    let fuzzed = ViewSet::new([("fuzz.views", text.as_str())]);
    let built_in = ViewSet::built_in();

    for (name, ty) in &variables {
        let Some(bound) = choose(&fuzzed, *ty, &world)
            .bound
            .or_else(|| choose(&built_in, *ty, &world).bound)
        else {
            continue;
        };

        let this = Place::Memory {
            address: world.address_of(name),
            ty: *ty,
        };

        world.work = Some(work.min(MAX_WORK));
        let mut checkpoints = Checkpoints::default();
        let mut elements = 0;

        match present(&bound, &mut world, this.clone(), &mut checkpoints) {
            Ok(presented) => {
                if let Some(text) = &presented.text {
                    assert!(text.bytes.len() <= crate::TextSummary::MAX_BYTES, "{name}");
                }

                assert!(
                    presented.summary.len() <= 64 * 1024,
                    "{name}: {}",
                    presented.summary.len()
                );
                elements = presented.count.map_or(0, crate::PresentedCount::known);
            }
            Err(Failure::Problem(_)) => {}
            Err(Failure::Debugger(error)) => panic!("{name}: {error}"),
        }

        world.work = Some(work.min(MAX_WORK));
        let offset = u64::from(input.byte()) * 16;
        let limit = u64::from(input.byte());

        if let Ok(page) = children(
            &bound,
            &mut world,
            this.clone(),
            elements,
            offset,
            limit,
            &mut checkpoints,
        ) {
            assert!(
                page.len() as u64 <= limit,
                "{name}: {} of {limit}",
                page.len()
            );
            for child in &page {
                if let Child::Element(index, _) | Child::Entry(index, ..) = child {
                    assert!(*index >= offset && *index < offset + limit, "{name}");
                }
            }
        }

        world.work = Some(work.min(MAX_WORK));
        let _ = length(&bound, &mut world, this.clone(), &mut checkpoints);
        world.work = Some(work.min(MAX_WORK));
        let _ = element_place(
            &bound,
            &mut world,
            this,
            i128::from(input.byte()) - 8,
            &mut checkpoints,
        );
    }
}

/// Which built-in view presents each container of the harness's world, so a
/// change to a view or to the harness that leaves a view unexercised fails.
#[cfg(test)]
pub fn built_in_choices() -> Vec<(String, Option<String>)> {
    let mut input = Input {
        data: &[],
        position: 0,
    };

    let (world, variables) = containers(&mut input);
    let views = ViewSet::built_in();
    variables
        .into_iter()
        .map(|(name, ty)| {
            let choice = choose(&views, ty, &world);
            (
                name,
                choice.bound.map(|bound| bound.view.header.to_string()),
            )
        })
        .collect()
}
