//! Rust's standard library, as the runtime that reports a program's
//! panics.
//!
//! std calls `__rustc::rust_panic(&mut dyn PanicPayload)` (an unmangled
//! `rust_panic` before Rust 1.87) once the panic hook has run and before
//! unwinding begins, so a panic is reported there before anything catches
//! it. std ships no debug information for its
//! private types, so the payload's type is named by the symbol of its
//! `Display::fmt` in the payload's vtable, and its layout is a convention
//! of the pinned toolchain, checked by the tests against the program's own
//! panic hook:
//!
//! - `panic_handler::FormatStringPayload` holds `string: Option<String>`
//!   first, which every hook fills with the formatted message before
//!   `rust_panic` runs;
//! - `panic_handler::StaticStrPayload` is a `&'static str`;
//! - `begin_panic::Payload<A>` holds `Option<A>`, which the program's own
//!   debug information describes, as it instantiated it;
//! - `resume_unwind::RewrapBox` holds the `Box<dyn Any + Send>` a caught
//!   panic left, whose type the `Any` vtable's `type_id` names.
//!
//! A `String` is its capacity, pointer, and length, and `None` of an
//! `Option<String>` is the capacity no allocation can have.

use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::sync::Arc;

use super::{
    Crossing, Partial, RuntimeException, RuntimeHook, RuntimeImage, RuntimeModel, RuntimeSignals,
    RuntimeStop, StartedTask, StoredValue, TaskContext, TaskPage, TaskRef, TaskStarter,
    ThreadActivity,
};

use crate::unwind::{RDI, RSI, RegisterFile};
use crate::{
    ExceptionFilter, ImageAddress, LanguageExceptionKind, StackSegment, ThreadId, VirtualAddress,
};

/// What Rust reports: every panic, as it begins.
pub const EXCEPTION_FILTERS: [ExceptionFilter; 1] = [PANIC];

const PANIC: ExceptionFilter = ExceptionFilter {
    id: "rust-panic",
    language: "Rust",
    kind: LanguageExceptionKind::Raised,
    label: "Rust panics",
    description: "Stop wherever a panic begins, before anything catches it, with the frame that \
                  panicked selected",
    default: true,
};

/// Where a trait object's vtable keeps its size, and its first method:
/// `Display::fmt` for a payload, `type_id` for an `Any`.
const VTABLE_SIZE: u64 = 8;
const VTABLE_FIRST_METHOD: u64 = 24;

/// `None` of an `Option<String>`: a capacity above `isize::MAX`.
const NO_STRING: u64 = 1 << 63;

/// The longest message read; a longer one is refused rather than cut.
const MAX_MESSAGE: u64 = 1 << 16;

/// The standard library's runtime, when the module carries it.
pub fn detect(image: &Arc<dyn RuntimeImage + Send + Sync>) -> Option<Arc<dyn RuntimeModel>> {
    // Only Rust's code is searched for its panic function: finding it
    // demangles every symbol of the image.
    let rust = image
        .producers()
        .iter()
        .any(|producer| producer.contains("(rustc version "));
    if !rust {
        return None;
    }

    // The function's name changed in Rust 1.87; a program has the one its
    // compiler gave it, and with only the newer, no panic in a program an
    // older Rust built would stop.
    let panic = crate::rust_names::RUST_PANIC
        .iter()
        .find_map(|name| image.function_answering(name))?;
    Some(Arc::new(RustRuntime {
        image: Arc::clone(image),
        hooks: [RuntimeHook {
            filter: &PANIC,
            address: panic.address,
        }],
    }))
}

#[derive(Debug)]
struct RustRuntime {
    image: Arc<dyn RuntimeImage + Send + Sync>,
    hooks: [RuntimeHook; 1],
}

/// A panic's payload, as its type says to read it.
struct Payload<'a> {
    stop: &'a dyn RuntimeStop,
    image: &'a (dyn RuntimeImage + Send + Sync),
}

