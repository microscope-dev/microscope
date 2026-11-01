//! Source-level spellings of mangled linker names.

mod dlang;

/// Bounds the C++ demangler's recursion so adversarial names cannot exhaust
/// the stack.
const CPP_RECURSION_LIMIT: u32 = 96;

/// Demangles a Rust (legacy or v0), Itanium C++, or D linker name. A D
/// name is what the symbol names, without its type (see [`dlang`]).
pub fn demangle(name: &str) -> Option<String> {
    demangle_with_scheme(name).map(|(demangled, _)| demangled)
}

/// The scheme a linker name was mangled by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Rust,
    Cpp,
    D,
}

/// Rust is tried first because its legacy scheme is a subset of the Itanium
/// grammar; the alternate rendering omits Rust's per-crate hash suffix.
fn demangle_with_scheme(name: &str) -> Option<(String, Scheme)> {
    if let Ok(demangled) = rustc_demangle::try_demangle(name) {
        return Some((format!("{demangled:#}"), Scheme::Rust));
    }

    if name.starts_with("_D") {
        return dlang::qualified_name(name).map(|name| (name, Scheme::D));
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
        .map(|demangled| (demangled, Scheme::Cpp))
}

/// Whether a mangled linker name demangles to a name a person writes. As
/// with gdb, the name may leave out the scopes that qualify it, a C++
/// function's parameters, and a function template's arguments:
/// `scale`, `shapes::scale`, and `shapes::scale(double)` all spell
/// `_ZN6shapes5scaleEd`, and `max` spells `int max<int>(int, int)`. A Rust
/// inherent method's type is written without its brackets: `Foo::bar`
/// spells `<krate::Foo>::bar`.
pub fn spells(mangled: &str, name: &str) -> bool {
    // Demangling every symbol of a module takes most of a second for a
    // large C++ program.
    if !Clue::of(name).is_in(mangled) {
        return false;
    }

    let Some((demangled, scheme)) = demangle_with_scheme(mangled) else {
        return false;
    };

    let (head, parameters) = split_parameters(&demangled);

    // A clone, such as `[clone .constprop.0]`, is other code.
    if parameters.contains("[clone") {
        return false;
    }

    let (written, written_parameters) = split_parameters(name);
    let (qualified, written) = match scheme {
        Scheme::Rust => (rust_spelling(head), rust_spelling(written)),
        Scheme::Cpp => (without_return_type(head).to_owned(), written.to_owned()),
        Scheme::D => (head.to_owned(), written.to_owned()),
    };

    let qualified = components(&qualified);
    let written = components(&written);
    let Some(scopes) = qualified.len().checked_sub(written.len()) else {
        return false;
    };

    let named = written.split_last().is_some_and(|(last, written_scopes)| {
        qualified.get(scopes..qualified.len() - 1) == Some(written_scopes)
            && qualified.last().is_some_and(|qualified| {
                qualified == last
                    || (!last.contains('<') && without_template_arguments(qualified) == *last)
            })
    });

    named
        && (written_parameters.is_empty()
            || parameters == written_parameters
            || parameters
                .strip_prefix(written_parameters)
                .is_some_and(|qualifiers| qualifiers.starts_with(' ')))
}

/// What a linker name must hold to spell a written name, as [`spells`]
/// reads it: the identifier the name's last part is, or the codes Itanium
/// mangles its operator to, or the abbreviation of one of `std`'s
/// classes. Checking for it first spares demangling every symbol.
#[derive(Debug, Clone, Copy)]
pub struct Clue<'a> {
    word: &'a str,
    codes: &'static [&'static str],
}

impl<'a> Clue<'a> {
    #[must_use]
    pub fn of(name: &'a str) -> Self {
        let last = without_template_arguments(last_part(name));
        let codes = if let Some(symbol) = last.strip_prefix("operator")
            && operator_symbol(symbol).is_some_and(|length| length == symbol.len())
        {
            OPERATORS
                .iter()
                .find(|(spelled, _)| *spelled == symbol.trim_start_matches(' '))
                .map_or(&[][..], |(_, codes)| *codes)
        } else if last.starts_with("operator ") {
            // A conversion operator is named by its type.
            &["cv"][..]
        } else {
            let class = last.strip_prefix('~').unwrap_or(last);
            STD_ABBREVIATIONS
                .iter()
                .find(|(spelled, _)| *spelled == class)
                .map_or(&[][..], |(_, codes)| *codes)
        };

        // An identifier is in the linker name as written; a destructor's is
        // its class's.
        let word = last.strip_prefix('~').unwrap_or(last);
        Self {
            word: if word.is_empty() { last } else { word },
            codes,
        }
    }

