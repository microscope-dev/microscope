# cpp_demangle, patched

This is cpp_demangle 0.5.1 from crates.io, the latest release, with one file
changed: `src/ast.rs`. Without the change, 0.5.1 can't parse several C++20
manglings that clang emits and Chromium is full of, so their names stay
mangled in profiles. The parts added here:

- Template parameter declarations (`Ty`, `Tk`, `Tn`, `Tt`, `Tp`) in template
  argument lists and in lambda signatures (`Ul`). They are skipped, as
  llvm-cxxfilt does. A `Tk` constraint's name still counts as a substitution
  candidate, so later `S_` references still resolve.
- Requires clauses (`Q <expression>`) in template arguments and after a
  function's parameter types. They are parsed and dropped.
- Template parameters of an enclosing level (`TL<level>_<index>_`). They print
  as `$T` / `$TN` like llvm-cxxfilt, since the demangler doesn't track which
  level's arguments are in scope.
- Empty argument packs in template and function arguments, which printed as
  stray commas.

To check the patch against upstream's own suite, including the libiberty
regression corpus, copy `src/ast.rs` into a checkout of the 0.5.1 tag and run
`cargo test` there.

Drop this copy and the `[patch.crates-io]` entry in the top-level
`Cargo.toml` once a cpp_demangle release parses these names.