impl Payload<'_> {
    fn word(&self, address: u64) -> Result<u64, Arc<str>> {
        let mut bytes = [0; 8];
        self.stop
            .read(VirtualAddress::new(address), &mut bytes)
            .then(|| u64::from_le_bytes(bytes))
            .ok_or_else(|| format!("the panic's memory at {address:#x} is unreadable").into())
    }

    /// The type that the method a vtable holds at `slot` implements a
    /// trait for, as `<T as Trait>::method` names it.
    ///
    /// A legacy-mangled symbol, the default for a program's own code
    /// before Rust 1.97, names an impl's generic parameters rather than
    /// their arguments: `<T as core::any::Any>::type_id`, or `Payload<A>`.
    /// The method's debug information says what each parameter is, which
    /// replaces it; a v0 symbol names the arguments, and nothing in it is
    /// a parameter's bare name.
    fn implementer(&self, vtable: u64, slot: u64) -> Result<String, Arc<str>> {
        let method = self.word(vtable + slot)?;
        let address = self
            .stop
            .load_bias()
            .unload(VirtualAddress::new(method))
            .ok_or("the panic's vtable points outside the program")?;
        let symbol = self.image.symbol_at(address).ok_or_else(|| {
            Arc::<str>::from(format!("no symbol names the method at {method:#x}"))
        })?;
        let implementer = implementer(&symbol).ok_or_else(|| format!("the panic's `{symbol}`"))?;

        // Rust 1.85's debug information leaves some instances' parameters
        // out, as `type_id`'s for `String`, but still names the function
        // with its arguments, `type_id<alloc::string::String>`.
        let mut generics = self.image.function_generics(address);
        if generics.is_empty() {
            generics = named_generics(implementer, self.image.function_name(address).as_deref());
        }

        Ok(substituted(implementer, &generics))
    }

    fn text(&self, pointer: u64, length: u64) -> Result<String, Arc<str>> {
        if length > MAX_MESSAGE {
            return Err(format!("the message is {length} bytes long").into());
        }

        let mut bytes = vec![0; usize::try_from(length).map_err(|_| "a long message")?];
        if !self.stop.read(VirtualAddress::new(pointer), &mut bytes) {
            return Err(format!("the message at {pointer:#x} is unreadable").into());
        }

        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The `&str` at `at`.
    fn str(&self, at: u64) -> Result<String, Arc<str>> {
        self.text(self.word(at)?, self.word(at + 8)?)
    }

    /// The `Option<String>` at `at`.
    fn string(&self, at: u64) -> Result<Option<String>, Arc<str>> {
        let capacity = self.word(at)?;
        if capacity == NO_STRING {
            return Ok(None);
        }

        self.text(self.word(at + 8)?, self.word(at + 16)?).map(Some)
    }

    /// The message a value of type `ty` at `at` is, when it is text.
    fn message(&self, ty: &str, at: u64) -> Result<Option<String>, Arc<str>> {
        match ty {
            "&str" => self.str(at).map(Some),
            "alloc::string::String" => self.string(at),
            _ => Ok(None),
        }
    }

    /// What `rust_panic` was given, at `data` with vtable `vtable`.
    fn report(&self, data: u64, vtable: u64) -> Result<RuntimeException, Arc<str>> {
        let ty = self.implementer(vtable, VTABLE_FIRST_METHOD)?;
        let ty = ty.as_str();
        let message = |text: String| RuntimeException {
            message: format!("panicked: {text}").into(),
            value: None,
        };

        let path = ty.split('<').next().unwrap_or(ty);
        match path.rsplit("::").next() {
            Some("FormatStringPayload") => self
                .string(data)?
                .map(message)
                .ok_or_else(|| "no panic hook formatted the message".into()),
            Some("StaticStrPayload") => self.str(data).map(message),
            Some("Payload") => {
                let held = argument(ty).ok_or_else(|| format!("the payload is a `{ty}`"))?;

                // `Option<&str>` and `Option<String>` keep their tag in a
                // niche; any other `A` keeps one before the value.
                Ok(self.message(held, data)?.map_or_else(
                    || RuntimeException {
                        message: format!("panicked with a value of type {held}").into(),
                        value: Some(format!("(*({}*){data:#x}).inner", quoted(ty)).into()),
                    },
                    message,
                ))
            }
            Some("RewrapBox") => {
                let (inner, any) = (self.word(data)?, self.word(data + 8)?);
                let held = self.implementer(any, VTABLE_FIRST_METHOD)?;
                let held = held.as_str();
                Ok(RuntimeException {
                    message: self
                        .message(held, inner)?
                        .map_or_else(
                            || format!("panic resumed with a value of type {held}"),
                            |text| format!("panic resumed: {text}"),
                        )
                        .into(),
                    value: None,
                })
            }
            _ => Err(format!("its payload is a `{ty}`, which microscope does not read").into()),
        }
    }
}

