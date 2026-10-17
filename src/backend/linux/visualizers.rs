//! What a value's drawings read: their inputs, and sequences of numbers in
//! bulk. Only the web page draws, and it asks for these.

use std::sync::Arc;

use crate::eval::types::{TypeSource, representation};
use crate::inspection::InspectionBudget;
use crate::model::ValueStorage;
use crate::view::run::{Failure, Input};
use crate::{
    BaseTypeEncoding, Error, InspectionLimits, NumberKind, Numbers, Renderer, Result, TypeKind,
    TypeReference, ValueChildrenReference, VirtualAddress, VisualizerInput, VisualizerInputs,
    VisualizerReference, VisualizerValue,
};

use super::Controller;
use super::evaluation::{StopMachine, StopPlace, ViewBound};
use super::memory::read_logical_block;
use super::native::InspectionOps;
use super::presentation::ModuleScope;

impl<P: InspectionOps> Controller<P> {
    /// Reads the inputs of one drawing of a value, with the bytes each
    /// `bytes(PTR, LEN)` names.
    pub(super) fn visualizer_inputs(
        &self,
        reference: &VisualizerReference,
        limits: InspectionLimits,
    ) -> Result<VisualizerInputs> {
        let children = &reference.children;
        let bound = Self::view_bound(children)?;
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let root = self.stack_root(children.stop_id, children.context)?;
        let frame = self.resolve_frame(inferior, &root, children.frame)?;
        let scope = self.frame_for(inferior, children.stop_id, &root, &frame);
        let mut budget = InspectionBudget::new(limits);
        let mut machine = StopMachine::new(&scope, &mut budget, true);
        let run = crate::view::run::inputs(
            &bound,
            reference.index,
            &mut machine,
            StopPlace::of(children).into(),
        );
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

    /// The bytes a drawing's `bytes(address, length)` names, which count
    /// toward the drawing's `read` so far, or why they cannot be read.
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

    /// Reads `length` bytes of the stopped program's memory at once, as
    /// stored, with no breakpoint's trap among them.
    fn read_block(&self, address: u64, length: u64) -> Result<Arc<[u8]>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let pid = inferior
            .public_stop
            .as_ref()
            .ok_or(Error::NotStopped)?
            .reader();
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

    /// The elements of a sequence of numbers read at once, when they lie
    /// next to each other in memory and take at most `most_bytes`.
    pub(super) fn numbers(
        &self,
        reference: &ValueChildrenReference,
        most_bytes: u64,
    ) -> Result<Option<Numbers>> {
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

    /// Where a value's elements lie when they lie next to each other: the
    /// first's address, how many there are, their type, and the distance
    /// between them.
    fn contiguous(
        &self,
        reference: &ValueChildrenReference,
    ) -> Result<Option<(u64, u64, TypeReference, u64)>> {
        if let Some(view) = &reference.view {
            if view.picked.is_some() {
                return Ok(None);
            }

            // A view that presents the value as another lends that one's
            // elements.
            if let Some(inner) = &view.inner {
                return Ok(self
                    .contiguous(inner)?
                    .filter(|(_, count, ..)| *count == view.elements));
            }

            let bound = Self::view_bound(reference)?;
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            let root = self.stack_root(reference.stop_id, reference.context)?;
            let frame = self.resolve_frame(inferior, &root, reference.frame)?;
            let scope = self.frame_for(inferior, reference.stop_id, &root, &frame);
            let mut budget = InspectionBudget::new(InspectionLimits::default());
            let mut machine = StopMachine::new(&scope, &mut budget, true);
            return match crate::view::run::contiguous(
                &bound,
                &mut machine,
                StopPlace::of(reference).into(),
            ) {
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

    /// The kind of number values of `ty` are, through its typedefs: an
    /// integer of at most 64 bits or a 32- or 64-bit float.
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

    /// The types of the module whose image is `image`.
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

    /// The view a capability presents through.
    fn view_bound(reference: &ValueChildrenReference) -> Result<Arc<ViewBound>> {
        reference
            .view
            .as_ref()
            .and_then(|view| view.bound.clone())
            .ok_or_else(|| Error::InvalidValueExpression("the value has no view".into()))?
            .downcast::<ViewBound>()
            .map_err(|_| {
                Error::InvalidValueExpression("the view belongs to another debugger".into())
            })
    }

    /// Every renderer drawings may name, in the order a name is looked up.
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
