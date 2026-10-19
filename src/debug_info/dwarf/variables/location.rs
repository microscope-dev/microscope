//! Location descriptions and DWARF expressions copied out of the debug sections.

use std::borrow::Cow;
use std::sync::Arc;

use gimli::Reader as _;
use rayon::prelude::*;

use crate::debug_info::dwarf::{DieWalk, DwarfError, Reader, Units, unit_dwarf};
use crate::{AddressRange, ImageAddress, VariableUnavailableReason};

use super::die::{ByteSize, base_type_encoding, byte_size_attribute};
use super::{ConstantValue, Metadata, MetadataAbsence, ValueDescription};

/// How many procedures one expression may call, directly or not.
const MAX_PROCEDURES: usize = 64;

pub(super) use crate::image::locations::{
    EvaluationUnit, Expression, LocationList, LocationListId, LocationTables, LocationsBuilder,
};

/// Operation families that determine where an object's storage lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ExpressionUse {
    ThreadLocal,
    Dereference,
    Frame,
    RegisterValue,
    Computed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LocationSelectionError {
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
}

/// The expression `list` holds at `address`: the one entry whose range
/// holds it, which overrides a default (range-less) entry; without an
/// address only a default can apply.
pub(super) fn select(
    list: LocationList<'_>,
    address: Option<ImageAddress>,
) -> std::result::Result<Option<Expression<'_>>, LocationSelectionError> {
    if let Some(address) = address {
        let mut specific = list
            .entries()
            .filter(|(range, _)| range.is_some_and(|range| range.contains(address)));

        if let Some((_, expression)) = specific.next() {
            if specific.next().is_some() {
                return Err(LocationSelectionError::Unavailable(
                    crate::UnsupportedVariableFeature::AlternativeLocations.into(),
                ));
            }

            return Ok(Some(expression));
        }
    }

    let mut defaults = list.entries().filter(|(range, _)| range.is_none());
    let expression = defaults.next().map(|(_, expression)| expression);
    if defaults.next().is_some() {
        return Err(LocationSelectionError::Malformed(
            "multiple default locations were supplied".into(),
        ));
    }

    if address.is_none() && expression.is_none() && !list.is_empty() {
        return Err(LocationSelectionError::Unavailable(
            VariableUnavailableReason::NoInstructionContext,
        ));
    }

    Ok(expression)
}

fn too_large(_: crate::image::locations::TooLarge) -> DwarfError {
    DwarfError::MalformedVariable("the module's locations do not fit an image".into())
}

pub(super) fn copy_optional_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    pool: &mut LocationsBuilder,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    value: Option<gimli::AttributeValue<Reader<'_>>>,
    absence: MetadataAbsence,
) -> Metadata<LocationListId> {
    let Some(value) = value else {
        return Metadata::Absent(absence);
    };

    match copy_location(dwarf, pool, unit_index, unit, value) {
        Ok(location) => Metadata::Value(location),
        Err(error) => Metadata::Malformed(error.to_string().into()),
    }
}

pub(super) fn copy_data_object_value(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    pool: &mut LocationsBuilder,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> Metadata<ValueDescription> {
    if let Some(location) = entry.attr_value(gimli::DW_AT_location) {
        return match copy_location(dwarf, pool, unit_index, unit, location) {
            Ok(location) => Metadata::Value(ValueDescription::Location(location)),
            Err(error) => Metadata::Malformed(error.to_string().into()),
        };
    }

    if let Some(value) = entry.attr_value(gimli::DW_AT_const_value) {
        return match copy_constant(value) {
            Ok(value) => Metadata::Value(ValueDescription::Constant(Box::new(value))),
            Err(error) => Metadata::Malformed(error),
        };
    }

    Metadata::Absent(MetadataAbsence::NoLocation)
}

pub(super) fn copy_data_object_value_with_origins(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    pool: &mut LocationsBuilder,
    units: &Units<'_>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'_>>)],
) -> Metadata<ValueDescription> {
    let value = copy_data_object_value(dwarf, pool, unit_index, unit, entry);
    if !matches!(value, Metadata::Absent(_)) {
        return value;
    }

    for (origin_unit, origin) in chain {
        let value = copy_data_object_value(dwarf, pool, *origin_unit, &units[*origin_unit], origin);
        if !matches!(value, Metadata::Absent(_)) {
            return value;
        }
    }

    Metadata::Absent(MetadataAbsence::NoLocation)
}

