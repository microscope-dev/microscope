//! Map and Set, as their ordered hash tables keep them (`ordered-hash-table.h`):
//! a fixed array whose first elements count its live and deleted entries
//! and its buckets, then the buckets, then the entries in insertion order,
//! each its key, its value for a map, and its chain. A deleted entry's key
//! is the hole.

use super::objects::{Object, Problem, Reader};
use super::own::{MAX_ENTRIES, is_hole};
use super::typed::SHOWN;
use crate::image::v8::{Constant, Field};

/// The size of the collection `object`, a `JSMap` when `map` and a `JSSet`
/// otherwise, and its first entries in insertion order, up to inspect's
/// limit: each key, with its value for a map. With `full` false, its size
/// alone.
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

/// As [`entries`], the first `limit` of them.
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
            .ok_or_else(|| format!("the table {:#x} keeps no count at {index}", table.0).into())
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
            "the table {:#x} holds {} live entries, not {live}",
            table.0,
            found.len()
        )
        .into());
    }
    Ok((found, live))
}
