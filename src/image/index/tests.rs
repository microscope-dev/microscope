use proptest::prelude::*;

use super::{Interval, containing, intervals, valid_intervals};
use crate::image::sample::{TARGET, range};
use crate::image::symbols::{self, SymbolView};
use crate::image::unwind::{FdeLookup, FdeMiss};
use crate::image::{Builder, Limits, StringsBuilder, TableKind};
use crate::{
    AddressRange, CodeRole, ImageAddress, SymbolBinding, SymbolExtent, SymbolExtentProvenance,
    SymbolId, SymbolInfo, SymbolKind,
};

/// Where a case's ranges lie: at the bottom of the address space, or up
/// to `u64::MAX`.
const REGIONS: [u64; 2] = [0, u64::MAX - 39];

/// Ranges of one region, some empty, some repeated, each with a value
/// below `values`, and addresses to look them up at: every range's
/// edges, and others.
fn ranges_and_addresses(
    values: u32,
) -> impl Strategy<Value = (Vec<(AddressRange<ImageAddress>, u32)>, Vec<u64>)> {
    prop::sample::select(REGIONS.to_vec()).prop_flat_map(move |region| {
        let entry = (
            0_u64..32,
            prop_oneof![1 => Just(0), 6 => 1_u64..9],
            0..values,
        )
            .prop_map(move |(offset, length, value)| {
                (range(region + offset, region + offset + length), value)
            });
        (
            proptest::collection::vec(entry, 0..10),
            proptest::collection::vec(0_u64..48, 0..6),
        )
            .prop_map(move |(mut entries, offsets)| {
                if let Some(&repeated) = entries.first() {
                    entries.push(repeated);
                }

                let mut addresses = offsets
                    .into_iter()
                    .map(|offset| region.saturating_add(offset))
                    .collect::<Vec<_>>();
                for (range, _) in &entries {
                    let (start, end) = (range.start.get(), range.end.get());
                    addresses.extend([start, end, end.saturating_sub(1), start.saturating_sub(1)]);
                }
                (entries, addresses)
            })
    })
}

/// The intervals containing `address`, by a scan of every entry: each
/// distinct one once, in descending (start, end, value) order.
fn scanned(entries: &[(AddressRange<ImageAddress>, u32)], address: u64) -> Vec<(u64, u64, u32)> {
    let mut found = entries
        .iter()
        .filter(|(range, _)| range.contains(ImageAddress::new(address)))
        .map(|(range, value)| (range.start.get(), range.end.get(), *value))
        .collect::<Vec<_>>();
    found.sort_unstable();
    found.dedup();
    found.reverse();
    found
}

proptest! {
    /// The index holds each nonempty interval once, in order, with valid
    /// running ends, and finds what a scan of the entries finds.
    #[test]
    fn intervals_containing_an_address_are_those_a_scan_finds(
        (entries, addresses) in ranges_and_addresses(4),
    ) {
        let index = intervals(entries.iter().copied());
        prop_assert!(valid_intervals(&index, 4));

        let mut distinct = entries
            .iter()
            .filter(|(range, _)| range.start < range.end)
            .collect::<Vec<_>>();
        distinct.sort_unstable_by_key(|(range, value)| (range.start, range.end, *value));
        distinct.dedup();
        prop_assert_eq!(index.len(), distinct.len());

        for address in addresses {
            let found = containing(&index, ImageAddress::new(address))
                .collect::<Vec<_>>();
            let expected = scanned(&entries, address)
                .into_iter()
                .map(|(_, _, value)| value)
                .collect::<Vec<_>>();
            prop_assert_eq!(found, expected, "at {:#x}", address);
        }
    }

    /// The entry found for an address is the one at the lowest offset
    /// that covers it, unless the section's first error comes first, and
    /// its function begins where the latest of its ranges covering the
    /// address does.
    #[test]
    fn an_fde_lookup_finds_the_first_entry_before_any_error(
        (entries, addresses) in ranges_and_addresses(64),
        error in proptest::option::of(0_u64..72),
    ) {
        let index: Vec<Interval> = intervals(entries.iter().copied());
        let lookup = FdeLookup {
            entries: &index,
            first_error: error.map(|offset| (offset, "malformed")),
        };

        for address in addresses {
            let covering = scanned(&entries, address);
            let first = covering.iter().map(|(_, _, offset)| *offset).min();
            let expected = match (first, error) {
                (Some(offset), Some(error)) if u64::from(offset) < error => Ok(offset as usize),
                (Some(offset), None) => Ok(offset as usize),
                (_, Some(_)) => Err(FdeMiss::Malformed("malformed")),
                (None, None) => Err(FdeMiss::NoEntry),
            };
            prop_assert_eq!(lookup.lookup(address), expected, "at {:#x}", address);

            let start = expected.ok().and_then(|offset| {
                covering
                    .iter()
                    .filter(|(_, _, value)| *value as usize == offset)
                    .map(|(start, _, _)| *start)
                    .next()
            });
            prop_assert_eq!(lookup.function_start(address), start, "at {:#x}", address);
        }
    }
}