/// The type `<T as Trait>::method` implements a trait for.
fn implementer(method: &str) -> Option<&str> {
    let inner = method.strip_prefix('<')?;
    let mut depth = 0_usize;
    for (at, character) in inner.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.checked_sub(1)?,
            ' ' if depth == 0 && inner[at..].starts_with(" as ") => return Some(&inner[..at]),
            _ => {}
        }
    }

    None
}

/// `ty` with each of its words that is the bare name of one of `generics`'
/// parameters replaced by the parameter's argument. A word within a path,
/// after `::`, names no parameter, which is never qualified.
fn substituted(ty: &str, generics: &[(Arc<str>, String)]) -> String {
    let mut out = String::with_capacity(ty.len());
    let mut rest = ty;
    while let Some(start) = rest.find(|c: char| c.is_alphanumeric() || c == '_') {
        out.push_str(&rest[..start]);
        let word = &rest[start..];
        let end = word
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(word.len());
        let (word, after) = word.split_at(end);
        let qualified = out.ends_with("::") || after.starts_with("::");
        match generics.iter().find(|(name, _)| **name == *word) {
            Some((_, argument)) if !qualified => out.push_str(argument),
            _ => out.push_str(word),
        }

        rest = after;
    }

    out.push_str(rest);
    out
}

/// The one generic parameter that `ty`, a legacy symbol's implementer,
/// names bare, with the one argument the function's debug-information
/// `name` ends in; none unless there is exactly one of each, as which
/// argument is which parameter's is otherwise unknown. A bare word
/// beginning with a capital is a parameter, as a legacy symbol spells
/// every other type by its path and Rust's primitive types in lowercase.
fn named_generics(ty: &str, name: Option<&str>) -> Vec<(Arc<str>, String)> {
    let mut parameters = BTreeSet::new();
    let mut rest = ty;
    while let Some(start) = rest.find(|c: char| c.is_alphanumeric() || c == '_') {
        let before = &rest[..start];
        let word = &rest[start..];
        let end = word
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(word.len());
        let (word, after) = word.split_at(end);
        let qualified = before.ends_with("::") || after.starts_with("::");
        if !qualified && word.starts_with(|c: char| c.is_ascii_uppercase()) {
            parameters.insert(word);
        }
        rest = after;
    }

    let arguments = name.and_then(argument).filter(|arguments| {
        let mut depth = 0_usize;
        arguments.chars().all(|c| {
            match c {
                '<' | '(' | '[' => depth += 1,
                '>' | ')' | ']' => depth = depth.saturating_sub(1),
                _ => {}
            }
            !(c == ',' && depth == 0)
        })
    });

    match (parameters.len(), parameters.first(), arguments) {
        (1, Some(parameter), Some(argument)) => {
            vec![(Arc::from(*parameter), argument.to_owned())]
        }
        _ => Vec::new(),
    }
}

