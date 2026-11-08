//! A process's mapped modules, and the index that finds them by address
//! for every frame of every sample without looking at each module.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{Image, Mapped};
use crate::debug_info::UnwindInfo;
use crate::unwind::{Modules, UnwindModule};
use crate::{AddressRange, ImageAddress, LoadedModule, ModuleImage, VirtualAddress};

/// The pieces of modules a process maps, in the order it mapped them, and
/// the index built of them, which any change to the pieces drops.
#[derive(Debug, Default, Clone)]
pub(super) struct Mappings {
    pieces: Vec<Mapped>,
    index: Option<Arc<ModuleIndex>>,
}

impl std::ops::Deref for Mappings {
    type Target = [Mapped];

    fn deref(&self) -> &[Mapped] {
        &self.pieces
    }
}

impl Mappings {
    /// The pieces, to change: the index is built again when next asked for.
    pub(super) fn edit(&mut self) -> &mut Vec<Mapped> {
        self.index = None;
        &mut self.pieces
    }

    /// Drops the index, as an image whose debug information was read since
    /// joins the modules a sample unwinds through.
    pub(super) fn forget_index(&mut self) {
        self.index = None;
    }

    /// The piece that maps `address`, as a search of the pieces in order
    /// finds it: by the index while it is built.
    pub(super) fn piece_at(&self, address: u64) -> Option<&Mapped> {
        let Some((index, by_start)) = self
            .index
            .as_ref()
            .and_then(|index| Some((index, index.by_start.as_deref()?)))
        else {
            return self
                .pieces
                .iter()
                .find(|piece| (piece.start..piece.end).contains(&address));
        };
        let span = span_at(by_start, address, &index.last_piece)?;
        self.pieces.get(span.owner)
    }

    /// The index of the pieces whose images `images` has debug information
    /// for, built if the pieces changed since.
    pub(super) fn index(&mut self, images: &[Image]) -> Arc<ModuleIndex> {
        let index = self
            .index
            .get_or_insert_with(|| Arc::new(ModuleIndex::new(&self.pieces, images)));
        Arc::clone(index)
    }
}

/// The modules a sample of a process unwinds through: the pieces it maps
/// whose images have debug information, in the order it mapped them, each
/// by its index there, and found by address in logarithmic time.
pub(super) struct ModuleIndex {
    owners: Vec<Mapped>,
    infos: Vec<(Arc<ModuleImage>, Arc<dyn UnwindInfo>)>,

    /// Where each owner's image describes code, by virtual address, apart
    /// and in order; where two describe the same address, the one mapped
    /// first, as a search of the owners in order finds.
    spans: Vec<Span>,

    /// Each nonempty piece, in the order of their starts, with its number
    /// as the owner: none if two overlap, as no pieces ever have, since then
    /// only a search in order finds the first.
    by_start: Option<Vec<Span>>,

    /// The span each search found last, tried first by the next: a stack's
    /// frames are mostly in a few modules, where a search in order finds
    /// them at once too.
    last_span: AtomicUsize,
    last_piece: AtomicUsize,
}

impl std::fmt::Debug for ModuleIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleIndex")
            .field("owners", &self.owners)
            .field("spans", &self.spans)
            .finish_non_exhaustive()
    }
}

/// Addresses `start..=last` of the owner numbered `owner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: u64,
    last: u64,
    owner: usize,
}

impl ModuleIndex {
    fn new(pieces: &[Mapped], images: &[Image]) -> Self {
        let (owners, infos): (Vec<Mapped>, Vec<_>) = pieces
            .iter()
            .filter_map(|piece| {
                let info = images.get(piece.image)?.info.as_ref()?;
                Some((*piece, (Arc::clone(&info.image), Arc::clone(&info.unwind))))
            })
            .unzip();
        let spans = spans(owners.iter().map(|owner| {
            let range = images
                .get(owner.image)
                .and_then(|image| image.info.as_ref())
                .map(|info| info.image.address_range());
            (owner.loaded, range)
        }));

        let mut by_start: Vec<Span> = pieces
            .iter()
            .enumerate()
            .filter(|(_, piece)| piece.start < piece.end)
            .map(|(owner, piece)| Span {
                start: piece.start,
                last: piece.end - 1,
                owner,
            })
            .collect();
        by_start.sort_unstable_by_key(|span| span.start);
        let apart = by_start
            .windows(2)
            .all(|pair| matches!(pair, [a, b] if a.last < b.start));
        debug_assert!(apart, "pieces overlap: {pieces:?}");

        Self {
            owners,
            infos,
            spans,
            by_start: apart.then_some(by_start),
            last_span: AtomicUsize::new(0),
            last_piece: AtomicUsize::new(0),
        }
    }

