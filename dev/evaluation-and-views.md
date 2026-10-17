# Expressions, inspection, and views

The expression and view systems share target values but have different jobs.
Expressions let a user name and compute with program values. Inspection turns
a located value into a bounded public tree. Views change that tree's logical
shape. Keep these layers separate.

## Code map

| Path | What lives there |
| --- | --- |
| `src/eval/syntax/` | Lexer, parser, syntax tree, and canonical printing |
| `src/eval/bind.rs`, `src/eval/ir.rs` | Name and type binding into target-neutral IR |
| `src/eval/interp.rs`, `src/eval/number.rs`, `src/eval/target.rs` | Evaluation, exact numeric rules, and target traits |
| `src/eval/dynamic.rs` | The rules of operations on values of a dynamic type |
| `src/inspection.rs` | Bounded conversion from located values to public value trees |
| `src/backend/linux/evaluation.rs`, `src/backend/linux/inspection.rs` | Debugger implementations of evaluation and inspection access |
| `src/view/syntax.rs`, `src/view/bind.rs`, `src/view/run.rs` | View parsing, binding, and execution |
| `src/view/scan.rs`, `src/view/kernel.rs`, `src/view/summary.rs` | View discovery, kernel programs, and summaries |
| `src/view_files.rs`, `views/` | View-file loading and built-in view definitions |
| `sdk/` | Headers and libraries exposed to instrumented programs and visualizers |
| `src/eval/tests.rs`, `src/view/tests.rs`, `tests/debugger/expressions.rs`, `tests/debugger/views.rs` | Core and public-path tests |

## Expression phases

`src/eval` has three main phases:

1. **Parse** source text into syntax without a program.
2. **Bind** names, members, types, operators, and layouts in one frame scope.
3. **Run** the bound IR against one validated stop through target traits.

Parsing must not perform I/O, read debug information, or depend on a source
language. Binding can query declarations and provider-normalized type facts,
but it does not read process values. Running reads registers and memory through
the debugger's target implementation.

A boundary test keeps process control, debug formats, file I/O, clocks, and
threads out of `src/eval`. Preserve that property when adding an operation.

## One language

microscope does not parse C expressions for C and Rust expressions for Rust. One
language works in every frame. Source-language providers decide which names
and type facts exist, not expression grammar or arithmetic.

This gives breakpoints, watches, views, DAP evaluation, and browser inputs one
set of semantics. Do not add a client-only parser or call a compiler to handle
one language feature.

When a source language has a useful construct, add it only if it has a neutral
meaning across target values. Source spelling differences can be handled by
name normalization and explicit casts.

## Names and scope

Binding resolves names in the selected frame's lexical scope, then globals and
qualified paths according to the reference rules. It must handle shadowing,
inline frames, namespaces or packages, and compiler-qualified names
deterministically.

Register names and debugger pseudo-values such as `$pc`, `$task`, and
`$future` are explicit namespaces. Do not let a program declaration silently
override them.

A bound name records the declaration and location operation needed at run
time. It does not cache the value from the stop used during binding.

## Dynamic types

A variable of an interpreted language, such as Python, has no static type:
its value's type is whatever object it holds when the expression runs. Such
a name binds through `Lookup::Dynamic` to `Ty::Dynamic`, and binding defers
every decision about it to the run:

- A member or an item plans a step through `Scope::plan_dynamic`, which the
  machine takes from the object it reaches (`Machine::step` and `entry`).
- An operand is loaded with `Machine::load_dynamic`, which gives the
  object's type name and a neutral `LoadedValue`: nothing, a truth value, an
  exact integer, a float, text, bytes, a collection's length, or an object.
- Arithmetic, comparisons, truth, and keys follow `src/eval/dynamic.rs`.
  Numbers keep microscope's exact rules, with truth values as 0 and 1. An
  operation a language defines differently from microscope, such as Python's
  integer `/` or floored `%`, is refused rather than computed the other way.
- A value that only its own type's code could compare or compute with, such
  as a list or an instance, is refused. The evaluator never asks the runtime
  to run code. A refusal is a type error only where the language's own
  operation would be one, as Python's `None + 1` is; where an operand's type
  could give a value, as text joined with text or an object's own operator
  does, it is unsupported.

A program is bound once and may run on values of different types, so every
check happens as it runs. A dynamic value is not assigned, not watched, and
has no size; `&x` is the object's address as a `void*`. The reverse is a
cast to a name `Scope::dynamic_type` accepts, such as `(py)p`: it binds to
`Op::Interpreted`, whose operand is a pointer or an integer, and runs
`Machine::dynamic_at` to reach the object at that address as a dynamic
value. The binder chooses the cast reading of `(py)&x` and `(py)-x` by the
same test, when no value in scope is named `py`. The fake world
`python` in `src/eval/fake.rs` holds dynamic values for the evaluator's own
tests and the reference's examples.

## Exact arithmetic

Integer arithmetic computes the mathematical result without C-style promotion
or overflow. Bit operations preserve their operand width. Only a cast
truncates. Signedness and width are part of typed bit operations, shifts, and
display.

Keep number parsing and arithmetic in `src/eval/number.rs`. Do not route an
integer through a host `usize`, pointer width, or floating-point value unless
the operation explicitly requires it.

Floating-point behavior uses explicit target widths. Comparisons and casts
must state their behavior for NaN, infinity, and out-of-range values.

## Lvalues, memory, and assignment

