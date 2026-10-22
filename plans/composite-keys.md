# Composite keys: plan

Status: implemented on branch `composite-keys`, 2026-10-22. This record
replaces the kickoff note of 2026-10-18, which was never committed. It
keeps the decisions, the evidence behind them, and the order of the work.

An expression looks up a map entry whose key is made of several values:

```text
(microscope) print grid[(1, 2)]
(microscope) print grid[1, 2]        # Python's spelling of the same key
(microscope) print grid[cell]        # `cell` is a tuple the program holds
(microscope) print visits[("ann", 3)]
```

There is one expression language for every source language. The key, its
syntax, and its errors belong to no one language. A tuple key works in:

- Python dicts, found as CPython's own lookup finds them;
- Rust `HashMap` and `BTreeMap` keyed by tuples;
- C++ `std::map`, `std::unordered_map`, and the other maps the libstdc++
  and libc++ views present, keyed by `std::pair` or `std::tuple`;
- Zig hash maps keyed by tuples (`struct { i32, i32 }`).

Go has no tuples. Its composite keys are structs, which are refused (see
"Out of scope").

## Out of scope

These are owner decisions from the kickoff, and they stand:

- **Struct keys.** Matching `(1, "a")` against a Go, C, C++, or Rust
  struct by position would guess what the user meant. Such a key is a type
  error that names the map's key type. A Rust tuple struct is a struct.
- **Tuples as values.** `print (1, 2)`, `t == (1, 2)`, and tuples in
  arithmetic stay refused. A tuple literal is only a map's key. The
  evaluator never builds values in the target, and `Value` gains no
  composite value.
- **Tuples in the view language** (`Dialect::View`): a syntax error.
- **`bytes` and `frozenset` keys** stay refused. Bytes would need a
  literal syntax decision and per-interpreter evidence for SipHash over
  bytes; they wait for a need.
- **Instance keys,** including a `namedtuple` or a class deriving from
  `tuple`: their class could define `__eq__` and `__hash__`, so a lookup
  that would compare with one is refused, as before.

## How keys worked before

| Piece | Where | Before this work |
| --- | --- | --- |
| Parser | `src/eval/syntax/parser.rs`: `parenthesized`, `grouped`, `postfix` | `(` begins a cast or a grouped expression; `[` takes one expression, a slice, or a range. `,` appears only in the view built-ins. |
| AST, printer | `src/eval/syntax/ast.rs`, `print.rs` | No tuple node. |
| Binder | `src/eval/bind.rs`: `entry`, `key`, `dynamic_index` | A key is a number, truth value, pointer, null, string, a record or slice place (text the program holds), or a dynamic value, which `Op::Load` loads. |
| Key | `src/eval/target.rs`: `enum Key` | Scalars and text. `Key::matches` compares it with a native map's presented key. |
| Interpreter | `src/eval/interp.rs`: `key`, `Op::Entry` | Builds a `Key`; a loaded dynamic value goes through `dynamic::key`, which refuses tuples. |
| Python key | `src/runtime_model/mod.rs`: `InterpretedKey` | Int, Float, Bool, Text, Nothing. |
| Python lookup | `src/runtime_model/python/expressions.rs` | Probes by CPython's hash, or scans where the hash is unknown. A stored tuple compares unequal to every scalar key. |
| Native maps | `src/view/run.rs`: `entry_place` | Presents each stored key and calls `Key::matches`. |
| Fake worlds | `src/eval/fake.rs` | Run the examples in `docs/expressions.md`. |

## Evidence gathered while planning

**CPython's tuple hash** is `tuplehash`, xxHash-based and unchanged since
3.8. As unsigned 64-bit wrapping arithmetic:

```text
P1 = 11400714785074694791
P2 = 14029467366897019727
P5 = 2870177450012600261
acc = P5
for item in items:
    acc = acc + hash(item) * P2      # no item hash is -1
    acc = rotate_left(acc, 31)
    acc = acc * P1
acc = acc + (len ^ (P5 ^ 3527539))
return 1546275796 if acc == u64::MAX else acc as i64
```

A Python reimplementation agreed with `hash(t)` for `()`, `(1,)`,
`(1, 2)`, `(1, (2, 3))`, `("a", 1.5, None, True)`, `(-1,)`,
`(2**61 - 1, -2)`, `((),)`, `(1.0, 2)`, and `(0.5, -0.0)` on every pinned
interpreter: 3.12.13, 3.13.13, 3.13t, 3.14.4, 3.14t, 3.15.0, 3.15t,
Debian's 3.13.5, and Ubuntu's 3.12.3 (2026-10-22). The unit test's
expected values come from that run.

