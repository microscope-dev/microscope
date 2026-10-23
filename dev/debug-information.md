# Debug information

Debug-information providers translate format- and language-specific data into
the neutral model used by the debugger. ELF, DWARF, Go tables, native register
layouts, and ABI rules stay behind this boundary.

## Code map

| Path | What lives there |
| --- | --- |
| `src/debug_info/mod.rs`, `src/debug_info/dispatch.rs` | Provider interfaces and format dispatch |
| `src/debug_info/dwarf.rs`, `src/debug_info/dwarf/` | DWARF loading, variables, locations, and type facts |
| `src/debug_info/elf.rs`, `src/debug_info/separate.rs` | ELF metadata and separate debug-file discovery |
| `src/debug_info/gopclntab.rs`, `src/debug_info/gopclntab/` | Go table decoding and Go-specific unwind data |
| `src/debug_info/x86_64.rs` | Architecture-specific register and unwind rules |
| `src/debug_info/x86_64/frames.rs` | Frames of undescribed code, proved from its instructions |
| `src/debug_info/perf_map.rs` | Perf maps: names and frames of code a compiler generated |
| `src/debug_info/gdb_jit.rs`, `src/backend/linux/jit_objects.rs` | Objects a JIT compiler registers through the GDB JIT interface |
| `src/backend/linux/code_copies.rs` | Copies of an image's code a program made in anonymous memory, and V8's compiled code |
| `src/debug_info/v8.rs`, `src/debug_info/runtime_code.rs` | V8's builtins and the frames of code V8 generates, unwound by its frame-pointer convention |
| `src/debug_info/cpython.rs`, `src/debug_info/cpython/` | CPython's runtime layout: its offsets table, generated layouts, and their checks |
| `src/image/` | Validated immutable table image and checked views |
| `src/model/image.rs`, `src/model/image/` | Public image model and location types |
| `src/cache.rs` | Persistent image cache and cache-key construction |
| `src/unwind/` | Generic caller iteration and sampled-stack unwinding |
| `src/source_map.rs` | Source path resolution and remapping |
| `src/image/tests.rs`, `src/cache/tests.rs`, `tests/debugger/debug_files.rs` | Image, cache, and external debug-file coverage |

## Images and bindings

A load produces one validated image of immutable tables under `src/image`.
`ModuleImage` and providers read those tables through checked views. Validate
all offsets, indexes, counts, and cross-table references before publishing the
image.

A module binding holds the program path, identifier, mapping, and separate
debug-file path. Do not put those values in the image. Keeping them separate
allows one image to serve several mappings and makes the cache safe to share.

The cache in `src/cache.rs` keys an entry by the bytes that affect its result
and by revisions of the loader sources. Tests use only a cache path they create
or one explicitly named with `MICROSCOPE_CACHE_DIR`.

## Providers

Normalize data at the provider boundary. The evaluator and clients must not
interpret DWARF expressions, ELF segments, Go pclntab records, or native
register numbers.

Data layout belongs to providers. They resolve field offsets, base classes,
variants, calling conventions, and register locations. If the available debug
information cannot support an answer, return an explicit absence or partial
result.

## Unwinding

The generic unwind loop iterates caller contexts. gimli owns DWARF CFI
interpretation. Runtime-specific stack transitions report enough information
to continue or return a typed reason why the stack ended.

Do not fill unknown registers with zero or assume a frame rule. A convincing
wrong stack is worse than a short stack with a reason.

## Sources

Resolve source paths while loading metadata. Read source contents lazily,
outside the ptrace controller thread. Path remapping changes source lookup, not
the identity or bytes of a module image.

## Loading pipeline

Loading separates file discovery, parsing, normalization, sealing, and
binding. Keep those phases visible because they fail for different reasons.

1. Resolve the executable or module file and any separate debug file.
2. Read object metadata, symbols, debug sections, unwind data, and
   language-specific tables.
3. Normalize provider output into table builders under `src/image`.
4. Seal the builders into one byte image.
5. Validate the complete image as if it came from an untrusted cache.
6. Create a `ModuleImage` view over the validated bytes.
7. Bind that image to a loaded module's virtual ranges.

No partially loaded image enters the public model. If one input cannot be
used, report why and decide explicitly whether the whole module fails or a
documented feature remains unavailable.

## The image container

`src/image` stores dense immutable tables and pools. Records use integer IDs
and offsets instead of Rust pointers. This keeps images relocatable, validates
all references once, and allows memory mapping and cache reuse.

