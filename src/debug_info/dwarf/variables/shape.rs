//! Value shapes: how a type's bytes are decoded and which children it has.

use std::sync::Arc;

use foldhash::{HashSet, HashSetExt};

use crate::model::{ArrayDimension, ArrayOrdering, RuntimeDimension};
use crate::{
    BaseClass, BaseType, EnumerationOrigin, Enumerator, RecordMember, SliceWords, TypeId, TypeInfo,
    TypeKind, TypeModifier, TypeReference, Variant, VariantDiscriminant, VariantSelection,
};

use super::codec::integer_bit_width;
use super::types::{TypeEntries, type_info_from};
use super::variant::is_single_default_variant;
use super::{MAX_SCALAR_BYTES, MAX_TYPE_RESOLUTION_DEPTH};

#[derive(Clone, Debug)]
pub(super) enum ValueShape {
    Scalar(BaseType),
    Enumeration {
        representation: BaseType,
        enumerators: Arc<[Enumerator]>,
        byte_size: u64,

        /// A language's enumeration, or constants Go gave a named type,
        /// which make only the values they name symbolic.
        origin: EnumerationOrigin,
    },
    Array {
        element: TypeId,
        dimensions: Arc<[ArrayDimension]>,
        ordering: ArrayOrdering,
        byte_size: u64,
    },

    /// An array bounded at run time, which a value resolves to an
    /// [`ValueShape::Array`] where its elements are.
    RuntimeArray {
        /// The canonical array type, whose expressions find its bounds.
        array: TypeId,
        element: TypeId,
        element_size: u64,
        dimensions: Arc<[RuntimeDimension]>,
        ordering: ArrayOrdering,

        /// The size of the descriptor, as an Ada array's, or zero when the
        /// producer gives none.
        byte_size: u64,
    },
    Slice {
        element: TypeId,
        byte_size: u64,
        words: SliceWords,
        text: bool,
    },
    Record {
        /// The canonical record DIE after aliases and qualifiers are removed.
        record: TypeId,
        members: Arc<[RecordMember]>,
        bases: Arc<[BaseClass]>,
        byte_size: u64,
    },
    Union {
        /// The canonical union DIE after aliases and qualifiers are removed.
        union: TypeId,
        members: Arc<[RecordMember]>,
        byte_size: u64,
    },
    Variant {
        /// The canonical aggregate DIE after aliases and qualifiers are removed.
        aggregate: TypeId,
        common_members: Arc<[RecordMember]>,
        bases: Arc<[BaseClass]>,
        discriminant: VariantDiscriminant,
        variants: Arc<[Variant]>,
        byte_size: u64,
    },
    Indirection {
        target: Option<TypeId>,
        byte_size: u64,
        address_class: u64,
    },

    /// A function value: null, or a pointer to its closure context.
    Function {
        byte_size: u64,
    },
}

/// Strips the aliases and qualifiers around `id` that share their target's
/// representation, returning the type beneath them.
pub(super) fn transparent_type_from(
    types: &(impl TypeEntries + ?Sized),
    id: TypeId,
) -> std::result::Result<(TypeId, &TypeInfo), ValueShapeError> {
    let mut current = id;
    let mut visited = HashSet::new();

    loop {
        if !visited.insert(current) {
            return Err(ValueShapeError::Malformed("type wrapper cycle".into()));
        }

        let info = type_info_from(types, current).map_err(ValueShapeError::Malformed)?;
        match info.kind {
            TypeKind::Modified { target, .. }
            | TypeKind::Named {
                target: Some(target),
                ..
            } => {
                transparent_representation(types, info, target)?;
                current = target.id;
            }
            TypeKind::Named { target: None, .. } => {
                return Err(ValueShapeError::Unsupported(
                    "incomplete named type has no representation target".into(),
                ));
            }
            _ => return Ok((current, info)),
        }
    }
}

