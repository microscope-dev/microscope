//! Expressions over an interpreter's objects: the names an interpreted
//! frame binds, and the places, steps, and values of the objects they
//! reach. Each is a value of a dynamic type to the evaluator, which the
//! runtime model reads as the expression runs.

use std::sync::Arc;

use super::evaluation::{Frame, Place, StopMachine, StopObject};
use super::frames::FrameScope;
use super::native::InspectionOps;
use super::runtimes::BoundRuntime;
use crate::InspectionLimits;
use crate::eval::error::ErrorKind;
use crate::eval::number::{Exact, Float, FloatFormat};
use crate::eval::target::{Key, Loaded, LoadedValue, Lookup, Refusal, Stop};
use crate::inspection::InspectionBudget;
use crate::runtime_model::{
    InterpretedFailure, InterpretedHandle, InterpretedKey, InterpretedKind, InterpretedName,
    InterpretedStep, RuntimeStop,
};
use crate::{Error, InspectedValue, Presentation, RuntimeId, VariableState, VirtualAddress};

/// A failure of the runtime model as the evaluator's stop.
fn stop(failure: InterpretedFailure) -> Stop {
    match failure {
        InterpretedFailure::Refused(kind, message) => {
            Stop::Refused(Refusal::new(kind, message.to_string()))
        }
        InterpretedFailure::Unavailable(reason) => {
            Stop::missing(VariableState::Unavailable(reason))
        }
    }
}

impl<P: InspectionOps> Frame<'_, P> {
    /// What `name` means in an interpreted frame, as its interpreter finds
    /// it; `None` in any other frame, or for a name it finds nothing by.
    pub(super) fn lookup_interpreted(
        &self,
        name: &str,
    ) -> Result<Option<Lookup<StopObject>>, Refusal> {
        let FrameScope::Interpreted { runtime, frame } = self.resolved().scope else {
            return Ok(None);
        };

        let bound = self
            .bound(runtime)
            .map_err(|error| Refusal::new(ErrorKind::Unsupported, error.to_string()))?;

        // Binding reads the frame's names, which its own budget pays for.
        let mut budget = InspectionBudget::new(InspectionLimits::default());
        let found = self.with_stop(&bound, |stop| {
            bound.model.interpreted_name(stop, frame, name, &mut budget)
        });

        match found {
            Ok(Some(found)) => Ok(Some(Lookup::Dynamic(StopObject::interpreted(
                bound.module.id,
                runtime,
                frame,
                found,
            )))),
            Ok(None) => Ok(None),
            Err(InterpretedFailure::Refused(kind, message)) => {
                Err(Refusal::new(kind, message.to_string()))
            }
            Err(InterpretedFailure::Unavailable(reason)) => Err(Refusal::new(
                ErrorKind::Unsupported,
                format!("the frame's names cannot be read: {reason}"),
            )),
        }
    }

    /// The runtime of the process whose objects a cast to the dynamic type
    /// `name` reaches, if one is.
    pub(super) fn interpreter_cast(&self, name: &str) -> Option<RuntimeId> {
        self.controller
            .runtimes(self.inferior())
            .into_iter()
            .find(|bound| {
                bound
                    .model
                    .object_types()
                    .is_some_and(|types| types.cast == name)
            })
            .map(|bound| bound.id)
    }

    /// The runtime numbered `runtime`, bound to the frame's process.
    pub(super) fn bound(&self, runtime: RuntimeId) -> crate::Result<BoundRuntime> {
        self.controller.bound_runtime(self.inferior(), runtime)
    }

    /// What `read` gives of the stop, as the code of `runtime`'s module
    /// sees it.
    pub(super) fn with_stop<T>(
        &self,
        runtime: &BoundRuntime,
        read: impl FnOnce(&dyn RuntimeStop) -> T,
    ) -> T {
        self.controller
            .with_runtime_stop(self.inferior(), runtime, self.root().reader(), read)
    }
}

impl<P: InspectionOps> StopMachine<'_, '_, P> {
    /// What `read` gives of the stop, from the runtime numbered `runtime`
    /// and on this machine's budget.
    fn interpreted<T>(
        &mut self,
        runtime: RuntimeId,
        read: impl FnOnce(&BoundRuntime, &dyn RuntimeStop, &mut InspectionBudget) -> T,
    ) -> Result<T, Stop> {
        let bound = self.frame.bound(runtime).map_err(Stop::Failed)?;
        let budget = &mut *self.budget;

        Ok(self
            .frame
            .with_stop(&bound, |stop| read(&bound, stop, budget)))
    }