/// A symbol: code with an extent, sized or unsized data, or neither.
fn symbol_info(id: u32, (start, length): (u64, u64), shape: u8) -> SymbolInfo {
    let address = ImageAddress::new(start);
    let length = length.min(u64::MAX - start);
    let end = ImageAddress::new(start + length);

    // No code begins at the last address, which no extent can hold.
    let shape = if start == u64::MAX && shape < 2 {
        3
    } else {
        shape
    };
    let (kind, extent, storage) = match shape {
        0 | 1 => (
            SymbolKind::Function,
            Some(SymbolExtent {
                range: AddressRange {
                    start: address,
                    end: ImageAddress::new(start + length.max(1)),
                },
                provenance: if shape == 0 {
                    SymbolExtentProvenance::Declared
                } else {
                    SymbolExtentProvenance::Inferred
                },
            }),
            None,
        ),
        2 => (
            SymbolKind::Data,
            None,
            Some(AddressRange {
                start: address,
                end,
            }),
        ),
        _ => (SymbolKind::Function, None, None),
    };

    SymbolInfo {
        id: SymbolId::new(id),
        name: format!("s{}", id % 3).into(),
        address,
        kind,
        binding: SymbolBinding::Global,
        exported: id.is_multiple_of(2),
        extent,
        storage,
        role: CodeRole::Ordinary,
    }
}

proptest! {
    /// `code_at` and `data_at` return, of the symbols whose code or
    /// storage contains an address, the one of least rank; failing sized
    /// storage, an unsized symbol at the address.
    #[test]
    fn a_symbol_at_an_address_is_the_least_ranked_that_holds_it(
        region in prop::sample::select(vec![0, u64::MAX - 31]),
        shapes in proptest::collection::vec(
            ((0_u64..32, prop_oneof![1 => Just(0), 4 => 1_u64..9]), 0_u8..4),
            0..10,
        ),
        offsets in proptest::collection::vec(0_u64..48, 0..6),
    ) {
        let infos = shapes
            .into_iter()
            .zip(0..)
            .map(|(((offset, length), shape), id)| {
                symbol_info(id, (region + offset, length), shape)
            })
            .collect::<Vec<_>>();
        let mut builder = Builder::new(TARGET);
        let mut strings = StringsBuilder::default();
        symbols::add_to(&mut builder, &mut strings, &infos, &[], &[]).unwrap();
        builder.bytes(TableKind::Strings, strings.into_bytes());
        let image = builder.seal(Limits::default()).unwrap();
        let view = SymbolView::new(&image);

        let addresses = offsets
            .into_iter()
            .map(|offset| region.saturating_add(offset))
            .chain(infos.iter().flat_map(|info| {
                let start = info.address.get();
                [start, start.saturating_sub(1), start.saturating_add(8)]
            }));
        for address in addresses {
            let at = ImageAddress::new(address);
            let least = |holds: &dyn Fn(&SymbolInfo) -> bool, code: bool| {
                infos
                    .iter()
                    .filter(|info| holds(info))
                    .map(|info| view.get(info.id).unwrap())
                    .min_by_key(|symbol| if code { symbol.extent_rank() } else { symbol.storage_rank() })
                    .map(symbols::Symbol::id)
            };

            let code = least(
                &|info| info.extent.is_some_and(|extent| extent.range.contains(at)),
                true,
            );
            prop_assert_eq!(view.code_at(at).map(symbols::Symbol::id), code, "at {:#x}", address);

            // An unsized symbol at `u64::MAX` names an address no
            // interval can end past, and is not found.
            let data = least(
                &|info| info.storage.is_some_and(|storage| storage.contains(at)),
                false,
            )
            .or_else(|| {
                least(
                    &|info| {
                        info.storage.is_some_and(|storage| {
                            storage.start == at && storage.end == at && address != u64::MAX
                        })
                    },
                    false,
                )
            });
            prop_assert_eq!(view.data_at(at).map(symbols::Symbol::id), data, "at {:#x}", address);
        }
    }
}