**Identity.** CPython's dict lookup tries `ep->me_key == key` before it
compares (`compare_generic` in `dictobject.c`), and a tuple compares its
items with `PyObject_RichCompareBool`, which also tries identity first.
A NaN float hashes as its object's address (`_Py_HashPointer`, since
3.10). On 3.12 and 3.14t, `nan = float("nan")`:

- `{nan: 1}[nan]` is `1`, and `hash(nan)` is `_Py_HashPointer(id(nan))`;
- `{(1, nan): 2}.get((1, nan))` is `2`: a new tuple holding the same NaN
  object finds the entry;
- `.get((1, float("nan")))` is `None`.

So microscope's lookup of a key the program holds must carry the object's
address all the way down, not only its value. Before this work,
`by_nan[nan]` was a missing-key error in microscope.

**Tuple comparison order.** `tuple_richcompare` has no early return for
different lengths. It compares items in order up to the shorter length,
stops at the first unequal pair, and only then compares lengths. An item
whose comparison would run code (an instance) is reached before the
lengths are compared. microscope follows the same order: an unknown item
before any unequal one makes the comparison unknown, which refuses the
lookup.

**Native tuple types,** read with `readelf --debug-dump=info` from
programs built in the dev shell:

- rustc names a tuple `(i32, i32)`, and its members are `__0`, `__1`, ….
  A tuple struct has the same members and its own name, which is how the
  debugger's existing tuple presentation (`presentation.rs`,
  `rust_tuple`) already tells them apart.
- Zig names a tuple `struct { i32, i32 }`, and its members are `0`,
  `1`, ….
- C++'s `std::tuple` has no members of its own to match. The libstdc++
  and libc++ views present it as `record { 0 = …, 1 = … }`, which
  `docs/views.md` already calls a tuple. The `std::pair` view presents
  `first` and `second` by name.

**What the native path reported before:** on a Rust
`HashMap<(i32, i32), &str>`, `grid[1]` was refused with "the map's keys
are `(i32, i32)`, which the integer 1 cannot equal", and no expression
could write a tuple.

## Design

### Syntax

- `(a, b)` and `(a,)` are tuple literals, and `()` is the empty tuple.
  `(a)` is still a grouped expression, and `(T)x` is still a cast:
  `parenthesized` tries a type name followed by `)` first, so a comma
  falls through to `grouped`. The ambiguity scan matches only
  `( name )` followed by `-`, `*`, or `&`, so a tuple never enters it.
- In brackets, `x[a, b]` is `x[(a, b)]`, as Python reads it, and `x[a,]`
  is `x[(a,)]`. `x[a, b:c]` and `x[a, b..c]` are syntax errors.
- `NodeKind::Tuple(Vec<NodeId>)` holds the items. The printer writes
  `(a, b)`, `(a,)`, and `()`, and a bracketed tuple as `x[(a, b)]`. Both
  spellings parse to the same tree.
- The view dialect refuses a tuple with a syntax error that says tuples
  are only an expression's map keys.

### Keys

`Key` (`src/eval/target.rs`) gains two variants:

```rust
/// Several keys in order, written `(a, b)`: a tuple.
Tuple(Vec<Key>),

/// The object of a dynamic type at this address, which the program
/// holds: its runtime finds it as its language's indexing does, by
/// identity first and then by value.
Object(u64),
```

`MAX_TUPLE_KEYS` (64) and `MAX_TUPLE_DEPTH` (8) bound a tuple key, whether
written or the program's. Hashing and comparing recurse along the key and
read the target's items for each of its items, so the bounds limit the
work and recursion of one lookup. A real composite key has a few values.
Past either bound, the key is refused as a limit error.

`Display` writes Python's and Rust's spelling: `(1, "a")`, `(1,)`, `()`.

### Binding

`Op::Entry`'s key becomes an `ir::KeyNode`: either one bound value, or a
tuple of key nodes. A tuple can't reach any other operand, because no
other op holds a `KeyNode`. Binding a `NodeKind::Tuple` anywhere else is a
type error: "a tuple is only a map's key, as in `d[(1, 2)]`". A tuple
index into a native array, slice, or pointer is a type error with the
hint "write `a[1][2]`".

The items of a tuple key into a value of a dynamic type are bound as
dynamic keys. A dynamic place stays a place, so that the interpreter can
pass its address. A key into a native map loads a dynamic value, as
before.

### Evaluating

`interp.rs`'s `key` builds `Key::Tuple` from a `KeyNode::Tuple` and
charges one unit of work per item. A place of a dynamic type becomes
`Key::Object(address)`.

