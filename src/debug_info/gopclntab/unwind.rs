//! Stack unwinding for Go frames using `.gopclntab` PC-to-SP tables.
//!
//! Go does not emit `.eh_frame` for Go functions and may strip `.debug_frame`,
//! so unwinding relies on `pcsp` deltas plus prologue-derived `RBP` save ranges.

use std::sync::Arc;

use super::{GoFunction, GoTable};
use crate::unwind::{
    DwarfRegister, FRAME_POINTER, FramePointerRule, MemoryReader, PROGRAM_COUNTER, RegisterFile,
    STACK_POINTER, UnwindStep,
};

use crate::{ImageAddress, UnwindTermination, VirtualAddress};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerFramePointer {
    Saved(VirtualAddress),

    Unchanged,

    Unknown,
}

#[derive(Debug)]
pub struct GoUnwind {
    table: Arc<GoTable>,

    frame_pointer_saved: Vec<Option<std::ops::Range<u64>>>,
}

pub fn frame_saves<'code>(
    table: &GoTable,
    code: impl Fn(u64, usize) -> Option<&'code [u8]>,
) -> Vec<Option<std::ops::Range<u64>>> {
    table
        .functions()
        .iter()
        .map(|function| {
            function
                .is_go()
                .then(|| table.prologue(function, &code).ok())
                .flatten()
                .and_then(|prologue| prologue.frame_pointer_saved)
        })
        .collect()
}

impl GoUnwind {
    pub const fn new(
        table: Arc<GoTable>,
        frame_pointer_saved: Vec<Option<std::ops::Range<u64>>>,
    ) -> Self {
        Self {
            table,
            frame_pointer_saved,
        }
    }

    fn function(&self, address: ImageAddress) -> Option<(usize, &GoFunction)> {
        let functions = self.table.functions();
        let address = address.get();
        let after = functions.partition_point(|function| function.entry <= address);
        let index = after.checked_sub(1)?;
        let function = functions.get(index)?;

        (function.contains(address) && function.is_go()).then_some((index, function))
    }

    pub fn is_go(&self, address: ImageAddress) -> bool {
        self.function(address).is_some()
    }

    pub fn compiled(&self, address: ImageAddress) -> bool {
        self.function(address)
            .is_some_and(|(_, function)| !function.facts.assembly)
    }

    fn sp_delta(
        &self,
        function: &GoFunction,
        address: ImageAddress,
    ) -> std::result::Result<u64, UnwindTermination> {
        let delta = self
            .table
            .sp_delta(function, address.get())
            .map_err(|error| UnwindTermination::CorruptUnwindInfo {
                description: error.to_string().into(),
            })?
            .ok_or_else(|| UnwindTermination::CorruptUnwindInfo {
                description: "the Go stack table does not cover the instruction".into(),
            })?;

        u64::try_from(delta).map_err(|_| UnwindTermination::CorruptUnwindInfo {
            description: "the Go stack table's delta is negative".into(),
        })
    }

    /// Go's `pcsp` measures the frame size below the saved return address, so the
    /// caller's `RSP` on entry sits one 8-byte return-address slot above it.
    pub fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
    ) -> Option<std::result::Result<VirtualAddress, UnwindTermination>> {
        let (_, function) = self.function(address)?;
        Some(self.sp_delta(function, address).and_then(|delta| {
            let sp = registers.get(STACK_POINTER).ok_or_else(|| {
                UnwindTermination::RegisterUnavailable {
                    register: "rsp".into(),
                }
            })?;
            sp.checked_add(delta)
                .and_then(|cfa| cfa.checked_add(8))
                .map(VirtualAddress::new)
                .ok_or_else(|| UnwindTermination::InvalidCaller {
                    description: "CFA arithmetic overflow".into(),
                })
        }))
    }

    pub fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        clobbered: &[DwarfRegister],
        memory: &mut dyn MemoryReader,
    ) -> Option<std::result::Result<UnwindStep, UnwindTermination>> {
        let cfa = match self.cfa(address, registers)? {
            Ok(cfa) => cfa,
            Err(termination) => return Some(Err(termination)),
        };

        Some((|| {
            let slot = VirtualAddress::new(cfa.get() - 8);
            let return_address = memory
                .read_u64(slot)
                .ok_or(UnwindTermination::MemoryReadFailed { address: slot })?;
            let mut caller = registers.clone();
            for register in clobbered {
                caller.remove(*register);
            }

            caller.set(PROGRAM_COUNTER, return_address);
            caller.set(STACK_POINTER, cfa.get());

            Ok(UnwindStep {
                registers: caller,
                cfa,
                signal_frame: false,
            })
        })())
    }

    /// On `x86_64`, Go pushes `RBP` directly below the return address (`cfa - 8`),
    /// placing it at `cfa - 16`; before any stack adjustment (`delta == 0`) in
    /// non-assembly Go code, the caller's `RBP` is still live in the register.
    pub fn caller_frame_pointer(
        &self,
        address: ImageAddress,
        cfa: VirtualAddress,
    ) -> Option<CallerFramePointer> {
        let (index, function) = self.function(address)?;
        let Ok(delta) = self.sp_delta(function, address) else {
            return Some(CallerFramePointer::Unknown);
        };

        let saved = self
            .frame_pointer_saved
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|saved| saved.contains(&address.get()));

        Some(if delta >= 8 && saved {
            CallerFramePointer::Saved(VirtualAddress::new(cfa.get() - 16))
        } else if delta == 0 && !function.facts.assembly {
            CallerFramePointer::Unchanged
        } else {
            CallerFramePointer::Unknown
        })
    }

    pub fn frame_pointer_rule(&self, address: ImageAddress) -> Option<FramePointerRule> {
        let (index, function) = self.function(address)?;
        let saved = self
            .frame_pointer_saved
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|saved| saved.contains(&address.get()));
        let delta = self.sp_delta(function, address).ok();

        Some(if saved && delta.is_some_and(|delta| delta >= 8) {
            FramePointerRule::Kept
        } else {
            FramePointerRule::NotKept
        })
    }

    pub fn recover_frame_pointer(
        &self,
        address: ImageAddress,
        current: &RegisterFile,
        step: &mut UnwindStep,
        memory: &mut dyn MemoryReader,
    ) {
        match self.caller_frame_pointer(address, step.cfa) {
            None => {}
            Some(CallerFramePointer::Saved(slot)) => match memory.read_u64(slot) {
                Some(value) => step.registers.set(FRAME_POINTER, value),
                None => step.registers.remove(FRAME_POINTER),
            },
            Some(CallerFramePointer::Unchanged) => match current.get(FRAME_POINTER) {
                Some(value) => step.registers.set(FRAME_POINTER, value),
                None => step.registers.remove(FRAME_POINTER),
            },
            Some(CallerFramePointer::Unknown) => step.registers.remove(FRAME_POINTER),
        }
    }
}
