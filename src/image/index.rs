//! Indexes several tables share the shape of: intervals ordered for the
//! lookup of every interval containing an address, and names ordered for
//! the lookup of every record a name names.

use rayon::slice::ParallelSliceMut as _;
use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::SharedRecord;
use super::strings::{StrId, Strings};
use crate::{AddressRange, ImageAddress};

/// One interval, in (start, end, value) order, with the greatest end of
/// it and every interval before it, which ends a backward search.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Interval {
    pub start: U64,
    pub end: U64,
    pub prefix_max_end: U64,

    /// The record the interval belongs to.
    pub value: U32,
}

impl SharedRecord for Interval {
    const NAME: &'static str = "Interval";
}

/// The intervals of `entries`, in order, once each, without empty ones.
pub fn intervals(
    entries: impl IntoIterator<Item = (AddressRange<ImageAddress>, u32)>,
) -> Vec<Interval> {
    let mut intervals = entries
        .into_iter()
        .filter(|(range, _)| range.start < range.end)
        .map(|(range, value)| Interval {
            start: range.start.get().into(),
            end: range.end.get().into(),
            prefix_max_end: 0.into(),
            value: value.into(),
        })
        .collect::<Vec<_>>();
    intervals.par_sort_unstable_by_key(|interval| {
        (
            interval.start.get(),
            interval.end.get(),
            interval.value.get(),
        )
    });

    // A record may name one range twice.
    intervals.dedup();
    let mut prefix_max_end = 0;
    for interval in &mut intervals {
        prefix_max_end = prefix_max_end.max(interval.end.get());
        interval.prefix_max_end = prefix_max_end.into();
    }

    intervals
}

/// The values of the intervals containing `address`, latest start first.
pub fn containing(intervals: &[Interval], address: ImageAddress) -> impl Iterator<Item = u32> + '_ {
    let address = address.get();
    let mut index = intervals.partition_point(|interval| interval.start.get() <= address);
    std::iter::from_fn(move || {
        while index > 0 {
            index -= 1;
            let interval = &intervals[index];
            if interval.prefix_max_end.get() <= address {
                return None;
            }

            if address < interval.end.get() {
                return Some(interval.value.get());
            }
        }

        None
    })
}

/// Whether `intervals` is an index of nonempty intervals over `values`
/// records, ordered, with correct running ends.
pub(super) fn valid_intervals(intervals: &[Interval], values: usize) -> bool {
    let mut prefix_max_end = 0;
    intervals.iter().all(|interval| {
        prefix_max_end = prefix_max_end.max(interval.end.get());
        interval.start.get() < interval.end.get()
            && (interval.value.get() as usize) < values
            && interval.prefix_max_end.get() == prefix_max_end
    }) && intervals.is_sorted_by(|earlier, later| {
        (earlier.start.get(), earlier.end.get(), earlier.value.get())
            < (later.start.get(), later.end.get(), later.value.get())
    })
}

/// One name of a record. An index orders them by the name's hash, then
/// the record, then where the pool keeps the name, so that building one
/// sorts numbers, and finding a name compares its bytes only where the
/// hash matches.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct NameEntry {
    pub hash: U32,
    pub name: U32,
    pub value: U32,
}

impl SharedRecord for NameEntry {
    const NAME: &'static str = "NameEntry";
}

/// The hash a name index orders a name by.
#[expect(clippy::cast_possible_truncation, reason = "the index keeps 32 bits")]
pub(super) fn hash(name: &[u8]) -> u32 {
    twox_hash::XxHash3_64::oneshot(name) as u32
}

/// An entry's place: by hash, then record, then name, so that two names
/// of one record whose hashes collide are both kept.
const fn key(entry: &NameEntry) -> (u32, u32, u32) {
    (entry.hash.get(), entry.value.get(), entry.name.get())
}

/// The name index of `entries`, each a name, where the pool holds it, and
/// the record it names.
pub fn names<'s>(entries: impl IntoIterator<Item = (&'s str, StrId, u32)>) -> Vec<NameEntry> {
    let mut names = entries
        .into_iter()
        .map(|(text, name, value)| NameEntry {
            hash: hash(text.as_bytes()).into(),
            name: name.0.into(),
            value: value.into(),
        })
        .collect::<Vec<_>>();
    names.par_sort_unstable_by_key(key);
    names.dedup_by_key(|entry| key(entry));
    names
}

/// The values `name` names, in order.
pub fn named<'a>(
    strings: Strings<'a>,
    names: &'a [NameEntry],
    name: &str,
) -> impl Iterator<Item = u32> + 'a {
    let wanted = hash(name.as_bytes());
    let first = names.partition_point(|entry| entry.hash.get() < wanted);
    let end = first + names[first..].partition_point(|entry| entry.hash.get() == wanted);
    let name = name.as_bytes().to_vec();
    names[first..end]
        .iter()
        .filter(move |entry| strings.bytes(StrId(entry.name.get())) == name)
        .map(|entry| entry.value.get())
}

/// Whether `names` is a name index over `values` records whose names are
/// in `strings`.
pub(super) fn valid_names(strings: &Strings<'_>, names: &[NameEntry], values: usize) -> bool {
    names.iter().all(|entry| {
        strings.contains(StrId(entry.name.get()))
            && (entry.value.get() as usize) < values
            && entry.hash.get() == hash(strings.bytes(StrId(entry.name.get())))
    }) && names.is_sorted_by(|earlier, later| key(earlier) < key(later))
}