Evaluation distinguishes a value from a place that can be read or written. A
place carries target address or register storage, type, bit range where
needed, and the stop it belongs to.

Assignment is a stopped-state mutation. It validates the `StopId`, target
width, location, accessibility, and exact conversion before writing. It must
not write a partial aggregate and then discover that a later member is
invalid.

Process writes invalidate stop-scoped presentation and read caches. Register
writes can invalidate frame and expression assumptions as well.

## Data layout

The evaluator never derives field offsets, enum storage, base classes, array
strides, slice words, or calling conventions from a type name. Providers put
layout facts in the model. Binding selects those facts; running applies them.

This rule is especially important for Rust enums, C++ inheritance and bit
fields, Go interfaces and maps, and compiler-generated coroutine state. A
layout copied into evaluator code will eventually disagree with a producer.

## Unavailable values and errors

Keep syntax errors, binding errors, runtime errors, and unavailable target
state distinct. Examples:

- an unknown name is a binding error;
- division by zero is an evaluation error;
- a variable outside its location range is unavailable;
- unreadable memory is unavailable with its address evidence;
- an unsupported DWARF operation is an explicit unsupported feature;
- a stale stop is a request error before evaluation begins.

An operator should propagate unavailable operands with the relevant operand
span. Do not convert unavailability to zero, false, null, or an empty value.

## Conditions and log messages

Breakpoint and watchpoint conditions bind and run through the same evaluator.
A condition must produce a truth value according to the documented conversion
rules. A failed or unavailable condition is reported and handled by explicit
breakpoint policy; it is not silently false.

Log messages are parsed into literal and expression segments. Each expression
uses the hit frame and stop. Bound work should be reusable where valid, but no
value can survive into a later stop.

## Inspection

Inspection converts a variable or expression value into `InspectedValue`,
`Presentation`, and paged `ValueChild` records. It is the client-facing bridge
between evaluator values and UI trees.

Every inspection carries limits. Recursion, memory bytes, reads, child count,
and work are charged across raw layout and view expansion. The result records
whether it completed and which limit ended it.

Children have stable ordering and references so clients can request later
pages. A reference includes enough stop and value identity to reject use after
the state changes.

## View files

`src/view/syntax.rs` parses `microscope-views 1` files without a program. A view
has a source-language selector, type pattern, checks, local bindings, a shape,
fields, summaries, and optional visualizers. An `extend` adds fields or
visualizers without replacing the presenting shape.

View discovery combines, in precedence order, command-line files, project
files, user files, views carried by a program, and built-ins. Later or more
specific sources must follow the documented precedence. `ViewCheck` and
`ViewExplanation` expose every candidate and rejection so a user can diagnose
which one won.

Parsing one bad view should preserve useful errors and the policy for the rest
of the file. Never bind a malformed definition partially.

## Type patterns

Patterns match normalized type identity: language, package or namespace path,
name, and generic arguments. Wildcards and captures operate on that identity,
not a demangled display string.

Binding checks that captured types and values support every expression in the
view. A view that names a type but cannot bind records the reason and lets the
next candidate try. A value-level check happens later and falls back to raw
presentation with a problem; it does not select another structurally different
view behind the user's back.

## View execution

View expressions use the expression evaluator through a restricted target.
They see `self`, members, captured pattern arguments, and view-local bindings.
They do not see arbitrary frame names, perform writes, call target functions,
open files, or read clocks.

Generators describe sequences and maps with bounded, cycle-aware traversal.
Kernels handle algorithms that need their own stack or complex iteration.
Every yielded item and memory read is charged to the outer inspection.

Checks state invariants such as length not exceeding capacity. On failure,
show raw storage and the failed check. Producing a plausible logical container
from contradictory metadata is not acceptable.

## Kernels

View kernels are compiled WebAssembly with a narrow host protocol for reads
and yielded values. Built-in kernels live under `views/kernels`; programs can
carry kernels through the SDK.

Execution has fuel, memory, read, and result bounds. A kernel cannot access a
program except through validated host calls. Its recorded runs can be replayed
without a process using `microscope views replay`.

When adding a host operation, define its deterministic input, output, failure,
and charge. Update recording and replay together so a run still names every
observable interaction.

## Visualizers

A view's `visualize` block selects bounded inputs. The debugger evaluates and
serializes those inputs at one stop. The web client runs the named JavaScript
renderer in a sandbox and validates the picture it returns.

The renderer never reads process memory directly. Numeric conversion is based
on target type, not value magnitude: a 64-bit integer remains a bigint even
when small. Any unavailable input prevents the renderer from receiving a
misleading partial object.

Bulk byte and number reads have a separate hard byte limit. Shape count,
dimensions, strings, and pixel buffers are validated before rendering.

The terminal and DAP clients expose ordinary presented values but do not run
JavaScript. Keep visualizer code and browser sandbox behavior out of the core
debugger.

## Testing changes

The public expression and view references are executable specifications. Add
the example that states the new behavior before implementation and run its
focused test to observe failure.

Use pure tests for parsing, binding, arithmetic, IR, pattern matching,
generators, summaries, and picture validation. Use debugger scenarios for
real scopes, locations, writes, carried views, kernels, and optimized
variables. Use browser tests for renderer sandboxing and drawings.

For evaluator or view work, run at least:

```sh
just test -E 'test(/eval|view/)'
just web-test docs.test.tsx
just docs
```

Broaden the filter for changes to inspection, debug layouts, runtime values,
or client protocols.
