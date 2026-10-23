//! V8, the JavaScript engine Node.js embeds: what its images say about
//! themselves.
//!
//! V8 compiles its builtins ahead of time into an embedded blob, which a
//! build that keeps its local symbols names: `v8_Default_embedded_blob_code_`
//! where the blob begins, and a `Builtins_` function symbol for each builtin.
//! No call-frame information describes them. Their frames are V8's own,
//! which keep the frame-pointer chain at each call they make: rbp holds the
//! address of the word that saved the caller's, with the return address in
//! the word above it.

use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};

/// A constant every V8 build exports, in its dynamic table too, for
/// debuggers: where a frame keeps its function.
const FRAME_FUNCTION_OFFSET: &str = "v8dbg_off_fp_function";

/// Whether `image` is V8's, which generates code whose frames keep the
/// frame-pointer chain at its calls.
pub fn is_v8(image: &crate::ModuleImage) -> bool {
    image.symbol_named(FRAME_FUNCTION_OFFSET).is_ok()
}

/// The symbol at which the embedded builtins begin.
const EMBEDDED_BLOB: &str = "v8_Default_embedded_blob_code_";

/// What the symbol of each builtin's code begins with.
const BUILTIN: &str = "Builtins_";

/// The variable each thread's current isolate is kept in.
pub const CURRENT_ISOLATE: &str = "_ZN2v88internal18g_current_isolate_E";

/// The exported function that reads [`CURRENT_ISOLATE`].
const GET_CURRENT: &str = "_ZN2v87Isolate10GetCurrentEv";

/// Adds to `thread_locals` where each thread's current isolate is, when
/// `object` is V8's and no symbol names its variable, as in a shared
/// library stripped of its local symbols: the exported accessor's code
/// says. A variable a symbol names is left as the symbol places it.
pub(super) fn add_thread_locals(
    object: &object::File<'_>,
    thread_locals: &mut std::collections::BTreeMap<
        std::sync::Arc<str>,
        Result<crate::ThreadLocal, std::sync::Arc<str>>,
    >,
) {
    if thread_locals.contains_key(CURRENT_ISOLATE) {
        return;
    }
    let Some(accessor) = object
        .symbols()
        .chain(object.dynamic_symbols())
        .find(|symbol| symbol.name() == Ok(GET_CURRENT) && symbol.kind() == SymbolKind::Text)
    else {
        return;
    };
    thread_locals.insert(
        CURRENT_ISOLATE.into(),
        super::elf::accessor_thread_local(object, accessor.address()),
    );
}

/// The address and code of each embedded builtin `object` names, in
/// address order. An image that is not V8's, or that names no builtin,
/// has none.
pub(super) fn builtins<'data>(object: &object::File<'data>) -> Vec<(u64, &'data [u8])> {
    let Some(blob) = object
        .symbols()
        .find(|symbol| symbol.name() == Ok(EMBEDDED_BLOB))
        .map(|symbol| symbol.address())
    else {
        return Vec::new();
    };

    let mut builtins: Vec<_> = object
        .symbols()
        .filter(|symbol| {
            symbol.kind() == SymbolKind::Text
                && symbol.size() > 0
                && symbol.address() >= blob
                && symbol.name().is_ok_and(|name| name.starts_with(BUILTIN))
        })
        .filter_map(|symbol| {
            let section = object.section_by_index(symbol.section_index()?).ok()?;
            if section.kind() != SectionKind::Text {
                return None;
            }
            let data = section.data().ok()?;
            let start = usize::try_from(symbol.address().checked_sub(section.address())?).ok()?;
            let end = start.checked_add(usize::try_from(symbol.size()).ok()?)?;
            Some((symbol.address(), data.get(start..end)?))
        })
        .collect();
    builtins.sort_unstable_by_key(|(address, _)| *address);
    builtins.dedup_by_key(|(address, _)| *address);
    builtins
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every build in the dev shell's matrix places each thread's current
    /// isolate: by its symbol, or, in Debian's libnode, which names none,
    /// by the code of the accessor it exports.
    #[test]
    fn every_build_places_the_current_isolate() {
        let root = std::env::var("MICROSCOPE_NODE").expect("MICROSCOPE_NODE; run in the dev shell");
        for entry in [
            "26",
            "24",
            "22",
            "nixpkgs-26",
            "nixpkgs-24",
            "nixpkgs-22",
            "debian-24",
        ] {
            let path = if entry == "debian-24" {
                format!("{root}/debian-24/root/usr/lib/x86_64-linux-gnu/libnode.so.137")
            } else {
                format!("{root}/{entry}/node")
            };
            let data = std::fs::read(&path).expect("the build");
            let object = object::File::parse(data.as_slice()).expect("an ELF file");
            let mut thread_locals = super::super::elf::load_thread_locals(&object);
            add_thread_locals(&object, &mut thread_locals);
            let place = thread_locals.get(CURRENT_ISOLATE);
            match entry {
                // F28: the offset its debug package's symbol gives.
                "debian-24" => assert_eq!(place, Some(&Ok(crate::ThreadLocal::Dynamic(0x170)))),
                _ => assert!(
                    matches!(
                        place,
                        Some(Ok(
                            crate::ThreadLocal::Offset(_) | crate::ThreadLocal::Slot(_)
                        ))
                    ),
                    "{entry}: {place:?}"
                ),
            }
        }
    }
}