    /// Whether `mangled` holds the clue, so that it may spell the name.
    #[must_use]
    pub fn is_in(self, mangled: &str) -> bool {
        !self.word.is_empty()
            && (mangled.contains(self.word) || self.codes.iter().any(|code| mangled.contains(code)))
    }
}

/// Itanium's operator names, as C++ spells each after `operator`, and the
/// codes it mangles them to: `-` is binary `mi` or unary `ng`.
const OPERATORS: &[(&str, &[&str])] = &[
    ("new", &["nw"]),
    ("new[]", &["na"]),
    ("delete", &["dl"]),
    ("delete[]", &["da"]),
    ("co_await", &["aw"]),
    ("+", &["pl", "ps"]),
    ("-", &["mi", "ng"]),
    ("&", &["an", "ad"]),
    ("*", &["ml", "de"]),
    ("~", &["co"]),
    ("/", &["dv"]),
    ("%", &["rm"]),
    ("|", &["or"]),
    ("^", &["eo"]),
    ("=", &["aS"]),
    ("+=", &["pL"]),
    ("-=", &["mI"]),
    ("*=", &["mL"]),
    ("/=", &["dV"]),
    ("%=", &["rM"]),
    ("&=", &["aN"]),
    ("|=", &["oR"]),
    ("^=", &["eO"]),
    ("<<", &["ls"]),
    (">>", &["rs"]),
    ("<<=", &["lS"]),
    (">>=", &["rS"]),
    ("==", &["eq"]),
    ("!=", &["ne"]),
    ("<", &["lt"]),
    (">", &["gt"]),
    ("<=", &["le"]),
    (">=", &["ge"]),
    ("<=>", &["ss"]),
    ("!", &["nt"]),
    ("&&", &["aa"]),
    ("||", &["oo"]),
    ("++", &["pp"]),
    ("--", &["mm"]),
    (",", &["cm"]),
    ("->*", &["pm"]),
    ("->", &["pt"]),
    ("()", &["cl"]),
    ("[]", &["ix"]),
];

/// The classes of `std` Itanium abbreviates, by the name their
/// constructors and destructors have.
const STD_ABBREVIATIONS: &[(&str, &[&str])] = &[
    ("allocator", &["Sa"]),
    ("basic_string", &["Sb"]),
    ("string", &["Ss"]),
    ("basic_istream", &["Si"]),
    ("ostream", &["So"]),
    ("basic_iostream", &["Sd"]),
];

/// How long the symbol of an operator is that begins `text`, which follows
/// the word `operator`, with the spaces before it.
fn operator_symbol(text: &str) -> Option<usize> {
    let symbol = text.trim_start_matches(' ');
    let spaces = text.len() - symbol.len();
    OPERATORS
        .iter()
        .map(|(spelled, _)| *spelled)
        .filter(|spelled| {
            symbol.starts_with(spelled)
                && !(spelled.starts_with(identifier_character)
                    && symbol[spelled.len()..].starts_with(identifier_character))
        })
        .map(str::len)
        .max()
        .filter(|length| spaces > 0 || !symbol[..*length].starts_with(identifier_character))
        .map(|length| spaces + length)
}

fn identifier_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