fn transparent_representation(
    types: &(impl TypeEntries + ?Sized),
    wrapper: &TypeInfo,
    target: TypeReference,
) -> std::result::Result<(), ValueShapeError> {
    let target = type_info_from(types, target.id).map_err(ValueShapeError::Malformed)?;

    if matches!(
        wrapper.kind,
        TypeKind::Modified {
            modifier: TypeModifier::Shared,
            ..
        }
    ) {
        return Err(ValueShapeError::Unsupported(
            "shared-qualified values require UPC distributed-memory semantics".into(),
        ));
    }

    if let (Some(wrapper_size), Some(target_size)) = (wrapper.byte_size, target.byte_size)
        && wrapper_size != target_size
    {
        return Err(ValueShapeError::Unsupported(
            format!(
                "transparent type wrapper size {wrapper_size} differs from target size {target_size}"
            )
            .into(),
        ));
    }

    Ok(())
}

/// Why a type has no value shape.
#[derive(Debug)]
pub(super) enum ValueShapeError {
    /// The type graph is defective.
    Malformed(Arc<str>),

    /// The type is valid but its shape is not implemented.
    Unsupported(Arc<str>),
}

/// The widest indirection representation `decode_address` can turn into a
/// `VirtualAddress`.
pub(super) const MAX_ADDRESS_BYTES: u64 = 8;

/// Resolves the storage size of a pointer or reference value. The type
/// builder leaves the size unset only for a non-default address class
/// without `DW_AT_byte_size`, which is unsupported rather than malformed.
pub(super) fn indirection_byte_size(
    byte_size: Option<u64>,
    address_class: u64,
    kind: &str,
) -> std::result::Result<u64, ValueShapeError> {
    match byte_size {
        Some(0) => Err(ValueShapeError::Malformed(
            format!("{kind} type has a zero byte size").into(),
        )),
        Some(byte_size) if byte_size > MAX_ADDRESS_BYTES => Err(ValueShapeError::Unsupported(
            format!(
                "{kind} type occupies {byte_size} bytes; addresses wider than \
                 {MAX_ADDRESS_BYTES} bytes are unsupported"
            )
            .into(),
        )),
        Some(byte_size) => Ok(byte_size),
        None if address_class != 0 => Err(ValueShapeError::Unsupported(
            format!("{kind} representation for address class {address_class} is unsupported")
                .into(),
        )),
        None => Err(ValueShapeError::Malformed(
            format!("{kind} type has no byte size").into(),
        )),
    }
}

/// The variant the sum `aggregate`, which stores no tag, holds: its one
/// variant, or the one variant that can hold a value at all, since every
/// other holds a value of a type with none, such as `Infallible`. Choosing
/// among several would be a guess, and a sum with no values holds none.
pub(super) fn tagless_variant(
    types: &(impl TypeEntries + ?Sized),
    aggregate: TypeId,
) -> Option<usize> {
    let Ok((
        _,
        TypeInfo {
            kind:
                TypeKind::Variant {
                    common_members,
                    variants,
                    ..
                },
            ..
        },
    )) = transparent_type_from(types, aggregate)
    else {
        return None;
    };

    if common_members
        .iter()
        .any(|member| uninhabited(types, member.type_ref.id, 0))
    {
        return None;
    }

    if is_single_default_variant(variants) {
        return Some(0);
    }

    let mut possible = variants.iter().enumerate().filter(|(_, variant)| {
        matches!(variant.selection, VariantSelection::Default)
            && !variant
                .members
                .iter()
                .any(|member| uninhabited(types, member.type_ref.id, 0))
    });

    match (possible.next(), possible.next()) {
        (Some((index, _)), None) => Some(index),
        _ => None,
    }
}

/// Whether no value of a type can exist: a sum that stores no tag and none
/// of whose variants can hold a value, or a record with a member of such a
/// type. Anything not known to be so is taken to have values.
fn uninhabited(types: &(impl TypeEntries + ?Sized), id: TypeId, depth: usize) -> bool {
    if depth >= MAX_TYPE_RESOLUTION_DEPTH {
        return false;
    }

    let Ok((_, info)) = transparent_type_from(types, id) else {
        return false;
    };

    match &info.kind {
        TypeKind::Variant {
            discriminant,
            common_members,
            variants,
            ..
        } => {
            matches!(discriminant.as_ref(), VariantDiscriminant::Absent)
                && (common_members
                    .iter()
                    .any(|member| uninhabited(types, member.type_ref.id, depth + 1))
                    || variants.iter().all(|variant| {
                        variant
                            .members
                            .iter()
                            .any(|member| uninhabited(types, member.type_ref.id, depth + 1))
                    }))
        }
        TypeKind::Record { members, .. } => members
            .iter()
            .any(|member| uninhabited(types, member.type_ref.id, depth + 1)),
        _ => false,
    }
}

