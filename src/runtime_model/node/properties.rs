//! An object's named properties, found as `JSReceiver::GetDataProperty`
//! finds them: the object's own, then its prototypes', where a property
//! that is an accessor ends the lookup with no value, since V8 calls no
//! getter there.
//!
//! A fast map keeps its own properties' names and details in its
//! descriptors, and their values in the object or its property array; a
//! dictionary map keeps all three in a `NameDictionary`. A Swiss dictionary,
//! a global object's, and an object V8 looks up through interceptors or
//! access checks are refused by name.

use super::objects::{Object, Problem, Reader};
use crate::image::v8::{Constant, Field};

/// The most prototypes followed: chains are a few objects long.
const MAX_PROTOTYPES: usize = 64;

/// The most entries of a dictionary searched.
const MAX_ENTRIES: i64 = 1 << 16;

/// A property as its holder keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Own {
    /// A data property, with its tagged value.
    Data(u64),
    /// An accessor, which `GetDataProperty` reads as `undefined`.
    Accessor,
    Absent,
}

/// The value of the data property `name` of `object` or its prototypes, as
/// a tagged word; `None` where none is, or the one found is an accessor.
pub(super) fn data_property(
    reader: &Reader<'_>,
    object: Object,
    name: &str,
) -> Result<Option<u64>, Problem> {
    let mut holder = object;
    for _ in 0..MAX_PROTOTYPES {
        let ty = reader.instance_type(holder)?;
        let functions = reader.constant(Constant::FirstJSFunctionType)?
            ..=reader.constant(Constant::LastJSFunctionType)?;
        let ordinary = [Constant::JSObjectType, Constant::JSObjectPrototypeType]
            .iter()
            .any(|kind| reader.constant(*kind).ok() == Some(ty));
        if !ordinary && !functions.contains(&ty) {
            return Err(format!(
                "{:#x}, of instance type {ty}, is no object whose properties are read",
                holder.0
            )
            .into());
        }

        let map = reader.map(holder)?;
        match own(reader, holder, map, name)? {
            Own::Data(value) => return Ok(Some(value)),
            Own::Accessor => return Ok(None),
            Own::Absent => {}
        }
        // The chain ends at `null`, an oddball.
        match reader.object(reader.field(map, Field::MapPrototype)?)? {
            Some(prototype) if !reader.is(prototype, Constant::OddballType)? => {
                holder = prototype;
            }
            _ => return Ok(None),
        }
    }
    Err(format!("the prototypes of {:#x} are too many", object.0).into())
}

/// The property `name` of `holder`, whose map is `map`.
fn own(reader: &Reader<'_>, holder: Object, map: Object, name: &str) -> Result<Own, Problem> {
    let bits = i64::from(
        reader
            .int32_field(map, Field::MapBitField3)?
            .cast_unsigned(),
    );
    if (bits >> reader.constant(Constant::BitField3IsDictionaryMapShift)?) & 1 != 0 {
        return dictionary(reader, holder, name);
    }
    let own = (bits & reader.constant(Constant::BitField3NumberOfOwnDescriptorsMask)?)
        >> reader.constant(Constant::BitField3NumberOfOwnDescriptorsShift)?;
    for index in 0..own {
        let key = descriptor(reader, map, index, Constant::PropDescKey)?;
        if !is_named(reader, key, name)? {
            continue;
        }
        let details = descriptor(reader, map, index, Constant::PropDescDetails)?;
        let details = reader
            .smi(details)?
            .ok_or("a property's details are no Smi")?;
        if part(reader, details, Constant::PropKindMask, None)?
            == reader.constant(Constant::PropKindAccessor)?
        {
            return Ok(Own::Accessor);
        }
        let location = part(
            reader,
            details,
            Constant::PropLocationMask,
            Some(Constant::PropLocationShift),
        )?;
        if location == reader.constant(Constant::PropLocationDescriptor)? {
            return descriptor(reader, map, index, Constant::PropDescValue).map(Own::Data);
        }
        let field = part(
            reader,
            details,
            Constant::PropIndexMask,
            Some(Constant::PropIndexShift),
        )?;
        return field_value(reader, holder, map, field).map(Own::Data);
    }
    Ok(Own::Absent)
}

/// A part of a property's `details`: the bits `mask` takes, shifted down by
/// `shift`, or by the mask's own zero bits where V8 gives no shift.
fn part(
    reader: &Reader<'_>,
    details: i64,
    mask: Constant,
    shift: Option<Constant>,
) -> Result<i64, Problem> {
    let mask = reader.constant(mask)?;
    let shift = match shift {
        Some(shift) => reader.constant(shift)?,
        None => i64::from(mask.trailing_zeros()),
    };
    Ok((details & mask) >> shift)
}

