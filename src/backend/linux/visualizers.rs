use std::sync::Arc;

use crate::eval::types::{TypeSource, representation};
use crate::inspection::InspectionBudget;
use crate::model::ValueStorage;
use crate::runtime_model::InterpretedArray;
use crate::view::run::{Failure, Input};
use crate::{
    BaseTypeEncoding, Error, InspectionLimits, NumberKind, Numbers, Renderer, Result, TypeKind,
    TypeReference, ValueChildrenReference, VirtualAddress, VisualizerInput, VisualizerInputs,
    VisualizerReference, VisualizerValue,
};

use super::Controller;
use super::evaluation::{Place, StopMachine};
use super::interpreted::InterpretedChildren;
use super::memory::read_logical_block;
use super::native::InspectionOps;
use super::presentation::{ModuleScope, view_place};

impl<P: InspectionOps> Controller<P> {
    pub(super) fn visualizer_inputs(
        &self,
        reference: &VisualizerReference,
        limits: InspectionLimits,
    ) -> Result<VisualizerInputs> {
        let children = &reference.children;
        let (bound, place) = view_place(children)?;
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let root = self.stack_root(children.stop_id, children.context)?;
        let frame = self.resolve_frame(inferior, &root, children.frame)?;
        let scope = self.frame_for(inferior, children.stop_id, &root, &frame);
        let mut budget = InspectionBudget::new(limits);
        let mut machine = StopMachine::new(&scope, &mut budget, true);
        let run = crate::view::run::inputs(&bound, reference.index, &mut machine, place);
        let inputs = match run {
            Ok(inputs) => inputs,
            Err(Failure::Debugger(error)) => return Err(error),
            Err(Failure::Problem(problem)) => {
                return Err(Error::ViewFailed(problem.to_string().into()));
            }
        };

        let mut read = 0_u64;
        let inputs = inputs
            .into_iter()
            .map(|(name, input, path)| {
                let value = match input {
                    Input::Value(value) => VisualizerValue::Value(value),
                    Input::Text(text) => VisualizerValue::Text(text),
                    Input::Bytes { address, length } => {
                        match self.drawing_bytes(address, length, &mut read) {
                            Ok(bytes) => VisualizerValue::Bytes(bytes),
                            Err(problem) => VisualizerValue::Problem(problem),
                        }
                    }
                };

                VisualizerInput { name, value, path }
            })
            .collect();

        Ok(VisualizerInputs {
            stop_id: children.stop_id,
            inputs,
        })
    }

    fn drawing_bytes(
        &self,
        address: u64,
        length: u64,
        read: &mut u64,
    ) -> std::result::Result<Arc<[u8]>, Arc<str>> {
        if read.saturating_add(length) > crate::MAX_DRAWING_BYTES {
            return Err(format!(
                "the drawing reads more than {} bytes",
                crate::MAX_DRAWING_BYTES
            )
            .into());
        }
        *read += length;

        if address == 0 && length > 0 {
            return Err("`bytes` reads at a null pointer".into());
        }

        self.read_block(address, length)
            .map_err(|error| Arc::from(error.to_string()))
    }

    fn read_block(&self, address: u64, length: u64) -> Result<Arc<[u8]>> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let pid = inferior.run.stop().ok_or(Error::NotStopped)?.reader();
        let size = usize::try_from(length).map_err(|_| Error::AddressOverflow)?;
        let read = read_logical_block(
            &self.ptrace,
            pid,
            &inferior.breakpoints,
            VirtualAddress::new(address),
            size,
        )?;

