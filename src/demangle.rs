//! Source-level spellings of mangled linker names.

mod dlang;

/// Bounds the C++ demangler's recursion so adversarial names cannot exhaust
/// the stack.
const CPP_RECURSION_LIMIT: u32 = 96;

/// Demangles a Rust (legacy or v0), Itanium C++, or D linker name. A D
/// name is what the symbol names, without its type (see [`dlang`]).
///
/// Rust is tried first because its legacy scheme is a subset of the Itanium
/// grammar; the alternate rendering omits Rust's per-crate hash suffix.
pub fn demangle(name: &str) -> Option<String> {
    if let Ok(demangled) = rustc_demangle::try_demangle(name) {
        return Some(format!("{demangled:#}"));
    }

    if name.starts_with("_D") {
        return dlang::qualified_name(name);
    }

    if !name.starts_with("_Z") {
        return None;
    }

    let symbol = cpp_demangle::Symbol::new_with_options(
        name,
        &cpp_demangle::ParseOptions::default().recursion_limit(CPP_RECURSION_LIMIT),
    )
    .ok()?;
    symbol
        .demangle_with_options(
            &cpp_demangle::DemangleOptions::new().recursion_limit(CPP_RECURSION_LIMIT),
        )
        .ok()
}

/// Whether a mangled linker name demangles to a name a person writes. As
/// with gdb, the name may leave out the scopes that qualify it, and a C++
/// function's parameters: `scale`, `shapes::scale`, and
/// `shapes::scale(double)` all spell `_ZN6shapes5scaleEd`.
pub fn spells(mangled: &str, name: &str) -> bool {
    let (written, written_parameters) = split_parameters(name);

    // Only a mangled name holding the name's last part can demangle to it,
    // which spares demangling every symbol of a module.
    let last = last_part(name);
    if last.is_empty() || !mangled.contains(last) {
        return false;
    }

    let Some(demangled) = demangle(mangled) else {
        return false;
    };

    let (qualified, parameters) = split_parameters(&demangled);

    // A clone, such as `[clone .constprop.0]`, is other code.
    if parameters.contains("[clone") {
        return false;
    }

    let named = qualified == written
        || qualified
            .strip_suffix(written)
            .is_some_and(|scopes| scopes.ends_with("::"));
    named
        && (written_parameters.is_empty()
            || parameters == written_parameters
            || parameters
                .strip_prefix(written_parameters)
                .is_some_and(|qualifiers| qualifiers.starts_with(' ')))
}

/// The part of a written or demangled name after its last scope, without
/// its parameters, which a name `spells` must share with its symbol's.
pub fn last_part(name: &str) -> &str {
    let (written, _) = split_parameters(name);
    written.rsplit("::").next().unwrap_or(written)
}

/// What a mangled name names, without its parameters: Nim mangles its
/// procedures as C++'s are, `_ZN6values7reachedE6string` naming
/// `values::reached`.
pub fn qualified_name(mangled: &str) -> Option<String> {
    let demangled = demangle(mangled)?;
    let (qualified, _) = split_parameters(&demangled);
    Some(qualified.to_owned())
}

/// The name an Ada programmer writes for an entity GNAT named, when its
/// encoding decodes exactly: scopes joined by `__`, which no Ada identifier
/// holds, become dots, and an overload's number (`__2`) and a body's mark
/// (`X`, `Xb`, `Xn`) are dropped. GNAT writes names in lower case and its
/// other encodings in upper case or with `___`, so any other spelling is
/// not decoded at all.
pub fn ada_name(encoded: &str) -> Option<String> {
    let name = ["Xb", "Xn", "X"]
        .into_iter()
        .find_map(|mark| encoded.strip_suffix(mark))
        .unwrap_or(encoded);
    let name = name
        .rsplit_once("__")
        .filter(|(_, number)| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
        .map_or(name, |(scoped, _)| scoped);
    let identifier = |part: &str| {
        part.starts_with(|first: char| first.is_ascii_lowercase())
            && !part.ends_with('_')
            && !part.contains("__")
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    };

    name.split("__")
        .all(identifier)
        .then(|| name.replace("__", "."))
}

/// A demangled Rust function's path, with no generic arguments and an
/// inherent method's type unwrapped: v0's
/// `<tokio::runtime::park::CachedParkThread>::block_on::<F>` and legacy's
/// `tokio::runtime::park::CachedParkThread::block_on` are both the path
/// `tokio::runtime::park::CachedParkThread::block_on`. `None` for a trait
/// implementation's method, which a path does not name, or unbalanced
/// brackets.
pub fn rust_path(demangled: &str) -> Option<String> {
    let unwrapped;
    let mut name = demangled;
    if let Some(inner) = name.strip_prefix('<') {
        let close = closing(inner)?;
        if inner[..close].contains(" as ") {
            return None;
        }

        unwrapped = format!("{}{}", &inner[..close], &inner[close + 1..]);
        name = &unwrapped;
    }

    let mut path = String::with_capacity(name.len());
    let mut depth = 0_usize;
    for (at, character) in name.char_indices() {
        match character {
            // A function type's arrow closes nothing.
            '>' if name[..at].ends_with('-') => {}
            '<' => depth += 1,
            '>' => depth = depth.checked_sub(1)?,
            _ if depth == 0 => path.push(character),
            _ => {}
        }
    }

    (depth == 0).then(|| path.replace("::::", "::").trim_end_matches("::").to_owned())
}

/// Where the `>` closing a `<` just before `text` is in it.
fn closing(text: &str) -> Option<usize> {
    let mut depth = 1_usize;
    for (at, character) in text.char_indices() {
        match character {
            '>' if text[..at].ends_with('-') => {}
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }

    None
}

/// A function's name and its parameter list, which begins at the first
/// parenthesis outside template arguments.
fn split_parameters(name: &str) -> (&str, &str) {
    let mut depth = 0_usize;
    for (index, character) in name.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            '(' if depth == 0 => return name.split_at(index),
            _ => {}
        }
    }

    (name, "")
}