    /// The place of the object a name the frame bound holds.
    pub(super) fn interpreted_name(
        &mut self,
        runtime: RuntimeId,
        frame: VirtualAddress,
        name: InterpretedName,
    ) -> Result<Place, Stop> {
        let object = match name {
            InterpretedName::Object(object) => object,
            InterpretedName::Variable(slot) => self
                .interpreted(runtime, |bound, stop, budget| {
                    bound.model.interpreted_variable(stop, frame, slot, budget)
                })?
                .map_err(stop)?,
        };

        Ok(Place::Interpreted { runtime, object })
    }

    /// The place a step from an interpreter's object reaches.
    pub(super) fn interpreted_step(
        &mut self,
        from: &Place,
        step: InterpretedStep<'_>,
    ) -> Result<Place, Stop> {
        let Place::Interpreted { runtime, object } = *from else {
            return Err(Stop::Failed(Error::internal(
                "the binder steps through attributes and items of interpreters' objects only",
            )));
        };

        let reached = self
            .interpreted(runtime, |bound, stop, budget| {
                bound.model.interpreted_step(stop, object, step, budget)
            })?
            .map_err(stop)?;
        Ok(Place::Interpreted {
            runtime,
            object: reached,
        })
    }

    /// The address of an interpreter's object, refused where its value
    /// lives in no memory.
    pub(super) fn interpreted_address(
        &self,
        runtime: RuntimeId,
        object: InterpretedHandle,
    ) -> Result<u64, Stop> {
        let bound = self.frame.bound(runtime).map_err(Stop::Failed)?;
        bound
            .model
            .interpreted_address(object)
            .map(VirtualAddress::get)
            .ok_or_else(|| {
                Stop::Refused(Refusal::new(
                    ErrorKind::NotAnLvalue,
                    "the interpreter's value lives in no memory, which has no address",
                ))
            })
    }

    /// The place of the object of `runtime` that a native pointer to its
    /// objects, whose value is `address`, points to.
    pub(super) fn interpreted_cast(
        &mut self,
        runtime: RuntimeId,
        address: VirtualAddress,
    ) -> Result<Place, Stop> {
        let object = self
            .interpreted(runtime, |bound, stop, budget| {
                bound.model.interpreted_handle(stop, address, budget)
            })?
            .map_err(stop)?;
        Ok(Place::Interpreted { runtime, object })
    }

    /// The length of an interpreter's object.
    pub(super) fn interpreted_length(
        &mut self,
        runtime: RuntimeId,
        object: InterpretedHandle,
    ) -> Result<u64, Stop> {
        self.interpreted(runtime, |bound, stop, budget| {
            bound.model.interpreted_length(stop, object, budget)
        })?
        .map_err(stop)
    }

    /// An interpreter's object, loaded for an operation.
    pub(super) fn interpreted_operand(
        &mut self,
        runtime: RuntimeId,
        object: InterpretedHandle,
    ) -> Result<Loaded, Stop> {
        let operand = self
            .interpreted(runtime, |bound, stop, budget| {
                bound.model.interpreted_operand(stop, object, budget)
            })?
            .map_err(stop)?;
        let text = || {
            operand.text.clone().ok_or_else(|| {
                Stop::Failed(Error::internal("the runtime reads text with its operand"))
            })
        };

        let value = match operand.kind {
            InterpretedKind::Nothing => LoadedValue::Nothing,
            InterpretedKind::Bool(value) => LoadedValue::Bool(value),
            InterpretedKind::Int(Some(value)) => LoadedValue::Int(Exact::from(value)),
            InterpretedKind::Int(None) => {
                return Err(Stop::Refused(Refusal::new(
                    ErrorKind::Arithmetic,
                    format!(
                        "the `{}` is beyond the 128 bits microscope computes with",
                        operand.type_name
                    ),
                )));
            }
            InterpretedKind::Float(value) => LoadedValue::Float(Float::from_f64(value)),
            InterpretedKind::Text => LoadedValue::Text(text()?),
            InterpretedKind::Bytes => LoadedValue::Bytes(text()?),
            InterpretedKind::Sequence { length } | InterpretedKind::Map { length } => {
                LoadedValue::Collection { length }
            }
            InterpretedKind::Instance | InterpretedKind::Other => LoadedValue::Object,
        };

        Ok(Loaded {
            type_name: Arc::clone(&operand.type_name),
            value,
        })
    }

