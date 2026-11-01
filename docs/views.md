# Views

Views control how microscope displays a type. A vector shows its elements
instead of a pointer, length, and capacity; a string shows text instead of
its storage fields. Write a `.views` file to do the same for your own types.

Views read the stopped program's memory. They cannot modify it, call its
functions, or perform I/O. Reads and computation have budgets. The original
value is always available under `[raw]` or with `print/r EXPR`.

## Write a view

Suppose your C program defines this vector:

```c
typedef struct {
    int *data;
    unsigned long n;
    unsigned long cap;
} intvec;
```

Save the following as `.microscope/views/app.views` in your project:

```microscope-view-example
microscope-views 1

view c intvec {
    check n <= cap
    show sequence(n) for i in range(n) => data[i]
    field capacity = cap
}
---
v => len=3 [10, 20, 30]
v => children: [0] = 10, [1] = 20, [2] = 30, capacity = 4, [raw]
none => len=0 []
bad => problem: check `n <= cap` failed: `n` is 9, `cap` is 4
```

The lines after `---` are example results, not part of the file. Here `v`
contains three integers, `none` is empty, and `bad` has `n = 9, cap = 4`.

`view c intvec` selects the type. `check` validates its length. `show`
defines its elements, and `field` adds a named child. With this view loaded,
`v[1]`, `len(v)`, and `cap(v)` work in expressions, including breakpoint
conditions. `cap(v)` reads the field named `capacity`.

Every file starts with `microscope-views 1`. Comments start with `#`.
Statements start on separate lines and may continue onto the next line.

## Load and inspect views

microscope tries these sources in order and uses the first matching view
that **binds**: its member names, types, and expressions must be valid for
the program's type.

| Priority | Source |
| --- | --- |
| 1 | Session files: `--views FILE`, the debug adapter's `viewFiles`, or `views load FILE`; latest first |
| 2 | Project files: `.microscope/views/*.views`, in filename order |
| 3 | User files: `$XDG_CONFIG_HOME/microscope/views/*.views`, or `~/.config/microscope/views/*.views`, in filename order |
| 4 | Views embedded in the program or a library; these apply only to that module's types |
| 5 | Built-in views |

The project root is the nearest ancestor of the working directory containing
`.microscope/` or `.git`. The debug adapter starts from the launch request's
`cwd`. Files load without a confirmation prompt. Errors are reported without
ending the session; a malformed view is skipped, not the rest of its file.

| Command | Purpose |
| --- | --- |
| `views` | List loaded files |
| `views load FILE` | Load or reload a session file |
| `views clear` | Remove files loaded with `views load` |
| `set views off` / `set views on` | Disable or restore views |
| `print/r EXPR` | Inspect one value without its view |
| `info view EXPR` | Show the selected view and rejected candidates |
| `views explain TYPE` | Explain selection for a type |
| `views check` | Check views against the program's types |

Without a debug session, use `microscope views explain PROGRAM TYPE` or
`microscope views check PROGRAM`. The latter exits with a failure if a
session or embedded view matches no type, binds to none of its matches, or
has an invalid visualizer. Use it to check your views in CI.

A binding failure tries the next candidate. A failure while reading a value
does **not**: failed checks, invalid tags, or inconsistent counts report a
problem and fall back to raw storage. An unreadable element appears as
`<unavailable>` and ends the summary preview.

## Match types

The declaration is `view LANGUAGE PATTERN { ... }`. Languages are `c`,
`c++`, `rust`, `go`, `zig`, `odin`, `d`, `nim`, `ada`, `python`,
`javascript`, or `any`. `any` excludes Python and JavaScript objects.

Patterns use the type's full path, with `::` or `.` separators. They match
type identity rather than compiler spelling: C++ inline namespaces are
optional, and `const int` and `int const` match the same type.

| Pattern | Meaning |
| --- | --- |
| `app::Point` | A type at this exact path |
| `app::**::Point` | Any number of path segments between `app` and `Point` |
| `alloc::**::Vec<T, _>` | Capture the element type as `T`; ignore the next argument |
| `app::Array<T, N>` | Capture a type and an integer argument |
| `app::Array<T, 3>` | Require the integer argument to be 3 |
| `` app::`{closure_env#0}` `` | Match a segment literally using backticks |

