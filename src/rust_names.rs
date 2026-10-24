//! The names rustc and Rust's standard library give the code and data
//! microscope looks for, with every spelling a supported release uses.
//!
//! None of these names is a stable interface: each changes when rustc or
//! std moves an item, renames a private static, or changes how it lowers
//! code, and a program carries whichever spelling the compiler that built
//! it used. A debugger built against one compiler meets programs built by
//! many. So each name here lists every spelling that releases from Rust
//! 1.85 (the first with edition 2024) through the newest nightly checked
//! use, with the release that changed it, and callers try them all. A name
//! that only one release uses fails on every other one, usually without a
//! word: the code that matches it finds nothing and goes on as if the
//! program had nothing to find.
//!
//! When a new release renames one, add its spelling here rather than
//! replacing the old one; programs built by the old releases still run.
//! `dev/runtime-models.md` describes how the releases were checked.

/// The function rustc makes to drop a value of type `T`, as the function's
/// debug information names it, each followed by `<T>`:
/// `core::ptr::drop_in_place::<T>` through Rust 1.96, and
/// `core::ptr::drop_glue::<T>` from 1.97.
pub const DROP_GLUE: [&str; 2] = ["drop_glue", "drop_in_place"];

/// The function std calls with a panic's payload once the panic hook has
/// run, before unwinding begins, as its symbol demangles:
/// `__rustc::rust_panic` from Rust 1.87, and an unmangled `rust_panic`
/// before.
pub const RUST_PANIC: [&str; 2] = ["__rustc::rust_panic", "rust_panic"];

/// The static that std's `thread_local!` keeps each variable's storage in,
/// within the variable's own namespace: `__RUST_STD_INTERNAL_VAL` from Rust
/// 1.92, and `VAL` before.
pub const THREAD_LOCAL_STORAGE: [&str; 2] = ["__RUST_STD_INTERNAL_VAL", "VAL"];

/// The paths of std's atomically counted pointer, as debug information
/// names its type: `alloc::sync::Arc` through Rust 1.97, and
/// `alloc::rcs::arc::Arc` in nightlies after, which moved it. `alloc::sync`
/// still exports it there, but debug information names a type where it is
/// defined.
pub const ARC: [&str; 2] = ["alloc::sync::Arc", "alloc::rcs::arc::Arc"];

/// The name of each function rustc makes to drop a `ty`, the type's full
/// name, in the order of [`DROP_GLUE`].
pub fn drop_glue_of(ty: &str) -> [String; 2] {
    DROP_GLUE.map(|glue| format!("{glue}<{ty}>"))
}

/// Whether a function, by the name its debug information gives it, is one
/// rustc makes to drop a value of some type.
pub fn is_drop_glue(name: &str) -> bool {
    DROP_GLUE.iter().any(|glue| {
        name.strip_prefix(glue)
            .is_some_and(|rest| rest.starts_with('<'))
    })
}

/// Every spelling of `name` with each path of [`ARC`] for `alloc::sync::Arc`,
/// as a name written with `alloc::sync::Arc` would be spelled by each
/// release; just `name` when it names no `Arc`.
pub fn with_each_arc(name: &str) -> Vec<String> {
    let [written, ..] = ARC;
    if !name.contains(written) {
        return vec![name.to_owned()];
    }

    ARC.iter().map(|path| name.replace(written, path)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both releases' glue is glue, and nothing else that begins the same
    /// way is: a function whose name only starts with a glue's, or the
    /// glue's own namespace.
    #[test]
    fn drop_glue_is_known_by_either_name() {
        assert!(is_drop_glue("drop_glue<cancel::waiting::{async_fn_env#0}>"));
        assert!(is_drop_glue(
            "drop_in_place<cancel::waiting::{async_fn_env#0}>"
        ));
        assert!(!is_drop_glue("drop_glue_slow<u8>"));
        assert!(!is_drop_glue("drop_in_place"));
        assert!(!is_drop_glue("drop<u8>"));
        assert_eq!(
            drop_glue_of("a::b"),
            [
                "drop_glue<a::b>".to_owned(),
                "drop_in_place<a::b>".to_owned()
            ]
        );
    }

    /// A name that spells `Arc` has one spelling for each of its paths,
    /// each `Arc` within replaced; another name has its own alone.
    #[test]
    fn a_name_with_an_arc_is_spelled_with_each_of_its_paths() {
        assert_eq!(
            with_each_arc("f<alloc::sync::Arc<a::Handle, alloc::alloc::Global>>"),
            [
                "f<alloc::sync::Arc<a::Handle, alloc::alloc::Global>>",
                "f<alloc::rcs::arc::Arc<a::Handle, alloc::alloc::Global>>",
            ]
        );
        assert_eq!(
            with_each_arc("f<alloc::sync::Arc<alloc::sync::Arc<u8>>>")[1],
            "f<alloc::rcs::arc::Arc<alloc::rcs::arc::Arc<u8>>>"
        );
        assert_eq!(
            with_each_arc("f<alloc::rc::Rc<u8>>"),
            ["f<alloc::rc::Rc<u8>>"]
        );
    }
}