/// What compilers append to a function's name to name a part of it they
/// moved out of line, as GCC's `f.cold` and `f.part.0`, LLVM's `f.cold.1`,
/// and BOLT's `f.warm`, or a copy of it they specialized, as GCC's
/// `f.isra.0`, `f.constprop.0`, and `f.lto_priv.0`. Each may be numbered.
const SPLIT_SUFFIXES: [&str; 6] = ["cold", "warm", "part", "isra", "constprop", "lto_priv"];

/// The function a symbol names a split-off part or a specialized copy of,
/// through any number of such suffixes, or the name itself.
pub fn split_from(name: &str) -> &str {
    let mut name = name;
    loop {
        let numbered = name
            .rsplit_once('.')
            .filter(|(_, number)| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            })
            .map(|(base, _)| base);
        let Some(base) = [Some(name), numbered]
            .into_iter()
            .flatten()
            .find_map(|candidate| {
                let (base, suffix) = candidate.rsplit_once('.')?;
                (!base.is_empty() && SPLIT_SUFFIXES.contains(&suffix)).then_some(base)
            })
        else {
            return name;
        };

        name = base;
    }
}

#[cfg(test)]
mod tests {
    use super::{ada_name, demangle, rust_path, spells};

    #[test]
    fn rust_paths_leave_out_generic_arguments_and_inherent_impls_brackets() {
        let path = |mangled: &str| rust_path(&demangle(mangled).expect("a Rust name"));
        let block_on = "tokio::runtime::park::CachedParkThread::block_on";
        for (mangled, expected) in [
            (
                "_RINvMs2_NtNtCshWOCllL2uKN_5tokio7runtime4parkNtB6_16CachedParkThread\
                 8block_onNCNvCsiiWaAJDmxH2_7workers3run0EB1h_",
                Some(block_on.to_owned()),
            ),
            (
                "_RNCINvMs2_NtNtCshWOCllL2uKN_5tokio7runtime4parkNtB8_16CachedParkThread\
                 8block_onNCNvCsiiWaAJDmxH2_7workers3run0E0B1j_",
                Some(format!("{block_on}::{{closure#0}}")),
            ),
            (
                "_ZN5tokio7runtime4park16CachedParkThread8block_on17h0123456789abcdefE",
                Some(block_on.to_owned()),
            ),
        ] {
            assert_eq!(path(mangled), expected, "{mangled}");
        }

        assert_eq!(
            rust_path("<alloc::boxed::Box<F> as core::future::Future>::poll"),
            None
        );
        assert_eq!(
            rust_path("core::ops::function::FnOnce<fn() -> u8>::call"),
            Some("core::ops::function::FnOnce::call".into())
        );
        assert_eq!(rust_path("a::b<c"), None);
    }

    #[test]
    fn written_names_spell_mangled_ones_with_or_without_scopes_and_parameters() {
        for (mangled, name, expected) in [
            ("_ZN6shapes5scaleEd", "shapes::scale", true),
            ("_ZN6shapes5scaleEd", "shapes::scale(double)", true),
            ("_ZN6shapes5scaleEd", "scale", true),
            ("_ZN6shapes5scaleEd", "shapes::scale(int)", false),
            ("_ZN6shapes5scaleEd", "apes::scale", false),
            ("_ZN6shapes5scaleEd", "shapes::scal", false),
            ("_ZNK6shapes6Widget4pickEv", "shapes::Widget::pick", true),
            ("_ZNK6shapes6Widget4pickEv", "Widget::pick()", true),
            ("_ZN5boxedIiE3getEv", "boxed<int>::get", true),
            ("_Z3bazi.constprop.0", "baz", false),
            (
                "_ZN4core3fmt5write17h0123456789abcdefE",
                "core::fmt::write",
                true,
            ),
            (
                "_RNvCsdHzz05DFzbQ_5crash9crash_now",
                "crash::crash_now",
                true,
            ),
            ("_RNvCsdHzz05DFzbQ_5crash9crash_now", "crash::crash", false),
            ("main", "main", false),
        ] {
            assert_eq!(spells(mangled, name), expected, "{mangled} as {name}");
        }
    }

