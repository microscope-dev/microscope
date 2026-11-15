use std::ops::Range;

use crate::{ImageAddress, LoadedModule, ModuleImage, SymbolKind};

pub const PAGE: u64 = 4096;

pub const PROBE: usize = 16;

pub const MAX_COPY: u64 = 256 * 1024 * 1024;

pub struct Source<'a, K> {
    pub key: K,
    pub image: &'a ModuleImage,
    pub loaded: LoadedModule,

    pub stored: Vec<(ImageAddress, ImageAddress)>,
}

#[derive(Debug)]
pub struct Proven<K, T> {
    pub source: K,
    pub image_start: ImageAddress,
    pub length: u64,
    pub found: T,
}

pub fn prove<K: Copy, T>(
    mapping: &Range<u64>,
    sources: &[Source<'_, K>],
    probe: &mut dyn FnMut(u64) -> Option<[u8; PROBE]>,
    compare: &mut dyn FnMut(u64, u64, u64) -> (u64, T),
) -> Option<Proven<K, T>> {
    let first = probe(mapping.start)?;
    if first.iter().all(|byte| *byte == 0) {
        return None;
    }

    sources
        .iter()
        .filter(|source| source.loaded.window.is_none())
        .find_map(|source| {
            source
                .stored
                .iter()
                .copied()
                .chain(candidates(source.image))
                .find_map(|(start, end)| {
                    let from = source.loaded.virtual_address(start).ok()?.get();
                    let limit = (end.get().checked_sub(start.get())?)
                        .min(mapping.end - mapping.start)
                        .min(MAX_COPY);
                    let probed = limit.min(PROBE as u64);
                    if compare(from, mapping.start, probed).0 < probed {
                        return None;
                    }
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
            let repeated = *previous == Some(candidate.0);
            *previous = Some(candidate.0);
            Some((!repeated).then_some(candidate))
        })
        .flatten()
}

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

    #[test]
    fn a_comparison_counts_the_equal_bytes_up_to_the_first_difference() {
        let memory: Vec<u8> = (0..64).map(|byte| byte % 16).collect();
        let mut read = |address: u64, length: usize| {
            let start = usize::try_from(address).ok()?;
            memory
                .get(start..)
                .map(|rest| rest[..length.min(rest.len())].to_vec())
        };
        assert_eq!(compare_bytes(&mut read, 0, 16, 48, 7), 48);
        assert_eq!(compare_bytes(&mut read, 0, 16, 60, 7), 48);
        assert_eq!(compare_bytes(&mut read, 0, 17, 10, 4), 0);
        assert_eq!(compare_bytes(&mut read, 1, 17, 10, 4), 10);
    }
}