/// A type's name as an expression writes it, its generic last segment in
/// backticks: ``std::panicking::begin_panic::`Payload<i32>` ``.
fn quoted(ty: &str) -> String {
    let generic = ty.find('<').unwrap_or(ty.len());
    let last = ty[..generic].rfind("::").map_or(0, |at| at + 2);
    format!("{}`{}`", &ty[..last], &ty[last..])
}

/// The one type argument of a generic type, such as `i32` of
/// `Payload<i32>`.
fn argument(ty: &str) -> Option<&str> {
    let (_, rest) = ty.split_once('<')?;
    rest.strip_suffix('>')
}

impl RuntimeModel for RustRuntime {
    fn tasks(
        &self,
        _stop: &dyn RuntimeStop,
        _start: u64,
        _limit: usize,
        _program_only: bool,
    ) -> Partial<TaskPage> {
        Partial {
            value: TaskPage {
                tasks: Vec::new(),
                next: None,
            },
            gaps: Vec::new(),
        }
    }

    fn thread_activity(&self, _stop: &dyn RuntimeStop, _thread: ThreadId) -> ThreadActivity {
        ThreadActivity::Outside
    }

    fn task_context(
        &self,
        _stop: &dyn RuntimeStop,
        _task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>> {
        Ok(None)
    }

    fn thread_stacks(
        &self,
        _stop: &dyn RuntimeStop,
        _thread: ThreadId,
    ) -> Result<Vec<(std::ops::Range<u64>, StackSegment)>, Arc<str>> {
        Ok(Vec::new())
    }

    fn cross(
        &self,
        _stop: &dyn RuntimeStop,
        _thread: ThreadId,
        _frame: &RegisterFile,
        _after_call: bool,
    ) -> Result<Crossing, Arc<str>> {
        Ok(Crossing::Stay)
    }

    fn signals(&self) -> RuntimeSignals {
        RuntimeSignals::default()
    }

    fn hooks(&self) -> &[RuntimeHook] {
        &self.hooks
    }

    fn exception(
        &self,
        stop: &dyn RuntimeStop,
        _hook: ImageAddress,
        registers: &RegisterFile,
    ) -> Result<RuntimeException, Arc<str>> {
        let argument = |register| {
            registers
                .get(register)
                .ok_or_else(|| Arc::<str>::from("the panic's payload is unavailable"))
        };

        let payload = Payload {
            stop,
            image: self.image.as_ref(),
        };

        let (data, vtable) = (argument(RDI)?, argument(RSI)?);

        // A payload's size bounds every read of it.
        let size = payload.word(vtable + VTABLE_SIZE)?;
        if size > 1 << 12 {
            return Err(format!("the payload claims to be {size} bytes").into());
        }

        payload.report(data, vtable)
    }

    fn dynamic_value(
        &self,
        _stop: &dyn RuntimeStop,
        _representation: &str,
        _value: StoredValue<'_>,
    ) -> Option<Result<super::DynamicValue, Arc<str>>> {
        None
    }

    fn stack_mover(&self) -> Option<ImageAddress> {
        None
    }

    fn moving_task(
        &self,
        _stop: &dyn RuntimeStop,
        _registers: &RegisterFile,
    ) -> Result<u64, Arc<str>> {
        Err("Rust's standard library moves no task".into())
    }

    fn task_stack(
        &self,
        _stop: &dyn RuntimeStop,
        _task: TaskRef,
    ) -> Result<Option<std::ops::Range<u64>>, Arc<str>> {
        Ok(None)
    }

    fn call_out(
        &self,
        _entry: ImageAddress,
        _registers: &RegisterFile,
    ) -> Option<Result<VirtualAddress, Arc<str>>> {
        None
    }

    fn task_starters(&self) -> Vec<TaskStarter> {
        Vec::new()
    }

    fn started_task(
        &self,
        _stop: &dyn RuntimeStop,
        _starter: ImageAddress,
        _registers: &RegisterFile,
    ) -> Result<Option<StartedTask>, Arc<str>> {
        Err("Rust's standard library starts no task".into())
    }

    fn task_noun(&self) -> &'static str {
        "task"
    }

