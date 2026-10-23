//! All of an object's own properties as V8 stores them: elements by index,
//! and named properties in JavaScript enumeration order
//! (`OrdinaryOwnPropertyKeys`): strings, then symbols, each in insertion
//! order. A fast map keeps that order in its descriptors; a dictionary
//! stores each property's enumeration index in its details. Private
//! symbols, which no program can enumerate, are left out.

use super::objects::{Object, Problem, Reader};
use super::properties::{descriptor, field_value, part};
use crate::image::v8::{Constant, Field};

/// The most dictionary entries or backing store elements read from one
/// object.
pub(super) const MAX_ENTRIES: i64 = 1 << 20;

/// What an own property holds, as stored on its holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Held {
    /// A tagged value.
    Data(u64),

    /// An element of a double array, by its bits and address; never the
    /// hole.
    Double { bits: u64, address: u64 },

    /// An accessor: an `AccessorPair` with the program's getter and setter,
    /// or an `AccessorInfo`, a V8 native accessor such as a function's
    /// `name`, which JavaScript sees as a data property.
    Accessor(Object),
}

/// A named own property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Named {
    /// The key: an internalized string or a symbol.
    pub(super) key: Object,
    pub(super) symbol: bool,

    /// A private symbol, which no program can enumerate, such as V8's
    /// `error_stack_symbol`.
    pub(super) private: bool,
    pub(super) enumerable: bool,
    pub(super) held: Held,
}

/// An element: an own property whose key is an array index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Element {
    pub(super) index: u32,
    pub(super) enumerable: bool,
    pub(super) held: Held,
}

/// How an object's map says its elements are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Store {
    /// Tagged values in a fixed array, with the hole for missing ones.
    Tagged,
    /// Doubles in a fixed double array, with the hole NaN for missing ones.
    Doubles,
    /// A number dictionary, keyed by index.
    Dictionary,
    /// A `String` wrapper: its characters, then tagged values after them in
    /// a fixed array or a dictionary.
    StringWrapper { dictionary: bool },
    /// A typed array, read as numbers of its element kind.
    Typed,
    /// Sloppy-mode `arguments`, whose elements alias the frame's.
    SloppyArguments,
}

/// The elements kind in `map`'s bit field 2.
pub(super) fn elements_kind(reader: &Reader<'_>, map: Object) -> Result<i64, Problem> {
    let bits = i64::from(reader.byte_field(map, Field::MapBitField2)?);
    part(
        reader,
        bits,
        Constant::MapElementsKindMask,
        Some(Constant::MapElementsKindShift),
    )
}

/// How an object of elements kind `kind` keeps its elements.
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

/// The elements of `holder` below index `below`, in index order, from its
/// backing store (for an array, only those below `length`). Refuses typed
/// arrays, sloppy `arguments`, and unknown kinds. For a `String` wrapper,
/// returns the elements after its characters.
pub(super) fn elements(
    reader: &Reader<'_>,
    holder: Object,
    length: Option<u32>,
    below: u32,
) -> Result<Vec<Element>, Problem> {
    let map = reader.map(holder)?;
    let store = store(reader, elements_kind(reader, map)?)?;
    let backing = || -> Result<Object, Problem> {
        let word = reader.field(holder, Field::JSObjectElements)?;
        reader
            .object(word)?
            .ok_or_else(|| format!("the elements of {:#x} are not an object", holder.0).into())
    };
    let below = length.map_or(below, |length| length.min(below));
    match store {
        Store::Tagged | Store::StringWrapper { dictionary: false } => {
            fast_elements(reader, backing()?, below)
        }
        Store::Doubles => double_elements(reader, backing()?, below),
        Store::Dictionary | Store::StringWrapper { dictionary: true } => {
            let mut found = dictionary_elements(reader, backing()?)?;
            found.retain(|element| element.index < below);
            Ok(found)
        }
        Store::Typed => Err("a typed array's elements are its numbers".into()),
        Store::SloppyArguments => {
            Err("sloppy-mode arguments alias the frame's variables, and microscope does not follow them".into())
        }
    }
}