fn copy_constant(
    value: gimli::AttributeValue<Reader<'_>>,
) -> std::result::Result<ConstantValue, Arc<str>> {
    Ok(match value {
        // Fixed-width forms carry raw bits; signedness comes from the type.
        gimli::AttributeValue::Data1(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data2(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data4(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data8(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data16(value) => ConstantValue::Fixed(value),
        gimli::AttributeValue::Udata(value) => ConstantValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Sdata(value) => ConstantValue::Signed(i128::from(value)),
        gimli::AttributeValue::Block(value) => ConstantValue::Bytes(Arc::from(
            value
                .to_slice()
                .map_err(|error| Arc::from(error.to_string()))?
                .into_owned(),
        )),
        _ => return Err("unsupported DW_AT_const_value form".into()),
    })
}

fn copy_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    pool: &mut LocationsBuilder,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    value: gimli::AttributeValue<Reader<'_>>,
) -> std::result::Result<LocationListId, DwarfError> {
    copy_location_with(dwarf, pool, unit_index, unit, value, copy_expression)
}

type CopyExpression = fn(
    &gimli::Dwarf<Reader<'_>>,
    &mut LocationsBuilder,
    usize,
    &gimli::Unit<Reader<'_>>,
    gimli::Expression<Reader<'_>>,
) -> std::result::Result<crate::image::locations::ExpressionId, DwarfError>;

fn copy_location_with(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    pool: &mut LocationsBuilder,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    value: gimli::AttributeValue<Reader<'_>>,
    copy_expression: CopyExpression,
) -> std::result::Result<LocationListId, DwarfError> {
    // DWARF 2 and 3 encode a single expression as a block.
    if let Some(expression) = value.exprloc_value() {
        let expression = copy_expression(dwarf, pool, unit_index, unit, expression)?;
        return pool.list(&[(None, expression)]).map_err(too_large);
    }

    let mut locations = dwarf
        .attr_locations(unit, value)?
        .ok_or(DwarfError::UnsupportedReferenceForm)?;
    let mut entries = Vec::new();

    // Iterate raw entries so DW_LLE_default_location keeps its fallback
    // semantics (range: None) instead of becoming a 0..u64::MAX range that
    // conflicts with every specific entry.
    while let Some(raw) = locations.next_raw()? {
        let is_default = matches!(&raw, gimli::RawLocListEntry::DefaultLocation { .. });
        let Some(location) = locations.convert_raw(raw)? else {
            continue;
        };

        if is_default {
            entries.push((
                None,
                copy_expression(dwarf, pool, unit_index, unit, location.data)?,
            ));
            continue;
        }

        if location.range.begin > location.range.end {
            return Err(DwarfError::InvalidRange);
        }

        if location.range.begin < location.range.end {
            entries.push((
                Some(AddressRange {
                    start: ImageAddress::new(location.range.begin),
                    end: ImageAddress::new(location.range.end),
                }),
                copy_expression(dwarf, pool, unit_index, unit, location.data)?,
            ));
        }
    }

    pool.list(&entries).map_err(too_large)
}

/// Copies an expression with the procedures it calls, which must be in its
/// own unit. Their indexed addresses are its own too: they run on its
/// evaluation, which resolves them from its table, and all are in one
/// unit, whose table they share.
pub(super) fn copy_expression(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    pool: &mut LocationsBuilder,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    expression: gimli::Expression<Reader<'_>>,
) -> std::result::Result<crate::image::locations::ExpressionId, DwarfError> {
    let encoding = unit.encoding();
    let mut addresses = indexed_addresses(dwarf, unit, &expression)?;
    let bytes: Cow<'_, [u8]> = expression.0.to_slice()?;
    let mut pending = calls(unit, &bytes, encoding, expression.0.endian())?;
    let mut procedures = Vec::<(u64, Option<LocationListId>)>::new();

    while let Some(offset) = pending.pop() {
        if procedures.iter().any(|(known, _)| *known == offset) {
            continue;
        }

        if procedures.len() == MAX_PROCEDURES {
            return Err(DwarfError::MalformedVariable(
                "an expression calls too many procedures".into(),
            ));
        }

        // A procedure elsewhere stays missing, and running it unsupported.
        let Some(entry_offset) =
            gimli::DebugInfoOffset(usize::try_from(offset).map_err(|_| DwarfError::InvalidRange)?)
                .to_unit_offset(&unit.header)
        else {
            continue;
        };

        let entry = unit.entry(entry_offset)?;
        let location = entry
            .attr_value(gimli::DW_AT_location)
            .map(|value| copy_location_with(dwarf, pool, unit_index, unit, value, copy_operations))
            .transpose()?;

        if let Some(location) = location {
            let tables = pool.tables();
            for (_, called) in tables.list(location).entries() {
                addresses.extend(called.addresses());
                pending.extend(calls(
                    unit,
                    called.bytes(),
                    encoding,
                    expression.0.endian(),
                )?);
            }
        }

        procedures.push((offset, location));
    }

    pool.expression(
        &bytes,
        u32::try_from(unit_index).map_err(|_| DwarfError::InvalidRange)?,
        encoding,
        &addresses,
        &procedures,
    )
    .map_err(too_large)
}

/// Copies an expression's operations without the procedures it calls.
fn copy_operations(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    pool: &mut LocationsBuilder,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    expression: gimli::Expression<Reader<'_>>,
) -> std::result::Result<crate::image::locations::ExpressionId, DwarfError> {
    let addresses = indexed_addresses(dwarf, unit, &expression)?;
    let bytes: Cow<'_, [u8]> = expression.0.to_slice()?;
    pool.expression(
        &bytes,
        u32::try_from(unit_index).map_err(|_| DwarfError::InvalidRange)?,
        unit.encoding(),
        &addresses,
        &[],
    )
    .map_err(too_large)
}

/// The addresses `.debug_addr` gives the indexes an expression names.
fn indexed_addresses(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    expression: &gimli::Expression<Reader<'_>>,
) -> std::result::Result<Vec<(u64, u64)>, DwarfError> {
    let mut addresses = Vec::new();
    let mut operations = expression.operations(unit.encoding());

    while let Some(operation) = operations.next()? {
        let (gimli::Operation::AddressIndex { index } | gimli::Operation::ConstantIndex { index }) =
            operation
        else {
            continue;
        };

        let address = unit_dwarf(dwarf, unit).address(unit, index)?;
        addresses.push((index.0 as u64, address));
    }

    Ok(addresses)
}

/// The `.debug_info` offsets of the entries an expression calls.
fn calls(
    unit: &gimli::Unit<Reader<'_>>,
    bytes: &[u8],
    encoding: gimli::Encoding,
    endian: gimli::RunTimeEndian,
) -> std::result::Result<Vec<u64>, DwarfError> {
    let reader = gimli::EndianSlice::new(bytes, endian);
    let mut operations = gimli::Expression(reader).operations(encoding);
    let mut called = Vec::new();

    while let Some(operation) = operations.next()? {
        let gimli::Operation::Call { offset } = operation else {
            continue;
        };

        let offset = match offset {
            gimli::DieReference::UnitRef(offset) => offset.to_debug_info_offset(&unit.header),
            gimli::DieReference::DebugInfoRef(offset) => Some(offset),
        };

        if let Some(offset) = offset {
            called.push(offset.0 as u64);
        }
    }

    Ok(called)
}

/// Records each unit's base types, which typed operations name, in unit
/// order. Each unit is read on its own in parallel, and recorded in order.
pub(super) fn load_evaluation_units(
    units: &Units<'_>,
    pool: &mut LocationsBuilder,
) -> std::result::Result<(), DwarfError> {
    let recorded = (0..units.len())
        .into_par_iter()
        .map(|index| evaluation_unit(units, index))
        .collect::<Vec<_>>();

    // The first unit to fail fails the load, as when they were read in turn.
    for recorded in recorded {
        pool.unit(&recorded?).map_err(too_large)?;
    }

    Ok(())
}

/// What unit `index` records for evaluation: its offset, its language,
/// and its base types.
fn evaluation_unit(
    units: &Units<'_>,
    index: usize,
) -> std::result::Result<EvaluationUnit, DwarfError> {
    let unit = &units[index];
    let mut recorded = EvaluationUnit {
        // The supplementary file's units lie in another section, which
        // the file's expressions never name.
        offset: unit
            .header
            .debug_info_offset()
            .filter(|_| !units.is_supplementary(index))
            .map(|offset| offset.0 as u64),
        language: units.inherited_language(index),
        ..EvaluationUnit::default()
    };

    let mut walk = DieWalk::new(unit)?;

    while let Some(die) = walk.next()? {
        if !matches!(
            die.tag,
            gimli::DW_TAG_compile_unit | gimli::DW_TAG_base_type
        ) {
            continue;
        }

        let entry = walk.decode()?;

        if entry.tag() == gimli::DW_TAG_compile_unit {
            if let Some(gimli::AttributeValue::Language(value)) =
                entry.attr_value(gimli::DW_AT_language)
            {
                recorded.language = Some(value);
            }

            continue;
        }

        let ByteSize::Constant(byte_size) = byte_size_attribute(entry) else {
            continue;
        };

        let Ok(raw_encoding) = base_type_encoding(entry) else {
            continue;
        };

        let encoding = gimli::DwAte(raw_encoding);

        if let Some(value_type) = dwarf_value_type(encoding, byte_size) {
            recorded
                .base_types
                .push((entry.offset().0 as u64, value_type));
        }
    }

    Ok(recorded)
}

const fn dwarf_value_type(encoding: gimli::DwAte, byte_size: u64) -> Option<gimli::ValueType> {
    use gimli::ValueType::{F32, F64, I8, I16, I32, I64, U8, U16, U32, U64};
    let signed = matches!(encoding, gimli::DW_ATE_signed | gimli::DW_ATE_signed_char);
    let unsigned = matches!(
        encoding,
        gimli::DW_ATE_boolean | gimli::DW_ATE_unsigned | gimli::DW_ATE_unsigned_char
    );
    Some(match (encoding, signed, unsigned, byte_size) {
        (gimli::DW_ATE_float, _, _, 4) => F32,
        (gimli::DW_ATE_float, _, _, 8) => F64,
        (_, true, _, 1) => I8,
        (_, true, _, 2) => I16,
        (_, true, _, 4) => I32,
        (_, true, _, 8) => I64,
        (_, _, true, 1) => U8,
        (_, _, true, 2) => U16,
        (_, _, true, 4) => U32,
        (_, _, true, 8) => U64,
        _ => return None,
    })
}