Capitalized argument names capture types or values; `_` ignores an argument.
Arguments use `<>`, `()`, or `[]` as appropriate for the language. Omitted
trailing arguments match anything, except that a C++ parameter pack must
be matched in full. `std::tuple<A, B>` matches only two-element tuples;
`std::tuple` matches all tuples. Lowercase argument names can match literal
type names such as Rust's `str`.

```microscope-view-example
microscope-views 1
view c++ app::**::Pair<T, N> {
    show sequence(N) for i in range(N) => items[i]
}
---
p => len=3 [2, 3, 4]
p => children: [0] = 2, [1] = 3, [2] = 4, [raw]
```

Here `p` is an `app::detail::Pair<int, 3>` with an `items[3]` member.
Some languages also have patterns for built-in kinds: Go's `map<K, V>`,
`chan<T>`, and `interface`; Odin's `map[K, V]`; and D's
`AssociativeArray(K, V)` for `V[K]`. D's `std.array.Appender(T)` matches
`Appender!T` in `std.array`.

## Expressions and statements

Views use [microscope expressions](expressions.md), not the source
language's expression syntax. `self` is the value being displayed. Native
members can be named directly: `n` means `self.n`. Local bindings and
captured arguments take precedence over members. Frame locals are not in
scope. Quote members named `or`, `for`, `if`, `else`, or `let` in backticks.

| Statement | Purpose |
| --- | --- |
| `let size = length or n` | Use the first expression that binds; evaluate it at most once per presentation |
| `type Cell = app::Cell<T>` | Name a type; `or` alternatives are allowed here too |
| `check n <= cap` | Reject inconsistent data; `&&` splits into separate checks |
| `show SHAPE` | Choose the presentation; at most one per execution path |
| `field capacity = cap` | Add a named child after the shape's children |
| `summary "{n} items"` | Replace the one-line summary |
| `hide data, cap` | Hide members or fields, but leave them accessible in `[raw]` |
| `format flags as hex` | Change how a member or field is displayed |

Without `show`, the view keeps the stored value's presentation. Use
`if COND { ... } else { ... }` or `match EXPR { VALUE => ... _ => ... }`
to choose shapes or groups of statements. A `match` without a matching arm
reports a problem.

Types can come from the program, captured arguments, `typeof(EXPR)`,
`arg(TYPE, N)` (zero-based), or a nested declaration such as
`typeof(self).Header`. View-only expression helpers are:

| Helper | Result |
| --- | --- |
| `inner(x)` | Unwrap records with one non-zero-sized member and no bases, repeatedly |
| `offsetof(TYPE, member)` | Byte offset of a direct member |
| `container_of(PTR, TYPE, member)` | Pointer to the containing record; `PTR` must point to the member's type or be `void *` |
| `global(NAME)` | Global in the value's module |

`or` handles layout differences, not runtime errors: `let size = length or n`
chooses by which member exists, not which value is readable.

## Choose a shape

| Shape | Presentation |
| --- | --- |
| `text(PTR)` / `text(PTR, LEN)` | Text, NUL-terminated unless a length is supplied; arrays and slices default to their length |
| `value(EXPR)` | Another value, including its view |
| `empty("None")` | No payload, with the supplied summary |
| `sequence(N) GENERATORS => ELEMENT` | Indexed elements |
| `map(N) GENERATORS => KEY : VALUE` | Key/value entries |
| `record { x = EXPR, y = EXPR }` | Named members; numeric names starting at 0 make a tuple |
| `tuple { x = EXPR, y = EXPR }` | Tuple elements regardless of member names |
| `dynamic(PTR, TYPE)` | The object at `PTR`, interpreted as `TYPE` |
| `dynamic(PTR, TYPE of CODE)` | Recover type arguments from the function at `CODE` |