    /// The presentation of the interpreter's object at `object`, which a
    /// native pointer to a type named one of `names` points to, where an
    /// interpreter of the process names its objects so and one is there;
    /// `None` otherwise, as for an address that holds no object.
    pub(super) fn interpreted_object(
        &mut self,
        names: &[Arc<str>],
        object: VirtualAddress,
    ) -> Result<Option<Arc<Presentation>>, Stop> {
        let frame = self.frame;
        let Some(bound) = frame
            .controller
            .runtimes(frame.inferior())
            .into_iter()
            .find(|bound| {
                bound
                    .model
                    .object_types()
                    .is_some_and(|types| names.iter().any(|name| **name == *types.native))
            })
        else {
            return Ok(None);
        };

        // A pointer that names none of the runtime's objects is presented
        // as the pointer it is.
        let budget = &mut *self.budget;
        let slot = frame.with_stop(&bound, |stop| {
            match bound.model.interpreted_handle(stop, object, budget) {
                Ok(object) => Ok(Some(bound.model.interpreted_value(stop, object, budget))),
                Err(InterpretedFailure::Refused(..)) => Ok(None),
                Err(failure) => Err(failure),
            }
        });
        let Some(slot) = slot.map_err(stop)? else {
            return Ok(None);
        };

        Ok(match self.present_interpreted(&bound, slot)? {
            (_, VariableState::Available { presentation, .. }) => presentation,
            _ => None,
        })
    }

    /// An interpreter's object, as inspection presents it: through the view
    /// its class has.
    pub(super) fn interpreted_present(
        &mut self,
        runtime: RuntimeId,
        object: InterpretedHandle,
    ) -> Result<InspectedValue, Stop> {
        let bound = self.frame.bound(runtime).map_err(Stop::Failed)?;
        let slot = self.interpreted(runtime, |bound, stop, budget| {
            bound.model.interpreted_value(stop, object, budget)
        })?;

        let (type_info, state) = self.present_interpreted(&bound, slot)?;
        Ok(crate::eval::target::Machine::finish(self, type_info, state))
    }

    /// An interpreter's object as the debugger presents it without a view.
    pub(super) fn interpreted_present_stored(
        &mut self,
        runtime: RuntimeId,
        object: InterpretedHandle,
    ) -> Result<InspectedValue, Stop> {
        let frame = self.frame;
        let (type_info, state) = self.interpreted(runtime, |bound, stop, budget| {
            let slot = bound.model.interpreted_value(stop, object, budget);
            super::interpreted::present_slot(
                (frame.stop_id, frame.root().context, frame.resolved().id),
                bound,
                slot,
            )
        })?;
        Ok(crate::eval::target::Machine::finish(self, type_info, state))
    }
}

/// A key an expression wrote, as an interpreter's object is indexed by.
pub(super) fn interpreted_key(key: &Key) -> Result<InterpretedKey, Stop> {
    let refused = |message: String| Stop::Refused(Refusal::new(ErrorKind::Type, message));
    Ok(match key {
        Key::Integer(value) => InterpretedKey::Int(value.to_i128().ok_or_else(|| {
            refused(format!(
                "the key {value} is beyond the 128 bits microscope computes with"
            ))
        })?),
        Key::Float(value) => {
            // A value no binary64 holds equals none of the interpreter's.
            let converted = value
                .convert_exactly(FloatFormat::Binary64)
                .ok_or_else(|| {
                    refused(format!(
                        "the key {value} is no binary64 float, which an interpreter's floats are"
                    ))
                })?;
            let crate::FloatValue::Binary64(bits) = converted.to_value() else {
                return Err(Stop::Failed(Error::internal(
                    "a float converted to binary64 is one",
                )));
            };

            InterpretedKey::Float(f64::from_bits(bits))
        }
        Key::Bool(value) => InterpretedKey::Bool(*value),
        Key::Text(bytes) => InterpretedKey::Text(bytes.clone()),
        Key::Nothing => InterpretedKey::Nothing,
        Key::Tuple(items) => InterpretedKey::Tuple(
            items
                .iter()
                .map(interpreted_key)
                .collect::<Result<_, _>>()?,
        ),
        Key::Object(object) => InterpretedKey::Object(InterpretedHandle::new(*object)),
        Key::Address(_) => {
            return Err(refused(
                "an interpreter's object is indexed by numbers, truth values, text, and the \
                 value for nothing, not by an address"
                    .to_owned(),
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A float key is the interpreter's own float only where it is one
    /// exactly: a wider value that rounds to one is no key it holds.
    #[test]
    fn a_float_key_is_an_interpreters_float_only_where_it_is_one_exactly() {
        let key = |value: Float| interpreted_key(&Key::Float(value));
        assert!(matches!(
            key(Float::from_f32(1.5)),
            Ok(InterpretedKey::Float(value)) if value.to_bits() == 1.5_f64.to_bits()
        ));

        // 1.5 + 2**-59 holds in an x87 value's 64 bits, not in binary64's 53.
        let wider = Float::from_value(crate::FloatValue::X87Extended {
            significand: 0xc000_0000_0000_0010,
            sign_exponent: 0x3fff,
        });
        assert!(
            matches!(key(wider), Err(Stop::Refused(_))),
            "{:?}",
            key(wider)
        );
    }
}