A native place that holds no text, and that `Machine::tuple_elements`
says is a tuple, becomes a `Key::Tuple` of its elements' values. This is
`m[k]`, where `k` is a tuple variable of the program. An element is read
as text when it holds text, as a tuple when it is one, and otherwise
loaded as a scalar.

### Python (`src/runtime_model/python/expressions.rs`)

`InterpretedKey` gains `Tuple(Vec<InterpretedKey>)` and
`Object(VirtualAddress)`. `interpreted_key` converts each key
recursively.

- **Resolve.** An `Object` key is read one level at a time. An int,
  float, bool, `None`, or str becomes its value. A tuple becomes a
  `Tuple` of `Object` items, each of which is the program's own object.
  Any other type is refused as before. A large int beyond 128 bits is
  refused.
- **Hash.** A tuple hashes as `tuplehash` over its items' hashes. Text
  needs the process's secret, as before. A held str uses the hash its
  object caches when one is set, a held NaN float hashes as its address,
  and anything else hashes as its value. If any item's hash is unknown,
  the tuple's is too, and the lookup scans.
- **Probe.** An `Object` key whose very object a probed entry holds is
  found before hashes are compared, as `compare_generic` does.
- **Compare.** A held object is equal to itself. A stored tuple against a
  wanted tuple compares in CPython's order, as described above. A stored
  tuple against a scalar is unequal, as before. A stored scalar against a
  tuple is unequal. A stored instance stays unknown.
- **Sequences.** `position` resolves a held int or bool, and a tuple
  index is a type error, as CPython's `numbers[1, 2]` is a `TypeError`.
- **Messages** write a held key as Python's repr of its value.

### Native maps

`Machine` gains:

```rust
/// The places of the elements of the tuple at `at`, in order, or `None`
/// when its type is no tuple.
fn tuple_elements(&mut self, at: &Self::Place) -> Result<Option<Vec<Self::Place>>, Stop>;
```

The default is `None`. The backend's `StopMachine` decides what a tuple
is, from normalized data:

1. A view that presents the type as a tuple: a `record` whose members are
   named by their positions, as the `std::tuple` views are, or the new
   `tuple { … }` shape. Each member's program gives an element's place.
2. Otherwise, a record whose members are a Rust tuple's (`__0`, `__1`, …,
   in a type rustc names `(…)`) or a Zig tuple's (`0`, `1`, …). This is
   the test the presentation of Rust tuples already used, moved where
   both can call it.

**View language.** `tuple { NAME = EXPR, … }` is a record whose members, in
order, are a tuple's elements. It presents as `record` does. The libstdc++
`std::pair` view, which also presents libc++'s pairs, becomes
`show tuple { first = first, second = second }`. The C++ standard makes a
pair tuple-like (`std::tuple_size`, `std::get`), and a view author says so
in the view. The evaluator doesn't guess it from a name.

**Matching.** `view::run::entry_place` keeps its scalar path. For a
`Key::Tuple`, it evaluates the key program to a place and calls
`target::tuple_matches`, which the fake world shares:

- If the key type is no tuple, the result is a type error that names the
  type: "the map's keys are `Point`, which is no tuple: a key of several
  values matches only a tuple's elements, by position".
- If the arity differs, the result is a type error that names both
  arities.
- Otherwise each element is matched recursively: a nested tuple with
  `tuple_elements`, and a scalar with the existing `Key::matches` on the
  element as presented. An unequal element makes the entry unequal even
  if another element is unreadable. Native comparison has no side
  effects, so order doesn't matter. An unreadable element with no
  unequal one leaves the entry unreadable, as an unreadable scalar key
  does today.

A type error comes from the first entry, so an empty map reports a missing
key rather than the type error. The scalar path already behaves this way.

## Tests

Each test is written before the code it covers, and seen to fail.

- **Parser and printer** (`src/eval/syntax/tests.rs`): tuples print and
  read back. `(a)`, `(int)x`, and `(py)x` read as before. Range and slice
  after a tuple index are syntax errors. The view dialect refuses tuples.
  `check_invariants` covers round trips for the fuzzer's inputs too.
- **Executable reference** (`docs/expressions.md`):
  - In `world: python`, a tuple-keyed dict is read with `grid[(1, 2)]`,
    `grid[1, 2]`, and a held `cell`. `grid[(2, 1)]` and `grid[(1,)]` are
    missing keys. `numbers[1, 2]` is a type error, and `print (1, 2)` and
    `(1, 2) == count` are type errors.
  - In `world: memory`, a map keyed by `(int, int)` tuples is read with
    literals and with a held tuple. An arity mismatch is a type error, and
    so is a struct-keyed map. `arr[1, 2]` is a type error.