Text supports byte characters, UTF-16, and UTF-32. Explicit lengths count
character units; summary lengths count bytes. Invalid character encodings
use U+FFFD.

```microscope-view-example
microscope-views 1
view c str_t {
    show text(p, len)
}
view rust alloc::**::Box<T> {
    show value(*inner(self))
}
---
s => "hello"
b => 42
```

Here `str_t` has a character pointer `p` and a length `len`; `b` is a
`Box<i32>` containing 42. A tagged value can select its payload:

```microscope-view-example
microscope-views 1
view c tagged {
    show match kind {
        0 => empty("None")
        1 => value(value)
    }
}
---
nothing => None
something => 7
strange => problem: `kind` is 5, which no arm of the match names
```

This `tagged` type has integer members `kind` and `value`.
For runtime-selected types, `dynamic(ptr, arg(typeof(self), index))`
chooses a type argument by index. For type-erased storage,
`dynamic(object, T of manage)` gets `T` from the function's debug information.
Missing type arguments or function metadata report a problem.

## Walk containers

Each generator starts with `for NAME in GENERATOR`. Nest generators to walk
buckets, add `if COND` to filter items, or add `let NAME = EXPR` to compute
a value per item.

| Generator | Traversal |
| --- | --- |
| `range(N)` | Integers 0 through N − 1; supports direct indexing |
| `list(HEAD, p => p->next)` | Follow links until null or back to `HEAD` |
| `inorder(ROOT, p => p->left, p => p->right)` | Binary tree, left subtree before node before right subtree |
| `kernel("NAME", ARG, ...)` | Items yielded by a WebAssembly kernel |

For a linked list with `head` and `count`, whose nodes have `value` and `next`:

```microscope-view-example
microscope-views 1
view c list {
    show sequence(count) for x in list(head, p => p->next) => x->value
}
---
three => len=3 [1, 2, 3]
short => problem: the view declares 5 elements and generates 3
```

For a tree with `root` and `count`, whose nodes have `key`, `value`, `left`,
and `right`:

```microscope-view-example
microscope-views 1
view c tree {
    show map(count) for x in inorder(root, p => p->left, p => p->right)
        => x->key : x->value
}
---
balanced => len=3 {1: 10, 2: 20, 3: 30}
balanced => children: 1: 10, 2: 20, 3: 30, [raw]
```

A declared count limits the output; ending before that count is an error.
Use `_` instead of a count to scan until the generators end. If the budget
ends that scan early, the length is a lower bound, such as `len>=40`.
Cycles other than a list returning to its head report a problem.

Sequences support `v[i]`; maps support `m[key]`, searching entries by equality,
not by position. Map values must refer to program storage. Tuple keys support
`m[a, b]`; their elements must also refer to storage rather than computed
values. Elements in storage retain their addresses and can be assigned
through ordinary debugger expressions.

## Format and extend a view

Formats are `hex`, `char`, `bytes`, `utf8`, `utf16`, `flags(ENUM)`,
`enum(ENUM)`, `duration(UNIT)`, and `time(UNIT)`. `flags` names the set bits;
`enum` names the matching enumerator. Time units are `ns`, `us`, `ms`, or `s`;
`time` displays a Unix timestamp in UTC. `utf8` requires valid
UTF-8 without control characters other than tab, newline, or carriage return.
Invalid text keeps its original presentation. An incompatible type prevents
the view from binding.

Use `format self as FORMAT` to format a `value` shape. `utf8` can also
replace a sequence's summary with text while retaining its byte children.
Summaries and `empty` strings accept `{EXPR}` or `{EXPR as FORMAT}`;
escape literal braces as `\{` and `\}`.

`extend LANGUAGE PATTERN` adds fields, formatting, or visualizers without
replacing an existing view. All matching extensions apply. They can contain
`let`, `type`, `field`, `hide`, `format`, and `visualize`, but not `show`.
Without a base view, they apply to the stored value.

```microscope-view-example
microscope-views 1
extend c tagged {
    hide value
    format kind as hex
    field next_kind = kind + 1
}
---
something => {kind: 0x1}
something => children: kind = 0x1, next_kind = 2, [raw]
```