/// The word `entry` of the descriptor at `index` of `map`'s descriptors,
/// one of [`Constant::PropDescKey`], details, and value.
pub(super) fn descriptor(
    reader: &Reader<'_>,
    map: Object,
    index: i64,
    entry: Constant,
) -> Result<u64, Problem> {
    let word = reader.field(map, Field::MapInstanceDescriptors)?;
    let descriptors = reader
        .object(word)?
        .filter(|array| {
            [
                Constant::DescriptorArrayType,
                Constant::StrongDescriptorArrayType,
            ]
            .iter()
            .any(|kind| reader.is(*array, *kind).unwrap_or(false))
        })
        .ok_or_else(|| format!("the descriptors of the map {:#x} are no array", map.0))?;
    let mut count = [0; 2];
    reader.bytes_into(
        reader.field_address(descriptors, Field::DescriptorArrayNumberOfAllDescriptors)?,
        &mut count,
    )?;
    if index >= i64::from(u16::from_le_bytes(count)) {
        return Err(format!("the map {:#x} has more descriptors than its array", map.0).into());
    }
    let slot = index * reader.constant(Constant::PropDescSize)? + reader.constant(entry)?;
    reader.word(
        reader.field_address(descriptors, Field::DescriptorArrayDescriptors)?
            + slot.cast_unsigned() * 8,
    )
}

/// The value of the field `index` of `holder`, whose map is `map`: in the
/// object for the first of them, then in its property array.
fn field_value(
    reader: &Reader<'_>,
    holder: Object,
    map: Object,
    index: i64,
) -> Result<u64, Problem> {
    let start = i64::from(reader.byte_field(
        map,
        Field::MapInobjectPropertiesStartOrConstructorFunctionIndex,
    )?);
    let size = i64::from(reader.byte_field(map, Field::MapInstanceSizeInWords)?);
    let inobject = size - start;
    if index < inobject {
        let tag = reader.constant(Constant::HeapObjectTag)?.cast_unsigned();
        return reader.word(holder.0 - tag + (start + index).cast_unsigned() * 8);
    }

    let properties = reader.typed_field(
        holder,
        Field::JSReceiverPropertiesOrHash,
        Constant::PropertyArrayType,
    )?;
    let bits = reader.constant(Constant::PropertyArrayLengthFieldSize)?;
    let length =
        reader.smi_field(properties, Field::PropertyArrayLengthAndHash)? & ((1 << bits) - 1);
    let index = index - inobject;
    if index >= length {
        return Err(format!("field {index} is past the properties of {:#x}", holder.0).into());
    }
    reader.word(
        reader.field_address(properties, Field::PropertyArrayObjects)? + index.cast_unsigned() * 8,
    )
}

/// The property `name` of `holder`, a dictionary-mode object.
fn dictionary(reader: &Reader<'_>, holder: Object, name: &str) -> Result<Own, Problem> {
    let word = reader.field(holder, Field::JSReceiverPropertiesOrHash)?;
    let table = reader
        .object(word)?
        .ok_or_else(|| format!("the properties of {:#x} are no object", holder.0))?;
    if !reader.is(table, Constant::NameDictionaryType)? {
        return Err(format!(
            "the properties of {:#x} are kept in a dictionary of instance type {}",
            holder.0,
            reader.instance_type(table)?
        )
        .into());
    }

    let capacity = reader
        .smi(reader.element(table, reader.constant(Constant::HashTableCapacityIndex)?)?)?
        .filter(|capacity| (0..=MAX_ENTRIES).contains(capacity))
        .ok_or_else(|| format!("the dictionary {:#x} has no capacity", table.0))?;
    let first = reader.constant(Constant::HashTablePrefixStartIndex)?
        + reader.constant(Constant::NameDictionaryShapePrefixSize)?;
    let size = reader.constant(Constant::NameDictionaryShapeEntrySize)?;
    for entry in 0..capacity {
        let at = first + entry * size;
        let key = reader.element(
            table,
            at + reader.constant(Constant::HashTableEntryKeyIndex)?,
        )?;
        if !is_named(reader, key, name)? {
            continue;
        }
        let details = reader.element(
            table,
            at + reader.constant(Constant::NameDictionaryEntryDetailsIndex)?,
        )?;
        let details = reader
            .smi(details)?
            .ok_or("a property's details are no Smi")?;
        if part(reader, details, Constant::PropKindMask, None)?
            == reader.constant(Constant::PropKindAccessor)?
        {
            return Ok(Own::Accessor);
        }
        return reader
            .element(
                table,
                at + reader.constant(Constant::NameDictionaryEntryValueIndex)?,
            )
            .map(Own::Data);
    }
    Ok(Own::Absent)
}

/// Whether the key `key` is the string `name`. A key is an internalized
/// string or a symbol; an empty or deleted entry's is an oddball.
pub(super) fn is_named(reader: &Reader<'_>, key: u64, name: &str) -> Result<bool, Problem> {
    let Some(key) = reader.object(key)? else {
        return Ok(false);
    };
    if reader.instance_type(key)? >= reader.constant(Constant::FirstNonstringType)? {
        return Ok(false);
    }
    let length = reader.int32_field(key, Field::StringLength)?;
    if usize::try_from(length).ok() != Some(name.encode_utf16().count()) {
        return Ok(false);
    }
    Ok(reader.string(key)? == name)
}