- **Unit tests:** the tuple hash against CPython's values above.
- **Python against CPython** (`tests/python/expressions.rs`, fixture
  `expressions.py`), across the whole interpreter matrix. The cases cover:
  - literal tuples, nested tuples, the empty tuple, one element, and
    `grid[1, 2]`;
  - `1.0 == 1` inside a tuple, and a missing key;
  - text items, both where the secret is proven and where the lookup
    scans;
  - a held tuple, and a held tuple that is a different object of equal
    value;
  - the NaN identity cases above;
  - an instance item that CPython would compare, which is refused;
  - `numbers[1, 2]`, which is a `TypeError`.
  
  `labels[pair]`, refused before, now agrees with CPython's `KeyError`.
- **Native, against each program's own answer:** new `keys` fixtures in
  Rust, C++, and Zig print `EXPECT` lines computed with the map's own
  lookup (`at`, `get`, `count`) and run through
  `tests/debugger/expressions.rs`. C++ builds with GCC at `-O0`, Clang at
  `-O2`, and Clang with libc++, whose pair and tuple layouts differ. Rust
  and Zig build unoptimized and optimized. The cases cover:
  - pair, tuple, and nested tuple keys;
  - string elements;
  - a held tuple variable;
  - a missing key, an arity mismatch, and a struct-keyed map.

  The Go expressions fixture adds a struct-keyed map, whose tuple key is
  refused.

Focused commands while working:

```sh
just test -E 'test(/eval|view/)'
just test --test python expressions
just test -E 'test(/expressions_agree/)'
just web-test docs.test.tsx
just docs
```

Then `just` for the full gate. Nothing here touches run control or
lifecycle, so `just stress` and `just sim` are not required.

## Documentation

- `docs/expressions.md`: "Literals" and "Postfix" say how tuples are
  written and that a tuple is only a map's key. "Maps" and "Python values"
  replace their "not supported yet" notes.
- `docs/views.md`: the `tuple { … }` shape, and what a tuple-like key
  type is.
- `docs/languages.md`: Python's tuple keys.
- `dev/evaluation-and-views.md`: one paragraph on composite keys and where
  each language's matching lives.

## Order of the work

1. Parser, AST, and printer, with their tests.
2. `Key::Tuple` and `Key::Object`, `KeyNode`, binding, and the refusals.
3. Python: hash, compare, resolution of held objects, and identity.
4. Native: `tuple_elements`, `tuple_matches`, the `tuple` view shape, and
   the `std::pair` view.
5. Fake worlds and the reference's examples.
6. Fixtures and their cases in every language.
7. Docs, then the full gate.

## What building it changed

- **A forwarding view's entries.** Zig's managed `std.AutoHashMap` is
  presented by `show value(unmanaged)`. Positional indexing already
  followed such a view (`element_type`, `presented_element`), but a key
  lookup did not, so `m[1]` on a managed Zig map was "cannot be indexed"
  before this work, with any key. `entry_type` now follows a `value`
  shape as `element_type` does, and `view::run::entry_place` hands the
  presented place to the new `Machine::presented_entry`. The Zig `keys`
  fixture's `ages[1]` is that fix's regression test.
- **Zig ReleaseSafe array hash maps** have no entry type in their debug
  information, so no view presents them, as the Zig containers test
  already notes. The `keys` fixture uses `AutoHashMap` throughout.
- **The tuple index error** names arrays, slices, and pointers only. A
  record whose view presents no map keeps the ordinary "cannot be
  indexed".
- **A held instance inside a tuple key**, `keyed[(seven, 2)]`, is refused
  even where CPython raises `KeyError`. CPython hashes the tuple first,
  which calls the instance's own `__hash__`. That code could raise or
  answer anything, and microscope does not run it, so not even a missing
  key is certain.
- **Native tests** are `tuple_keys_find_what_each_programs_maps_find` in
  `tests/debugger/expressions.rs`, over `keys-rust-o0`/`-o2`,
  `keys-cpp-gcc-o0`, `keys-cpp-clang-o2`, `keys-cpp-libcxx-o0`, and
  `keys-zig-o0`/`-o2`. Go's struct key is a row of its expressions
  fixture.

## Later

- A Zig tuple key is summarized as `{<2 fields>}` in a map's entries,
  where a Rust tuple shows as `(1, 2)`. That is presentation, not lookup.
- Proving the str hash secret from a tuple key's stored hash, so that a
  dict with no str keys can probe rather than scan for a tuple with text
  items. Scanning is correct, and the scan is already bounded.
- `bytes` keys and a bytes literal.
- `namedtuple` keys, whose class keeps `tuple`'s `__eq__` and `__hash__`.
