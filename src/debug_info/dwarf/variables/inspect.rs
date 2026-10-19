//! Inspecting data objects: following value paths and materializing values.

use std::collections::BTreeSet;
use std::sync::Arc;

use foldhash::{HashSet, HashSetExt};

use crate::debug_info::dwarf::DwarfError;
use crate::debug_info::{
    ObjectStorage, PlannedStep, Step, StorageClass, VariableContext, VariableRuntime,
};

use crate::inspection::InspectionBudget;
use crate::model::{ArrayDimension, ArrayOrdering, ValueStorage};
use crate::{
    Accessibility, AddressValue, BaseType, BaseTypeEncoding, ByteOrder, CodeInstanceId,
    DereferenceReference, DereferenceState, DereferenceUnavailableReason, DereferencedValue, Error,
    ImageAddress, InspectedValue, IntegerValue, RecordMember, RecordMemberLayout, Result,
    ScalarValue, SliceWords, TypeId, TypeInfo, TypeKind, TypeReference, ValueChild, ValueChildPage,
    ValueChildRelationship, ValueChildren, ValueChildrenReference, Variable, VariableInvalidReason,
    VariableMalformedKind, VariableState, VariableUnavailableReason, VariableValue,
    VariableValueSource, Variant, VariantDiscriminant, VirtualAddress,
};

use super::codec::{
    complex_part, decode_address, decode_integer_value, decode_scalar, significant_bytes,
    unsigned_value,
};

use super::evaluate::{
    EvaluateError, FrameBase, FrameBaseCache, FrameBaseContext, evaluate,
    evaluate_dynamic_aggregate_address, materialize_constant,
};

use super::generic::Generic;
use super::location::{Expression, ExpressionUse, LocationSelectionError, select};
use super::pieces::storage_from_pieces;
use super::runtime_array::{Resolved, element_offset};
use super::shape::tagless_variant;
use super::shape::{
    ValueShape, ValueShapeError, indirection_byte_size, transparent_type_from, value_shape_from,
};

use super::storage;
use super::types::{
    DynamicAggregateChild, DynamicAggregateLayoutKey, TypeResolution, type_info_from,
};

use super::variant::selected_variant_index;
use super::{
    ConstantValue, DwarfVariableInfo, MAX_AGGREGATE_DEPTH, MAX_EVALUATION_MEMORY_BYTES, Metadata,
    MetadataAbsence, ValueDescription, malformed_reason,
};

use crate::image::variables::{Capture, Object, VariableFunction};

/// One transition between storages, planned from types alone. Index steps
/// take their index values when they are applied.
#[derive(Clone)]
pub(in crate::debug_info) enum PathStep {
    Dereference {
        target: TypeId,
        byte_size: u64,
        address_class: u64,
    },
    ArrayIndex {
        dimensions: Arc<[ArrayDimension]>,
        ordering: ArrayOrdering,
        element_size: u64,
    },

    /// An index into the array bounded at run time `array`, whose value
    /// finds its bounds.
    RuntimeArrayIndex {
        array: TypeId,
    },
    SliceIndex {
        element_size: u64,
        descriptor_size: u64,
        words: SliceWords,
    },
    Member(Box<PlannedMemberStep>),
    Unavailable(VariableUnavailableReason),
}

#[derive(Clone)]
pub(in crate::debug_info) struct PlannedMemberStep {
    pub(super) aggregate: TypeId,
    pub(super) child: DynamicAggregateChild,
    pub(super) member: RecordMember,
    pub(super) required_variant: Option<(usize, VariantDiscriminant, Arc<[Variant]>)>,
}

/// One step from an aggregate into a member or base on the way to a member
/// found through anonymous members and base classes.
#[derive(Clone)]
struct MemberHop {
    aggregate: TypeId,
    child: DynamicAggregateChild,

    /// The member, or a base class as the member it amounts to.
    member: RecordMember,

    /// Whether the hop enters a virtual base, which every path through it
    /// shares.
    virtual_base: bool,

    /// The dereference that follows the member, when it is an embedded
    /// pointer whose members Go promotes.
    then: Option<PathStep>,
}

/// How many aggregates one member lookup may examine.
const MAX_MEMBER_SEARCH: usize = 4_096;

/// The most words a slice descriptor may hold: Odin's dynamic arrays, the
/// largest, hold five.
const MAX_SLICE_WORDS: u64 = 8;

/// The one path when every path found reaches the same subobject: paths
/// through one virtual base reach one object, since the derived object
/// holds a virtual base once however many classes derive from it.
fn one_subobject(paths: &[Vec<MemberHop>]) -> Option<&[MemberHop]> {
    // A path's subobject: from its last virtual base onward, or all of it.
    let key = |path: &[MemberHop]| {
        let start = path.iter().rposition(|hop| hop.virtual_base);
        let tail = &path[start.unwrap_or(0)..];
        let base = start.map(|start| path[start].member.type_ref.id);
        (
            base,
            tail.iter()
                .skip(usize::from(start.is_some()))
                .map(|hop| (hop.aggregate, hop.child))
                .collect::<Vec<_>>(),
        )
    };

    let (first, rest) = paths.split_first()?;
    let expected = key(first);
    rest.iter()
        .all(|path| key(path) == expected)
        .then_some(first.as_slice())
}

pub(super) struct DecodedSlice {
    pub(super) source: VariableValueSource,
    pub(super) raw: Arc<[u8]>,
    pub(super) address: VirtualAddress,
    pub(super) length: u64,
    pub(super) capacity: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ScalarDecodeError {
    Unavailable(VariableUnavailableReason),
    Invalid(VariableInvalidReason),
    Malformed(Arc<str>),
}

/// Maps an evaluation failure to its variable state; only fatal failures
/// escape as errors.
pub(super) fn evaluate_error_state(
    error: EvaluateError,
    kind: VariableMalformedKind,
) -> Result<VariableState> {
    match error {
        EvaluateError::Unavailable(reason) => Ok(VariableState::Unavailable(reason)),
        EvaluateError::Malformed(description) => Ok(VariableState::Malformed(malformed_reason(
            kind,
            description,
        ))),
        EvaluateError::Fatal(description) => Err(Error::VariableRuntime(description)),
    }
}

/// The state of a value whose type has no usable shape.
fn shape_error_state(error: ValueShapeError) -> VariableState {
    match error {
        ValueShapeError::Malformed(description) => VariableState::Malformed(malformed_reason(
            VariableMalformedKind::InvalidTypeGraph,
            description,
        )),
        ValueShapeError::Unsupported(_) => {
            VariableState::Unavailable(crate::UnsupportedVariableFeature::TypeRepresentation.into())
        }
    }
}

/// A shape failure for a type its parent already shaped, which only
/// inconsistent metadata can cause.
fn shape_error(error: ValueShapeError) -> Error {
    let (ValueShapeError::Malformed(description) | ValueShapeError::Unsupported(description)) =
        error;
    Error::debug_info(DwarfError::MalformedVariable(description))
}

/// Splits a shape failure into malformed metadata, an error, and an
/// unsupported representation, `None`.
fn supported_shape<T>(result: std::result::Result<T, ValueShapeError>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(ValueShapeError::Malformed(description)) => Err(Error::debug_info(
            DwarfError::MalformedVariable(description),
        )),
        Err(ValueShapeError::Unsupported(_)) => Ok(None),
    }
}

/// The hop from `aggregate` into its base class subobject `index`.
fn base_hop(aggregate: TypeId, index: usize, base: &crate::BaseClass) -> MemberHop {
    MemberHop {
        aggregate,
        child: DynamicAggregateChild::Base(index),
        member: RecordMember {
            name: None,
            type_ref: base.type_ref,
            layout: base.layout,
            accessibility: base.accessibility,
            artificial: false,
            embedded: false,
            declaration: None,
        },
        virtual_base: base.virtuality == crate::BaseClassVirtuality::Virtual,
        then: None,
    }
}

const fn dereference_reference(
    context: VariableContext,
    target_type: TypeId,
    target: crate::model::DereferenceTarget,
) -> DereferenceReference {
    DereferenceReference {
        stop_id: context.stop_id,
        context: context.context,
        frame: context.frame,
        module: context.module,
        image: context.image,
        context_address: context.address,
        target_type,
        target,
    }
}

/// The byte offset an array index step reaches with `indices`, checked
/// against the array's static bounds; `None` for any other step.
pub(in crate::debug_info) fn array_byte_offset(
    step: &PathStep,
    indices: &[i128],
) -> Result<Option<i64>> {
    let PathStep::ArrayIndex {
        dimensions,
        ordering,
        element_size,
    } = step
    else {
        return Ok(None);
    };

    element_byte_offset(dimensions, *ordering, indices, *element_size).map(Some)
}

/// The byte offset of the element at source `indices` of an array laid
/// out in `ordering`, checked against its bounds.
fn element_byte_offset(
    dimensions: &[ArrayDimension],
    ordering: ArrayOrdering,
    indices: &[i128],
    element_size: u64,
) -> Result<i64> {
    if indices.len() != dimensions.len() {
        return Err(Error::InvalidValueExpression(format!(
            "an array of {} dimensions takes as many indices, not {}",
            dimensions.len(),
            indices.len()
        )));
    }

    let mut position = Vec::with_capacity(indices.len());

    for (index, dimension) in indices.iter().copied().zip(dimensions.iter()) {
        let relative = index
            .checked_sub(dimension.lower_bound)
            .and_then(|relative| u64::try_from(relative).ok())
            .filter(|relative| *relative < dimension.count)
            .ok_or(Error::ValueIndexOutOfBounds {
                index,
                lower_bound: dimension.lower_bound,
                count: dimension.count,
            })?;
        position.push((relative, dimension.count));
    }

    // The dimension whose elements are adjacent comes last.
    if ordering == ArrayOrdering::ColumnMajor {
        position.reverse();
    }

    let mut linear = 0_u64;

    for (relative, count) in position {
        linear = linear
            .checked_mul(count)
            .and_then(|value| value.checked_add(relative))
            .ok_or_else(|| {
                Error::InvalidValueExpression("array element index overflows".to_owned())
            })?;
    }

    linear
        .checked_mul(element_size)
        .and_then(|offset| i64::try_from(offset).ok())
        .ok_or_else(|| Error::InvalidValueExpression("array element offset overflows".to_owned()))
}

/// The source indices of the row-major element `index` of an array.
fn array_source_indices(dimensions: &[ArrayDimension], index: u64) -> Result<Vec<i128>> {
    let mut indices = vec![0_i128; dimensions.len()];
    let mut remaining = index;

    for (dimension_index, dimension) in dimensions.iter().enumerate().rev() {
        if dimension.count == 0 {
            return Err(Error::debug_info(DwarfError::MalformedVariable(
                "a non-empty array page has a zero-sized dimension".into(),
            )));
        }

        let relative = remaining % dimension.count;
        remaining /= dimension.count;
        indices[dimension_index] = dimension
            .lower_bound
            .checked_add(i128::from(relative))
            .ok_or_else(|| {
                Error::debug_info(DwarfError::MalformedVariable(
                    "array source index overflows".into(),
                ))
            })?;
    }

    Ok(indices)
}

pub(super) fn static_member_layout_is_valid(
    record_size: Option<u64>,
    member_size: Option<u64>,
    layout: RecordMemberLayout,
) -> bool {
    match layout {
        RecordMemberLayout::ByteOffset(offset) => {
            let Some(record_size) = record_size else {
                return false;
            };

            offset <= record_size
                && member_size.is_none_or(|size| {
                    offset
                        .checked_add(size)
                        .is_some_and(|end| end <= record_size)
                })
        }

        RecordMemberLayout::BitRange {
            bit_offset,
            bit_size,
        } => record_size.is_some_and(|record_size| {
            record_size.checked_mul(8).is_some_and(|record_bits| {
                bit_offset
                    .checked_add(bit_size)
                    .is_some_and(|end| end <= record_bits)
            })
        }),
        RecordMemberLayout::Runtime => true,
    }
}