    /// The modules, by index.
    pub(super) fn owners(&self) -> &[Mapped] {
        &self.owners
    }
}

impl Modules for ModuleIndex {
    #[expect(
        clippy::indexing_slicing,
        reason = "infos and owners are unzipped from one list, and callers pass an index module_at found"
    )]
    fn module(&self, index: usize) -> UnwindModule<'_> {
        debug_assert!(
            index < self.owners.len(),
            "module {index} of {}",
            self.owners.len()
        );
        let (image, unwind) = &self.infos[index];
        UnwindModule {
            loaded: self.owners[index].loaded,
            image,
            unwind: unwind.as_ref(),
        }
    }

    fn module_at(&self, address: VirtualAddress) -> Option<(usize, ImageAddress)> {
        let owner = span_at(&self.spans, address.get(), &self.last_span)?.owner;
        let image_address = self.owners.get(owner)?.loaded.load_bias.unload(address);
        debug_assert!(image_address.is_some(), "a span lies below its bias");
        Some((owner, image_address?))
    }
}

/// The span of `spans` that holds `address`, trying first the one at
/// `last`, which this then sets to the one found.
fn span_at<'a>(spans: &'a [Span], address: u64, last: &AtomicUsize) -> Option<&'a Span> {
    let holds = |span: &&Span| span.start <= address && address <= span.last;
    if let Some(span) = spans.get(last.load(Ordering::Relaxed)).filter(holds) {
        return Some(span);
    }

    let at = spans
        .partition_point(|span| span.start <= address)
        .checked_sub(1)?;
    let span = spans.get(at).filter(holds)?;
    last.store(at, Ordering::Relaxed);
    Some(span)
}

