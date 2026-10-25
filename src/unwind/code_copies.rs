//! The proof that a process's anonymous code is a copy of a module's code,
//! read through whatever reads the process: the debugger's stopped threads,
//! or the memory of a process the profiler or a syscall trace watches run.
//!
//! A runtime may copy some of its own machine code into memory no file
//! backs and run it there: V8 copies its embedded builtins next to each of
//! its code ranges. A copy that is byte for byte a range of a loaded
//! module's code is that code at another address, so its symbols, roles,
//! and unwind rules hold unchanged. The proof is a full comparison of the
//! bytes, from where copies begin: a function that begins a page of the
//! module, as code copied whole begins, or code a runtime says the process
//! keeps in the module, as V8's builtins where no symbol names them.

use std::ops::Range;

use crate::{ImageAddress, LoadedModule, ModuleImage, SymbolKind};

/// The size of the pages copies begin on.
pub const PAGE: u64 = 4096;

/// How many of a copy's first bytes pick the function it may copy.
pub const PROBE: usize = 16;

/// The most of a copy compared, past which its window ends.
pub const MAX_COPY: u64 = 256 * 1024 * 1024;

/// A module whose code a mapping may copy.
pub struct Source<'a, K> {
    /// What the caller knows the module by.
    pub key: K,
    pub image: &'a ModuleImage,
    pub loaded: LoadedModule,

    /// The ranges of the image a runtime says the process keeps code in,
    /// where a copy may begin too.
    pub stored: Vec<(ImageAddress, ImageAddress)>,
}

/// A range of a module's code a mapping's bytes begin with, and what the
/// comparison that proved it found on the way.
#[derive(Debug)]
pub struct Proven<K, T> {
    pub source: K,
    pub image_start: ImageAddress,
    pub length: u64,
    pub found: T,
}

/// The range of a source's code the anonymous `mapping` begins with a copy
/// of, if any. `probe` reads the mapping's first [`PROBE`] bytes, and
/// `compare(source, copy, limit)` counts how many of the `limit` bytes at
/// the process's address `copy` are those at `source`, with whatever it
/// found on the way, as traps a debugger installed at the source.
pub fn prove<K: Copy, T>(
    mapping: &Range<u64>,
    sources: &[Source<'_, K>],
    probe: &mut dyn FnMut(u64) -> Option<[u8; PROBE]>,
    compare: &mut dyn FnMut(u64, u64, u64) -> (u64, T),
) -> Option<Proven<K, T>> {
    let first = probe(mapping.start)?;
    // Unused memory proves nothing.
    if first.iter().all(|byte| *byte == 0) {
        return None;
    }

    sources
        .iter()
        .filter(|source| source.loaded.window.is_none())
        .find_map(|source| {
            candidates(source.image)
                .chain(source.stored.iter().copied())
                .find_map(|(start, end)| {
                    let from = source.loaded.virtual_address(start).ok()?.get();
                    let limit = (end.get().checked_sub(start.get())?)
                        .min(mapping.end - mapping.start)
                        .min(MAX_COPY);
                    let probed = limit.min(PROBE as u64);
                    if compare(from, mapping.start, probed).0 < probed {
                        return None;
                    }
                    // Code that ends before the probe does is proven whole.
                    let (length, found) = compare(from, mapping.start, limit);
                    (length >= probed).then_some(Proven {
                        source: source.key,
                        image_start: start,
                        length,
                        found,
                    })
                })
        })
}

/// The functions of `image`'s code that begin a page, with where the code
/// they lie in ends, where a copy of its code may begin.
pub fn candidates(image: &ModuleImage) -> impl Iterator<Item = (ImageAddress, ImageAddress)> + '_ {
    let sections = image.sections();
    image
        .symbols()
        .filter(|symbol| {
            symbol.kind() == SymbolKind::Function && symbol.address().get() % PAGE == 0
        })
        .filter_map(move |symbol| {
            let address = symbol.address();
            sections
                .iter()
                .find(|section| section.executable && section.range.contains(address))
                .map(|section| (address, section.range.end))
        })
        .scan(None, |previous, candidate| {
            // Aliases at one address are one candidate.
            let repeated = *previous == Some(candidate.0);
            *previous = Some(candidate.0);
            Some((!repeated).then_some(candidate))
        })
        .flatten()
}

/// How many of the `limit` bytes at `copy` equal those at `source`, both
/// read by `read(address, length)`, which gives what it could read, in
/// chunks of `chunk` bytes.
pub fn compare_bytes(
    read: &mut dyn FnMut(u64, usize) -> Option<Vec<u8>>,
    source: u64,
    copy: u64,
    limit: u64,
    chunk: usize,
) -> u64 {
    let mut length = 0;
    while length < limit {
        let size = usize::try_from((limit - length).min(chunk as u64)).unwrap_or(chunk);
        let (Some(original), Some(copied)) =
            (read(source + length, size), read(copy + length, size))
        else {
            return length;
        };
        if let Some(differs) = original.iter().zip(&copied).position(|(a, b)| a != b) {
            return length + differs as u64;
        }
        let same = original.len().min(copied.len());
        length += same as u64;
        if same < size {
            return length;
        }
    }
    length
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A comparison ends at the first byte that differs, and where either
    /// side cannot be read further.
    #[test]
    fn a_comparison_counts_the_equal_bytes_up_to_the_first_difference() {
        let memory: Vec<u8> = (0..64).map(|byte| byte % 16).collect();
        let mut read = |address: u64, length: usize| {
            let start = usize::try_from(address).ok()?;
            memory
                .get(start..)
                .map(|rest| rest[..length.min(rest.len())].to_vec())
        };
        // 16 bytes repeat at 16, 32, and 48.
        assert_eq!(compare_bytes(&mut read, 0, 16, 48, 7), 48);
        assert_eq!(compare_bytes(&mut read, 0, 16, 60, 7), 48);
        assert_eq!(compare_bytes(&mut read, 0, 17, 10, 4), 0);
        assert_eq!(compare_bytes(&mut read, 1, 17, 10, 4), 10);
    }
}
