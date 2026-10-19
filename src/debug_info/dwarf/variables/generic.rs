//! The types Go's generic values really have. Go compiles a generic
//! function once per shape, such as `go.shape.int` for every type whose
//! underlying type is `int`, and describes its values by a typedef of the
//! shape. The typedef names an entry of the dictionary the function takes
//! as `.dict`, which points to the runtime's descriptor of the type
//! argument; the descriptor's offset among the runtime's types is the
//! `DW_AT_go_runtime_type` of the type the debug information describes.

use crate::inspection::InspectionBudget;
use crate::model::ValueStorage;
use crate::{
    CodeInstanceId, ImageAddress, RecordMemberLayout, ShapeUnresolvedReason, TypeId,
    VariableUnavailableReason, VirtualAddress,
};

use super::codec::decode_address;
use super::evaluate::{EvaluateError, FrameBaseCache};
use super::shape::ValueShape;
use super::storage;
use super::types::TypeResolution;
use super::{DwarfVariableInfo, VariableRuntime};

/// The global locating the runtime's type descriptors.
const MODULE_DATA: &str = "runtime.firstmoduledata";

/// Why a type argument is unknown, or the failure that ends inspection.
enum Failure {
    Unresolved(ShapeUnresolvedReason),
    Fatal(std::sync::Arc<str>),
}

impl From<ShapeUnresolvedReason> for Failure {
    fn from(reason: ShapeUnresolvedReason) -> Self {
        Self::Unresolved(reason)
    }
}

/// What a generic value's type turned out to be.
pub(super) enum Generic {
    /// Not a type parameter's.
    Plain,

    /// The type argument.
    Resolved(TypeId),

    /// The shape, and why the type argument is unknown.
    Unresolved(TypeId, ShapeUnresolvedReason),
}