/// The spans of the modules `owners`, each its load and the image addresses
/// its image describes, in which an address is found as a search of them in
/// order finds it: the first whose image, unloaded by its bias and within
/// its window, describes it.
fn spans(
    owners: impl Iterator<Item = (LoadedModule, Option<AddressRange<ImageAddress>>)>,
) -> Vec<Span> {
    // Each owner's addresses, as a half-open range wide enough for the
    // last address there is.
    let mut ranges: Vec<(u128, u128, usize)> = owners
        .enumerate()
        .filter_map(|(owner, (loaded, range))| {
            let mut range = range?;
            if let Some(window) = loaded.window {
                range.start = range.start.max(window.start);
                range.end = range.end.min(window.end);
            }
            let bias = u128::from(loaded.load_bias.get());
            let start = bias + u128::from(range.start.get());
            let end = (bias + u128::from(range.end.get())).min(1 << 64);
            (start < end).then_some((start, end, owner))
        })
        .collect();
    ranges.sort_unstable();

    // A sweep over every range's ends: between two, the first owner of
    // those whose ranges hold the addresses there finds them.
    let mut ends: Vec<u128> = ranges
        .iter()
        .flat_map(|&(start, end, _)| [start, end])
        .collect();
    ends.sort_unstable();
    ends.dedup();

    let mut spans: Vec<Span> = Vec::new();
    let mut open = std::collections::BTreeSet::new();
    let mut closing = std::collections::BinaryHeap::new();
    let mut next = 0;
    for (&start, &end) in ends.iter().zip(ends.iter().skip(1)) {
        while let Some(&(range_start, range_end, owner)) = ranges.get(next)
            && range_start <= start
        {
            open.insert(owner);
            closing.push(std::cmp::Reverse((range_end, owner)));
            next += 1;
        }
        while let Some(&std::cmp::Reverse((range_end, owner))) = closing.peek()
            && range_end <= start
        {
            open.remove(&owner);
            closing.pop();
        }
        let Some(&owner) = open.first() else {
            continue;
        };

        // Both ends lie at most at 2^64, and `start` below `end`.
        let (Ok(start), Ok(last)) = (u64::try_from(start), u64::try_from(end - 1)) else {
            continue;
        };
        match spans.last_mut() {
            Some(span) if span.owner == owner && span.last.checked_add(1) == Some(start) => {
                span.last = last;
            }
            _ => spans.push(Span { start, last, owner }),
        }
    }

    spans
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::{LoadBias, ModuleId, ModuleImageId};

    /// The owner a search of `owners` in order finds `address` in, as
    /// `unwind_module_at` searches.
    fn searched(
        owners: &[(LoadedModule, Option<AddressRange<ImageAddress>>)],
        address: u64,
    ) -> Option<usize> {
        owners.iter().position(|(loaded, range)| {
            range.is_some_and(|range| {
                loaded
                    .load_bias
                    .unload(VirtualAddress::new(address))
                    .is_some_and(|at| loaded.windows(at) && range.contains(at))
            })
        })
    }

    fn owner() -> impl Strategy<Value = (LoadedModule, Option<AddressRange<ImageAddress>>)> {
        // Small addresses so that ranges overlap, and biases that wrap, as
        // the vDSO's may, and that reach the last address there is.
        let address = prop_oneof![0_u64..64, (u64::MAX - 8)..=u64::MAX];
        let bias = prop_oneof![
            Just(0_u64),
            0_u64..32,
            Just(u64::MAX - 4),
            Just(u64::MAX - 40)
        ];
        let range = (address, 0_u64..48).prop_map(|(start, length)| AddressRange {
            start: ImageAddress::new(start),
            end: ImageAddress::new(start.saturating_add(length)),
        });
        let window = proptest::option::of(range.clone());
        (bias, proptest::option::weighted(0.9, range), window).prop_map(|(bias, range, window)| {
            (
                LoadedModule {
                    id: ModuleId::new(1),
                    image: ModuleImageId::new(1),
                    load_bias: LoadBias::new(bias),
                    window,
                },
                range,
            )
        })
    }

    fn image() -> Image {
        Image {
            info: None,
            id: ModuleImageId::new(1),
            module: 0,
            probes: None,
            anonymous: false,
            deferred: None,
            generated: false,
            v8: false,
            code_keepers: std::sync::OnceLock::new(),
            copied: None,
        }
    }

    /// Pieces apart, some empty and some touching, in any order.
    fn pieces() -> impl Strategy<Value = Vec<Mapped>> {
        proptest::collection::vec((0_u64..3, 0_u64..4), 0..12)
            .prop_map(|lengths| {
                let mut end = 0;
                lengths
                    .into_iter()
                    .map(|(gap, length)| {
                        let start = end + gap;
                        end = start + length;
                        Mapped {
                            start,
                            end,
                            image: 0,
                            loaded: LoadedModule {
                                id: ModuleId::new(1),
                                image: ModuleImageId::new(1),
                                load_bias: LoadBias::new(0),
                                window: None,
                            },
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .prop_shuffle()
    }

    proptest! {
        /// With the index built, a piece is found where a search of the
        /// pieces in order finds it.
        #[test]
        fn the_index_finds_the_piece_a_search_in_order_does(pieces in pieces()) {
            let mut mappings = Mappings::default();
            mappings.edit().extend_from_slice(&pieces);
            let searched = mappings.clone();
            mappings.index(&[image()]);
            prop_assert!(mappings.index.as_ref().is_some_and(|index| index.by_start.is_some()));

            let at = |mappings: &Mappings, address| {
                mappings.piece_at(address).map(|piece| (piece.start, piece.end))
            };
            let probes = pieces
                .iter()
                .flat_map(|piece| [piece.start.wrapping_sub(1), piece.start, piece.end.wrapping_sub(1), piece.end])
                .chain([0, u64::MAX]);
            for address in probes {
                prop_assert_eq!(at(&mappings, address), at(&searched, address), "{}", address);
            }
        }

        /// The spans find each address where a search of the modules in
        /// order does: in the first that describes it, or in none. Owners
        /// overlap, wrap, and reach the last address.
        #[test]
        fn spans_find_an_address_where_a_search_in_order_does(
            owners in proptest::collection::vec(owner(), 0..8),
        ) {
            let spans = spans(owners.iter().copied());
            let last = AtomicUsize::new(0);
            for pair in spans.windows(2) {
                prop_assert!(pair[0].last < pair[1].start, "{spans:?}");
            }
            let probes = owners
                .iter()
                .flat_map(|(loaded, range)| {
                    let bias = loaded.load_bias.get();
                    let ends = range.iter().chain(&loaded.window).flat_map(|range| {
                        [range.start.get(), range.end.get()]
                    });
                    ends.flat_map(move |end| {
                        let at = end.wrapping_add(bias);
                        [at.wrapping_sub(1), at, at.wrapping_add(1)]
                    })
                })
                .chain([0, 1, 100, u64::MAX - 1, u64::MAX]);
            for address in probes {
                prop_assert_eq!(
                    span_at(&spans, address, &last).map(|span| span.owner),
                    searched(&owners, address),
                    "{:#x} in {:?}",
                    address,
                    spans
                );
            }
        }
    }
}
