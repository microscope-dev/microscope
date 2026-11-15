use rayon::slice::ParallelSliceMut as _;
use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::SharedRecord;
use super::strings::{StrId, Strings};
use crate::{AddressRange, ImageAddress};

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Interval {
    pub start: U64,
    pub end: U64,
    pub prefix_max_end: U64,

    pub value: U32,
}

impl SharedRecord for Interval {
    const NAME: &'static str = "Interval";
}

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

    intervals.dedup();
    let mut prefix_max_end = 0;
    for interval in &mut intervals {
        prefix_max_end = prefix_max_end.max(interval.end.get());
        interval.prefix_max_end = prefix_max_end.into();
    }

    intervals
}

pub fn containing(intervals: &[Interval], address: ImageAddress) -> impl Iterator<Item = u32> + '_ {
    let address = address.get();
    let mut index = intervals.partition_point(|interval| interval.start.get() <= address);
    std::iter::from_fn(move || {
        while index > 0 {
            index -= 1;
            let interval = intervals.get(index)?;
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

#[expect(clippy::cast_possible_truncation, reason = "the index keeps 32 bits")]
pub(super) fn hash(name: &[u8]) -> u32 {
    twox_hash::XxHash3_64::oneshot(name) as u32
}

const fn key(entry: &NameEntry) -> (u32, u32, u32) {
    (entry.hash.get(), entry.value.get(), entry.name.get())
}

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

pub fn named<'a>(
    strings: Strings<'a>,
    names: &'a [NameEntry],
    name: &str,
) -> impl Iterator<Item = u32> + 'a {
    let wanted = hash(name.as_bytes());
    let first = names.partition_point(|entry| entry.hash.get() < wanted);
    let rest = names.get(first..).unwrap_or_default();
    let matching = rest
        .get(..rest.partition_point(|entry| entry.hash.get() == wanted))
        .unwrap_or_default();
    let name = name.as_bytes().to_vec();
    matching
        .iter()
        .filter(move |entry| strings.bytes(StrId(entry.name.get())) == name)
        .map(|entry| entry.value.get())
}

pub(super) fn valid_names(strings: &Strings<'_>, names: &[NameEntry], values: usize) -> bool {
    names.iter().all(|entry| {
        strings.contains(StrId(entry.name.get()))
            && (entry.value.get() as usize) < values
            && entry.hash.get() == hash(strings.bytes(StrId(entry.name.get())))
    }) && names.is_sorted_by(|earlier, later| key(earlier) < key(later))
}

#[cfg(test)]
mod tests;