/// The present elements of a fixed array below `below`.
fn fast_elements(reader: &Reader<'_>, array: Object, below: u32) -> Result<Vec<Element>, Problem> {
    let ty = reader.instance_type(array)?;
    if ty != reader.constant(Constant::FixedArrayType)? {
        // An object with no elements has the empty fixed array, or the
        // empty fixed double array if its kind is double.
        return Err(format!("the elements {:#x} are not a fixed array", array.0).into());
    }
    let capacity = reader.smi_field(array, Field::FixedArrayBaseLength)?;
    let count = capacity.min(i64::from(below)).clamp(0, MAX_ENTRIES);
    let words = words(
        reader,
        reader.field_address(array, Field::FixedArrayObjects)?,
        count,
    )?;
    let mut found = Vec::new();
    for (index, word) in (0_u32..).zip(words) {
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

/// The present elements of a fixed double array below `below`.
fn double_elements(
    reader: &Reader<'_>,
    array: Object,
    below: u32,
) -> Result<Vec<Element>, Problem> {
    // An empty double array is the empty fixed array.
    if reader.is(array, Constant::FixedArrayType)?
        && reader.smi_field(array, Field::FixedArrayBaseLength)? == 0
    {
        return Ok(Vec::new());
    }
    if !reader.is(array, Constant::FixedDoubleArrayType)? {
        return Err(format!("the elements {:#x} are not a fixed double array", array.0).into());
    }
    let capacity = reader.smi_field(array, Field::FixedArrayBaseLength)?;
    let count = capacity.min(i64::from(below)).clamp(0, MAX_ENTRIES);
    let hole = reader.constant(Constant::HoleNanInt64)?.cast_unsigned();
    let base = reader.field_address(array, Field::FixedDoubleArrayValues)?;
    let words = words(reader, base, count)?;
    Ok((0_u32..)
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

/// The elements in a number dictionary, in index order.
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

/// The index in a number dictionary key, or `None` for an empty or deleted
/// entry.
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
        // Undefined for an empty entry, or the hole for a deleted one.
        return Ok(None);
    }
    let number = f64::from_bits(reader.word(reader.field_address(key, Field::HeapNumberValue)?)?);
    // An index too large for a Smi is stored as a number below 2^32 - 1.
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

/// The named own properties of `holder`: strings first, then symbols, each
/// in insertion order.
pub(super) fn named(reader: &Reader<'_>, holder: Object) -> Result<Vec<Named>, Problem> {
    let mut found = all_named(reader, holder)?;
    found.retain(|property| !property.private);
    Ok(found)
}

/// Like [`named`], but includes properties keyed by private symbols.
pub(super) fn all_named(reader: &Reader<'_>, holder: Object) -> Result<Vec<Named>, Problem> {
    let map = reader.map(holder)?;
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
    // A stable sort keeps each kind's own order.
    found.sort_by_key(|property| property.symbol);
    Ok(found)
}

/// The properties in a fast map's own descriptors, in descriptor order.
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

/// The properties in a dictionary-mode object's name dictionary, in
/// enumeration index order.
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

/// The index of the first element of each entry in the hash table `table`,
/// whose prefix is `prefix` words and whose entries are `size` words.
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

/// A property key, whether it is a symbol, and whether it is a private
/// one; `None` for an empty or deleted entry.
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
    // undefined or the hole.
    Ok(None)
}

/// What a property whose details are `details` holds, from `value`.
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

/// Whether a property whose details are `details` is enumerable.
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

/// Whether `word` is one of V8's holes, which mark a missing value.
pub(super) fn is_hole(reader: &Reader<'_>, word: u64) -> Result<bool, Problem> {
    reader
        .object(word)?
        .map_or(Ok(false), |object| reader.is(object, Constant::HoleType))
}

/// The `count` words at `address`, in one read.
pub(super) fn words(reader: &Reader<'_>, address: u64, count: i64) -> Result<Vec<u64>, Problem> {
    let count = usize::try_from(count).map_err(|_| "a negative count of words")?;
    let mut bytes = vec![0; count * 8];
    reader.bytes_into(address, &mut bytes)?;
    Ok(bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| u64::from_le_bytes(*word))
        .collect())
}

/// The properties in a global object's global dictionary, in enumeration
/// index order. Each is a `PropertyCell` with its name, value, and details;
/// a deleted property's cell holds the hole.
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