/// An integer of an enumeration-like type: symbolic when it is one of the
/// type's constants, or else the bitwise OR of some of its single-bit
/// constants: for constants Go gave a named type, as Delve reads them, and
/// for a language's enumeration of flags. A language's enumeration
/// otherwise stays symbolic with no name, and anything else is its number,
/// so `time.Duration(1500000000)` names no constant.
fn symbolic(
    value: IntegerValue,
    enumerators: &[crate::Enumerator],
    origin: crate::EnumerationOrigin,
    byte_size: u64,
) -> VariableValue {
    let exact = enumerators
        .iter()
        .filter(|enumerator| enumerator.value == value)
        .cloned()
        .collect::<Vec<_>>();
    let language = origin == crate::EnumerationOrigin::Language;
    if !exact.is_empty() || (language && !are_flags(enumerators)) {
        return VariableValue::Enumeration {
            value,
            matches: exact.into(),
        };
    }

    // Bit patterns in the type's width, where a signed type's lowest
    // value is one bit.
    let mask = u32::try_from(byte_size.saturating_mul(8))
        .ok()
        .and_then(|bits| 1_u128.checked_shl(bits))
        .map_or(u128::MAX, |limit| limit - 1);
    let bits = |value: IntegerValue| match value {
        IntegerValue::Signed(value) => value.cast_unsigned() & mask,
        IntegerValue::Unsigned(value) => value & mask,
    };

    let mut remaining = bits(value);
    let mut flags = Vec::new();

    for enumerator in enumerators {
        let flag = bits(enumerator.value);
        if flag.is_power_of_two() && remaining & flag != 0 {
            remaining &= !flag;
            flags.push(enumerator.clone());
        }
    }

    if remaining == 0 && !flags.is_empty() {
        return VariableValue::Enumeration {
            value,
            matches: flags.into(),
        };
    }

    if language {
        return VariableValue::Enumeration {
            value,
            matches: Arc::from([]),
        };
    }

    VariableValue::Scalar(match value {
        IntegerValue::Signed(value) => ScalarValue::Signed(value),
        IntegerValue::Unsigned(value) => ScalarValue::Unsigned(value),
    })
}

/// Whether a language's enumeration is one of flags, as `enum { READ = 1,
/// WRITE = 2 }` is: every constant is zero or a single bit, at least two
/// are bits, and they do not count 0, 1, 2 as a sequence of three does.
fn are_flags(enumerators: &[crate::Enumerator]) -> bool {
    let value = |enumerator: &crate::Enumerator| match enumerator.value {
        IntegerValue::Signed(value) => u128::try_from(value).ok(),
        IntegerValue::Unsigned(value) => Some(value),
    };

    let mut bits = 0_u128;
    let mut zero = false;

    for enumerator in enumerators {
        match value(enumerator) {
            Some(0) => zero = true,
            Some(bit) if bit.is_power_of_two() => bits |= bit,
            _ => return false,
        }
    }

    bits.count_ones() >= 2 && !(zero && bits == 0b11)
}

impl DwarfVariableInfo {
    /// Classifies an object's storage from the operations of its location
    /// expressions. Thread-local and indirect forms dominate frame-relative
    /// ones, which dominate static addresses.
    pub(super) fn object_storage(&self, object: Object<'_>) -> ObjectStorage {
        let ranges = self.visible_ranges(object);
        let Some(location) = object.location() else {
            return ObjectStorage {
                class: StorageClass::NotMemory,
                ranges,
            };
        };

        let tables = self.locations();
        let location = tables.list(location);
        let mut uses = BTreeSet::new();

        for (_, expression) in location.entries() {
            if self.expression_uses(expression, &mut uses).is_err() {
                return ObjectStorage {
                    class: StorageClass::NotMemory,
                    ranges,
                };
            }
        }

        let class = if uses.contains(&ExpressionUse::ThreadLocal) {
            StorageClass::ThreadLocal
        } else if uses.contains(&ExpressionUse::Dereference) || object.escaped().is_some() {
            StorageClass::Indirect
        } else if uses.contains(&ExpressionUse::RegisterValue)
            || uses.contains(&ExpressionUse::Computed)
            || location.is_empty()
        {
            StorageClass::NotMemory
        } else if uses.contains(&ExpressionUse::Frame) {
            let one_place = |list: super::location::LocationList<'_>| {
                let mut entries = list.entries().map(|(_, expression)| expression.bytes());
                entries
                    .next()
                    .is_none_or(|first| entries.all(|bytes| bytes == first))
            };

            let single_frame_base = match object.frame_base() {
                Metadata::Value(frame_base) => one_place(tables.list(frame_base)),
                Metadata::Absent(_) => true,
                Metadata::Malformed(_) => false,
            };

            StorageClass::Frame {
                stable: one_place(location) && single_frame_base,
                moving_stack: location.entries().any(|(_, expression)| {
                    tables.unit_language(expression.unit()) == Some(gimli::DW_LANG_Go)
                }),
            }
        } else {
            StorageClass::Static
        };

