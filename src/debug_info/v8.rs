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