The container has a header, table directory, table bytes, and trailer. Table
kinds identify their schema. String and path pools deduplicate text. Indexes
provide deterministic lookup without rebuilding hash maps for every binding.

Adding a table or field requires answers to four questions:

- What provider-neutral fact does it represent?
- How is every integer, range, flag, and cross-reference validated?
- What revision makes older cache entries incompatible?
- Can a public view read it without allocating or trusting unchecked bytes?

Byte validity is only the first validation layer. A record can have valid Rust
layout while its offset points outside a pool, its range is reversed, its ID
names the wrong table, or two indexes disagree. Validate semantic structure
before exposing views.

## Image revisions and cache keys

`src/image::revisions()` contributes source revisions to the cache key. Bump
the relevant revision when a loader or schema change can alter the meaning of
cached bytes. Do not bump it for a refactor that provably leaves output and
validation unchanged.

The cache key names every input whose bytes affect the image: module content,
separate debug content, supplementary data, and loader revision. Paths do not
identify content. The same bytes at another path should reuse an image; new
bytes at the same path must not.

A cache hit still validates the image. Cache files are not trusted merely
because microscope wrote them. Failed or interrupted writes must not leave an
entry that later appears complete.

Tests run with the user cache disabled unless a recipe explicitly supplies
`MICROSCOPE_CACHE_DIR`. Cache tests create their own scratch directory. `just
test` warms and uses `target/image-cache`; `just stress` loads afresh to
exercise the loader repeatedly.

## Module discovery and identity

For a live process, mappings establish runtime address ranges and file
bindings. Files in another mount namespace may need to be opened through
`/proc/PID/root`. Match the mapped file by device and inode evidence rather
than trusting a path that resolves differently for the debugger.

For a core dump, notes and mappings describe the original process. Use build
IDs and recorded identity to find matching files under their original paths,
a sysroot, or module search directories. `allow-module-mismatch` is a public
opt-out from proof, not permission for ordinary loading to guess.

Two kinds of module live only in memory. The vDSO is one. The other is each
object a JIT compiler registers through the GDB JIT interface, as CPython
3.15's JIT does. The controller walks the list in each
`__jit_debug_descriptor` at every published stop, through
`debug_info::gdb_jit`. It reads each object's ELF image, loads it uncached at
load bias zero, and names it for its address and a hash of its bytes, so
replaced code is a new module. Such an image has no program headers, so its
address range is its allocated sections'. A list read while the compiler
changes it is read as far as it is sound; what could not be read is
recorded, and a frame in unread code stays unnamed.

A program may also copy part of a loaded image's code into anonymous memory
and run it there, as V8 copies its embedded builtins next to each code
range. `src/backend/linux/code_copies.rs` finds such copies at each module
refresh, and in a core when it opens. A candidate mapping is executable and
not writable, and no file backs it. Its first bytes must equal a function
that begins a page of a loaded image. The proof is a full comparison of the
mapping against the image's code, cached per mapping until the mapping
changes. A proven copy becomes a second binding of the same image whose
`LoadedModule::window` is the copied range of image addresses.
`virtual_address` and `image_address` refuse addresses outside a window, so
breakpoint resolution plants a location in a copy only for code the copy
holds. The copy's symbols, roles, and unwind rules are the image's.

A trap the debugger installed in the source before the program copied it is
in the copy too. The comparison accepts a `0xCC` where the source has an
installed site and records it. The trap comes out before the copy's own sites
go in. A thread that hits such a trap first is classified as a carried trap,
rewound, and resumed once the refresh has registered the copy.

An ELF build ID identifies file content for lookup; it is not a runtime module
ID. `ModuleId` identifies one public session binding. `ModuleImageId`
identifies immutable image content in the model. Keep these roles separate.

## ELF boundary

ELF parsing owns load segments, sections, symbols, build IDs, interpreter
facts, and core notes. Translate these into neutral addresses and module
records before handing them inward.

Do not assume section headers exist in a runtime image or that symbols have
complete extents. Record the evidence used to infer a boundary so disassembly
and symbol lookup can report conflicts and shortfalls.

Separate debug files can be found through explicit directories, system debug
directories, build-ID layouts, supplementary links, and debuginfod. Keep
network fetching outside the controller. Verify that fetched or discovered
files match the module before loading them.

## DWARF boundary

DWARF providers own compilation units, DIE traversal, line programs, location
lists, range lists, expressions, CFI, and producer-specific repairs. Generic
code should see source files, functions, types, variable descriptions,
locations, and unwind rules.