        ObjectStorage { class, ranges }
    }

    fn expression_uses(
        &self,
        expression: Expression<'_>,
        uses: &mut BTreeSet<ExpressionUse>,
    ) -> std::result::Result<(), gimli::Error> {
        let reader = gimli::EndianSlice::new(expression.bytes(), self.endian);
        let mut operations = gimli::Expression(reader).operations(expression.encoding());

        while let Some(operation) = operations.next()? {
            uses.extend(match operation {
                gimli::Operation::TLS => Some(ExpressionUse::ThreadLocal),
                gimli::Operation::Deref { .. } => Some(ExpressionUse::Dereference),
                gimli::Operation::FrameOffset { .. }
                | gimli::Operation::RegisterOffset { .. }
                | gimli::Operation::CallFrameCFA => Some(ExpressionUse::Frame),
                gimli::Operation::Register { .. } => Some(ExpressionUse::RegisterValue),
                gimli::Operation::StackValue
                | gimli::Operation::ImplicitValue { .. }
                | gimli::Operation::ImplicitPointer { .. }
                | gimli::Operation::Piece { .. }
                | gimli::Operation::EntryValue { .. } => Some(ExpressionUse::Computed),
                _ => None,
            });
        }

        Ok(())
    }

    pub(super) fn type_info(&self, id: TypeId) -> std::result::Result<&TypeInfo, Arc<str>> {
        type_info_from(&*self.types, id)
    }

    pub(super) fn value_shape(
        &self,
        id: TypeId,
    ) -> std::result::Result<ValueShape, ValueShapeError> {
        value_shape_from(&*self.types, id)
    }

    pub(super) fn function_at(&self, address: ImageAddress) -> Option<VariableFunction<'_>> {
        self.catalog().function_at(address)
    }

    fn transparent_type(
        &self,
        id: TypeId,
    ) -> std::result::Result<(TypeId, &TypeInfo), ValueShapeError> {
        transparent_type_from(&*self.types, id)
    }

    /// Whether a type is a function's, whose values are code.
    fn is_code(&self, id: TypeId) -> bool {
        self.transparent_type(id)
            .is_ok_and(|(_, info)| matches!(info.kind, TypeKind::Signature { .. }))
    }

    fn validate_static_member_layout(&self, record: TypeId, member: &RecordMember) -> Result<()> {
        let record_size = self.type_info(record).ok().and_then(|info| info.byte_size);
        let member_size = self
            .type_info(member.type_ref.id)
            .ok()
            .and_then(|info| info.byte_size);
        if !static_member_layout_is_valid(record_size, member_size, member.layout) {
            return Err(Error::debug_info(DwarfError::MalformedVariable(
                "record member extends beyond its containing object".into(),
            )));
        }

        Ok(())
    }

    /// Every path to a member named `name` of the record or union
    /// `aggregate`, found as C and C++ find names: among its own members,
    /// counting the members of its anonymous members as its own, and only
    /// then in its base classes, whose names it hides. Go's embedded fields
    /// promote their members as Go does: the shallowest win, and an
    /// embedded pointer is followed. `enclosing` holds the records the
    /// search is inside, which an embedded pointer may lead back to.
    fn member_paths(
        &self,
        aggregate: TypeId,
        name: &str,
        work: &mut usize,
        depth: usize,
        enclosing: &mut Vec<TypeId>,
    ) -> Result<Vec<Vec<MemberHop>>> {
        *work += 1;
        if *work > MAX_MEMBER_SEARCH || depth > MAX_AGGREGATE_DEPTH {
            return Err(Error::InvalidValueExpression(
                "looking up a member through anonymous members and base classes exceeds its limit"
                    .to_owned(),
            ));
        }

        let malformed = |description| Error::debug_info(DwarfError::MalformedVariable(description));
        let info = self.type_info(aggregate).map_err(malformed)?;
        let (members, bases): (&[RecordMember], &[crate::BaseClass]) = match &info.kind {
            TypeKind::Record { members, bases, .. } => (members, bases),
            TypeKind::Union { members, .. } => (members, &[]),
            _ => return Ok(Vec::new()),
        };

        let hop = |index, member: &RecordMember| MemberHop {
            aggregate,
            child: DynamicAggregateChild::Member(index),
            member: member.clone(),
            virtual_base: false,
            then: None,
        };

        let mut found = members
            .iter()
            .enumerate()
            .filter(|(_, member)| !member.artificial && member.name.as_deref() == Some(name))
            .map(|(index, member)| vec![hop(index, member)])
            .collect::<Vec<_>>();

        for (index, member) in members.iter().enumerate() {
            if member.name.is_some() || member.artificial || !found.is_empty() {
                continue;
            }

            let Ok((inner, _)) = self.transparent_type(member.type_ref.id) else {
                continue;
            };

            for path in self.member_paths(inner, name, work, depth + 1, enclosing)? {
                let mut whole = vec![hop(index, member)];
                whole.extend(path);
                found.push(whole);
            }
        }

        if !found.is_empty() {
            return Ok(found);
        }

        enclosing.push(aggregate);
        let promoted = self.promoted_paths(aggregate, members, name, work, depth, enclosing);
        enclosing.pop();
        let promoted = promoted?;
        if !promoted.is_empty() {
            return Ok(promoted);
        }

        for (index, base) in bases.iter().enumerate() {
            let Ok((inner, _)) = self.transparent_type(base.type_ref.id) else {
                continue;
            };

            for path in self.member_paths(inner, name, work, depth + 1, enclosing)? {
                let mut whole = vec![base_hop(aggregate, index, base)];
                whole.extend(path);
                found.push(whole);
            }
        }

        Ok(found)
    }

    /// The shallowest paths to a member named `name` through the embedded
    /// fields among `members`, which Go promotes: each embedded field's
    /// own search finds its shallowest, and the shallowest of those win.
    fn promoted_paths(
        &self,
        aggregate: TypeId,
        members: &[RecordMember],
        name: &str,
        work: &mut usize,
        depth: usize,
        enclosing: &mut Vec<TypeId>,
    ) -> Result<Vec<Vec<MemberHop>>> {
        let mut found: Vec<Vec<MemberHop>> = Vec::new();

        for (index, member) in members.iter().enumerate() {
            if !member.embedded || member.artificial {
                continue;
            }

            let Ok((inner, info)) = self.transparent_type(member.type_ref.id) else {
                continue;
            };

            let (record, then) = match &info.kind {
                TypeKind::Pointer {
                    target: Some(target),
                    address_class,
                } => {
                    let Ok((record, _)) = self.transparent_type(target.id) else {
                        continue;
                    };

                    let Some(byte_size) = supported_shape(indirection_byte_size(
                        info.byte_size,
                        *address_class,
                        "pointer",
                    ))?
                    else {
                        continue;
                    };

                    let step = PathStep::Dereference {
                        target: target.id,
                        byte_size,
                        address_class: *address_class,
                    };

                    (record, Some(step))
                }
                _ => (inner, None),
            };

            if enclosing.contains(&record) {
                continue;
            }

            for path in self.member_paths(record, name, work, depth + 1, enclosing)? {
                let mut whole = vec![MemberHop {
                    aggregate,
                    child: DynamicAggregateChild::Member(index),
                    member: member.clone(),
                    virtual_base: false,
                    then: then.clone(),
                }];
                whole.extend(path);
                found.push(whole);
            }
        }

        if let Some(shallowest) = found.iter().map(Vec::len).min() {
            found.retain(|path| path.len() == shallowest);
        }

        Ok(found)
    }

    /// Every path from the record `aggregate` to a base class subobject
    /// whose type `is_target` accepts, through its bases' bases.
    fn base_paths(
        &self,
        aggregate: TypeId,
        is_target: &dyn Fn(TypeId) -> bool,
        work: &mut usize,
        depth: usize,
    ) -> Result<Vec<Vec<MemberHop>>> {
        *work += 1;
        if *work > MAX_MEMBER_SEARCH || depth > MAX_AGGREGATE_DEPTH {
            return Err(Error::InvalidValueExpression(
                "looking up a base class exceeds its limit".to_owned(),
            ));
        }

        let malformed = |description| Error::debug_info(DwarfError::MalformedVariable(description));
        let info = self.type_info(aggregate).map_err(malformed)?;
        let TypeKind::Record { bases, .. } = &info.kind else {
            return Ok(Vec::new());
        };

        let mut found = Vec::new();

        for (index, base) in bases.iter().enumerate() {
            let Ok((inner, _)) = self.transparent_type(base.type_ref.id) else {
                continue;
            };

            let hop = base_hop(aggregate, index, base);
            if is_target(base.type_ref.id) || is_target(inner) {
                found.push(vec![hop]);
                continue;
            }

            for path in self.base_paths(inner, is_target, work, depth + 1)? {
                let mut whole = vec![hop.clone()];
                whole.extend(path);
                found.push(whole);
            }
        }

        Ok(found)
    }

    /// Plans one structural step from `from`, which reads no program
    /// state. A member step follows pointers to the record that holds the
    /// member; an index step takes one index for a slice and one per
    /// dimension for an array, of the `available` the caller holds.
    #[expect(
        clippy::too_many_lines,
        reason = "each step keeps the typed failure of every type shape it meets"
    )]
    pub(super) fn plan_step(&self, from: TypeId, step: Step<'_>) -> Result<PlannedStep> {
        enum AggregateMembers<'a> {
            /// A record's or union's, which are searched through anonymous
            /// members and bases.
            Direct,
            Variant {
                common_members: &'a [RecordMember],
                discriminant: &'a VariantDiscriminant,
                variants: &'a Arc<[Variant]>,
            },
        }

        let malformed = |description| Error::debug_info(DwarfError::MalformedVariable(description));
        let planned = |steps, consumed, result| PlannedStep {
            steps,
            consumed,
            result,
        };

        let unsupported =
            || PathStep::Unavailable(crate::UnsupportedVariableFeature::TypeRepresentation.into());
        let mut steps = Vec::new();

        match step {
            // A generic pointer's shape, Go's `go.shape.*uint8`, points to
            // whatever its type argument does, which only running finds.
            Step::Deref if self.type_facts().dictionary_index(from).is_some() => {
                let reason = VariableUnavailableReason::ValueAccess(
                    crate::ValueAccessUnavailableReason::UnspecifiedPointee,
                );
                Ok(planned(vec![PathStep::Unavailable(reason)], 0, None))
            }
            Step::Deref => {
                let Some((_canonical, info)) = supported_shape(self.transparent_type(from))? else {
                    return Ok(planned(vec![unsupported()], 0, None));
                };

                let (target, address_class) = match &info.kind {
                    TypeKind::Pointer {
                        target: Some(target),
                        address_class,
                    }
                    | TypeKind::Reference {
                        target,
                        address_class,
                        ..
                    } => (target.id, *address_class),
                    TypeKind::Pointer { target: None, .. } => {
                        let reason = VariableUnavailableReason::ValueAccess(
                            crate::ValueAccessUnavailableReason::UnspecifiedPointee,
                        );
                        return Ok(planned(vec![PathStep::Unavailable(reason)], 0, None));
                    }
                    _ => {
                        let reason = VariableUnavailableReason::ValueAccess(
                            crate::ValueAccessUnavailableReason::NotPointerOrReference,
                        );
                        return Ok(planned(vec![PathStep::Unavailable(reason)], 0, None));
                    }
                };

                match supported_shape(indirection_byte_size(
                    info.byte_size,
                    address_class,
                    "pointer or reference",
                ))? {
                    Some(byte_size) => steps.push(PathStep::Dereference {
                        target,
                        byte_size,
                        address_class,
                    }),
                    None => steps.push(unsupported()),
                }

                Ok(planned(steps, 0, Some(target)))
            }
            Step::Base(target) => {
                let type_name = || {
                    self.type_info(from)
                        .map(|info| Arc::clone(&info.name))
                        .map_err(malformed)
                };

                let Some((aggregate, _)) = supported_shape(self.transparent_type(from))? else {
                    return Ok(planned(vec![unsupported()], 0, None));
                };

                let paths = self.base_paths(aggregate, target.is_target, &mut 0, 0)?;
                let Some(path) = one_subobject(&paths) else {
                    let base = Arc::from(target.name);
                    let type_name = type_name()?;
                    return Err(if paths.is_empty() {
                        Error::BaseNotFound { base, type_name }
                    } else {
                        Error::AmbiguousBase { base, type_name }
                    });
                };

                let result = self.plan_hops(path, &mut steps)?;
                Ok(planned(steps, 0, result))
            }
            Step::Index { available } => {
                let source_info = self.type_info(from).map_err(malformed)?;
                let Some((canonical, info)) = supported_shape(self.transparent_type(from))? else {
                    return Ok(planned(vec![unsupported()], 1, None));
                };

                let (element, consumed) = match &info.kind {
                    TypeKind::RuntimeArray {
                        element,
                        dimensions,
                        ..
                    } => {
                        if available < dimensions.len() {
                            return Err(Error::IncompleteArrayIndex {
                                type_name: Arc::clone(&source_info.name),
                                expected: dimensions.len(),
                                supplied: available,
                            });
                        }

                        steps.push(PathStep::RuntimeArrayIndex { array: canonical });
                        return Ok(planned(steps, dimensions.len(), Some(element.id)));
                    }

                    TypeKind::Array {
                        element,
                        dimensions,
                        ..
                    } => {
                        if available < dimensions.len() {
                            return Err(Error::IncompleteArrayIndex {
                                type_name: Arc::clone(&source_info.name),
                                expected: dimensions.len(),
                                supplied: available,
                            });
                        }

                        (element.id, dimensions.len())
                    }
                    TypeKind::Slice { element, .. } => (element.id, 1),
                    _ => {
                        return Err(Error::IndexAccessOnNonIndexable {
                            type_name: Arc::clone(&source_info.name),
                        });
                    }
                };

                let Some(element_shape) = supported_shape(self.value_shape(element))? else {
                    return Ok(planned(vec![unsupported()], consumed, Some(element)));
                };

                let element_size = element_shape.byte_size();

                match &info.kind {
                    TypeKind::Array {
                        dimensions,
                        ordering,
                        ..
                    } => steps.push(PathStep::ArrayIndex {
                        dimensions: Arc::clone(dimensions),
                        ordering: *ordering,
                        element_size,
                    }),
                    TypeKind::Slice { words, .. } => {
                        let descriptor_size = info
                            .byte_size
                            .ok_or_else(|| malformed("slice descriptor has no byte size".into()))?;
                        steps.push(PathStep::SliceIndex {
                            element_size,
                            descriptor_size,
                            words: *words,
                        });
                    }
                    _ => {
                        return Err(Error::internal(
                            "only arrays and slices were accepted as indexable",
                        ));
                    }
                }

                Ok(planned(steps, consumed, Some(element)))
            }
            Step::Member(member_name) => {
                let mut current = from;
                let mut indirections = HashSet::new();
                let (aggregate, aggregate_members) = loop {
                    let source_info = self.type_info(current).map_err(malformed)?;
                    let Some((canonical, info)) = supported_shape(self.transparent_type(current))?
                    else {
                        steps.push(unsupported());
                        return Ok(planned(steps, 0, None));
                    };

                    match &info.kind {
                        TypeKind::Pointer {
                            target: Some(target),
                            address_class,
                        }
                        | TypeKind::Reference {
                            target,
                            address_class,
                            ..
                        } => {
                            if !indirections.insert(canonical) || steps.len() >= MAX_AGGREGATE_DEPTH
                            {
                                return Err(Error::InvalidValueExpression(
                                    "pointer traversal exceeds its limit or contains a cycle"
                                        .to_owned(),
                                ));
                            }

                            match supported_shape(indirection_byte_size(
                                info.byte_size,
                                *address_class,
                                "pointer or reference",
                            ))? {
                                Some(byte_size) => steps.push(PathStep::Dereference {
                                    target: target.id,
                                    byte_size,
                                    address_class: *address_class,
                                }),
                                None => steps.push(unsupported()),
                            }

                            current = target.id;
                        }
                        TypeKind::Record { .. } | TypeKind::Union { .. } => {
                            break (canonical, AggregateMembers::Direct);
                        }

                        TypeKind::Variant {
                            common_members,
                            discriminant,
                            variants,
                            ..
                        } => {
                            break (
                                canonical,
                                AggregateMembers::Variant {
                                    common_members,
                                    discriminant: discriminant.as_ref(),
                                    variants,
                                },
                            );
                        }
                        _ => {
                            return Err(Error::MemberAccessOnNonRecord {
                                member: member_name.to_owned(),
                                type_name: Arc::clone(&source_info.name),
                            });
                        }
                    }
                };

                let lookup_failure = |found: usize| {
                    let type_name = match self.type_info(aggregate) {
                        Ok(info) => Arc::clone(&info.name),
                        Err(description) => return malformed(description),
                    };

                    let member = member_name.to_owned();
                    if found == 0 {
                        Error::MemberNotFound { member, type_name }
                    } else {
                        Error::AmbiguousMember {
                            member,
                            type_name,
                            candidates: Vec::new(),
                        }
                    }
                };

                let AggregateMembers::Variant {
                    common_members,
                    discriminant,
                    variants,
                } = aggregate_members
                else {
                    let paths =
                        self.member_paths(aggregate, member_name, &mut 0, 0, &mut Vec::new())?;
                    let Some(path) = one_subobject(&paths) else {
                        if paths.len() > 1 {
                            return Err(Error::AmbiguousMember {
                                member: member_name.to_owned(),
                                type_name: self
                                    .type_info(aggregate)
                                    .map(|info| Arc::clone(&info.name))
                                    .map_err(malformed)?,
                                candidates: paths.iter().map(|path| self.path_text(path)).collect(),
                            });
                        }

                        return Err(lookup_failure(paths.len()));
                    };

                    let result = self.plan_hops(path, &mut steps)?;
                    return Ok(planned(steps, 0, result));
                };

                let named = |member: &&RecordMember| {
                    !member.artificial && member.name.as_deref() == Some(member_name)
                };

                let mut matching = common_members
                    .iter()
                    .enumerate()
                    .filter(|(_, member)| named(member))
                    .map(|(index, member)| (DynamicAggregateChild::Member(index), None, member))
                    .collect::<Vec<_>>();

                for (variant_index, variant) in variants.iter().enumerate() {
                    matching.extend(
                        variant
                            .members
                            .iter()
                            .enumerate()
                            .filter(|(_, member)| named(member))
                            .map(|(member_index, member)| {
                                (
                                    DynamicAggregateChild::VariantMember {
                                        variant: variant_index,
                                        member: member_index,
                                    },
                                    Some((
                                        variant_index,
                                        discriminant.clone(),
                                        Arc::clone(variants),
                                    )),
                                    member,
                                )
                            }),
                    );
                }

                let [(child, required_variant, member)] = matching.as_slice() else {
                    return Err(lookup_failure(matching.len()));
                };

                self.validate_static_member_layout(aggregate, member)?;
                steps.push(PathStep::Member(Box::new(PlannedMemberStep {
                    aggregate,
                    child: *child,
                    member: (*member).clone(),
                    required_variant: required_variant.clone(),
                })));

                Ok(planned(steps, 0, Some(member.type_ref.id)))
            }
        }
    }

    /// Plans the member steps along one lookup path, returning the type it
    /// reaches.
    fn plan_hops(&self, path: &[MemberHop], steps: &mut Vec<PathStep>) -> Result<Option<TypeId>> {
        for hop in path {
            self.validate_static_member_layout(hop.aggregate, &hop.member)?;
            steps.push(PathStep::Member(Box::new(PlannedMemberStep {
                aggregate: hop.aggregate,
                child: hop.child,
                member: hop.member.clone(),
                required_variant: None,
            })));
            steps.extend(hop.then.clone());
        }

        Ok(path.last().map(|hop| hop.member.type_ref.id))
    }

    /// A lookup path as the selections that write it: each member's name,
    /// and each base class's type name.
    fn path_text(&self, path: &[MemberHop]) -> String {
        path.iter()
            .filter_map(|hop| match (&hop.member.name, hop.child) {
                (Some(name), _) => Some(name.to_string()),
                (None, DynamicAggregateChild::Base(_)) => self
                    .type_info(hop.member.type_ref.id)
                    .ok()
                    .map(|info| info.name.to_string()),
                (None, _) => None,
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    /// The index of the data object `name` names in the selected logical
    /// frame, whose innermost declaration hides the others.
    pub(super) fn visible_object(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        name: &str,
    ) -> Result<Object<'_>> {
        self.locals_read()?;
        let function = self
            .function_at(address)
            .ok_or_else(|| Error::VariableNotFound(name.to_owned()))?;
        let mut named = function
            .objects()
            .filter(|object| {
                object.instance() == selected
                    && object.name() == name
                    && self.visible_at(*object, address)
            })
            .collect::<Vec<_>>();
        let depth = named
            .iter()
            .map(|object| object.lexical_depth())
            .max()
            .ok_or_else(|| Error::VariableNotFound(name.to_owned()))?;
        named.retain(|object| object.lexical_depth() == depth);
        let [object] = named.as_slice() else {
            return Err(Error::AmbiguousVariable(name.to_owned()));
        };

        Ok(*object)
    }

    pub(super) fn located_data_object(
        &self,
        variable: Object<'_>,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<ValueStorage, EvaluateError> {
        let storage = self.located_slot(variable, address, runtime, frame_base_cache, budget)?;
        let Some(pointer) = variable.escaped() else {
            return Ok(storage);
        };

        // A variable moved to the heap is where its slot points.
        let byte_size = self
            .value_shape(pointer)
            .map_err(EvaluateError::from)?
            .byte_size();
        let size =
            usize::try_from(byte_size).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let (_, raw) = storage::read(&storage, size, runtime, budget)?;
        let address = decode_address(&raw, byte_size, self.target)?;
        if address.get() == 0 {
            return Err(VariableUnavailableReason::ValueAccess(
                crate::ValueAccessUnavailableReason::NullPointer,
            )
            .into());
        }

        Ok(ValueStorage::Memory(address))
    }

    /// Where a data object's location says it is: for a variable moved to
    /// the heap, the pointer to it.
    fn located_slot(
        &self,
        variable: Object<'_>,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<ValueStorage, EvaluateError> {
        if let Some(description) = variable.malformed() {
            return Err(EvaluateError::Malformed(description));
        }

        let type_id = match variable.type_info() {
            TypeResolution::Resolved(id) => id,
            TypeResolution::Malformed(description) => {
                return Err(EvaluateError::Malformed(description));
            }
        };

        let shape = self
            .value_shape(variable.escaped().unwrap_or(type_id))
            .map_err(EvaluateError::from)?;
        let description = match variable.value() {
            Metadata::Value(description) => description,
            Metadata::Absent(MetadataAbsence::NoLocation) => {
                return Err(EvaluateError::Unavailable(
                    VariableUnavailableReason::OptimizedOut(crate::OptimizedOutReason::NoLocation),
                ));
            }
            Metadata::Absent(MetadataAbsence::NoFrameBase | MetadataAbsence::NotApplicable) => {
                return Err(EvaluateError::Malformed(
                    "data-object value cannot contain a frame-base absence".into(),
                ));
            }
            Metadata::Malformed(description) => {
                return Err(EvaluateError::Malformed(description));
            }
        };

        if let ValueDescription::Constant(constant) = &description {
            let constant = &**constant;

            // An integer form holds at most 64 bits. Clang writes a wider
            // complex constant's real part alone that way, and the imaginary
            // part it leaves out is not zero.
            if matches!(&shape, ValueShape::Scalar(base)
                if base.encoding == BaseTypeEncoding::ComplexFloating && base.byte_size > 8)
                && !matches!(constant, ConstantValue::Bytes(_))
            {
                return Err(EvaluateError::Malformed(
                    "a complex constant wider than 64 bits is not a block".into(),
                ));
            }

            let raw = materialize_constant(
                constant,
                usize::try_from(shape.byte_size())
                    .map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
                self.target,
            )?;
            let end = raw.len();

            return Ok(ValueStorage::Bytes {
                source: VariableValueSource::Constant,
                raw,
                start: 0,
                end,
                address: None,
            });
        }

        let ValueDescription::Location(location) = description else {
            return Err(EvaluateError::Malformed(
                "constant value reached location lookup".into(),
            ));
        };

        let expression = select(self.locations().list(location), address)
            .map_err(|error| match error {
                LocationSelectionError::Unavailable(reason) => EvaluateError::Unavailable(reason),
                LocationSelectionError::Malformed(description) => {
                    EvaluateError::Malformed(description)
                }
            })?
            .ok_or({
                EvaluateError::Unavailable(VariableUnavailableReason::UnavailableAtInstruction)
            })?;
        let frame_base_location = variable.frame_base();
        let mut frame_base = FrameBase::Lazy(FrameBaseContext {
            location: &frame_base_location,
            tables: self.locations(),
            address,
            cache: frame_base_cache,
        });
        let pieces = evaluate(
            expression,
            self.endian,
            address,
            &mut frame_base,
            runtime,
            budget,
        )?;

        storage_from_pieces(
            &pieces,
            shape.byte_size(),
            shape.scalar(),
            self.endian,
            self.target,
            runtime,
        )
    }

    /// Reads a scalar's bytes, of which only the significant ones must be
    /// readable: an x87 long double's value is its first ten bytes, and a
    /// location may describe them without the padding after them. Says
    /// whether the padding was unreadable, in which case its bytes read as
    /// zero and are no part of the value.
    fn read_scalar(
        &self,
        storage: &ValueStorage,
        base: &BaseType,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<(VariableValueSource, Arc<[u8]>, bool), EvaluateError> {
        let size = usize::try_from(base.byte_size)
            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let significant = significant_bytes(base, self.target);
        let whole = storage::read(storage, size, runtime, budget);
        let Err(error) = whole else {
            return whole.map(|(source, raw)| (source, raw, false));
        };

        if significant == base.byte_size {
            return Err(error);
        }

        let narrowed = storage::narrow(storage.clone(), significant);
        let significant =
            usize::try_from(significant).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let Ok((source, raw)) = storage::read(&narrowed, significant, runtime, budget) else {
            return Err(error);
        };

        let mut padded = raw.to_vec();
        padded.resize(size, 0);

        Ok((source, padded.into(), true))
    }

    fn decode_slice(
        &self,
        storage: &ValueStorage,
        byte_size: u64,
        words: SliceWords,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<DecodedSlice, EvaluateError> {
        let pointer_bytes = self.pointer_bytes();

        // Validate the metadata's size before reading, so a bogus size cannot
        // spend the request's memory budget: a descriptor is a few words.
        let word_count = byte_size / pointer_bytes as u64;
        if !byte_size.is_multiple_of(pointer_bytes as u64)
            || !(words.span()..=MAX_SLICE_WORDS).contains(&word_count)
        {
            return Err(EvaluateError::Malformed(
                "slice descriptor size does not match its target layout".into(),
            ));
        }

        let size =
            usize::try_from(byte_size).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let (source, raw) = storage::read(storage, size, runtime, budget)?;
        let word = |index: u8| {
            let index = usize::from(index);
            unsigned_value(
                &raw[index * pointer_bytes..(index + 1) * pointer_bytes],
                self.target.byte_order,
            )
            .map_err(|reason| Arc::<str>::from(reason.to_string()))
            .and_then(|value| {
                u64::try_from(value)
                    .map_err(|_| Arc::<str>::from("slice word exceeds target address width"))
            })
        };

        let address = word(words.data)
            .map(VirtualAddress::new)
            .map_err(EvaluateError::Malformed)?;
        let length = word(words.length).map_err(EvaluateError::Malformed)?;
        let capacity = if let Some(capacity) = words.capacity {
            let capacity = word(capacity).map_err(EvaluateError::Malformed)?;
            if capacity < length {
                return Err(EvaluateError::Malformed(
                    "slice length exceeds its capacity".into(),
                ));
            }

            Some(capacity)
        } else {
            None
        };

        if address.get() == 0 && length != 0 {
            return Err(EvaluateError::Malformed(
                "non-empty slice has a null data pointer".into(),
            ));
        }

        Ok(DecodedSlice {
            source,
            raw,
            address,
            length,
            capacity,
        })
    }

    /// A function value from its stored pointer: nil, or the code its
    /// closure context begins with, and what the closure captured.
    fn function_value(
        &self,
        raw: &[u8],
        byte_size: u64,
        type_id: TypeId,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<(VariableValue, ValueChildren), EvaluateError> {
        let closure = decode_address(raw, byte_size, self.target)?;
        if closure.get() == 0 {
            let value = VariableValue::Function {
                code: None,
                function: None,
            };

            return Ok((value, ValueChildren::NotApplicable));
        }

        let size =
            usize::try_from(byte_size).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let (_, word) = storage::read(&ValueStorage::Memory(closure), size, runtime, budget)?;
        let code = decode_address(&word, byte_size, self.target)?;
        let function = runtime
            .image_address(code)
            .and_then(|address| self.catalog().go_function(address));
        let children = match function.map(VariableFunction::captures) {
            Some(Ok(captures)) if captures.is_empty() => ValueChildren::NotApplicable,
            Some(Ok(captures)) => ValueChildren::Available(Self::child_reference(
                &ValueStorage::Memory(closure),
                context,
                type_id,
                captures.len() as u64,
                None,
            )),
            Some(Err(_)) | None => {
                ValueChildren::Unavailable(VariableUnavailableReason::ValueAccess(
                    crate::ValueAccessUnavailableReason::UndescribedClosure,
                ))
            }
        };

        let value = VariableValue::Function {
            code: Some(code),
            function: function.and_then(VariableFunction::name).map(Arc::from),
        };

        Ok((value, children))
    }

    /// What the closure a function value's context belongs to captured.
    fn closure_captures(
        &self,
        closure: &ValueStorage,
        byte_size: u64,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Capture>> {
        let malformed = |description: &str| {
            Error::debug_info(DwarfError::MalformedVariable(description.into()))
        };

        let size =
            usize::try_from(byte_size).map_err(|_| malformed("function value is too wide"))?;
        let captures = storage::read(closure, size, runtime, budget)
            .and_then(|(_, word)| decode_address(&word, byte_size, self.target))
            .ok()
            .and_then(|code| runtime.image_address(code))
            .and_then(|address| self.catalog().go_function(address))
            .map(VariableFunction::captures);

        match captures {
            Some(Ok(captures)) => Ok(captures),
            _ => Err(malformed(
                "the closure's captures changed while it was inspected",
            )),
        }
    }

    /// A captured variable as a member of its closure, and whether the
    /// closure holds a pointer to it, as Go writes `&name`, rather than a
    /// copy. A captured pointer's member is the variable it points to.
    fn capture_member(
        &self,
        capture: &Capture,
        image: crate::ModuleImageId,
    ) -> Result<(RecordMember, bool)> {
        let malformed = |description: &str| {
            Error::debug_info(DwarfError::MalformedVariable(description.into()))
        };

        let TypeResolution::Resolved(type_id) = &capture.type_info else {
            return Err(malformed(
                "a closure's captured variable has a malformed type",
            ));
        };

        let (name, type_id, by_reference) = match capture.name.strip_prefix('&') {
            None => (Arc::clone(&capture.name), *type_id, false),
            Some(name) => {
                let pointer = self.type_info(*type_id).map_err(|description| {
                    Error::debug_info(DwarfError::MalformedVariable(description))
                })?;
                let TypeKind::Pointer {
                    target: Some(target),
                    ..
                } = pointer.kind
                else {
                    return Err(malformed(
                        "a variable a closure captured by reference is not a pointer",
                    ));
                };

                (Arc::from(name), target.id, true)
            }
        };

        let member = RecordMember {
            name: Some(name),
            type_ref: TypeReference { image, id: type_id },
            layout: RecordMemberLayout::ByteOffset(capture.offset),
            accessibility: Accessibility::Public,
            artificial: false,
            embedded: false,
            declaration: None,
        };

        Ok((member, by_reference))
    }

    /// The float type of a complex type's parts, if it is complex.
    fn complex_part_type(&self, base: &BaseType) -> Option<TypeId> {
        if base.encoding != BaseTypeEncoding::ComplexFloating {
            return None;
        }

        let part = complex_part(base);
        self.type_facts()
            .complex_part(&part.base_name, part.byte_size)
    }

    fn child_reference(
        storage: &ValueStorage,
        context: VariableContext,
        target_type: TypeId,
        total: u64,
        active_variant: Option<usize>,
    ) -> Arc<ValueChildrenReference> {
        Arc::new(ValueChildrenReference {
            stop_id: context.stop_id,
            context: context.context,
            frame: context.frame,
            module: context.module,
            image: context.image,
            context_address: context.address,
            target_type,
            storage: storage.clone(),
            total,
            active_variant,
            view: None,
            placement: None,
        })
    }

    fn bit_field_storage(
        &self,
        storage: &ValueStorage,
        type_id: TypeId,
        bit_offset: u64,
        bit_size: u64,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<ValueStorage, EvaluateError> {
        let shape = self.value_shape(type_id).map_err(EvaluateError::from)?;
        let base = match &shape {
            ValueShape::Scalar(base) => base,
            ValueShape::Enumeration { representation, .. } => representation,
            _ => {
                return Err(EvaluateError::Unavailable(
                    VariableUnavailableReason::ValueAccess(
                        crate::ValueAccessUnavailableReason::NonIntegralBitField,
                    ),
                ));
            }
        };

        let storage_bits = base
            .byte_size
            .checked_mul(8)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        if bit_size == 0 || bit_size > storage_bits || bit_size > 128 {
            return Err(EvaluateError::Malformed(
                "bit-field width exceeds its declared scalar storage".into(),
            ));
        }

        let mut value = storage::read_bits(
            storage,
            bit_offset,
            bit_size,
            self.target.byte_order,
            runtime,
            budget,
        )?;

        if matches!(
            base.encoding,
            BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
        ) && bit_size < 128
            && value & (1_u128 << (bit_size - 1)) != 0
        {
            value |= u128::MAX << bit_size;
        }

        let byte_size = usize::try_from(base.byte_size)
            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let full = value.to_le_bytes();
        let mut raw = full[..byte_size].to_vec();

        if self.target.byte_order == ByteOrder::Big {
            raw.reverse();
        }

        let raw: Arc<[u8]> = raw.into();
        let end = raw.len();

        Ok(ValueStorage::Bytes {
            source: VariableValueSource::Computed,
            raw,
            start: 0,
            end,
            address: None,
        })
    }

    fn runtime_member_storage(
        &self,
        storage: &ValueStorage,
        aggregate: TypeId,
        child: DynamicAggregateChild,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<ValueStorage, EvaluateError> {
        let object_address = storage::concrete_address(storage).ok_or({
            EvaluateError::Unavailable(VariableUnavailableReason::ValueAccess(
                crate::ValueAccessUnavailableReason::NoConcreteObjectAddress,
            ))
        })?;
        let key = DynamicAggregateLayoutKey { aggregate, child };
        let address = evaluate_dynamic_aggregate_address(
            self.type_facts(),
            self.locations(),
            key,
            self.endian,
            runtime,
            budget,
            object_address,
        )?;

        Ok(ValueStorage::Memory(address))
    }

    fn active_variant_from_storage(
        &self,
        storage: &ValueStorage,
        aggregate: TypeId,
        discriminant: &VariantDiscriminant,
        variants: &[Variant],
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<Option<usize>, EvaluateError> {
        self.variant_selection_from_storage(
            storage,
            aggregate,
            discriminant,
            variants,
            runtime,
            budget,
        )
        .map(|(_, active)| active)
    }

    fn variant_selection_from_storage(
        &self,
        storage: &ValueStorage,
        aggregate: TypeId,
        discriminant: &VariantDiscriminant,
        variants: &[Variant],
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<(Option<IntegerValue>, Option<usize>), EvaluateError> {
        let VariantDiscriminant::Stored(member) = discriminant else {
            // Without stored discriminator bytes, only a variant known to be
            // the only one that can hold a value is active.
            return tagless_variant(&*self.types, aggregate)
                .map(|index| (None, Some(index)))
                .ok_or_else(|| {
                    EvaluateError::Unavailable(
                        crate::UnsupportedVariableFeature::TypeRepresentation.into(),
                    )
                });
        };

        let shape = self.value_shape(member.type_ref.id)?;
        let representation = match &shape {
            ValueShape::Scalar(base)
                if !matches!(
                    base.encoding,
                    BaseTypeEncoding::Floating | BaseTypeEncoding::ComplexFloating
                ) =>
            {
                base
            }
            ValueShape::Enumeration { representation, .. } => representation,
            _ => {
                return Err(EvaluateError::Malformed(
                    "variant discriminator type is not integral".into(),
                ));
            }
        };

        let selected = self.aggregate_child_storage(
            storage,
            aggregate,
            DynamicAggregateChild::Discriminant,
            member.type_ref.id,
            member.layout,
            runtime,
            budget,
        )?;
        let size = usize::try_from(representation.byte_size)
            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let (_, raw) = storage::read(&selected, size, runtime, budget)?;
        let value = decode_integer_value(representation, &raw, self.target.byte_order)
            .map_err(EvaluateError::Malformed)?;
        let active = selected_variant_index(variants, value).map_err(EvaluateError::Malformed)?;

        Ok((Some(value), active))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "implicit-pointer resolution requires target bounds and the shared evaluation context"
    )]
    fn resolve_implicit_pointer(
        &self,
        debug_info_offset: u64,
        byte_offset: i64,
        target: TypeId,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<ValueStorage, EvaluateError> {
        let Some(object) = self.catalog().object_at_offset(debug_info_offset) else {
            return self.procedure_referent(
                debug_info_offset,
                byte_offset,
                target,
                address,
                runtime,
                budget,
            );
        };

        let referenced_type = match object.type_info() {
            TypeResolution::Resolved(id) => id,
            TypeResolution::Malformed(description) => {
                return Err(EvaluateError::Malformed(description));
            }
        };

        let referenced_size = self.value_shape(referenced_type)?.byte_size();
        let target_size = self.value_shape(target)?.byte_size();
        implicit_pointer_range(byte_offset, target_size, referenced_size)
            .map_err(EvaluateError::Unavailable)?;
        let referenced =
            self.located_data_object(object, address, runtime, frame_base_cache, budget)?;
        if matches!(referenced, ValueStorage::ImplicitPointer { .. }) {
            return Err(EvaluateError::Unavailable(
                crate::UnsupportedVariableFeature::CrossDieEvaluation.into(),
            ));
        }

        storage::offset(referenced, byte_offset)
    }

    /// The bytes an implicit pointer points at in a `DW_TAG_dwarf_procedure`,
    /// such as a string literal GCC keeps as an implicit value, which bounds
    /// it.
    fn procedure_referent(
        &self,
        debug_info_offset: u64,
        byte_offset: i64,
        target: TypeId,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<ValueStorage, EvaluateError> {
        let procedure = self.catalog().procedure(debug_info_offset).ok_or_else(|| {
            EvaluateError::Unavailable(crate::UnsupportedVariableFeature::CrossDieEvaluation.into())
        })?;
        let location = match procedure {
            Metadata::Value(location) => location,
            Metadata::Absent(_) => {
                return Err(VariableUnavailableReason::OptimizedOut(
                    crate::OptimizedOutReason::NoLocation,
                )
                .into());
            }
            Metadata::Malformed(description) => {
                return Err(EvaluateError::Malformed(description));
            }
        };

        let expression = select(self.locations().list(location), address)
            .map_err(|error| match error {
                LocationSelectionError::Unavailable(reason) => EvaluateError::Unavailable(reason),
                LocationSelectionError::Malformed(description) => {
                    EvaluateError::Malformed(description)
                }
            })?
            .ok_or(EvaluateError::Unavailable(
                VariableUnavailableReason::UnavailableAtInstruction,
            ))?;

        // A procedure belongs to no function, so it has no frame base.
        let pieces = evaluate(
            expression,
            self.endian,
            address,
            &mut FrameBase::Unsupported,
            runtime,
            budget,
        )?;
        let [
            gimli::Piece {
                size_in_bits: None,
                bit_offset: None,
                location: gimli::Location::Bytes { value },
            },
        ] = pieces.as_slice()
        else {
            return Err(crate::UnsupportedVariableFeature::ImplicitPointer.into());
        };

        let raw: Arc<[u8]> = Arc::from(value.slice());
        let target_size = self.value_shape(target)?.byte_size();
        let (start, _) = implicit_pointer_range(byte_offset, target_size, raw.len() as u64)
            .map_err(EvaluateError::Unavailable)?;

        // What follows the referent, such as the rest of a string, is the
        // procedure's too.
        let end = raw.len();
        Ok(ValueStorage::Bytes {
            source: VariableValueSource::Constant,
            raw,
            start: usize::try_from(start)
                .map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
            end,
            address: None,
        })
    }

    /// Applies planned steps to storage, with `indices` as the values of
    /// their index step. An index outside an array's static bounds is an
    /// error; anything the program state cannot provide is an
    /// [`EvaluateError`].
    #[expect(
        clippy::too_many_arguments,
        reason = "steps run against one frame's runtime, frame base, and budget"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "each storage transition keeps its own typed failure"
    )]
    pub(super) fn apply_steps(
        &self,
        mut storage: ValueStorage,
        steps: &[PathStep],
        indices: &[i128],
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        frame_base: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<ValueStorage, EvaluateError>> {
        macro_rules! attempt {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(error) => return Ok(Err(error)),
                }
            };
        }

        for step in steps {
            storage = match step {
                PathStep::Dereference {
                    target,
                    byte_size,
                    address_class,
                } => {
                    storage = storage::narrow(storage, *byte_size);

                    if let ValueStorage::ImplicitPointer {
                        debug_info_offset,
                        byte_offset,
                    } = storage
                    {
                        attempt!(self.resolve_implicit_pointer(
                            debug_info_offset,
                            byte_offset,
                            *target,
                            address,
                            runtime,
                            frame_base,
                            budget,
                        ))
                    } else {
                        if *address_class != 0 {
                            return Ok(Err(EvaluateError::Unavailable(
                                VariableUnavailableReason::ValueAccess(
                                    crate::ValueAccessUnavailableReason::AddressClass(
                                        *address_class,
                                    ),
                                ),
                            )));
                        }

                        let size =
                            attempt!(usize::try_from(*byte_size).map_err(|_| {
                                VariableUnavailableReason::EvaluationLimit.into()
                            }));
                        let (_, raw) = attempt!(storage::read(&storage, size, runtime, budget));
                        let pointer = attempt!(decode_address(&raw, *byte_size, self.target));
                        if pointer.get() == 0 {
                            return Ok(Err(EvaluateError::Unavailable(
                                VariableUnavailableReason::ValueAccess(
                                    crate::ValueAccessUnavailableReason::NullPointer,
                                ),
                            )));
                        }

                        ValueStorage::Memory(pointer)
                    }
                }
                PathStep::ArrayIndex { .. } => {
                    let byte_offset = array_byte_offset(step, indices)?.unwrap_or_default();
                    attempt!(storage::offset(storage, byte_offset))
                }
                PathStep::RuntimeArrayIndex { array } => {
                    let shape = attempt!(self.value_shape(*array).map_err(EvaluateError::from));

                    match attempt!(
                        self.resolve_runtime_array(&shape, &storage, address, runtime, budget)
                    ) {
                        Resolved::Elements {
                            data,
                            dimensions,
                            strides,
                        } => {
                            if indices.len() != dimensions.len() {
                                return Err(Error::InvalidValueExpression(format!(
                                    "an array of {} dimensions takes as many indices, not {}",
                                    dimensions.len(),
                                    indices.len()
                                )));
                            }

                            let at = attempt!(element_offset(&dimensions, &strides, indices));
                            attempt!(storage::offset(data, at))
                        }
                        Resolved::NotAllocated | Resolved::NotAssociated => {
                            return Ok(Err(EvaluateError::Unavailable(
                                VariableUnavailableReason::ValueAccess(
                                    crate::ValueAccessUnavailableReason::NullPointer,
                                ),
                            )));
                        }
                    }
                }

                PathStep::SliceIndex {
                    element_size,
                    descriptor_size,
                    words,
                } => {
                    let [index] = indices else {
                        return Err(Error::InvalidValueExpression(format!(
                            "a slice takes one index, not {}",
                            indices.len()
                        )));
                    };

                    let index =
                        u64::try_from(*index).map_err(|_| Error::ValueIndexOutOfBounds {
                            index: *index,
                            lower_bound: 0,
                            count: 0,
                        })?;
                    let decoded = attempt!(self.decode_slice(
                        &storage,
                        *descriptor_size,
                        *words,
                        runtime,
                        budget,
                    ));
                    if index >= decoded.length {
                        return Ok(Err(EvaluateError::Unavailable(
                            VariableUnavailableReason::IndexOutOfBounds {
                                index: i128::from(index),
                                lower_bound: 0,
                                count: decoded.length,
                            },
                        )));
                    }

                    let byte_offset = attempt!(
                        index
                            .checked_mul(*element_size)
                            .and_then(|offset| i64::try_from(offset).ok())
                            .ok_or_else(|| VariableUnavailableReason::EvaluationLimit.into())
                    );
                    attempt!(storage::offset(
                        ValueStorage::Memory(decoded.address),
                        byte_offset,
                    ))
                }
                PathStep::Member(step) => {
                    let PlannedMemberStep {
                        aggregate,
                        child,
                        member,
                        required_variant,
                    } = step.as_ref();

                    if let Some((required, discriminant, variants)) = required_variant {
                        let active = attempt!(self.active_variant_from_storage(
                            &storage,
                            *aggregate,
                            discriminant,
                            variants,
                            runtime,
                            budget,
                        ));
                        if active != Some(*required) {
                            let name = variants
                                .get(*required)
                                .and_then(|variant| variant.name.as_deref())
                                .unwrap_or("<anonymous>");
                            return Ok(Err(EvaluateError::Unavailable(
                                VariableUnavailableReason::ValueAccess(
                                    crate::ValueAccessUnavailableReason::InactiveVariant(Some(
                                        name.into(),
                                    )),
                                ),
                            )));
                        }
                    }

                    attempt!(self.aggregate_child_storage(
                        &storage,
                        *aggregate,
                        *child,
                        member.type_ref.id,
                        member.layout,
                        runtime,
                        budget,
                    ))
                }
                PathStep::Unavailable(reason) => {
                    return Ok(Err(EvaluateError::Unavailable(reason.clone())));
                }
            };
        }

        Ok(Ok(storage))
    }

    pub(super) fn materialize_inspected_value(
        &self,
        type_id: TypeId,
        type_info: TypeInfo,
        storage: &ValueStorage,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue> {
        let shape = match self.value_shape(type_id) {
            Ok(shape) => shape,
            Err(error) => {
                return Ok(inspected_value(
                    Some(type_info),
                    shape_error_state(error),
                    budget,
                ));
            }
        };

        let state =
            self.materialize_value_state(type_id, &shape, storage, context, runtime, budget)?;
        Ok(inspected_value(Some(type_info), state, budget))
    }

    /// Decodes a value without its text.
    pub(super) fn decode_state(
        &self,
        type_id: TypeId,
        storage: &ValueStorage,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<VariableState> {
        match self.value_shape(type_id) {
            Ok(shape) => {
                self.materialize_shape_state(type_id, &shape, storage, context, runtime, budget)
            }
            Err(error) => Ok(shape_error_state(error)),
        }
    }

    /// Materializes a value, with its text when it is a string, and checks
    /// that a pointer can be dereferenced.
    fn materialize_value_state(
        &self,
        type_id: TypeId,
        shape: &ValueShape,
        storage: &ValueStorage,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<VariableState> {
        let mut state =
            self.materialize_shape_state(type_id, shape, storage, context, runtime, budget)?;

        // An array bounded at run time found its text where its elements are.
        if let VariableState::Available { value, text, .. } = &mut state
            && !matches!(shape, ValueShape::RuntimeArray { .. })
        {
            *text = self
                .text_summary(
                    type_id,
                    shape,
                    value,
                    storage,
                    context.address,
                    runtime,
                    budget,
                )
                .map(Arc::new);
        }

        self.constrain_dereference(&mut state, shape);
        Ok(state)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "each value shape has a distinct lazy summary and child capability"
    )]
    fn materialize_shape_state(
        &self,
        type_id: TypeId,
        shape: &ValueShape,
        storage: &ValueStorage,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<VariableState> {
        let storage = &storage::narrow(storage.clone(), shape.byte_size());
        let leaf = |source, raw, value, dereference| VariableState::Available {
            source,
            raw: Some(raw),
            value,
            dereference,
            children: ValueChildren::NotApplicable,
            text: None,
            presentation: None,
        };

        let with_children = |value, total, active| VariableState::Available {
            source: storage::source(storage),
            raw: None,
            value,
            dereference: DereferenceState::NotApplicable,
            children: ValueChildren::Available(Self::child_reference(
                storage, context, type_id, total, active,
            )),
            text: None,
            presentation: None,
        };

        let read =
            |size: u64,
             runtime: &mut dyn VariableRuntime,
             budget: &mut InspectionBudget|
             -> std::result::Result<(VariableValueSource, Arc<[u8]>), EvaluateError> {
                let size = usize::try_from(size)
                    .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
                storage::read(storage, size, runtime, budget)
            };

        Ok(match shape {
            ValueShape::Scalar(base) => match self.read_scalar(storage, base, runtime, budget) {
                Ok((source, raw, padding)) => match decode_scalar(base, &raw, self.target) {
                    // Bytes the program does not hold are no part of the
                    // value it shows.
                    Ok(value) if padding => {
                        let Some(significant) =
                            usize::try_from(significant_bytes(base, self.target))
                                .ok()
                                .and_then(|count| raw.get(..count))
                        else {
                            return Err(Error::internal(
                                "a scalar's significant bytes exceed its storage",
                            ));
                        };

                        leaf(
                            source,
                            Arc::from(significant),
                            VariableValue::Scalar(value),
                            DereferenceState::NotApplicable,
                        )
                    }

                    // A complex number's parts are its children.
                    Ok(value @ ScalarValue::Complex { .. })
                        if self.complex_part_type(base).is_some() =>
                    {
                        VariableState::Available {
                            source,
                            raw: Some(raw),
                            value: VariableValue::Scalar(value),
                            dereference: DereferenceState::NotApplicable,
                            children: ValueChildren::Available(Self::child_reference(
                                storage, context, type_id, 2, None,
                            )),
                            text: None,
                            presentation: None,
                        }
                    }
                    Ok(value) => leaf(
                        source,
                        raw,
                        VariableValue::Scalar(value),
                        DereferenceState::NotApplicable,
                    ),
                    Err(ScalarDecodeError::Unavailable(reason)) => {
                        VariableState::Unavailable(reason)
                    }
                    Err(ScalarDecodeError::Invalid(reason)) => VariableState::Invalid {
                        source,
                        raw,
                        reason,
                    },
                    Err(ScalarDecodeError::Malformed(description)) => VariableState::Malformed(
                        malformed_reason(VariableMalformedKind::InconsistentLayout, description),
                    ),
                },
                Err(error) => {
                    evaluate_error_state(error, VariableMalformedKind::InvalidExpression)?
                }
            },
            ValueShape::Enumeration {
                representation,
                enumerators,
                byte_size,
                origin,
            } => match read(*byte_size, runtime, budget) {
                Ok((source, raw)) => {
                    match decode_integer_value(representation, &raw, self.target.byte_order) {
                        Ok(value) => {
                            let value = symbolic(value, enumerators, *origin, *byte_size);
                            leaf(source, raw, value, DereferenceState::NotApplicable)
                        }
                        Err(description) => VariableState::Malformed(malformed_reason(
                            VariableMalformedKind::InvalidTypeGraph,
                            description,
                        )),
                    }
                }
                Err(error) => {
                    evaluate_error_state(error, VariableMalformedKind::InvalidExpression)?
                }
            },
            ValueShape::Indirection {
                target,
                byte_size,
                address_class,
            } => match storage {
                ValueStorage::ImplicitPointer {
                    debug_info_offset,
                    byte_offset,
                } => {
                    let dereference = target.map_or_else(
                        || DereferenceState::Unavailable {
                            pointee: None,
                            reason: DereferenceUnavailableReason::UnspecifiedPointee,
                        },
                        |target_type| {
                            DereferenceState::Available(Box::new(dereference_reference(
                                context,
                                target_type,
                                crate::model::DereferenceTarget::ImplicitPointer {
                                    debug_info_offset: *debug_info_offset,
                                    byte_offset: *byte_offset,
                                },
                            )))
                        },
                    );
                    VariableState::Available {
                        source: VariableValueSource::ImplicitPointer,
                        raw: None,
                        value: VariableValue::ImplicitPointer,
                        dereference,
                        children: ValueChildren::NotApplicable,
                        text: None,
                        presentation: None,
                    }
                }
                _ => match read(*byte_size, runtime, budget) {
                    Ok((source, raw)) => match decode_address(&raw, *byte_size, self.target) {
                        Ok(address) => {
                            let dereference = if *address_class != 0 {
                                DereferenceState::Unavailable {
                                    pointee: None,
                                    reason: DereferenceUnavailableReason::AddressClass(
                                        *address_class,
                                    ),
                                }
                            } else if address.get() == 0 {
                                DereferenceState::Unavailable {
                                    pointee: None,
                                    reason: DereferenceUnavailableReason::Null,
                                }
                            } else if let Some(target_type) = target {
                                DereferenceState::Available(Box::new(dereference_reference(
                                    context,
                                    *target_type,
                                    crate::model::DereferenceTarget::Address(address),
                                )))
                            } else {
                                DereferenceState::Unavailable {
                                    pointee: None,
                                    reason: DereferenceUnavailableReason::UnspecifiedPointee,
                                }
                            };

                            // A pointer to code names the function it enters.
                            let function = target
                                .filter(|target| self.is_code(*target) && address.get() != 0)
                                .and_then(|_| runtime.function_at(address));

                            leaf(
                                source,
                                raw,
                                VariableValue::Address(AddressValue { address, function }),
                                dereference,
                            )
                        }
                        Err(error) => {
                            evaluate_error_state(error, VariableMalformedKind::InconsistentLayout)?
                        }
                    },
                    Err(error) => {
                        evaluate_error_state(error, VariableMalformedKind::InvalidExpression)?
                    }
                },
            },
            ValueShape::Function { byte_size } => match read(*byte_size, runtime, budget) {
                Ok((source, raw)) => {
                    match self.function_value(&raw, *byte_size, type_id, context, runtime, budget) {
                        Ok((value, children)) => VariableState::Available {
                            source,
                            raw: Some(raw),
                            value,
                            dereference: DereferenceState::NotApplicable,
                            children,
                            text: None,
                            presentation: None,
                        },
                        Err(error) => {
                            evaluate_error_state(error, VariableMalformedKind::InconsistentLayout)?
                        }
                    }
                }
                Err(error) => {
                    evaluate_error_state(error, VariableMalformedKind::InvalidExpression)?
                }
            },
            ValueShape::Array { dimensions, .. } => {
                let Some(total) = dimensions
                    .iter()
                    .try_fold(1_u64, |total, dimension| total.checked_mul(dimension.count))
                else {
                    return Ok(VariableState::Unavailable(
                        VariableUnavailableReason::EvaluationLimit,
                    ));
                };

                with_children(
                    VariableValue::Array {
                        dimensions: Arc::clone(dimensions),
                    },
                    total,
                    None,
                )
            }
            ValueShape::RuntimeArray { .. } => {
                let resolved = match self.resolve_runtime_array(
                    shape,
                    storage,
                    context.address,
                    runtime,
                    budget,
                ) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        return evaluate_error_state(
                            error,
                            VariableMalformedKind::InconsistentLayout,
                        );
                    }
                };

                let (data, bounds, strides) = match resolved {
                    Resolved::Elements {
                        data,
                        dimensions,
                        strides,
                    } => (data, dimensions, strides),
                    absent @ (Resolved::NotAllocated | Resolved::NotAssociated) => {
                        return Ok(VariableState::Available {
                            source: storage::source(storage),
                            raw: None,
                            value: if matches!(absent, Resolved::NotAllocated) {
                                VariableValue::NotAllocated
                            } else {
                                VariableValue::NotAssociated
                            },
                            dereference: DereferenceState::NotApplicable,
                            children: ValueChildren::NotApplicable,
                            text: None,
                            presentation: None,
                        });
                    }
                };

                let Some(placement) = Resolved::placement(&bounds, &strides) else {
                    return Ok(VariableState::Unavailable(
                        VariableUnavailableReason::ValueAccess(
                            crate::ValueAccessUnavailableReason::UnknownLength,
                        ),
                    ));
                };

                let Some(total) = placement
                    .dimensions
                    .iter()
                    .try_fold(1_u64, |total, dimension| total.checked_mul(dimension.count))
                else {
                    return Ok(VariableState::Unavailable(
                        VariableUnavailableReason::EvaluationLimit,
                    ));
                };

                let value = VariableValue::Array {
                    dimensions: Arc::clone(&placement.dimensions),
                };

                let text = self
                    .resolved_text(type_id, shape, &data, &placement, runtime, budget)
                    .map(Arc::new);
                let mut reference = Self::child_reference(&data, context, type_id, total, None);
                Arc::make_mut(&mut reference).placement = Some(Arc::new(placement));

                VariableState::Available {
                    source: storage::source(&data),
                    raw: None,
                    value,
                    dereference: DereferenceState::NotApplicable,
                    children: ValueChildren::Available(reference),
                    text,
                    presentation: None,
                }
            }

            ValueShape::Slice {
                element: _,
                byte_size,
                words,
                ..
            } => {
                let decoded = match self.decode_slice(storage, *byte_size, *words, runtime, budget)
                {
                    Ok(value) => value,
                    Err(error) => {
                        return evaluate_error_state(
                            error,
                            VariableMalformedKind::InconsistentLayout,
                        );
                    }
                };

                let backing = ValueStorage::Memory(decoded.address);
                VariableState::Available {
                    source: decoded.source,
                    raw: Some(decoded.raw),
                    value: VariableValue::Slice {
                        length: decoded.length,
                        capacity: decoded.capacity,
                    },
                    dereference: DereferenceState::NotApplicable,
                    children: ValueChildren::Available(Self::child_reference(
                        &backing,
                        context,
                        type_id,
                        decoded.length,
                        None,
                    )),
                    text: None,
                    presentation: None,
                }
            }
            ValueShape::Record { members, bases, .. } => {
                let total =
                    u64::try_from(bases.len().saturating_add(members.len())).unwrap_or(u64::MAX);
                with_children(VariableValue::Record, total, None)
            }
            ValueShape::Union { members, .. } => {
                let total = u64::try_from(members.len()).unwrap_or(u64::MAX);
                with_children(VariableValue::Union, total, None)
            }

            ValueShape::Variant {
                aggregate,
                common_members,
                bases,
                discriminant,
                variants,
                ..
            } => {
                let (discriminant_value, active) = match self.variant_selection_from_storage(
                    storage,
                    *aggregate,
                    discriminant,
                    variants,
                    runtime,
                    budget,
                ) {
                    Ok(active) => active,
                    Err(error) => {
                        return evaluate_error_state(
                            error,
                            VariableMalformedKind::InconsistentLayout,
                        );
                    }
                };

                let total = bases
                    .len()
                    .saturating_add(common_members.len())
                    .saturating_add(
                        active
                            .and_then(|index| variants.get(index))
                            .map_or(0, |variant| variant.members.len()),
                    );
                with_children(
                    VariableValue::Variant {
                        discriminant: discriminant_value,
                        active: active
                            .and_then(|index| variants.get(index))
                            .cloned()
                            .map(Arc::new),
                    },
                    u64::try_from(total).unwrap_or(u64::MAX),
                    active,
                )
            }
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "child layout evaluation keeps aggregate identity, storage, type, runtime, and budget explicit"
    )]
    fn aggregate_child_storage(
        &self,
        storage: &ValueStorage,
        aggregate: TypeId,
        child: DynamicAggregateChild,
        type_id: TypeId,
        layout: RecordMemberLayout,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<ValueStorage, EvaluateError> {
        match layout {
            RecordMemberLayout::ByteOffset(offset) => storage::offset(
                storage.clone(),
                i64::try_from(offset).map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
            ),
            RecordMemberLayout::BitRange {
                bit_offset,
                bit_size,
            } => self.bit_field_storage(storage, type_id, bit_offset, bit_size, runtime, budget),
            RecordMemberLayout::Runtime => {
                self.runtime_member_storage(storage, aggregate, child, runtime, budget)
            }
        }
    }

    fn materialize_child(
        &self,
        relationship: ValueChildRelationship,
        type_id: TypeId,
        storage: std::result::Result<ValueStorage, EvaluateError>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChild> {
        let type_info = self
            .type_info(type_id)
            .map_err(|description| Error::debug_info(DwarfError::MalformedVariable(description)))?
            .clone();
        let state = match storage {
            Err(error) => evaluate_error_state(error, VariableMalformedKind::InvalidExpression)?,
            Ok(storage) => match self.value_shape(type_id) {
                Err(error) => shape_error_state(error),
                Ok(shape) => self
                    .materialize_value_state(type_id, &shape, &storage, context, runtime, budget)?,
            },
        };

        Ok(ValueChild {
            relationship,
            type_info,
            state,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "stable child ordering and each aggregate layout are intentionally handled together"
    )]
    pub(super) fn value_child_page(
        &self,
        reference: &ValueChildrenReference,
        offset: u64,
        limit: u32,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChildPage> {
        let context = VariableContext {
            stop_id: reference.stop_id,
            context: reference.context,
            frame: reference.frame,
            module: reference.module,
            image: reference.image,
            address: reference.context_address,
        };

        let mut storage = reference.storage.clone();
        let mut storage_failure = None;

        if let ValueStorage::ImplicitPointer {
            debug_info_offset,
            byte_offset,
        } = storage.clone()
        {
            let mut frame_base = FrameBaseCache::Empty;
            match self.resolve_implicit_pointer(
                debug_info_offset,
                byte_offset,
                reference.target_type,
                reference.context_address,
                runtime,
                &mut frame_base,
                budget,
            ) {
                Ok(resolved) => storage = resolved,
                Err(error) => storage_failure = Some(error),
            }
        }

        let shape = self
            .value_shape(reference.target_type)
            .map_err(shape_error)?;

        // An array bounded at run time is the array its value found.
        let shape = match (shape, &reference.placement) {
            (
                ValueShape::RuntimeArray {
                    element, ordering, ..
                },
                Some(placement),
            ) => ValueShape::Array {
                element,
                dimensions: Arc::clone(&placement.dimensions),
                ordering,
                byte_size: 0,
            },
            (ValueShape::RuntimeArray { .. }, None) => {
                return Err(Error::debug_info(DwarfError::MalformedVariable(
                    "an array bounded at run time has no bounds for its children".into(),
                )));
            }
            (shape, _) => shape,
        };

        // A closure's children are what it captured.
        let captures = match &shape {
            ValueShape::Function { byte_size } => Some(
                self.closure_captures(&storage, *byte_size, runtime, budget)?
                    .iter()
                    .map(|capture| self.capture_member(capture, reference.image))
                    .collect::<Result<Vec<_>>>()?,
            ),
            _ => None,
        };

        let expected_total = match &shape {
            ValueShape::Function { .. } => captures
                .as_ref()
                .and_then(|captures| u64::try_from(captures.len()).ok()),
            ValueShape::Array { dimensions, .. } => dimensions
                .iter()
                .try_fold(1_u64, |total, dimension| total.checked_mul(dimension.count)),
            ValueShape::Slice { .. } => Some(reference.total),
            ValueShape::Record { members, bases, .. } => {
                u64::try_from(members.len().saturating_add(bases.len())).ok()
            }
            ValueShape::Union { members, .. } => u64::try_from(members.len()).ok(),
            ValueShape::Scalar(base) if base.encoding == BaseTypeEncoding::ComplexFloating => {
                Some(2)
            }

            ValueShape::Variant {
                common_members,
                bases,
                variants,
                ..
            } => {
                let active_members = reference.active_variant.map_or(Some(0), |active| {
                    variants.get(active).map(|variant| variant.members.len())
                });
                active_members.and_then(|active_members| {
                    u64::try_from(
                        bases
                            .len()
                            .saturating_add(common_members.len())
                            .saturating_add(active_members),
                    )
                    .ok()
                })
            }
            _ => None,
        };

        if expected_total != Some(reference.total) {
            return Err(Error::debug_info(DwarfError::MalformedVariable(
                "value child capability does not match its aggregate metadata".into(),
            )));
        }

        let full_requested_end = reference.total.min(offset.saturating_add(u64::from(limit)));
        if offset >= full_requested_end {
            return Ok(ValueChildPage {
                stop_id: reference.stop_id,
                offset,
                total: reference.total,
                children: Arc::from([]),
                completion: budget.completion(),
                usage: budget.usage(),
            });
        }

        let requested_end =
            full_requested_end.min(offset.saturating_add(budget.remaining_value_nodes()));
        if requested_end == offset {
            let _ = budget.consume_value_nodes(1);
            return Ok(ValueChildPage {
                stop_id: reference.stop_id,
                offset,
                total: reference.total,
                children: Arc::from([]),
                completion: budget.completion(),
                usage: budget.usage(),
            });
        }

        let page_len = usize::try_from(requested_end - offset)
            .map_err(|_| Error::internal("child page length exceeds usize"))?;
        let mut children = Vec::with_capacity(page_len);

        // Linear pages are the common high-volume path. Capture one bounded
        // contiguous interval so scalar children do not turn a 256-element
        // page into 256 ptrace reads. If the coalesced read crosses an
        // inaccessible boundary, fall back to per-child reads so accessible
        // siblings are still reported accurately.
        let linear_storage = if storage_failure.is_some() {
            None
        } else {
            match &shape {
                // Strides place each element of an array bounded at run
                // time.
                ValueShape::Array { .. } if reference.placement.is_some() => None,
                // An array laid out column by column keeps a page's rows
                // apart.
                ValueShape::Array {
                    dimensions,
                    ordering: ArrayOrdering::ColumnMajor,
                    ..
                } if dimensions.len() > 1 => None,
                ValueShape::Array { element, .. } | ValueShape::Slice { element, .. } => {
                    let element_shape = self.value_shape(*element).ok();
                    let requires_bytes = element_shape.as_ref().is_some_and(|shape| {
                        matches!(
                            shape,
                            ValueShape::Scalar(_)
                                | ValueShape::Enumeration { .. }
                                | ValueShape::Indirection { .. }
                                | ValueShape::Slice { .. }
                        )
                    });
                    let stride = element_shape.as_ref().map(ValueShape::byte_size);

                    match (&storage, stride, requires_bytes) {
                        (ValueStorage::Memory(_), Some(stride), true) => {
                            let count = requested_end - offset;
                            let span = count
                                .checked_sub(1)
                                .and_then(|count| count.checked_mul(stride))
                                .and_then(|prefix| prefix.checked_add(stride))
                                .and_then(|span| usize::try_from(span).ok());
                            let first = offset
                                .checked_mul(stride)
                                .and_then(|offset| i64::try_from(offset).ok())
                                .and_then(|offset| storage::offset(storage.clone(), offset).ok());

                            match (first, span) {
                                (Some(first), Some(span))
                                    if span <= MAX_EVALUATION_MEMORY_BYTES
                                        && budget.remaining_memory_reads() != 0
                                        && u64::try_from(span).is_ok_and(|span| {
                                            span <= budget.remaining_memory_bytes()
                                        }) =>
                                {
                                    storage::read(&first, span, runtime, budget).ok().map(
                                        |(source, raw)| {
                                            let end = raw.len();
                                            ValueStorage::Bytes {
                                                source,
                                                raw,
                                                start: 0,
                                                end,
                                                address: storage::concrete_address(&first),
                                            }
                                        },
                                    )
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        };

        // A complex number's children are its real and imaginary parts.
        let complex_parts = match &shape {
            ValueShape::Scalar(base) => self.complex_part_type(base).map(|part| {
                let half = base.byte_size / 2;
                let member = |name: &str, offset| RecordMember {
                    name: Some(name.into()),
                    type_ref: TypeReference {
                        image: reference.image,
                        id: part,
                    },
                    layout: RecordMemberLayout::ByteOffset(offset),
                    accessibility: Accessibility::Public,
                    artificial: false,
                    embedded: false,
                    declaration: None,
                };
                [member("real", 0), member("imag", half)]
            }),
            _ => None,
        };

        // An aggregate's children are its bases, then its members, then the
        // members of its active variant.
        let aggregate = match &shape {
            ValueShape::Record {
                record,
                members,
                bases,
                ..
            } => Some((*record, &bases[..], &members[..], None)),
            ValueShape::Union { union, members, .. } => Some((*union, &[][..], &members[..], None)),
            ValueShape::Variant {
                aggregate,
                common_members,
                bases,
                variants,
                ..
            } => Some((
                *aggregate,
                &bases[..],
                &common_members[..],
                reference
                    .active_variant
                    .map(|active| (active, &variants[active].members[..])),
            )),
            _ => complex_parts
                .as_ref()
                .map(|parts| (reference.target_type, &[][..], &parts[..], None)),
        };

        for index in offset..requested_end {
            if budget.consume_value_nodes(1).is_err() {
                break;
            }

            let (relationship, type_id, child_storage) = if let Some(captures) = &captures {
                let Some((member, by_reference)) = usize::try_from(index)
                    .ok()
                    .and_then(|index| captures.get(index))
                else {
                    return Err(Error::internal("child index is past its captures"));
                };

                let RecordMemberLayout::ByteOffset(capture_offset) = member.layout else {
                    return Err(Error::internal("captures are not located by their offset"));
                };

                let mut child_storage = i64::try_from(capture_offset)
                    .map_err(|_| VariableUnavailableReason::EvaluationLimit.into())
                    .and_then(|capture_offset| storage::offset(storage.clone(), capture_offset));

                if *by_reference {
                    let pointer_size = shape.byte_size();
                    child_storage = child_storage.and_then(|slot| {
                        let size = usize::try_from(pointer_size)
                            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
                        let (_, raw) = storage::read(&slot, size, runtime, budget)?;
                        let address = decode_address(&raw, pointer_size, self.target)?;
                        if address.get() == 0 {
                            return Err(VariableUnavailableReason::ValueAccess(
                                crate::ValueAccessUnavailableReason::NullPointer,
                            )
                            .into());
                        }

                        Ok(ValueStorage::Memory(address))
                    });
                }

                (
                    ValueChildRelationship::Member(member.clone()),
                    member.type_ref.id,
                    child_storage,
                )
            } else if let Some((aggregate, bases, members, variant)) = aggregate {
                let index = usize::try_from(index)
                    .map_err(|_| Error::internal("aggregate child index exceeds usize"))?;
                let (child, relationship, type_id, layout) = if let Some(base) = bases.get(index) {
                    (
                        DynamicAggregateChild::Base(index),
                        ValueChildRelationship::Base(base.clone()),
                        base.type_ref.id,
                        base.layout,
                    )
                } else {
                    let index = index - bases.len();
                    let (child, member) = if let Some(member) = members.get(index) {
                        (DynamicAggregateChild::Member(index), member)
                    } else {
                        let Some((variant, variant_members)) = variant else {
                            return Err(Error::debug_info(DwarfError::MalformedVariable(
                                "variant child capability has no active arm".into(),
                            )));
                        };

                        let member = index - members.len();
                        (
                            DynamicAggregateChild::VariantMember { variant, member },
                            &variant_members[member],
                        )
                    };

                    (
                        child,
                        ValueChildRelationship::Member(member.clone()),
                        member.type_ref.id,
                        member.layout,
                    )
                };

                let child_storage = self.aggregate_child_storage(
                    &storage, aggregate, child, type_id, layout, runtime, budget,
                );
                (relationship, type_id, child_storage)
            } else {
                let (ValueShape::Array { element, .. } | ValueShape::Slice { element, .. }) =
                    &shape
                else {
                    return Err(Error::debug_info(DwarfError::MalformedVariable(
                        "a non-aggregate value produced a child capability".into(),
                    )));
                };

                let stride = self.value_shape(*element).map_err(shape_error)?.byte_size();
                let storage_index = if linear_storage.is_some() {
                    index - offset
                } else {
                    index
                };

                let mut child_storage = storage_index
                    .checked_mul(stride)
                    .and_then(|offset| i64::try_from(offset).ok())
                    .ok_or_else(|| VariableUnavailableReason::EvaluationLimit.into())
                    .and_then(|offset| {
                        storage::offset(
                            linear_storage.clone().unwrap_or_else(|| storage.clone()),
                            offset,
                        )
                    });
                let relationship = match &shape {
                    ValueShape::Array {
                        dimensions,
                        ordering,
                        ..
                    } => {
                        let indices = array_source_indices(dimensions, index)?;

                        // Elements are listed row by row wherever they are.
                        if let Some(placement) = &reference.placement {
                            let bounds = placement
                                .dimensions
                                .iter()
                                .map(|dimension| (dimension.lower_bound, Some(dimension.count)))
                                .collect::<Vec<_>>();
                            child_storage = element_offset(&bounds, &placement.strides, &indices)
                                .and_then(|at| storage::offset(storage.clone(), at));
                        } else if *ordering == ArrayOrdering::ColumnMajor {
                            let at = element_byte_offset(dimensions, *ordering, &indices, stride)?;
                            child_storage = storage::offset(storage.clone(), at);
                        }

                        ValueChildRelationship::ArrayElement {
                            index,
                            indices: indices.into(),
                        }
                    }
                    _ => ValueChildRelationship::SliceElement { index },
                };

                (relationship, *element, child_storage)
            };

            let child_storage = storage_failure
                .clone()
                .map_or(child_storage, std::result::Result::Err);
            children.push(self.materialize_child(
                relationship,
                type_id,
                child_storage,
                context,
                runtime,
                budget,
            )?);

            if budget.exhaustion().is_some() {
                children.pop();
                break;
            }
        }

        if requested_end < full_requested_end && budget.exhaustion().is_none() {
            let _ = budget.consume_value_nodes(1);
        }

        Ok(ValueChildPage {
            stop_id: reference.stop_id,
            offset,
            total: reference.total,
            children: children.into(),
            completion: budget.completion(),
            usage: budget.usage(),
        })
    }

    pub(super) fn inspect_data_object(
        &self,
        variable: Object<'_>,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> Result<Variable> {
        let invalid = |description| {
            VariableState::Malformed(malformed_reason(
                VariableMalformedKind::InvalidAttribute,
                description,
            ))
        };

        let type_id = match (variable.malformed(), variable.type_info()) {
            (Some(description), _) | (None, TypeResolution::Malformed(description)) => {
                return Ok(data_object(variable, None, invalid(description)));
            }
            (None, TypeResolution::Resolved(id)) => id,
        };

        // A generic value has its type argument, or else its shape.
        let (type_id, unresolved_shape) =
            match self.generic_type(type_id, variable.instance(), address, runtime, budget)? {
                Generic::Plain => (type_id, None),
                Generic::Resolved(argument) => (argument, None),
                Generic::Unresolved(shape, reason) => (shape, Some(reason)),
            };
        let type_info = match self.type_info(type_id) {
            Ok(info) => info.clone(),
            Err(description) => return Ok(data_object(variable, None, invalid(description))),
        };

        let state = match self.value_shape(type_id) {
            Ok(shape) => {
                match self.located_data_object(variable, address, runtime, frame_base_cache, budget)
                {
                    Ok(storage) => self.materialize_value_state(
                        type_id, &shape, &storage, context, runtime, budget,
                    )?,
                    Err(error) => {
                        evaluate_error_state(error, VariableMalformedKind::InvalidAttribute)?
                    }
                }
            }
            Err(error) => shape_error_state(error),
        };

        let mut state = state;

        // A shape's pointer points to whatever its unknown type argument
        // does, not to the shape's pointee.
        if unresolved_shape.is_some()
            && let VariableState::Available { dereference, .. } = &mut state
            && matches!(dereference, DereferenceState::Available(_))
        {
            *dereference = DereferenceState::Unavailable {
                pointee: None,
                reason: DereferenceUnavailableReason::UnspecifiedPointee,
            };
        }

        let mut variable = data_object(variable, Some(type_info), state);
        variable.unresolved_shape = unresolved_shape;

        Ok(variable)
    }

    fn constrain_dereference(&self, state: &mut VariableState, shape: &ValueShape) {
        let ValueShape::Indirection {
            target: Some(target),
            ..
        } = shape
        else {
            return;
        };

        let VariableState::Available { dereference, .. } = state else {
            return;
        };

        // The pointee type describes the dereferenced expression, so it is
        // rendered against `*expr`. Resolve it once for both the downgrade of an
        // available dereference and the backfill of an already-unavailable one.
        let pointee = self.type_info(*target).ok().cloned().map(Box::new);

        match dereference {
            DereferenceState::Available(_) => {
                let reason = match self.type_info(*target) {
                    Err(description) => Some(DereferenceUnavailableReason::Malformed(
                        malformed_reason(VariableMalformedKind::InvalidTypeGraph, description),
                    )),
                    Ok(TypeInfo {
                        kind: TypeKind::Unspecified,
                        ..
                    }) => Some(DereferenceUnavailableReason::UnspecifiedPointee),
                    Ok(_) => match self.value_shape(*target) {
                        Ok(_) => None,
                        Err(ValueShapeError::Malformed(description)) => {
                            Some(DereferenceUnavailableReason::Malformed(malformed_reason(
                                VariableMalformedKind::InvalidTypeGraph,
                                description,
                            )))
                        }
                        Err(ValueShapeError::Unsupported(description)) => Some(
                            DereferenceUnavailableReason::UnsupportedPointee(description),
                        ),
                    },
                };

                if let Some(reason) = reason {
                    *dereference = DereferenceState::Unavailable { pointee, reason };
                }
            }
            DereferenceState::Unavailable { pointee: slot, .. } => {
                // Backfill the pointee metadata for reasons produced upstream
                // (a null pointer or an unsupported address class) that knew the
                // target type but did not resolve it.
                if slot.is_none() {
                    *slot = pointee;
                }
            }
            DereferenceState::NotApplicable => {}
        }
    }

    pub(super) fn dereference_value(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<DereferencedValue> {
        let type_info = self
            .type_info(reference.target_type)
            .map_err(|reason| Error::debug_info(DwarfError::MalformedVariable(reason)))?
            .clone();
        let state = match budget.consume_value_nodes(1) {
            Ok(()) => self.dereferenced_state(reference, runtime, budget)?,
            Err(exhaustion) => VariableState::Unavailable(exhaustion.into()),
        };

        Ok(DereferencedValue {
            type_info,
            state,
            completion: budget.completion(),
            usage: budget.usage(),
        })
    }

    fn dereferenced_state(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<VariableState> {
        let shape = match self.value_shape(reference.target_type) {
            Ok(shape) => shape,
            Err(error) => return Ok(shape_error_state(error)),
        };

        let storage = match reference.target {
            crate::model::DereferenceTarget::Address(address) => ValueStorage::Memory(address),
            crate::model::DereferenceTarget::ImplicitPointer {
                debug_info_offset,
                byte_offset,
            } => match self.resolve_implicit_pointer(
                debug_info_offset,
                byte_offset,
                reference.target_type,
                reference.context_address,
                runtime,
                &mut FrameBaseCache::Empty,
                budget,
            ) {
                Ok(storage) => storage,
                Err(error) => {
                    return evaluate_error_state(error, VariableMalformedKind::InvalidExpression);
                }
            },
        };

        let context = VariableContext {
            stop_id: reference.stop_id,
            context: reference.context,
            frame: reference.frame,
            module: reference.module,
            image: reference.image,
            address: reference.context_address,
        };

        self.materialize_value_state(
            reference.target_type,
            &shape,
            &storage,
            context,
            runtime,
            budget,
        )
    }
}

pub(super) fn implicit_pointer_range(
    byte_offset: i64,
    size: u64,
    containing_size: u64,
) -> std::result::Result<(u64, u64), VariableUnavailableReason> {
    let unavailable = || {
        VariableUnavailableReason::ValueAccess(
            crate::ValueAccessUnavailableReason::ImplicitPointerOutOfBounds {
                offset: byte_offset,
                size,
                containing_size,
            },
        )
    };

    let start = u64::try_from(byte_offset).map_err(|_| unavailable())?;
    let end = start
        .checked_add(size)
        .ok_or(VariableUnavailableReason::EvaluationLimit)?;
    if end > containing_size {
        return Err(unavailable());
    }

    Ok((start, end))
}

pub(super) const fn inspected_value(
    type_info: Option<TypeInfo>,
    state: VariableState,
    budget: &InspectionBudget,
) -> InspectedValue {
    InspectedValue {
        type_info,
        state,
        completion: budget.completion(),
        usage: budget.usage(),
    }
}

pub(super) fn data_object(
    variable: Object<'_>,
    type_info: Option<TypeInfo>,
    state: VariableState,
) -> Variable {
    Variable {
        kind: variable.kind(),
        global: None,
        name: variable.name().into(),
        declaration: variable.declaration(),
        type_info,
        unresolved_shape: None,
        state,
    }
}
