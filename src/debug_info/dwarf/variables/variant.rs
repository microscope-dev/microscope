//! Variant parts: discriminant lists and selecting the active variant.

use std::sync::Arc;

use gimli::Reader as _;

use crate::debug_info::dwarf::Reader;
use crate::{
    BaseType, BaseTypeEncoding, ByteOrder, IntegerValue, Variant, VariantSelection, VariantSelector,
};

use super::MAX_VARIANT_METADATA;
use super::codec::{
    checked_integer_value, compare_integer_values, enumeration_constant, read_sleb128_i128,
    read_uleb128_u128,
};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum VariantMetadataError {
    Malformed(Arc<str>),
    Limit,
}

impl From<Arc<str>> for VariantMetadataError {
    fn from(reason: Arc<str>) -> Self {
        Self::Malformed(reason)
    }
}

#[derive(Default)]
pub(super) struct VariantMetadataBudget {
    pub(super) items: usize,
}

impl VariantMetadataBudget {
    pub(super) const fn consume(&mut self) -> std::result::Result<(), VariantMetadataError> {
        if self.items >= MAX_VARIANT_METADATA {
            return Err(VariantMetadataError::Limit);
        }

        self.items += 1;
        Ok(())
    }
}

fn read_discriminant_leb128(
    bytes: &[u8],
    cursor: &mut usize,
    base: &BaseType,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let value = if matches!(
        base.encoding,
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
    ) {
        IntegerValue::Signed(read_sleb128_i128(bytes, cursor)?)
    } else {
        IntegerValue::Unsigned(read_uleb128_u128(bytes, cursor)?)
    };

    checked_integer_value(value, base)
}

pub(super) fn copy_variant_selection(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    representation: &BaseType,
    byte_order: ByteOrder,
    budget: &mut VariantMetadataBudget,
) -> std::result::Result<VariantSelection, VariantMetadataError> {
    let exact = entry.attr_value(gimli::DW_AT_discr_value);
    let list = entry.attr_value(gimli::DW_AT_discr_list);
    if exact.is_some() && list.is_some() {
        return Err(VariantMetadataError::Malformed(
            "variant has both DW_AT_discr_value and DW_AT_discr_list".into(),
        ));
    }

    if let Some(value) = exact {
        budget.consume()?;
        return enumeration_constant(value, representation, byte_order)
            .map(|value| VariantSelection::Selectors(Arc::from([VariantSelector::Value(value)])))
            .map_err(VariantMetadataError::Malformed);
    }

    let Some(list) = list else {
        return Ok(VariantSelection::Default);
    };

    let gimli::AttributeValue::Block(list) = list else {
        return Err(VariantMetadataError::Malformed(
            "DW_AT_discr_list does not use a block form".into(),
        ));
    };

    let bytes = list
        .to_slice()
        .map_err(|error| VariantMetadataError::Malformed(error.to_string().into()))?;
    parse_discriminant_list(bytes.as_ref(), representation, budget)
}

pub(super) fn parse_discriminant_list(
    bytes: &[u8],
    representation: &BaseType,
    budget: &mut VariantMetadataBudget,
) -> std::result::Result<VariantSelection, VariantMetadataError> {
    if bytes.is_empty() {
        return Err(VariantMetadataError::Malformed(
            "DW_AT_discr_list is empty".into(),
        ));
    }

    let mut cursor = 0_usize;
    let mut selectors = Vec::new();
    while cursor < bytes.len() {
        budget.consume()?;
        let descriptor = bytes[cursor];
        cursor += 1;
        let low = read_discriminant_leb128(bytes, &mut cursor, representation)?;
        let selector = match descriptor {
            value if value == gimli::DW_DSC_label.0 => VariantSelector::Value(low),
            value if value == gimli::DW_DSC_range.0 => {
                let high = read_discriminant_leb128(bytes, &mut cursor, representation)?;
                VariantSelector::Range { low, high }
            }
            _ => {
                return Err(VariantMetadataError::Malformed(
                    format!("DW_AT_discr_list has unknown descriptor {descriptor:#x}").into(),
                ));
            }
        };

        selectors.push(selector);
    }

    Ok(VariantSelection::Selectors(selectors.into()))
}

fn variant_selection_matches(
    selection: &VariantSelection,
    value: IntegerValue,
) -> std::result::Result<bool, Arc<str>> {
    let VariantSelection::Selectors(selectors) = selection else {
        return Ok(false);
    };

    for selector in selectors.iter() {
        let matches = match *selector {
            VariantSelector::Value(expected) => expected == value,
            VariantSelector::Range { low, high } => {
                !compare_integer_values(value, low)?.is_lt()
                    && !compare_integer_values(value, high)?.is_gt()
            }
        };

        if matches {
            return Ok(true);
        }
    }

    Ok(false)
}

pub(super) fn selected_variant_index(
    variants: &[Variant],
    value: IntegerValue,
) -> std::result::Result<Option<usize>, Arc<str>> {
    let mut selected = None;
    let mut default = None;
    for (index, variant) in variants.iter().enumerate() {
        match &variant.selection {
            VariantSelection::Default => default = Some(index),
            VariantSelection::Selectors(_) => {
                if variant_selection_matches(&variant.selection, value)? {
                    selected = Some(index);
                }
            }
        }
    }

    Ok(selected.or(default))
}

pub(super) fn validate_variant_selections(
    variants: &[Variant],
) -> std::result::Result<(), Arc<str>> {
    let mut default_count = 0_usize;
    let mut ranges = Vec::new();
    for (variant_index, variant) in variants.iter().enumerate() {
        match &variant.selection {
            VariantSelection::Default => default_count += 1,
            VariantSelection::Selectors(selectors) => {
                if selectors.is_empty() {
                    return Err("explicit variant selector list is empty".into());
                }

                for selector in selectors.iter() {
                    let (low, high) = match *selector {
                        VariantSelector::Value(value) => (value, value),
                        VariantSelector::Range { low, high } => (low, high),
                    };

                    if compare_integer_values(low, high)?.is_gt() {
                        return Err("variant discriminator range is reversed".into());
                    }

                    ranges.push((low, high, variant_index));
                }
            }
        }
    }

    if default_count > 1 {
        return Err("variant part has multiple default variants".into());
    }

    ranges.sort_by(|left, right| {
        compare_integer_values(left.0, right.0).unwrap_or(std::cmp::Ordering::Equal)
    });

    for pair in ranges.windows(2) {
        if !compare_integer_values(pair[0].1, pair[1].0)?.is_lt() {
            return Err("variant discriminator selectors overlap".into());
        }
    }

    Ok(())
}

/// Whether a variant part has exactly one variant, selected by default.
pub(super) const fn is_single_default_variant(variants: &[Variant]) -> bool {
    matches!(
        variants,
        [Variant {
            selection: VariantSelection::Default,
            ..
        }]
    )
}