    #[test]
    fn rust_and_cpp_names_demangle_and_other_names_do_not() {
        for (mangled, expected) in [
            // Rust v0, as current toolchains emit it, without crate hashes.
            (
                "_RNvCsdHzz05DFzbQ_5crash9crash_now",
                Some("crash::crash_now"),
            ),
            // Rust legacy, without the trailing hash.
            (
                "_ZN4core3fmt5write17h0123456789abcdefE",
                Some("core::fmt::write"),
            ),
            ("_ZN3foo3barEv", Some("foo::bar()")),
            ("_ZNSt6thread6detachEv", Some("std::thread::detach()")),
            // GCC clones keep their suffix.
            ("_Z3bazi.constprop.0", Some("baz(int) [clone .constprop.0]")),
            ("main", None),
            ("_start", None),
            ("_Z", None),
            ("_ZZZZZZZZ", None),
        ] {
            assert_eq!(demangle(mangled).as_deref(), expected, "{mangled}");
        }
    }

    /// Checked against libiberty's D demangler, less parameters and with
    /// template arguments left out.
    #[test]
    fn d_names_demangle_to_what_they_name_or_not_at_all() {
        for (mangled, expected) in [
            // A function nested in another, which is followed by its type.
            (
                "_D2rt6dmain212_d_run_main2UAAamPUQgZiZ6runAllMFZv",
                Some("rt.dmain2._d_run_main2.runAll"),
            ),
            (
                "_D3std6socket9TcpSocket6__vtblZ",
                Some("std.socket.TcpSocket.__vtbl"),
            ),
            // Template instances, with values, back references to their
            // names, and symbols as arguments.
            (
                "_D3std10functional__T6safeOpVAyaa1_3cZ__TQuTmTiZQBbFNaNbNiNfKmKiZb",
                Some("std.functional.safeOp!(…).safeOp!(…).safeOp"),
            ),
            (
                "_D3std11concurrency__T8initOnceS_DQBg8datetime8timezone9LocalTime9singletonFNeZ5guardObZQCoFNcLObZOb",
                Some("std.concurrency.initOnce!(…).initOnce"),
            ),
            // A type that refers back to a function type.
            (
                "_D3std11concurrency14FiberScheduler6createMFNbDFZvZ4wrapMQk",
                Some("std.concurrency.FiberScheduler.create.wrap"),
            ),
            ("_Dmain", Some("D main")),
            // A thunk, names cut short, and what only begins as D's do.
            (
                "_DThn16_4core8internal2gc4impl6manualQp8ManualGC6enableMFZv",
                None,
            ),
            ("_D2rt6dmain212_d_run_main2UAAam", None),
            ("_D3std", None),
            ("_D3stdQz", None),
            ("_DYNAMIC", None),
        ] {
            assert_eq!(demangle(mangled).as_deref(), expected, "{mangled}");
        }

        let nested = format!("_D1f{}v", "P".repeat(10_000));
        assert_eq!(demangle(&nested), None);
    }

    #[test]
    fn gnat_names_decode_exactly_or_not_at_all() {
        for (encoded, expected) in [
            ("values__reached", Some("values.reached")),
            (
                "ada__characters__handling__to_upper",
                Some("ada.characters.handling.to_upper"),
            ),
            // An overload's number and a body's mark are not the name's.
            (
                "ada__characters__handling__to_upper__2",
                Some("ada.characters.handling.to_upper"),
            ),
            (
                "ada__exceptions__exception_data__append_info_natXn",
                Some("ada.exceptions.exception_data.append_info_nat"),
            ),
            (
                "ada__exceptions__exception_data__append_info_exception_name__2Xn",
                Some("ada.exceptions.exception_data.append_info_exception_name"),
            ),
            ("values", Some("values")),
            // Other encodings, and what no Ada name is, stay as GNAT wrote
            // them.
            ("ada__containers__Tcount_typeB", None),
            ("ada__containers___elabs", None),
            ("ada__characters__handling__to_string__L_6__T144b___L", None),
            ("system__secondary_stack__ss_allocate__2__3", None),
            ("_ada_values", None),
            ("values__", None),
            ("values__2", Some("values")),
            ("main", Some("main")),
        ] {
            assert_eq!(ada_name(encoded).as_deref(), expected, "{encoded}");
        }
    }

    #[test]
    fn deeply_nested_cpp_names_are_rejected_instead_of_exhausting_the_stack() {
        let nested = format!("_Z1f{}i{}", "PFv".repeat(10_000), "E".repeat(10_000));
        assert_eq!(demangle(&nested), None);
    }
}