/// The offsets in `name` of the characters outside brackets, operator
/// symbols, a conversion operator's type, and `(anonymous namespace)`,
/// with the characters, until `stop` says to stop at one.
fn find_outside(name: &str, mut stop: impl FnMut(usize, char) -> bool) -> Option<usize> {
    const ANONYMOUS: &str = "(anonymous namespace)";

    let mut depth = 0_usize;
    let mut skip_to = 0;
    let mut converting = false;
    for (at, character) in name.char_indices() {
        if at < skip_to {
            continue;
        }

        if depth == 0 {
            let rest = &name[at..];
            let word_starts = !name[..at].ends_with(identifier_character);
            if word_starts
                && let Some(after) = rest.strip_prefix("operator")
                && !after.starts_with(identifier_character)
            {
                match operator_symbol(after) {
                    Some(length) => skip_to = at + "operator".len() + length,
                    None => converting = true,
                }
            }

            if rest.starts_with(ANONYMOUS) {
                skip_to = at + ANONYMOUS.len();
                continue;
            }

            if !(converting && character == ' ') && stop(at, character) {
                return Some(at);
            }
        }

        match character {
            // A function type's arrow closes nothing.
            '>' if name[..at].ends_with('-') => {}
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }

        if character == '(' && depth == 1 {
            converting = false;
        }
    }

    None
}

/// The components of a qualified name, outermost first.
fn components(name: &str) -> Vec<&str> {
    let mut components = Vec::new();
    let mut start = 0;
    let mut previous = None;
    find_outside(name, |at, character| {
        if character == ':' && previous == Some((at - 1, ':')) {
            components.push(&name[start..at - 1]);
            start = at + 1;
            previous = None;
        } else {
            previous = Some((at, character));
        }
        false
    });
    components.push(&name[start..]);
    components
}

/// A C++ function's name without the return type a function template's
/// name begins with: `max<int>` for `int max<int>`.
fn without_return_type(name: &str) -> &str {
    let mut last_space = None;
    find_outside(name, |at, character| {
        if character == ' ' {
            last_space = Some(at);
        }
        false
    });
    last_space.map_or(name, |at| &name[at + 1..])
}

/// A name without the template arguments that end it: `max` for
/// `max<int>`, and `operator<` for itself.
fn without_template_arguments(name: &str) -> &str {
    find_outside(name, |_, character| character == '<')
        .filter(|_| name.ends_with('>'))
        .map_or(name, |at| &name[..at])
}

/// A demangled Rust name as a person writes it: an inherent method's type
/// without its brackets, and generic arguments without the `::` before
/// them, as `krate::Foo::bar<T>` for `<krate::Foo>::bar::<T>`. A trait
/// implementation's method keeps its brackets.
fn rust_spelling(name: &str) -> String {
    let unwrapped = name.strip_prefix('<').and_then(|inner| {
        let close = closing(inner)?;
        (!inner[..close].contains(" as "))
            .then(|| format!("{}{}", &inner[..close], &inner[close + 1..]))
    });
    unwrapped.as_deref().unwrap_or(name).replace("::<", "<")
}