DWARF is frequently partial or internally inconsistent. Bound recursion,
references, expression work, ranges, and table sizes. A malformed entry should
carry enough unit and offset context to diagnose the producer without exposing
raw DWARF concepts through every public API.

Type identity is not a DIE offset. Compilers may emit declarations and
definitions in several units, imported types, aliases, split debug data, and
equivalent types at different offsets. Keep canonical identity and named
relationships in the image model.

## Variable locations

A source variable can be constant, live in memory, live in one or more
register pieces, depend on a frame base, require a DWARF procedure, or be
unavailable over part of its scope. Capture the description statically, then
evaluate it against one frame and stop.

Do not read a location while loading an image. Do not discard an unsupported
operation and continue with the stack as if it succeeded. Preserve the
difference between optimized out, malformed debug information, unsupported
producer behavior, unreadable process state, and a variable outside its live
range.

Entry values and caller-state reconstruction need explicit evidence. Never
substitute the current register value for a requested entry value.

A thread-local variable's location is an offset in its module's TLS block,
and the C library's structures say where each thread's copy of the block is.
`src/backend/linux/tls.rs` reads musl's structures, whose layout is fixed
ABI, and glibc's through the `_thread_db_*` descriptors glibc exports for
debuggers, following glibc's own lookup. glibc defines the descriptors in
libc since 2.34 and in libpthread before. The TLS fixtures run on Ubuntu's
glibc 2.27, 2.31, and 2.35, which the dev shell provides in
`MICROSCOPE_UBUNTU_GLIBC`, as well as on the toolchain's glibc and musl.

A runtime model reads a thread-local variable without debug information,
through the image's `ThreadLocals` table, which `elf::load_thread_locals`
fills from TLS symbols. A `ThreadLocal` is where the image's own code finds
the variable. `Offset` is a fixed offset from the thread pointer, as for an
executable's variables. `Slot` is a GOT slot the loader fills with that
offset (initial exec). `Dynamic` is an offset in the module's own block, which
`__tls_get_addr` reaches through the thread's DTV (general and local
dynamic). `RuntimeStop::thread_local_address` resolves all three; `Dynamic`
goes through the C library lookup above. A stripped library may name no
symbol for a variable. `elf::accessor_thread_local` then decodes the
placement from the code of a function that reads it, by the x86-64 TLS ABI's
sequences. V8 uses this for `g_current_isolate_` in Debian's `libnode`,
through the exported `v8::Isolate::GetCurrent`.

## Types and layout

Providers resolve ABI and language layout into explicit model facts:

- byte size, alignment where known, and byte order;
- record members, bases, offsets, and bit ranges;
- arrays, dimensions, ordering, and dynamic bounds;
- pointer, reference, slice, and function representations;
- enums, discriminants, variants, and storage;
- calling conventions and return-value capture;
- language-specific attributes such as Go kind information.

The evaluator asks providers for these facts. It must not reproduce C, C++,
Rust, or Go layout rules. When a dynamic offset needs stopped program state,
represent that operation explicitly rather than hiding it in a nominal type.

## Source lines and functions

Line tables map image addresses to source statements. Functions and inline
instances form a related but distinct index. Optimized code can have several
source rows at one address, non-contiguous function ranges, inline chains, and
instructions with no source row.

Keep all useful evidence and let stepping or presentation choose according to
its documented rule. Do not force the loader to invent one canonical line for
every instruction.

Source paths are normalized and remapped while metadata is loaded so equal
files compare consistently. File contents stay lazy. A missing source file
must not prevent symbols, types, disassembly, or unwinding from loading.

## Unwinding design

The generic loop in `src/unwind` operates on `FrameContext`, register files,
memory reads, and a `CallerProvider`. The DWARF provider uses gimli to
interpret CFI. Go stack transitions and signal frames add explicit transition
rules at their edges. PLT stubs, which mold and lld leave without CFI, get a
rule decoded at load: how many words each stub instruction finds pushed.

V8's code has no CFI either, and its frames follow V8's convention instead:
code calls only from a frame it has entered, whose rbp holds the address of
the word that saved the caller's rbp, 16 bytes below the canonical frame
address. Two pieces apply it. `src/debug_info/v8.rs` finds an image's
embedded builtins by their `Builtins_` symbols past
`v8_Default_embedded_blob_code_`. The `ProvedFrames` table, decoded at load,
holds each builtin's frame from its entry, as far as
`x86_64::frames::CodeFrames` proves it from the instructions, then the
convention's frame at each call it makes. A frame in the copy of the
builtins (see module discovery) uses the image's table. Code V8 compiles for
JavaScript lies in anonymous memory that the process can write. In a process
that has loaded a V8 image, `code_copies.rs` makes each such mapping a
pseudo-module, `debug_info::runtime_code`, with no symbols. It unwinds by the
convention, except where the instructions at the frame's own address prove
otherwise: a `push rbp; mov rbp, rsp` prologue that has not run yet, or a
`ret`. A caller's address is the end of the call it made, which is never
either. Debian's shared `libnode` names no builtin, so its own copy of the
builtins is not described until the V8 runtime model reads their bounds from
the process (G2 in `plans/node.html`).