        match read.completion {
            crate::MemoryReadCompletion::Complete => Ok(read.bytes.into()),
            crate::MemoryReadCompletion::Incomplete { next_address, .. } => Err(Error::ViewFailed(
                format!(
                    "the {length} bytes at {address:#x} cannot all be read: {next_address} cannot"
                )
                .into(),
            )),
        }
    }

    pub(super) fn numbers(
        &self,
        reference: &ValueChildrenReference,
        most_bytes: u64,
    ) -> Result<Option<Numbers>> {
        if let Some(array) = Self::interpreted_array(reference) {
            return self.array_numbers(reference, &array, most_bytes);
        }

        let Some((address, count, element, stride)) = self.contiguous(reference)? else {
            return Ok(None);
        };

        let Some(kind) = self.number_kind(element) else {
            return Ok(None);
        };

        let Some(size) = count.checked_mul(kind.size()) else {
            return Ok(None);
        };

        if stride != kind.size() || size > most_bytes {
            return Ok(None);
        }

        let bytes = if size == 0 {
            Arc::from([])
        } else {
            self.read_block(address, size)?
        };

        Ok(Some(Numbers {
            stop_id: reference.stop_id,
            kind,
            count,
            bytes,
        }))
    }

    fn interpreted_array(reference: &ValueChildrenReference) -> Option<Arc<InterpretedArray>> {
        let view = reference.view.as_ref()?;

        if let Some(children) = Self::interpreted_children_of(reference) {
            return children.array.clone();
        }

        let inner = view.inner.as_ref().filter(|_| view.picked.is_none())?;
        Self::interpreted_array(inner).filter(|array| array.shape.first() == Some(&view.elements))
    }

    fn interpreted_children_of(reference: &ValueChildrenReference) -> Option<&InterpretedChildren> {
        reference
            .view
            .as_ref()?
            .bound
            .as_ref()?
            .downcast_ref::<InterpretedChildren>()
    }

    fn array_numbers(
        &self,
        reference: &ValueChildrenReference,
        array: &InterpretedArray,
        most_bytes: u64,
    ) -> Result<Option<Numbers>> {
        let Some((address, count, kind)) = array.contiguous() else {
            return Ok(None);
        };

        let Some(size) = count
            .checked_mul(kind.size())
            .filter(|size| *size <= most_bytes)
        else {
            return Ok(None);
        };

        let bytes = if size == 0 {
            Arc::from([])
        } else {
            self.read_block(address.get(), size)?
        };

        Ok(Some(Numbers {
            stop_id: reference.stop_id,
            kind,
            count,
            bytes,
        }))
    }

    fn contiguous(
        &self,
        reference: &ValueChildrenReference,
    ) -> Result<Option<(u64, u64, TypeReference, u64)>> {
        if let Some(view) = &reference.view {
            if view.picked.is_some() || Self::interpreted_children_of(reference).is_some() {
                return Ok(None);
            }

            if let Some(inner) = &view.inner {
                return Ok(self
                    .contiguous(inner)?
                    .filter(|(_, count, ..)| *count == view.elements));
            }

            let (bound, place) = view_place(reference)?;
            if !matches!(place, Place::Native(_)) {
                return Ok(None);
            }

            let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
            let root = self.stack_root(reference.stop_id, reference.context)?;
            let frame = self.resolve_frame(inferior, &root, reference.frame)?;
            let scope = self.frame_for(inferior, reference.stop_id, &root, &frame);
            let mut budget = InspectionBudget::new(InspectionLimits::default());
            let mut machine = StopMachine::new(&scope, &mut budget, true);

            return match crate::view::run::contiguous(&bound, &mut machine, place) {
                Ok(found) => Ok(found.filter(|(_, count, ..)| *count == view.elements)),
                Err(Failure::Debugger(error)) => Err(error),
                Err(Failure::Problem(_)) => Ok(None),
            };
        }

        let Some(types) = self.types(reference.image) else {
            return Ok(None);
        };

        let types: &dyn TypeSource = &types;
        let ValueStorage::Memory(address) = reference.storage else {
            return Ok(None);
        };

        if reference.placement.is_some() {
            return Ok(None);
        }

        let ty = TypeReference {
            image: reference.image,
            id: reference.target_type,
        };

        let element = match representation(types, ty).map(|(_, info)| &info.kind) {
            Ok(TypeKind::Array {
                element,
                dimensions,
                ..
            }) if dimensions.len() == 1 => *element,
            Ok(TypeKind::Slice {
                element,
                text: false,
                ..
            }) => *element,
            _ => return Ok(None),
        };

        let Some(stride) = types.type_info(element).and_then(|info| info.byte_size) else {
            return Ok(None);
        };

        Ok(Some((address.get(), reference.total, element, stride)))
    }

    fn number_kind(&self, ty: TypeReference) -> Option<NumberKind> {
        let types = self.types(ty.image)?;
        let (_, info) = representation(&types, ty).ok()?;
        let TypeKind::Base(base) = &info.kind else {
            return None;
        };

        if base.bit_size.is_some_and(|bits| bits != base.byte_size * 8) {
            return None;
        }

        Some(match (base.encoding, base.byte_size) {
            (BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter, 1) => NumberKind::I8,
            (BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter, 1) => NumberKind::U8,
            (BaseTypeEncoding::Signed, 2) => NumberKind::I16,
            (BaseTypeEncoding::Unsigned, 2) => NumberKind::U16,
            (BaseTypeEncoding::Signed, 4) => NumberKind::I32,
            (BaseTypeEncoding::Unsigned, 4) => NumberKind::U32,
            (BaseTypeEncoding::Signed, 8) => NumberKind::I64,
            (BaseTypeEncoding::Unsigned, 8) => NumberKind::U64,
            (BaseTypeEncoding::Floating, 4) => NumberKind::F32,
            (BaseTypeEncoding::Floating, 8) => NumberKind::F64,
            _ => return None,
        })
    }

    fn types(&self, image: crate::ModuleImageId) -> Option<ModuleScope<'_, P>> {
        let module = self
            .modules
            .values()
            .find(|module| module.loaded.image == image)?;
        Some(ModuleScope {
            controller: self,
            module,
        })
    }

    pub(super) fn renderers(&self) -> Arc<[Arc<Renderer>]> {
        let built_in = crate::view::ViewSet::built_in();
        self.views
            .set
            .renderers()
            .chain(
                self.modules
                    .values()
                    .flat_map(|module| module.image.views().renderers()),
            )
            .chain(built_in.renderers())
            .cloned()
            .collect()
    }
}