    fn own_source(&self, path: &Path) -> bool {
        standard_library(path)
    }
}

/// Whether a function is the standard library's, by its name, for code
/// with no lines to tell by: an item of `std`, `core`, or `alloc`, or a
/// method of one of their types, as `<std::fs::File>::open`, of a
/// standard trait, as `<&std::fs::File as std::io::Read>::read`. A
/// program's own trait on a standard type, as `<std::fs::File as
/// app::Settings>::load`, or a standard trait on its own type, as
/// `<app::Config as core::fmt::Debug>::fmt`, is the program's.
pub fn standard_library_function(name: &str) -> bool {
    let standard = |path: &str| {
        let path = path.trim_start_matches('&');
        let path = path.strip_prefix("mut ").unwrap_or(path);
        let path = path.strip_prefix("dyn ").unwrap_or(path);
        ["std::", "core::", "alloc::"]
            .iter()
            .any(|krate| path.starts_with(krate))
    };
    let Some(inner) = name.strip_prefix('<') else {
        return standard(name);
    };

    // `<Type as Trait>::method`: the `as` and the `>` outside any generic
    // arguments of the type's own.
    let mut depth = 0_usize;
    let mut trait_at = None;
    let mut end = inner.len();
    for (at, byte) in inner.bytes().enumerate() {
        match byte {
            b'<' => depth += 1,
            b'>' if depth == 0 => {
                end = at;
                break;
            }
            b'>' => depth -= 1,
            b' ' if depth == 0 && trait_at.is_none() && inner[at..].starts_with(" as ") => {
                trait_at = Some(at);
            }
            _ => {}
        }
    }

    trait_at.map_or_else(
        || standard(&inner[..end]),
        |at| standard(&inner[..at]) && standard(&inner[at + 4..end]),
    )
}