GCC's epilogues pop the registers a caller keeps but leave the CFI rules that
name their slots, now below the stack pointer, until the function returns. A
sample's copy of the stack begins at the stack pointer, so those slots are
unreadable there. The `RestoredFrames` table, decoded at load, records the
instructions where the code proves the registers already hold the caller's
values. Only rows whose frame is gone (CFA = rsp+8) yet still name a slot are
read, from the row's first instruction. Code that runs straight to `ret`
without writing rsp or a callee-saved register proves all of them. Otherwise,
a `pop` or `leave` that ends the previous row and loads the slot the row names
at CFA-16 proves that one register, until the code writes it or may branch.
The proofs only decide what a failed slot read means; memory that holds the
slot, as a live process's does, is still read.

Each step yields a caller or a typed termination. Terminations include normal
stack ends as well as missing memory, missing registers, unsupported rules,
cycles, and bounds. Keep the frames already established when the next one
cannot be proven.

Frame-pointer fallback is a named policy with target rules. It is not a silent
replacement for arbitrary failed CFI. Profile unwinding has different
available memory from a stopped debugger and uses the same generic loop with a
sampled-state reader.

## Language metadata

Language providers can add static facts without changing clients:

- Go pclntab and runtime symbols identify code roles, stack transitions, and
  goroutine support.
- Rust compiler output identifies coroutine state, captures, and resume
  points used by the async runtime model.
- Type-name normalization handles compiler spellings used by expressions and
  views.

- CPython's `.PyRuntime` section and its symbols give the layout the Python
  runtime model reads, and the roles of the evaluation loop and its call
  machinery.

These facts belong in image tables when they depend only on module bytes. Live
runtime traversal belongs in the runtime model and reads them through the
bound module.

### CPython

`debug_info::cpython` finds the runtime in a module's `.PyRuntime` section and
its version in `Py_Version`. From 3.13 the section begins with
`_Py_DebugOffsets`, whose offsets are read by name under the minor version's
schema in `cpython/schemas.rs`. The fields it lacks come from
`cpython/layouts.gen.rs`, which `scripts/cpython-layouts.sh` generates by
compiling `scripts/cpython-layouts.c` against each interpreter's headers in
`MICROSCOPE_CPYTHON`. Patch releases change structures the table does not
describe: 3.14.8's interpreter state is 24 bytes longer than 3.14.4's from its
garbage collector's state on. So the generated layout is the one, among the
builds of the minor version, that agrees with the table on every field both
name. A table no build agrees with is refused, and so is one that builds
agree with but lay out differently where the table is silent. A prerelease is read only by a layout generated for that
exact release. 3.12 has no table, so its layout is the generated one for its
exact release. Every layout is validated before it is stored: each required
field is present and lies within its structure, and no structure read whole
exceeds a bound.

Each layout is checked three ways, so a new release that moves a field fails
in one place:

- against its headers, by `tests/python/layouts.rs`: the generated file is
  current, each schema is its version's `_Py_DebugOffsets`, and the widths and
  tags the readers assume are the headers';
- against the target's own table, by
  `each_interpreter_in_the_matrix_agrees_with_its_headers`;
- against DWARF, for each interpreter with a debug output, by
  `cpython/conformance.rs`.

When a new CPython release arrives, add it to the flake's interpreter set, run
`scripts/cpython-layouts.sh` in the dev shell, add a schema if its minor
version is new, and run these tests.

## Load testing

Use unit tests for table validation, indexing, address boundaries, and pure
provider algorithms. Use real fixtures for producer behavior. Compiler output
is the contract being tested, so cover the compact GCC/Clang, optimization,
PIE, frame-pointer, language, and runtime matrix where it changes results.

Fuzz entry points validate object parsing, image validation, DWARF expressions,
dispatch, symbols, Go tables, disassembly, and capture formats. A fuzz target
must remain bounded and run through the contained recipe.

For a schema or loader change, run the focused image and debug-information
tests, cache tests, `just bench-smoke`, and the normal gate. Inspect allocation
movement before accepting a baseline change.