pub(super) fn value_shape_from(
    types: &(impl TypeEntries + ?Sized),
    id: TypeId,
) -> std::result::Result<ValueShape, ValueShapeError> {
    nested_value_shape(types, id, 0)
}

/// Computes a value shape for a type nested `depth` arrays deep. Deeper
/// nesting than the resolution limit is unsupported rather than recursed.
#[expect(
    clippy::too_many_lines,
    reason = "each normalized type shape has distinct validation"
)]
fn nested_value_shape(
    types: &(impl TypeEntries + ?Sized),
    id: TypeId,
    depth: usize,
) -> std::result::Result<ValueShape, ValueShapeError> {
    if depth >= MAX_TYPE_RESOLUTION_DEPTH {
        return Err(ValueShapeError::Unsupported(
            "array nesting exceeds its limit".into(),
        ));
    }

    let (current, info) = transparent_type_from(types, id)?;
    match &info.kind {
        TypeKind::Base(base) => {
            if base.byte_size == 0 {
                // A scalar encoding cannot occupy zero bytes; treat it as
                // defective rather than decoding empty storage.
                return Err(ValueShapeError::Malformed(
                    "base type has a zero byte size".into(),
                ));
            }

            if base.byte_size > MAX_SCALAR_BYTES {
                return Err(ValueShapeError::Unsupported(
                    format!("scalar type occupies {} bytes", base.byte_size).into(),
                ));
            }

            let mut base = base.clone();
            base.name = Arc::clone(
                &type_info_from(types, id)
                    .map_err(ValueShapeError::Malformed)?
                    .name,
            );

            Ok(ValueShape::Scalar(base))
        }

        TypeKind::Enumeration {
            representation,
            enumerators,
            origin,
            ..
        } => {
            if representation.byte_size == 0 {
                return Err(ValueShapeError::Malformed(
                    "enumeration has a zero byte size".into(),
                ));
            }

            if representation.byte_size > MAX_SCALAR_BYTES {
                return Err(ValueShapeError::Unsupported(
                    format!("enumeration occupies {} bytes", representation.byte_size).into(),
                ));
            }

            integer_bit_width(representation).map_err(ValueShapeError::Malformed)?;
            let mut representation = representation.clone();
            representation.name = Arc::clone(
                &type_info_from(types, id)
                    .map_err(ValueShapeError::Malformed)?
                    .name,
            );

            Ok(ValueShape::Enumeration {
                byte_size: representation.byte_size,
                representation,
                enumerators: Arc::clone(enumerators),
                origin: *origin,
            })
        }

        TypeKind::Array {
            element,
            dimensions,
            ordering,
        } => {
            let element_shape = nested_value_shape(types, element.id, depth + 1)?;
            let mut count = 1_u64;

            for dimension in dimensions.iter() {
                count = count.checked_mul(dimension.count).ok_or_else(|| {
                    ValueShapeError::Unsupported("array element count overflows".into())
                })?;
            }

            let element_size = element_shape.byte_size();
            let byte_size = count
                .checked_mul(element_size)
                .ok_or_else(|| ValueShapeError::Unsupported("array byte size overflows".into()))?;

            Ok(ValueShape::Array {
                element: element.id,
                dimensions: Arc::clone(dimensions),
                ordering: *ordering,
                byte_size,
            })
        }

        TypeKind::RuntimeArray {
            element,
            dimensions,
            ordering,
        } => {
            let element_size = nested_value_shape(types, element.id, depth + 1)?.byte_size();
            Ok(ValueShape::RuntimeArray {
                array: current,
                element: element.id,
                element_size,
                dimensions: Arc::clone(dimensions),
                ordering: *ordering,
                byte_size: info.byte_size.unwrap_or(0),
            })
        }

        TypeKind::Slice {
            element,
            words,
            text,
        } => {
            let byte_size = info.byte_size.ok_or_else(|| {
                ValueShapeError::Malformed("slice descriptor has no byte size".into())
            })?;
            Ok(ValueShape::Slice {
                element: element.id,
                byte_size,
                words: *words,
                text: *text,
            })
        }

        TypeKind::Record {
            members,
            bases,
            incomplete,
            ..
        } => {
            if *incomplete {
                return Err(ValueShapeError::Unsupported(
                    "incomplete record values are unsupported".into(),
                ));
            }

            let byte_size = info.byte_size.ok_or_else(|| {
                ValueShapeError::Malformed("complete record type has no byte size".into())
            })?;
            Ok(ValueShape::Record {
                record: current,
                members: Arc::clone(members),
                bases: Arc::clone(bases),
                byte_size,
            })
        }

        TypeKind::Union {
            members,
            incomplete,
        } => {
            if *incomplete {
                return Err(ValueShapeError::Unsupported(
                    "incomplete union values are unsupported".into(),
                ));
            }

            let byte_size = info.byte_size.ok_or_else(|| {
                ValueShapeError::Malformed("complete union type has no byte size".into())
            })?;
            Ok(ValueShape::Union {
                union: current,
                members: Arc::clone(members),
                byte_size,
            })
        }

        TypeKind::Variant {
            common_members,
            bases,
            discriminant,
            variants,
            incomplete,
            ..
        } => {
            if *incomplete {
                return Err(ValueShapeError::Unsupported(
                    "incomplete variant values are unsupported".into(),
                ));
            }

            if !matches!(discriminant.as_ref(), VariantDiscriminant::Stored(_))
                && tagless_variant(types, current).is_none()
            {
                return Err(ValueShapeError::Unsupported(
                    "tagless variant selection is unsupported".into(),
                ));
            }

            let byte_size = info.byte_size.ok_or_else(|| {
                ValueShapeError::Malformed("complete variant type has no byte size".into())
            })?;
            Ok(ValueShape::Variant {
                aggregate: current,
                common_members: Arc::clone(common_members),
                bases: Arc::clone(bases),
                discriminant: discriminant.as_ref().clone(),
                variants: Arc::clone(variants),
                byte_size,
            })
        }

        TypeKind::Pointer {
            target,
            address_class,
        } => {
            let byte_size = indirection_byte_size(info.byte_size, *address_class, "pointer")?;
            Ok(ValueShape::Indirection {
                target: target.map(|target| target.id),
                byte_size,
                address_class: *address_class,
            })
        }

        TypeKind::Reference {
            target,
            address_class,
            ..
        } => {
            let byte_size = indirection_byte_size(info.byte_size, *address_class, "reference")?;
            Ok(ValueShape::Indirection {
                target: Some(target.id),
                byte_size,
                address_class: *address_class,
            })
        }
        TypeKind::Modified { .. } | TypeKind::Named { .. } => Err(ValueShapeError::Malformed(
            "a type wrapper was not stripped before its shape was computed".into(),
        )),
        TypeKind::Function => Ok(ValueShape::Function {
            byte_size: info.byte_size.ok_or_else(|| {
                ValueShapeError::Malformed("function value type has no byte size".into())
            })?,
        }),
        TypeKind::Unspecified => Err(ValueShapeError::Unsupported(
            "unspecified values are unsupported".into(),
        )),
        TypeKind::Signature { .. } => Err(ValueShapeError::Unsupported(
            "a function's code is not a value".into(),
        )),
        TypeKind::Opaque { description } => {
            Err(ValueShapeError::Unsupported(Arc::clone(description)))
        }
    }
}

impl ValueShape {
    pub(super) const fn byte_size(&self) -> u64 {
        match self {
            Self::Scalar(base) => base.byte_size,
            Self::Enumeration { byte_size, .. }
            | Self::Indirection { byte_size, .. }
            | Self::Function { byte_size }
            | Self::Array { byte_size, .. }
            | Self::RuntimeArray { byte_size, .. }
            | Self::Slice { byte_size, .. }
            | Self::Record { byte_size, .. }
            | Self::Union { byte_size, .. }
            | Self::Variant { byte_size, .. } => *byte_size,
        }
    }

    pub(super) const fn scalar(&self) -> Option<&BaseType> {
        match self {
            Self::Scalar(base) => Some(base),
            Self::Enumeration { .. }
            | Self::Indirection { .. }
            | Self::Function { .. }
            | Self::Array { .. }
            | Self::RuntimeArray { .. }
            | Self::Slice { .. }
            | Self::Record { .. }
            | Self::Union { .. }
            | Self::Variant { .. } => None,
        }
    }
}