/// Whether a source file is the standard library's, wherever the toolchain
/// put it: `/rustc/<commit>/library/<crate>/…` for its own code, and
/// `…/rustlib/src/rust/library/<crate>/…` for generic code the program
/// instantiated.
pub fn standard_library(path: &Path) -> bool {
    let parts = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    parts.iter().enumerate().any(|(at, part)| {
        let toolchain = match at.checked_sub(3).map(|start| &parts[start..at]) {
            Some([_, "rustc", commit]) => commit.len() == 40,
            Some(["rustlib", "src", "rust"]) => true,
            _ => at == 2 && parts[0] == "rustc" && parts[1].len() == 40,
        };
        *part == "library"
            && toolchain
            && parts.get(at + 1).is_some_and(|krate| {
                matches!(
                    *krate,
                    "core" | "alloc" | "std" | "panic_unwind" | "panic_abort"
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_name_the_types_they_implement_for() {
        assert_eq!(
            implementer(
                "<std::panicking::begin_panic::Payload<&str> as core::panic::PanicPayload>::get"
            ),
            Some("std::panicking::begin_panic::Payload<&str>")
        );
        assert_eq!(
            implementer("<alloc::string::String as core::any::Any>::type_id"),
            Some("alloc::string::String")
        );
        assert_eq!(implementer("core::fmt::write"), None);
        assert_eq!(
            argument("std::panicking::begin_panic::Payload<i32>"),
            Some("i32")
        );
        assert_eq!(
            quoted("std::panicking::begin_panic::Payload<alloc::string::String>"),
            "std::panicking::begin_panic::`Payload<alloc::string::String>`"
        );
    }

    /// A legacy symbol's parameters become the arguments the function's
    /// debug information gives, wherever they stand bare, and nothing
    /// within a path does, nor a word that only begins like a parameter.
    /// Rust 1.85 leaves some generic instances' parameters out of their
    /// debug information but names the function with its argument: the
    /// one bare parameter a legacy implementer names takes the one
    /// argument the name ends in, and nothing is guessed when either is
    /// not exactly one.
    #[test]
    fn a_lone_parameter_takes_the_argument_its_function_is_named_with() {
        let pairs = |found: Vec<(Arc<str>, String)>| {
            found
                .into_iter()
                .map(|(name, argument)| (name.to_string(), argument))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            pairs(named_generics("T", Some("type_id<alloc::string::String>"))),
            [("T".to_owned(), "alloc::string::String".to_owned())]
        );
        assert_eq!(
            pairs(named_generics(
                "std::panicking::begin_panic::Payload<A>",
                Some("fmt<alloc::vec::Vec<(u8, u16)>>")
            )),
            [("A".to_owned(), "alloc::vec::Vec<(u8, u16)>".to_owned())]
        );

        // Two arguments, two parameters, none at all, or a concrete path.
        assert!(named_generics("T", Some("f<u8, u16>")).is_empty());
        assert!(named_generics("Pair<A, B>", Some("f<u8>")).is_empty());
        assert!(named_generics("T", Some("type_id")).is_empty());
        assert!(named_generics("T", None).is_empty());
        assert!(named_generics("alloc::string::String", Some("f<u8>")).is_empty());
    }

    #[test]
    fn a_legacy_symbols_parameters_are_their_arguments() {
        let generics = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, argument)| (Arc::from(*name), (*argument).to_owned()))
                .collect::<Vec<_>>()
        };

        let string = generics(&[("T", "alloc::string::String")]);
        assert_eq!(substituted("T", &string), "alloc::string::String");
        assert_eq!(
            substituted(
                "std::panicking::begin_panic::Payload<A>",
                &generics(&[("A", "i32")])
            ),
            "std::panicking::begin_panic::Payload<i32>"
        );

        // A v0 symbol's concrete names are left alone, even where a part
        // of a path or a longer word spells a parameter.
        let concrete = "alloc::boxed::Box<T::A, Tail>";
        assert_eq!(substituted(concrete, &string), concrete);
        assert_eq!(substituted("a::T", &string), "a::T");
        assert_eq!(
            substituted("&[T; 2]", &string),
            "&[alloc::string::String; 2]"
        );
        assert_eq!(substituted("T", &[]), "T");
    }

    #[test]
    fn the_standard_library_is_its_items_and_its_types_methods_by_name() {
        for name in [
            "std::rt::lang_start_internal",
            "<std::fs::File>::open::<&str>",
            "<&std::fs::File as std::io::Read>::read",
            "<&dyn core::ops::function::Fn<()> as core::ops::function::FnOnce<()>>::call_once",
            "alloc::raw_vec::finish_grow",
            "<alloc::vec::Vec<app::Row> as core::fmt::Debug>::fmt",
        ] {
            assert!(super::standard_library_function(name), "{name}");
        }
        // A program's own impls of standard traits, and its generics over
        // standard types, are its own.
        for name in [
            "<app::Config as core::fmt::Debug>::fmt",
            "<std::fs::File as app::Settings>::load",
            "<std::collections::HashMap<u8, Vec<u8>> as app::Store<u8>>::save",
            "app::load::<std::fs::File>",
            "<fn() as core::ops::function::FnOnce<()>>::call_once",
            "main",
        ] {
            assert!(!super::standard_library_function(name), "{name}");
        }
    }

    #[test]
    fn the_standard_library_is_its_sources_under_the_toolchain() {
        let runtime = |path: &str| standard_library(Path::new(path));
        assert!(runtime(
            "/rustc/375b1431b7d89d1c2e2bc168c011848ae12b7d14/library/core/src/option.rs"
        ));
        assert!(runtime(
            "/nix/store/x-rust-default/lib/rustlib/src/rust/library/std/src/panicking.rs"
        ));
        assert!(!runtime("/home/me/project/library/core/src/main.rs"));
        assert!(!runtime("tests/fixtures/rust/tokio/panics/src/main.rs"));
    }
}