## Python objects

Python patterns name the module and qualified class name, such as
`app::Point` or `enum::Enum`. Selection tries the object's class, then its
bases in Python's method resolution order, once per class per stop.
Use `self.name` for attributes; bare names do not select attributes.

```microscope-view-example
microscope-views 1
view python app::Point {
    show record { x = self.x, y = self.y }
    summary "Point({self.x}, {self.y})"
}
---
point => Point(3, 4)
point => children: x = 3, y = 4, [raw]
gone => problem: the `app.Point` has no attribute `x`
```

Attributes are checked at runtime. Properties and other operations that need
Python code are not evaluated. See [Python expressions](expressions.md#python-values).

## JavaScript objects

JavaScript views also cover classes defined in TypeScript. Use `self.name`
for data properties. Getters and proxies are not evaluated. Selection follows
the prototype chain, so a base-class view also applies to derived objects.

Class paths come from the constructor's script: `/srv/app/models.js` gives
`srv::app::models::Point`, matched by `**::models::Point`.
Node scripts use paths such as `node::buffer::Buffer`; V8 built-ins use
names such as `TypedArray` or `Map`.

## Kernels and visualizers

Use a **kernel** when a container needs an algorithm that the generators
cannot express. It is a WebAssembly function that reads memory and yields
one to eight 64-bit words per item. The view supplies layout information and
casts yielded addresses to the appropriate types. Kernels cannot import
anything except microscope's `read` and `yield`; their execution is budgeted.
Traps, nonzero return codes, and incorrect item sizes report a problem.

Place `NAME.wasm` beside the view file and call `kernel("NAME", ...)`.
Arguments are integers, pointer addresses, or booleans (0 or 1), up to 32
per call. A file's own kernels take precedence over built-ins.
See the [C kernel SDK](https://github.com/microscope-dev/microscope/blob/main/sdk/c/microscope_kernel.h)
for the interface, an example, and build flags. `views record FILE EXPR` records runs;
`microscope views replay FILE --kernel NAME.wasm` checks a replacement offline.

A **visualizer** draws a value in `microscope web`, without changing terminal
or debug-adapter output. Add `visualize "line-plot" { values = self }` to
a view or extension to plot a sequence. Inputs can be expressions, literal
strings, or `bytes(PTR, LEN)` for bulk data. Multiple visualizers can apply;
one that fails to bind does not disable the rest of the view.
Place custom renderers in `NAME.js` beside the view file. See
[Visualizers](visualizers.md) for built-ins, renderer APIs, and sandbox limits.

## Share views with a library

C and C++ programs can embed files using `microscope_views.h` from
[`sdk/c`](https://github.com/microscope-dev/microscope/tree/main/sdk/c):

```c
#include "microscope_views.h"
MICROSCOPE_VIEWS_FILE("views/app.views");
```

Rust programs use `microscope_views::microscope_views_file!(PATH)` from
[`sdk/rust`](https://github.com/microscope-dev/microscope/tree/main/sdk/rust).
Both SDKs also embed kernels and renderers. Files are included at build time
in `.debug_microscope_views`, not loaded into the running program, and removed
by `strip --strip-debug`.

Built-in views cover standard-library containers and common types in C++,
Rust, Go, Zig, Odin, D, Nim, Ada, Python, and JavaScript, plus glibc locks and
tokio types. Browse the
[built-in files](https://github.com/microscope-dev/microscope/tree/main/views)
for examples and exact coverage. Some language values, such as Rust enums
and slices, already display correctly without a view; see
[language support](languages.md).

## Limits

Summaries preview at most 16 elements or about 96 characters, read text up to
256 bytes, and apply nested views at most four levels deep. A shortened
summary is not the whole container: expand it to request more children.

View files are limited to 256 KiB and 1,024 definitions. Generators nest at
most four deep, binary trees at most 128 levels, and uncounted scans yield at
most 16,777,216 elements. Kernels are limited to 256 KiB of code and 4 MiB
of memory. All work also shares the inspection's read and computation budgets.
