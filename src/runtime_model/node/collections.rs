//! Map and Set, read from their ordered hash tables (`ordered-hash-table.h`).
//! The table is a fixed array: counts of live entries, deleted entries and
//! buckets, then the buckets, then the entries in insertion order. Each
//! entry is its key, its value for a map, and its chain. A deleted entry's
//! key is the hole.

use super::objects::{Object, Problem, Reader};
use super::own::{MAX_ENTRIES, is_hole};
use super::typed::SHOWN;
use crate::image::v8::{Constant, Field};

/// The size of `object` (a `JSMap` if `map`, else a `JSSet`) and its first
/// entries in insertion order, up to inspect's limit: each key, plus its
/// value for a map. If `full` is false, only the size.
#[allow(
    clippy::type_complexity,
    reason = "a key and a map's value, as tagged words"
)]
pub(super) fn entries(
    reader: &Reader<'_>,
    object: Object,
    map: bool,
    full: bool,
) -> Result<(Vec<(u64, Option<u64>)>, i64), Problem> {
    all_entries(reader, object, map, if full { SHOWN } else { 0 })
}

/// Like [`entries`], but reads up to `limit` entries.
#[allow(
    clippy::type_complexity,
    reason = "a key and a map's value, as tagged words"
)]
pub(super) fn all_entries(
    reader: &Reader<'_>,
    object: Object,
    map: bool,
    limit: u64,
) -> Result<(Vec<(u64, Option<u64>)>, i64), Problem> {
    use Constant as C;
    let (ty, entry_size, value_offset, [elements, deleted, buckets, start]) = if map {
        (
            C::OrderedHashMapType,
            C::OrderedHashMapEntrySize,
            Some(C::OrderedHashMapValueOffset),
            [
                C::OrderedHashMapNumberOfElementsIndex,
                C::OrderedHashMapNumberOfDeletedElementsIndex,
                C::OrderedHashMapNumberOfBucketsIndex,
                C::OrderedHashMapHashTableStartIndex,
            ],
        )
    } else {
        (
            C::OrderedHashSetType,
            C::OrderedHashSetEntrySize,
            None,
            [
                C::OrderedHashSetNumberOfElementsIndex,
                C::OrderedHashSetNumberOfDeletedElementsIndex,
                C::OrderedHashSetNumberOfBucketsIndex,
                C::OrderedHashSetHashTableStartIndex,
            ],
        )
    };
    let table = reader.typed_field(object, Field::JSCollectionTable, ty)?;
    let count = |index| -> Result<i64, Problem> {
        let index = reader.constant(index)?;
        reader
            .smi(reader.element(table, index)?)?
            .filter(|count| (0..=MAX_ENTRIES).contains(count))
            .ok_or_else(|| format!("the table {:#x} has no valid count at {index}", table.0).into())
    };
    let live = count(elements)?;
    if limit == 0 {
        return Ok((Vec::new(), live));
    }
    let deleted = count(deleted)?;
    let buckets = count(buckets)?;
    let first = reader.constant(start)? + buckets;
    let size = reader.constant(entry_size)?;
    let shown = i64::try_from(limit).unwrap_or(i64::MAX);

    let mut found = Vec::new();
    for entry in 0..live + deleted {
        if i64::try_from(found.len()).unwrap_or(i64::MAX) >= shown {
            break;
        }
        let at = first + entry * size;
        let key = reader.element(table, at)?;
        if is_hole(reader, key)? {
            continue;
        }
        let value = value_offset
            .map(|offset| -> Result<u64, Problem> {
                reader.element(table, at + reader.constant(offset)?)
            })
            .transpose()?;
        found.push((key, value));
    }
    let expected = live.min(shown);
    if i64::try_from(found.len()).unwrap_or(i64::MAX) != expected {
        return Err(format!(
            "the table {:#x} has {} live entries, but its count says {live}",
            table.0,
            found.len()
        )
        .into());
    }
    Ok((found, live))
}
