//! Unwinding Go code by its stack table, as the runtime's traceback does:
//! the canonical frame address is SP plus the table's delta at the
//! instruction, plus the return address the call pushed, which lies just
//! below it. Go's call-frame information is generated from the same table,
//! but a stripped image has none.

use std::sync::Arc;

use super::{GoFunction, GoTable};
use crate::unwind::{
    DwarfRegister, FramePointerRule, MemoryReader, RBP, RETURN_ADDRESS, RSP, RegisterFile,
    UnwindStep,
};

use crate::{ImageAddress, UnwindTermination, VirtualAddress};

/// What a frame's caller had in its frame-pointer register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerFramePointer {
    /// The prologue saved it in the word at this address.
    Saved(VirtualAddress),

    /// The function has not changed it: Go code compiled by Go allocates
    /// no register to rbp, and it has not yet pushed it or has popped it.
    Unchanged,

    /// Nothing proves where it is.
    Unknown,
}

/// A Go function table with what unwinding needs of each function.
#[derive(Debug)]
pub struct GoUnwind {
    table: Arc<GoTable>,

    /// Where each function keeps its caller's frame pointer saved, by index.
    frame_pointer_saved: Vec<Option<std::ops::Range<u64>>>,
}

/// Where each function keeps its caller's frame pointer saved, by index,
/// from every function's prologue in the image's code.
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
    /// Unwinds by `table`, with [`frame_saves`]'s answer for it.
    pub const fn new(
        table: Arc<GoTable>,
        frame_pointer_saved: Vec<Option<std::ops::Range<u64>>>,
    ) -> Self {
        Self {
            table,
            frame_pointer_saved,
        }
    }

    /// The function Go compiled or assembled whose code contains an image
    /// address, with its index.
    fn function(&self, address: ImageAddress) -> Option<(usize, &GoFunction)> {
        let functions = self.table.functions();
        let address = address.get();
        let after = functions.partition_point(|function| function.entry <= address);
        let index = after.checked_sub(1)?;
        let function = &functions[index];

        (function.contains(address) && function.is_go()).then_some((index, function))
    }

    /// Whether Go compiled or assembled the code at an image address.
    pub fn is_go(&self, address: ImageAddress) -> bool {
        self.function(address).is_some()
    }

    /// Whether Go compiled the code at an image address, rather than
    /// assembled it: compiled code follows the register ABI, which keeps
    /// the goroutine's `g` in R14 throughout, where assembly may overwrite
    /// it. Every release whose table this reads has that ABI.
    pub fn compiled(&self, address: ImageAddress) -> bool {
        self.function(address)
            .is_some_and(|(_, function)| !function.facts.assembly)
    }

    /// How far SP is below its value at the function's entry.
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

    /// The canonical frame address of the Go frame executing `address`, or
    /// `None` when Go did not compile the code there.
    pub fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
    ) -> Option<std::result::Result<VirtualAddress, UnwindTermination>> {
        let (_, function) = self.function(address)?;
        Some(self.sp_delta(function, address).and_then(|delta| {
            let sp = registers
                .get(RSP)
                .ok_or_else(|| UnwindTermination::RegisterUnavailable {
                    register: "rsp".into(),
                })?;
            sp.checked_add(delta)
                .and_then(|cfa| cfa.checked_add(8))
                .map(VirtualAddress::new)
                .ok_or_else(|| UnwindTermination::InvalidCaller {
                    description: "CFA arithmetic overflow".into(),
                })
        }))
    }

    /// Reconstructs the caller of the Go frame executing `address`, which
    /// keeps none of the `clobbered` registers, or `None` when Go did not
    /// compile the code there.
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

            caller.set(RETURN_ADDRESS, return_address);
            caller.set(RSP, cfa.get());

            Ok(UnwindStep {
                registers: caller,
                cfa,
                signal_frame: false,
            })
        })())
    }

    /// Where the caller's frame pointer is, for the Go frame executing
    /// `address` with canonical frame address `cfa`, or `None` when Go did
    /// not compile the code there.
    ///
    /// The assembler's prologue pushes rbp first of all its changes to SP
    /// and each epilogue pops it last, so while SP is at least 8 below its
    /// entry value the caller's rbp is in the word below the return address
    /// (see [`super::Prologue`]). Where SP is at its entry value, compiled
    /// Go code has not changed rbp, which the compiler never allocates;
    /// assembly may have.
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

    /// Whether the Go frame executing `address` keeps the frame-pointer
    /// chain, or `None` when Go did not compile the code there. Its rbp is
    /// the word below its return address once the prologue saved the
    /// caller's there; compiled Go code keeps no other value in it.
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

    /// Applies [`Self::caller_frame_pointer`] to a reconstructed caller.
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
                Some(value) => step.registers.set(RBP, value),
                None => step.registers.remove(RBP),
            },
            Some(CallerFramePointer::Unchanged) => match current.get(RBP) {
                Some(value) => step.registers.set(RBP, value),
                None => step.registers.remove(RBP),
            },
            Some(CallerFramePointer::Unknown) => step.registers.remove(RBP),
        }
    }
}
