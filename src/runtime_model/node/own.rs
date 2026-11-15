use std::ops::Range;

use super::objects::{Object, Problem, Reader};
use super::properties::{descriptor, field_value, part};
use crate::image::v8::{Constant, Field};

pub(super) const MAX_ENTRIES: i64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Held {
    Data(u64),

    Double { bits: u64, address: u64 },

    Accessor(Object),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Named {
    pub(super) key: Object,
    pub(super) symbol: bool,

    pub(super) private: bool,
    pub(super) enumerable: bool,
    pub(super) held: Held,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Element {
    pub(super) index: u32,
    pub(super) enumerable: bool,
    pub(super) held: Held,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Store {
    Tagged,
    Doubles,
    Dictionary,
    StringWrapper { dictionary: bool },
    Typed,
    SloppyArguments,
}

pub(super) fn elements_kind(reader: &Reader<'_>, map: Object) -> Result<i64, Problem> {
    let bits = i64::from(reader.byte_field(map, Field::MapBitField2)?);
    part(
        reader,
        bits,
        Constant::MapElementsKindMask,
        Some(Constant::MapElementsKindShift),
    )
}

fn plainly_held(reader: &Reader<'_>, map: Object, named: bool) -> Result<(), Problem> {
    let bits = i64::from(reader.byte_field(map, Field::MapBitField)?);
    let (mask, shift, kind) = if named {
        (
            Constant::MapHasNamedInterceptorMask,
            Constant::MapHasNamedInterceptorShift,
            "properties",
        )
    } else {
        (
            Constant::MapHasIndexedInterceptorMask,
            Constant::MapHasIndexedInterceptorShift,
            "elements",
        )
    };
    if part(reader, bits, mask, Some(shift))? != 0 {
        return Err(format!(
            "an interceptor gives this object's {kind}, and microscope does not call interceptors"
        )
        .into());
    }
    let checked = part(
        reader,
        bits,
        Constant::MapIsAccessCheckNeededMask,
        Some(Constant::MapIsAccessCheckNeededShift),
    )?;
    if checked != 0 {
        return Err(
            "V8 checks access to this object, and microscope does not run the check".into(),
        );
    }
    Ok(())
}

pub(super) fn store(reader: &Reader<'_>, kind: i64) -> Result<Store, Problem> {
    let is = |constants: &[Constant]| {
        constants
            .iter()
            .any(|constant| reader.constant(*constant).ok() == Some(kind))
    };
    Ok(
        if is(&[
            Constant::ElementsKindPackedSmi,
            Constant::ElementsKindHoleySmi,
            Constant::ElementsKindPacked,
            Constant::ElementsKindHoley,
            Constant::ElementsKindPackedNonextensible,
            Constant::ElementsKindHoleyNonextensible,
            Constant::ElementsKindPackedSealed,
            Constant::ElementsKindHoleySealed,
            Constant::ElementsKindPackedFrozen,
            Constant::ElementsKindHoleyFrozen,
            Constant::ElementsKindSharedArray,
        ]) {
            Store::Tagged
        } else if is(&[
            Constant::ElementsKindPackedDouble,
            Constant::ElementsKindHoleyDouble,
        ]) {
            Store::Doubles
        } else if is(&[Constant::ElementsKindDictionary]) {
            Store::Dictionary
        } else if is(&[Constant::ElementsKindFastStringWrapper]) {
            Store::StringWrapper { dictionary: false }
        } else if is(&[Constant::ElementsKindSlowStringWrapper]) {
            Store::StringWrapper { dictionary: true }
        } else if is(&[
            Constant::ElementsKindFastSloppyArguments,
            Constant::ElementsKindSlowSloppyArguments,
        ]) {
            Store::SloppyArguments
        } else if super::typed::element_type(reader, kind).is_some() {
            Store::Typed
        } else {
            return Err(format!("microscope does not read elements kind {kind}").into());
        },
    )
}

pub(super) fn elements(
    reader: &Reader<'_>,
    holder: Object,
    range: Range<u32>,
) -> Result<Vec<Element>, Problem> {
    let map = reader.map(holder)?;
    plainly_held(reader, map, false)?;
    let store = store(reader, elements_kind(reader, map)?)?;
    let backing = || -> Result<Object, Problem> {
        let word = reader.field(holder, Field::JSObjectElements)?;
        reader
            .object(word)?
            .ok_or_else(|| format!("the elements of {:#x} are not an object", holder.0).into())
    };
    let found = match store {
        Store::Tagged | Store::StringWrapper { dictionary: false } => {
            fast_elements(reader, backing()?, range.clone())
        }
        Store::Doubles => double_elements(reader, backing()?, range.clone()),
        Store::Dictionary | Store::StringWrapper { dictionary: true } => {
            let mut found = dictionary_elements(reader, backing()?)?;
            found.retain(|element| range.contains(&element.index));
            Ok(found)
        }
        Store::Typed => Err("a typed array's elements are its numbers".into()),
        Store::SloppyArguments => {
            Err("sloppy-mode arguments alias the frame's variables, and microscope does not follow them".into())
        }
    }?;
    debug_assert!(
        found.iter().all(|element| range.contains(&element.index)),
        "the elements of {:#x} hold an index outside {range:?}",
        holder.0,
    );
    debug_assert!(
        found.is_sorted_by_key(|element| element.index),
        "the elements of {:#x} are out of index order",
        holder.0,
    );
    Ok(found)
}

fn element_range(
    reader: &Reader<'_>,
    array: Object,
    range: Range<u32>,
) -> Result<Range<u32>, Problem> {
    let capacity = reader.smi_field(array, Field::FixedArrayBaseLength)?;
    let capacity = u32::try_from(capacity).map_err(|_| {
        format!(
            "the elements {:#x} have invalid capacity {capacity}",
            array.0
        )
    })?;
    let end = range.end.min(capacity);
    let start = range.start.min(end);
    let count = end - start;
    if i64::from(count) > MAX_ENTRIES {
        return Err(format!("reading {count} elements exceeds the limit of {MAX_ENTRIES}").into());
    }
    Ok(start..end)
}

fn fast_elements(
    reader: &Reader<'_>,
    array: Object,
    range: Range<u32>,
) -> Result<Vec<Element>, Problem> {
    let ty = reader.instance_type(array)?;
    if ty != reader.constant(Constant::FixedArrayType)? {
        return Err(format!("the elements {:#x} are not a fixed array", array.0).into());
    }
    let range = element_range(reader, array, range)?;
    let base = reader.field_address(array, Field::FixedArrayObjects)?;
    let address = base
        .checked_add(8 * u64::from(range.start))
        .ok_or("the element address overflows")?;
    let words = words(reader, address, i64::from(range.end - range.start))?;
    let mut found = Vec::new();
    for (index, word) in range.zip(words) {
        if is_hole(reader, word)? {
            continue;
        }
        found.push(Element {
            index,
            enumerable: true,
            held: Held::Data(word),
        });
    }
    Ok(found)
}

fn double_elements(
    reader: &Reader<'_>,
    array: Object,
    range: Range<u32>,
) -> Result<Vec<Element>, Problem> {
    if reader.is(array, Constant::FixedArrayType)?
        && reader.smi_field(array, Field::FixedArrayBaseLength)? == 0
    {
        return Ok(Vec::new());
    }
    if !reader.is(array, Constant::FixedDoubleArrayType)? {
        return Err(format!("the elements {:#x} are not a fixed double array", array.0).into());
    }
    let range = element_range(reader, array, range)?;
    let hole = reader.constant(Constant::HoleNanInt64)?.cast_unsigned();
    let base = reader.field_address(array, Field::FixedDoubleArrayValues)?;
    let address = base
        .checked_add(8 * u64::from(range.start))
        .ok_or("the element address overflows")?;
    let words = words(reader, address, i64::from(range.end - range.start))?;
    Ok(range
        .zip(words)
        .filter(|(_, bits)| *bits != hole)
        .map(|(index, bits)| Element {
            index,
            enumerable: true,
            held: Held::Double {
                bits,
                address: base + 8 * u64::from(index),
            },
        })
        .collect())
}

fn dictionary_elements(reader: &Reader<'_>, table: Object) -> Result<Vec<Element>, Problem> {
    if !reader.is(table, Constant::NumberDictionaryType)? {
        return Err(format!("the elements {:#x} are not a number dictionary", table.0).into());
    }
    let mut found = Vec::new();
    for entry in entries(
        reader,
        table,
        Constant::NumberDictionaryShapePrefixSize,
        Constant::NumberDictionaryShapeEntrySize,
    )? {
        let key = reader.element(
            table,
            entry + reader.constant(Constant::HashTableEntryKeyIndex)?,
        )?;
        let Some(index) = index_of(reader, key)? else {
            continue;
        };
        let details = smi(
            reader,
            reader.element(
                table,
                entry + reader.constant(Constant::NumberDictionaryEntryDetailsIndex)?,
            )?,
        )?;
        let value = reader.element(
            table,
            entry + reader.constant(Constant::NumberDictionaryEntryValueIndex)?,
        )?;
        found.push(Element {
            index,
            enumerable: enumerable(reader, details)?,
            held: held(reader, details, value)?,
        });
    }
    found.sort_by_key(|element| element.index);
    Ok(found)
}

fn index_of(reader: &Reader<'_>, key: u64) -> Result<Option<u32>, Problem> {
    if let Some(index) = reader.smi(key)? {
        return u32::try_from(index)
            .map(Some)
            .map_err(|_| format!("a dictionary has the invalid index {index}").into());
    }
    let Some(key) = reader.object(key)? else {
        return Ok(None);
    };
    if !reader.is(key, Constant::HeapNumberType)? {
        return Ok(None);
    }
    let number = f64::from_bits(reader.word(reader.field_address(key, Field::HeapNumberValue)?)?);
    if number.fract() != 0.0 || !(0.0..4_294_967_295.0).contains(&number) {
        return Err(format!("a dictionary has the invalid index {number}").into());
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a whole number from 0 below 2^32 - 1"
    )]
    Ok(Some(number as u32))
}

pub(super) fn named(reader: &Reader<'_>, holder: Object) -> Result<Vec<Named>, Problem> {
    let mut found = all_named(reader, holder)?;
    found.retain(|property| !property.private);
    Ok(found)
}

pub(super) fn all_named(reader: &Reader<'_>, holder: Object) -> Result<Vec<Named>, Problem> {
    let map = reader.map(holder)?;
    plainly_held(reader, map, true)?;
    let bits = i64::from(
        reader
            .int32_field(map, Field::MapBitField3)?
            .cast_unsigned(),
    );
    let mut found = if (bits >> reader.constant(Constant::BitField3IsDictionaryMapShift)?) & 1 != 0
    {
        dictionary(reader, holder)?
    } else {
        let own = (bits & reader.constant(Constant::BitField3NumberOfOwnDescriptorsMask)?)
            >> reader.constant(Constant::BitField3NumberOfOwnDescriptorsShift)?;
        fast(reader, holder, map, own)?
    };
    found.sort_by_key(|property| property.symbol);
    debug_assert!(
        !found
            .iter()
            .any(|property| matches!(property.held, Held::Double { .. })),
        "a named property of {:#x} holds a double element",
        holder.0,
    );
    Ok(found)
}

fn fast(reader: &Reader<'_>, holder: Object, map: Object, own: i64) -> Result<Vec<Named>, Problem> {
    let mut found = Vec::new();
    for index in 0..own {
        let key = descriptor(reader, map, index, Constant::PropDescKey)?;
        let Some((key, symbol, private)) = key_of(reader, key)? else {
            continue;
        };
        let details = smi(
            reader,
            descriptor(reader, map, index, Constant::PropDescDetails)?,
        )?;
        let location = part(
            reader,
            details,
            Constant::PropLocationMask,
            Some(Constant::PropLocationShift),
        )?;
        let value = if location == reader.constant(Constant::PropLocationDescriptor)? {
            descriptor(reader, map, index, Constant::PropDescValue)?
        } else {
            let field = part(
                reader,
                details,
                Constant::PropIndexMask,
                Some(Constant::PropIndexShift),
            )?;
            field_value(reader, holder, map, field)?
        };
        found.push(Named {
            key,
            symbol,
            private,
            enumerable: enumerable(reader, details)?,
            held: held(reader, details, value)?,
        });
    }
    Ok(found)
}

fn dictionary(reader: &Reader<'_>, holder: Object) -> Result<Vec<Named>, Problem> {
    let word = reader.field(holder, Field::JSReceiverPropertiesOrHash)?;
    let table = reader
        .object(word)?
        .ok_or_else(|| format!("the properties of {:#x} are not an object", holder.0))?;
    if reader.is(table, Constant::GlobalDictionaryType)? {
        return global_dictionary(reader, table);
    }
    if !reader.is(table, Constant::NameDictionaryType)? {
        return Err(format!(
            "the properties of {:#x} are in a dictionary of unexpected instance type {}",
            holder.0,
            reader.instance_type(table)?
        )
        .into());
    }

    let mut found = Vec::new();
    for entry in entries(
        reader,
        table,
        Constant::NameDictionaryShapePrefixSize,
        Constant::NameDictionaryShapeEntrySize,
    )? {
        let key = reader.element(
            table,
            entry + reader.constant(Constant::HashTableEntryKeyIndex)?,
        )?;
        let Some((key, symbol, private)) = key_of(reader, key)? else {
            continue;
        };
        let details = smi(
            reader,
            reader.element(
                table,
                entry + reader.constant(Constant::NameDictionaryEntryDetailsIndex)?,
            )?,
        )?;
        let value = reader.element(
            table,
            entry + reader.constant(Constant::NameDictionaryEntryValueIndex)?,
        )?;
        let order = part(
            reader,
            details,
            Constant::PropertyDetailsDictionaryStorageMask,
            Some(Constant::PropertyDetailsDictionaryStorageShift),
        )?;
        found.push((
            order,
            Named {
                key,
                symbol,
                private,
                enumerable: enumerable(reader, details)?,
                held: held(reader, details, value)?,
            },
        ));
    }
    found.sort_by_key(|(order, _)| *order);
    Ok(found.into_iter().map(|(_, property)| property).collect())
}

fn entries(
    reader: &Reader<'_>,
    table: Object,
    prefix: Constant,
    size: Constant,
) -> Result<impl Iterator<Item = i64>, Problem> {
    let capacity = reader
        .smi(reader.element(table, reader.constant(Constant::HashTableCapacityIndex)?)?)?
        .filter(|capacity| (0..=MAX_ENTRIES).contains(capacity))
        .ok_or_else(|| format!("the dictionary {:#x} has no capacity", table.0))?;
    let first = reader.constant(Constant::HashTablePrefixStartIndex)? + reader.constant(prefix)?;
    let size = reader.constant(size)?;
    Ok((0..capacity).map(move |entry| first + entry * size))
}

fn key_of(reader: &Reader<'_>, key: u64) -> Result<Option<(Object, bool, bool)>, Problem> {
    let Some(key) = reader.object(key)? else {
        return Ok(None);
    };
    let ty = reader.instance_type(key)?;
    if ty < reader.constant(Constant::FirstNonstringType)? {
        return Ok(Some((key, false, false)));
    }
    if ty == reader.constant(Constant::SymbolType)? {
        return Ok(Some((key, true, super::values::is_private(reader, key)?)));
    }
    Ok(None)
}

fn held(reader: &Reader<'_>, details: i64, value: u64) -> Result<Held, Problem> {
    if part(reader, details, Constant::PropKindMask, None)?
        != reader.constant(Constant::PropKindAccessor)?
    {
        return Ok(Held::Data(value));
    }
    let accessor = reader
        .object(value)?
        .filter(|accessor| {
            [Constant::AccessorPairType, Constant::AccessorInfoType]
                .iter()
                .any(|kind| reader.is(*accessor, *kind).unwrap_or(false))
        })
        .ok_or("an accessor property does not hold an accessor")?;
    Ok(Held::Accessor(accessor))
}

fn enumerable(reader: &Reader<'_>, details: i64) -> Result<bool, Problem> {
    let attributes = part(
        reader,
        details,
        Constant::PropAttributesMask,
        Some(Constant::PropAttributesShift),
    )?;
    Ok(attributes & reader.constant(Constant::PropAttributesDontEnum)? == 0)
}

fn smi(reader: &Reader<'_>, word: u64) -> Result<i64, Problem> {
    reader
        .smi(word)?
        .ok_or_else(|| "a property's details are not a Smi".into())
}

pub(super) fn is_hole(reader: &Reader<'_>, word: u64) -> Result<bool, Problem> {
    reader
        .object(word)?
        .map_or(Ok(false), |object| reader.is(object, Constant::HoleType))
}

pub(super) fn words(reader: &Reader<'_>, address: u64, count: i64) -> Result<Vec<u64>, Problem> {
    let count = usize::try_from(count).map_err(|_| "a negative count of words")?;
    let mut bytes = vec![0; count * 8];
    reader.bytes_into(address, &mut bytes)?;
    let words: Vec<u64> = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| u64::from_le_bytes(*word))
        .collect();
    debug_assert_eq!(
        words.len(),
        count,
        "reading {count} words at {address:#x} gave another number",
    );
    Ok(words)
}

fn global_dictionary(reader: &Reader<'_>, table: Object) -> Result<Vec<Named>, Problem> {
    let mut found = Vec::new();
    for entry in entries(
        reader,
        table,
        Constant::GlobalDictionaryShapePrefixSize,
        Constant::GlobalDictionaryShapeEntrySize,
    )? {
        let cell = reader.element(
            table,
            entry + reader.constant(Constant::HashTableEntryKeyIndex)?,
        )?;
        let Some(cell) = reader.object(cell)? else {
            continue;
        };
        if !reader.is(cell, Constant::PropertyCellType)? {
            continue;
        }
        let value = reader.field(cell, Field::PropertyCellValue)?;
        if is_hole(reader, value)? {
            continue;
        }
        let Some((key, symbol, private)) =
            key_of(reader, reader.field(cell, Field::PropertyCellName)?)?
        else {
            continue;
        };
        let details = smi(
            reader,
            reader.field(cell, Field::PropertyCellPropertyDetailsRaw)?,
        )?;
        let order = part(
            reader,
            details,
            Constant::PropertyDetailsDictionaryStorageMask,
            Some(Constant::PropertyDetailsDictionaryStorageShift),
        )?;
        found.push((
            order,
            Named {
                key,
                symbol,
                private,
                enumerable: enumerable(reader, details)?,
                held: held(reader, details, value)?,
            },
        ));
    }
    found.sort_by_key(|(order, _)| *order);
    Ok(found.into_iter().map(|(_, property)| property).collect())
}