/// The part of a written or demangled name after its last scope, without
/// its parameters, which a name `spells` must share with its symbol's.
pub fn last_part(name: &str) -> &str {
    let (written, _) = split_parameters(name);
    components(written).last().copied().unwrap_or(written)
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
/// parenthesis outside template arguments and operator symbols.
fn split_parameters(name: &str) -> (&str, &str) {
    find_outside(name, |_, character| character == '(').map_or((name, ""), |at| name.split_at(at))
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
    use proptest::prelude::*;

    use super::{ada_name, demangle, rust_path, spells};

    /// C++20's manglings, as Chromium's build has them: constrained
    /// template parameters (`Tk`, `Tp`), whose constraint's name later
    /// substitutions count; requires clauses (`Q`); template parameters of
    /// an enclosing level (`TL`); and generic lambdas (`UlTy`). Each part
    /// expected is `llvm-cxxfilt`'s.
    #[test]
    fn cpp20_constraints_requires_clauses_and_generic_lambdas_demangle() {
        for (mangled, parts) in [
            (
                "_ZN15partition_alloc13PartitionRoot10FreeInlineILNS_8internal9FreeFlagsE0ETpTkNS_8FreeHintEJEEEvPvDpT0_",
                &["partition_alloc::PartitionRoot::FreeInline<"][..],
            ),
            (
                "_ZN15partition_alloc13PartitionRoot17FreeInUnknownRootILNS_8internal9FreeFlagsE6ETpTkNS_8FreeHintEJNS2_4base11StrongAliasINS_15FreeSizeHintTagEmEEEEEvPvDpT0_",
                &[
                    "partition_alloc::PartitionRoot::FreeInUnknownRoot<",
                    "partition_alloc::internal::base::StrongAlias<partition_alloc::FreeSizeHintTag, unsigned long>",
                ],
            ),
            (
                "_ZN4base12ObserverListIN16OmniboxEditModel8ObserverELb0ELNS_28ObserverListReentrancyPolicyE0ENS_8internal22CheckedObserverAdapterEE21NotifyAllowReentrancyIMS2_FvvEJEQsr3stdE9invocableITL0__PT_DpRKTL0_0_EEEvSB_DpRKT0_",
                &[
                    "base::ObserverList<OmniboxEditModel::Observer, false",
                    "::NotifyAllowReentrancy<void (OmniboxEditModel::Observer::*)()>",
                ],
            ),
            (
                "_ZNKSt4__Cr6ranges13__upper_boundclB9sqn240000ITkNS0_13forward_rangeERN4base4spanIKN5blink17OffsetMappingUnitELm18446744073709551615EPS7_EEjMS6_KFjvETkNS_26indirect_strict_weak_orderIPKT0_NS_16__projected_implIDTclL_ZNS0_5__cpo5beginEEclsr3stdE7declvalIRT_EEEET1_E6__typeEEENS0_4lessEEENS_7_IfImplIX14borrowed_rangeISJ_EEE7_SelectISL_NS0_8danglingEEEOSJ_RSF_T2_SM_",
                &[
                    "std::__Cr::ranges::__upper_bound::operator()",
                    "base::span<blink::OffsetMappingUnit const",
                ],
            ),
            (
                "_ZNKSt4__Cr6ranges7__equalclB9sqn240000ITkNS0_11input_rangeEN4base4spanIKhLm18446744073709551615EPS5_EETkNS0_11input_rangeES7_NS0_8equal_toENS_8identityES9_Q21indirectly_comparableIDTclL_ZNS0_5__cpo5beginEEclsr3stdE7declvalIRT_EEEEDTclL_ZNSA_5beginEEclsr3stdE7declvalIRT0_EEEET1_T2_T3_EEEbOSB_OSE_SH_SI_SJ_",
                &[
                    "std::__Cr::ranges::__equal::operator()",
                    "std::__Cr::ranges::equal_to, std::__Cr::identity, std::__Cr::identity>",
                ],
            ),
            (
                "_ZNSt4__Cr24__copy_move_unwrap_itersB9sqn240000INS_11__copy_implEN4base25CheckedContiguousIteratorIKcEES5_NS3_IcEETnNS_9enable_ifIXsr12__can_rewrapIT0_T2_EE5valueEiE4typeELi0EEENS_15__in_out_resultIS8_S9_EES8_T1_S9_",
                &[
                    "std::__Cr::__copy_move_unwrap_iters",
                    "base::CheckedContiguousIterator<char>",
                ],
            ),
            (
                "_ZN5blink12_GLOBAL__N_124DetermineAlgorithmAndRunIZNS0_19LayoutWithAlgorithmERKNS_21LayoutAlgorithmParamsEEUlTyPT_E_EEvS4_RKS5_",
                &[
                    "blink::(anonymous namespace)::DetermineAlgorithmAndRun<",
                    "blink::(anonymous namespace)::LayoutWithAlgorithm(blink::LayoutAlgorithmParams const&)",
                ],
            ),
        ] {
            let demangled = demangle(mangled);
            for part in parts {
                assert!(
                    demangled.as_deref().is_some_and(|name| name.contains(part)),
                    "{mangled}: {demangled:?} lacks {part}"
                );
            }
        }
    }

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

    #[test]
    fn operators_destructors_and_templates_spell_their_symbols() {
        for (mangled, name) in [
            ("_ZN1AclEv", "A::operator()"),
            ("_ZN1AclEv", "A::operator()()"),
            ("_ZN1AclEv", "operator()"),
            ("_ZN6WidgetD2Ev", "Widget::~Widget"),
            ("_ZN6WidgetD2Ev", "~Widget"),
            ("_ZN3fooltERKNS_1AES2_", "foo::operator<"),
            (
                "_ZN3fooltERKNS_1AES2_",
                "foo::operator<(foo::A const&, foo::A const&)",
            ),
            ("_Z3maxIiET_S0_S0_", "max"),
            ("_Z3maxIiET_S0_S0_", "max<int>"),
            ("_Z3maxIiET_S0_S0_", "max<int>(int, int)"),
            ("_ZNSaIcED1Ev", "std::allocator<char>::~allocator"),
            // Shrunk by the properties below.
            ("_Z1aIiEiv", "a<int>"),
            ("_RNvMCsdHzz05DFzbQ_1aNtCsdHzz05DFzbQ_1a1a1a", "a::a::a"),
            (
                "_RNvMCsdHzz05DFzbQ_5krateNtCsdHzz05DFzbQ_5krate3Foo3bar",
                "Foo::bar",
            ),
        ] {
            assert!(spells(mangled, name), "{mangled} as {name}");
        }

        for (mangled, name) in [
            ("_ZN3fooltERKNS_1AES2_", "foo::operator>"),
            ("_ZN3fooltERKNS_1AES2_", "operator<<"),
            ("_Z3maxIiET_S0_S0_", "max<char>"),
            ("_Z3maxIiET_S0_S0_", "int max<int>"),
            ("_ZN6WidgetD2Ev", "Widget::Widget"),
            ("_ZN5boxedIiE3getEv", "t>::get"),
        ] {
            assert!(!spells(mangled, name), "{mangled} as {name}");
        }
    }

    /// A builtin type's Itanium code and its spelling.
    const BUILTINS: [(&str, &str); 6] = [
        ("i", "int"),
        ("c", "char"),
        ("d", "double"),
        ("b", "bool"),
        ("l", "long"),
        ("m", "unsigned long"),
    ];

    /// Operators as C++ spells them after `operator` and their codes.
    const OPERATOR_CODES: [(&str, &str); 10] = [
        ("()", "cl"),
        ("<", "lt"),
        ("<<", "ls"),
        ("<=>", "ss"),
        ("==", "eq"),
        ("[]", "ix"),
        ("->", "pt"),
        (" new", "nw"),
        (" delete[]", "da"),
        ("+", "pl"),
    ];

    #[derive(Debug, Clone)]
    enum Last {
        Function(String),
        Operator(usize),
        Constructor(&'static str),
        Destructor(&'static str),
    }

    /// A C++ function as a person writes it, which mangles and demangles
    /// to known text.
    #[derive(Debug, Clone)]
    struct Cpp {
        scopes: Vec<String>,

        /// The last scope's template arguments, by builtin.
        class_arguments: Vec<usize>,
        last: Last,

        /// A function template's arguments and return type.
        template: Option<(Vec<usize>, usize)>,
        parameters: Vec<usize>,
        constant: bool,
    }

    fn source_name(identifier: &str) -> String {
        format!("{}{identifier}", identifier.len())
    }

    fn builtin_codes(builtins: &[usize]) -> String {
        builtins
            .iter()
            .map(|builtin| BUILTINS[*builtin].0)
            .collect()
    }

    fn builtin_list(builtins: &[usize]) -> String {
        builtins
            .iter()
            .map(|builtin| BUILTINS[*builtin].1)
            .collect::<Vec<_>>()
            .join(", ")
    }

    impl Cpp {
        fn mangled(&self) -> String {
            let mut name = String::new();
            for scope in &self.scopes {
                name.push_str(&source_name(scope));
            }
            if !self.class_arguments.is_empty() {
                name = format!("{name}I{}E", builtin_codes(&self.class_arguments));
            }
            name.push_str(&match &self.last {
                Last::Function(identifier) => source_name(identifier),
                Last::Operator(operator) => OPERATOR_CODES[*operator].1.to_owned(),
                Last::Constructor(code) | Last::Destructor(code) => (*code).to_owned(),
            });
            if let Some((arguments, _)) = &self.template {
                name = format!("{name}I{}E", builtin_codes(arguments));
            }
            if !self.scopes.is_empty() {
                let constant = if self.constant { "K" } else { "" };
                name = format!("N{constant}{name}E");
            }

            let returns = self
                .template
                .as_ref()
                .map_or("", |(_, returns)| BUILTINS[*returns].0);
            let parameters = if self.parameters.is_empty() {
                "v".to_owned()
            } else {
                builtin_codes(&self.parameters)
            };
            format!("_Z{name}{returns}{parameters}")
        }

        /// Each component as a person writes it, the last with and without
        /// a function template's arguments.
        fn components(&self) -> (Vec<String>, String, String) {
            let mut scopes = self.scopes.clone();
            if let Some(last) = scopes.last_mut()
                && !self.class_arguments.is_empty()
            {
                *last = format!("{last}<{}>", builtin_list(&self.class_arguments));
            }
            let class = self.scopes.last().cloned().unwrap_or_default();
            let last = match &self.last {
                Last::Function(identifier) => identifier.clone(),
                Last::Operator(operator) => format!("operator{}", OPERATOR_CODES[*operator].0),
                Last::Constructor(_) => class,
                Last::Destructor(_) => format!("~{class}"),
            };
            let with_arguments = match &self.template {
                Some((arguments, _)) => format!("{last}<{}>", builtin_list(arguments)),
                None => last.clone(),
            };
            (scopes, last, with_arguments)
        }

        fn parameters(&self) -> String {
            format!("({})", builtin_list(&self.parameters))
        }

        fn demangled(&self) -> String {
            let (mut components, _, last) = self.components();
            components.push(last);
            let returns = self
                .template
                .as_ref()
                .map_or(String::new(), |(_, returns)| {
                    format!("{} ", BUILTINS[*returns].1)
                });
            let constant = if self.constant { " const" } else { "" };
            format!(
                "{returns}{}{}{constant}",
                components.join("::"),
                self.parameters()
            )
        }

        /// What gdb accepts: each scope suffix, the last part with or
        /// without its template arguments, and with no parameters or
        /// the exact ones.
        fn spellings(&self) -> Vec<String> {
            let (scopes, last, with_arguments) = self.components();
            let mut lasts = vec![with_arguments.clone()];
            if last != with_arguments {
                lasts.push(last);
            }
            let mut parameters = vec![String::new(), self.parameters()];
            if self.constant {
                parameters.push(format!("{} const", self.parameters()));
            }

            let mut spellings = Vec::new();
            for from in 0..=scopes.len() {
                for last in &lasts {
                    for parameters in &parameters {
                        let mut components = scopes[from..].to_vec();
                        components.push(last.clone());
                        spellings.push(format!("{}{parameters}", components.join("::")));
                    }
                }
            }
            spellings
        }

        /// Spellings of other functions: another scope, a scope cut short,
        /// other template arguments, and other parameters.
        fn others(&self) -> Vec<String> {
            let (scopes, _, with_arguments) = self.components();
            let full = |scopes: &[String], last: &str, parameters: &str| {
                let mut components = scopes.to_vec();
                components.push(last.to_owned());
                format!("{}{parameters}", components.join("::"))
            };

            let mut others = vec![
                full(&scopes, &format!("q::{with_arguments}"), ""),
                format!("q::{}", full(&scopes, &with_arguments, "")),
                full(
                    &scopes,
                    &with_arguments,
                    &format!(
                        "({})",
                        builtin_list(&[self.parameters.clone(), vec![1]].concat())
                    ),
                ),
            ];
            for (at, scope) in scopes.iter().enumerate() {
                let mut other = scopes.clone();
                other[at] = format!("q{scope}");
                others.push(full(&other[at..], &with_arguments, ""));
                if scope.len() > 1 && !scope.starts_with('q') {
                    let mut cut = scopes[at..].to_vec();
                    cut[0] = scope[1..].to_owned();
                    others.push(full(&cut, &with_arguments, ""));
                }
            }
            if let Last::Function(identifier) = &self.last {
                others.push(full(&scopes, &format!("q{identifier}"), ""));
            }
            if self.template.is_some() {
                let (_, last, _) = self.components();
                others.push(full(&scopes, &format!("{last}<float>"), ""));
            }
            others
        }
    }

    fn identifier() -> impl Strategy<Value = String> {
        "[a-p][a-z0-9_]{0,5}"
    }

    fn builtins(range: std::ops::Range<usize>) -> impl Strategy<Value = Vec<usize>> {
        proptest::collection::vec(0..BUILTINS.len(), range)
    }

    fn cpp() -> impl Strategy<Value = Cpp> {
        (proptest::collection::vec(identifier(), 0..3), any::<bool>())
            .prop_flat_map(|(scopes, class_template)| {
                let class = !scopes.is_empty();
                let last = if class {
                    prop_oneof![
                        identifier().prop_map(Last::Function),
                        (0..OPERATOR_CODES.len()).prop_map(Last::Operator),
                        prop::sample::select(vec!["C1", "C2"]).prop_map(Last::Constructor),
                        prop::sample::select(vec!["D0", "D1", "D2"]).prop_map(Last::Destructor),
                    ]
                    .boxed()
                } else {
                    prop_oneof![
                        identifier().prop_map(Last::Function),
                        (0..OPERATOR_CODES.len()).prop_map(Last::Operator),
                    ]
                    .boxed()
                };
                let class_arguments = if class && class_template {
                    builtins(1..3).boxed()
                } else {
                    Just(Vec::new()).boxed()
                };
                (
                    Just(scopes),
                    class_arguments,
                    last,
                    proptest::option::of((builtins(1..3), 0..BUILTINS.len())),
                    builtins(0..4),
                    any::<bool>(),
                )
            })
            .prop_map(
                |(scopes, class_arguments, last, template, parameters, constant)| {
                    let function = matches!(last, Last::Function(_));
                    let method = matches!(last, Last::Function(_) | Last::Operator(_));
                    Cpp {
                        constant: constant && method && !scopes.is_empty(),
                        scopes,
                        class_arguments,
                        last,
                        template: template.filter(|_| function),
                        parameters,
                    }
                },
            )
    }

    /// A Rust function's path and how each scheme mangles it: legacy, and
    /// v0 as a free function or an inherent method of a type, whose
    /// demangling wraps the type in brackets.
    fn rust() -> impl Strategy<Value = (String, Vec<String>)> {
        (
            proptest::collection::vec(identifier(), 0..3),
            identifier(),
            identifier(),
            0..3_u8,
        )
            .prop_map(|(modules, krate, function, scheme)| {
                // An inherent method's type is the last of its modules.
                let scheme = if modules.is_empty() {
                    scheme.min(1)
                } else {
                    scheme
                };
                let crate_root = format!("CsdHzz05DFzbQ_{}", source_name(&krate));
                let modules_path = |prefix: String, modules: &[String]| {
                    modules.iter().fold(prefix, |path, module| {
                        format!("Nt{path}{}", source_name(module))
                    })
                };
                let mut path = vec![krate];
                path.extend(modules.iter().cloned());
                path.push(function.clone());
                let mangled = match scheme {
                    0 => format!(
                        "_ZN{}17h0123456789abcdefE",
                        path.iter()
                            .map(|part| source_name(part))
                            .collect::<String>()
                    ),
                    1 => format!(
                        "_RNv{}{}",
                        modules_path(crate_root, &modules),
                        source_name(&function)
                    ),
                    _ => {
                        // The last module is the type the method is of.
                        let impl_path = crate_root.clone();
                        let type_path = modules_path(crate_root, &modules);
                        format!("_RNvM{impl_path}{type_path}{}", source_name(&function))
                    }
                };
                (mangled, path)
            })
    }

    proptest::proptest! {
        #[test]
        fn a_cpp_symbol_spells_each_written_name_of_it_and_no_other(cpp in cpp()) {
            let mangled = cpp.mangled();
            proptest::prop_assert_eq!(
                demangle(&mangled),
                Some(cpp.demangled()),
                "{}",
                mangled
            );
            for name in cpp.spellings() {
                proptest::prop_assert!(spells(&mangled, &name), "{} as {}", mangled, name);
            }
            for name in cpp.others() {
                proptest::prop_assert!(!spells(&mangled, &name), "{} as {}", mangled, name);
            }
        }

        #[test]
        fn a_rust_symbol_spells_each_scope_suffix_of_its_path((mangled, path) in rust()) {
            proptest::prop_assert!(demangle(&mangled).is_some(), "{}", mangled);
            for from in 0..path.len() {
                let name = path[from..].join("::");
                proptest::prop_assert!(spells(&mangled, &name), "{} as {}", mangled, name);

                let other = format!("q{}", path[from..].join("::"));
                proptest::prop_assert!(!spells(&mangled, &other), "{} as {}", mangled, other);
            }
            let other = format!("q::{}", path.join("::"));
            proptest::prop_assert!(!spells(&mangled, &other), "{} as {}", mangled, other);
        }
    }
}