impl DwarfVariableInfo {
    /// The type a value of type `type_id` really has, in the frame of
    /// `instance` at `address`, when `type_id` is a type parameter.
    pub(super) fn generic_type(
        &self,
        type_id: TypeId,
        instance: Option<CodeInstanceId>,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> crate::Result<Generic> {
        let Some(index) = self.type_facts().dictionary_index(type_id) else {
            return Ok(Generic::Plain);
        };

        let shape = match self.type_info(type_id).map(|info| &info.kind) {
            Ok(crate::TypeKind::Named {
                target: Some(target),
                ..
            }) => target.id,
            _ => type_id,
        };

        match self.type_argument(index, shape, instance, address, runtime, budget) {
            Ok(argument) => Ok(Generic::Resolved(argument)),
            Err(Failure::Unresolved(reason)) => Ok(Generic::Unresolved(shape, reason)),
            Err(Failure::Fatal(description)) => Err(crate::Error::VariableRuntime(description)),
        }
    }

    fn type_argument(
        &self,
        index: u64,
        shape: TypeId,
        instance: Option<CodeInstanceId>,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<TypeId, Failure> {
        let dictionary = self.dictionary(instance, address, runtime, budget)?;
        let entry = index
            .checked_mul(8)
            .and_then(|offset| dictionary.get().checked_add(offset))
            .map(VirtualAddress::new)
            .ok_or(ShapeUnresolvedReason::DictionaryUnavailable(
                VariableUnavailableReason::EvaluationLimit,
            ))?;
        let descriptor = self
            .pointer_at(&ValueStorage::Memory(entry), runtime, budget)
            .map_err(|error| unavailable(error, ShapeUnresolvedReason::DictionaryUnavailable))?;
        let (types, end) = self.type_descriptors(runtime, budget)?;
        if !(types..end).contains(&descriptor.get()) {
            return Err(ShapeUnresolvedReason::ForeignType.into());
        }

        let argument = self
            .types
            .view()
            .go_runtime_type(descriptor.get() - types)
            .ok_or(ShapeUnresolvedReason::UndescribedType)?;

        // A dictionary read where the function has not stored it yet holds
        // whatever was there; a type laid out unlike the shape is not the
        // argument.
        if !self.same_layout(argument, shape) {
            return Err(ShapeUnresolvedReason::MismatchedShape.into());
        }

        Ok(argument)
    }

    /// The address of the frame's dictionary, its `.dict` parameter.
    fn dictionary(
        &self,
        instance: Option<CodeInstanceId>,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<VirtualAddress, Failure> {
        let at = address.ok_or(ShapeUnresolvedReason::NoDictionary)?;
        let function = self
            .function_at(at)
            .ok_or(ShapeUnresolvedReason::NoDictionary)?;
        let dictionary = function
            .objects()
            .find(|object| {
                object.name() == ".dict"
                    && object.instance() == instance
                    && self.visible_at(*object, at)
            })
            .ok_or(ShapeUnresolvedReason::NoDictionary)?;

        // Optimized Go places its dictionary, for the whole function, in
        // the register argument's spill slot at the frame's CFA, though the
        // function may never spill it there, and the slot may hold another
        // call's. Unoptimized Go says where it is before and after it
        // spills it.
        if let Some(location) = dictionary.location()
            && self
                .locations()
                .list(location)
                .entries()
                .any(|(range, expression)| {
                    range.is_none()
                        && expression.bytes() == [gimli::constants::DW_OP_call_frame_cfa.0]
                })
        {
            return Err(ShapeUnresolvedReason::UnreliableDictionary.into());
        }

        let storage = self
            .located_data_object(
                dictionary,
                address,
                runtime,
                &mut FrameBaseCache::Empty,
                budget,
            )
            .map_err(|error| unavailable(error, ShapeUnresolvedReason::DictionaryUnavailable))?;

        self.pointer_at(&storage, runtime, budget)
            .map_err(|error| unavailable(error, ShapeUnresolvedReason::DictionaryUnavailable))
    }

    /// Where the runtime's type descriptors begin and end, as
    /// `runtime.firstmoduledata` records them.
    fn type_descriptors(
        &self,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<(u64, u64), Failure> {
        let missing = ShapeUnresolvedReason::ModuleDataUnavailable(
            VariableUnavailableReason::OptimizedOut(crate::OptimizedOutReason::NoLocation),
        );
        let module_data = self
            .catalog()
            .globals()
            .find(|object| object.name() == MODULE_DATA)
            .ok_or(missing)?;
        let TypeResolution::Resolved(type_id) = module_data.type_info() else {
            return Err(ShapeUnresolvedReason::Malformed(
                "runtime.firstmoduledata has a malformed type".into(),
            )
            .into());
        };

        let Ok(ValueShape::Record { members, .. }) = self.value_shape(type_id) else {
            return Err(ShapeUnresolvedReason::Malformed(
                "runtime.firstmoduledata is not a structure".into(),
            )
            .into());
        };

        let offset = |name: &str| {
            members
                .iter()
                .find(|member| member.name.as_deref() == Some(name))
                .and_then(|member| match member.layout {
                    RecordMemberLayout::ByteOffset(offset) => i64::try_from(offset).ok(),
                    _ => None,
                })
                .ok_or_else(|| {
                    ShapeUnresolvedReason::Malformed(
                        format!("runtime.moduledata has no {name} at an offset").into(),
                    )
                })
        };

        let (types, end) = (offset("types")?, offset("etypes")?);
        let storage = self
            .located_data_object(
                module_data,
                None,
                runtime,
                &mut FrameBaseCache::Empty,
                budget,
            )
            .map_err(|error| unavailable(error, ShapeUnresolvedReason::ModuleDataUnavailable))?;

        let mut field = |offset| {
            storage::offset(storage.clone(), offset)
                .and_then(|field| self.pointer_at(&field, runtime, budget))
                .map(VirtualAddress::get)
                .map_err(|error| unavailable(error, ShapeUnresolvedReason::ModuleDataUnavailable))
        };

        Ok((field(types)?, field(end)?))
    }

    /// The pointer-sized word stored at `storage`.
    fn pointer_at(
        &self,
        storage: &ValueStorage,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<VirtualAddress, EvaluateError> {
        let (_, raw) = storage::read(storage, 8, runtime, budget)?;
        let address = decode_address(&raw, 8, self.target)?;
        if address.get() == 0 {
            return Err(VariableUnavailableReason::ValueAccess(
                crate::ValueAccessUnavailableReason::NullPointer,
            )
            .into());
        }

        Ok(address)
    }

    /// Whether values of two types are stored alike: the same scalar
    /// encoding, or the same kind of value of the same size.
    fn same_layout(&self, left: TypeId, right: TypeId) -> bool {
        let (Ok(left), Ok(right)) = (self.value_shape(left), self.value_shape(right)) else {
            return false;
        };

        // An integer and a Go type with constants are both integers.
        let integral = |shape: &ValueShape| match shape {
            ValueShape::Scalar(base)
            | ValueShape::Enumeration {
                representation: base,
                ..
            } => Some(base.encoding),
            _ => None,
        };

        let same_kind = match (integral(&left), integral(&right)) {
            (Some(left), Some(right)) => left == right,
            (None, None) => std::mem::discriminant(&left) == std::mem::discriminant(&right),
            _ => false,
        };

        same_kind && left.byte_size() == right.byte_size()
    }
}

/// An evaluation failure as the reason a type argument is unknown.
fn unavailable(
    error: EvaluateError,
    reason: fn(VariableUnavailableReason) -> ShapeUnresolvedReason,
) -> Failure {
    match error {
        EvaluateError::Unavailable(unavailable) => Failure::Unresolved(reason(unavailable)),
        EvaluateError::Malformed(description) => {
            Failure::Unresolved(ShapeUnresolvedReason::Malformed(description))
        }
        EvaluateError::Fatal(description) => Failure::Fatal(description),
    }
}
