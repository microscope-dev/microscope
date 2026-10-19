//! Stepping into one chosen call of a line. The line's calls are its
//! code's call instructions from the stopped instruction on. A step into
//! one of them steps in as usual, but at any other call instruction it
//! runs the call to its return, as a step over a call instruction does,
//! and goes on from there.

use std::collections::BTreeSet;
use std::sync::Arc;

use nix::unistd::Pid;

use crate::disassembly::{AssemblySyntax, ControlFlow, InstructionReferenceKind, RawDecode};
use crate::protocol::{StepTarget, StopId};
use crate::{AddressRange, Error, ImageAddress, Result, VirtualAddress};

use super::frames::{
    describe_address, selected_code_instance, source_for_code_instance, source_line_changed,
};

use super::memory::read_logical_memory;
use super::native::{InspectionOps, LinuxTraceOps};
use super::{ActiveKind, Controller, Inferior, ResumeGuard};

/// The most code of one line decoded for its calls.
const MOST_LINE_BYTES: u64 = 64 * 1024;

impl<P: InspectionOps> Controller<P> {
    /// The calls of the line thread `pid` is stopped at, in its selected
    /// frame's code, from the stopped instruction on, in address order.
    pub(super) fn step_targets(&self, stop_id: StopId, pid: Pid) -> Result<Arc<[StepTarget]>> {
        let inferior = self.stopped_inferior(stop_id, pid)?;
        let registers = self.ptrace.registers(pid)?;
        let pc = VirtualAddress::new(registers.rip);
        let Some(location) = locate(self, inferior, pc) else {
            return Ok(Arc::from([]));
        };

        let presentation = self.presentation_for_stopped_thread(pid)?;
        let Some(instance_id) = selected_code_instance(&location, &presentation)? else {
            return Ok(Arc::from([]));
        };

        let Some(source) = source_for_code_instance(&self.module_image, &location, instance_id)
        else {
            return Ok(Arc::from([]));
        };

        let Some(instance) = self.module_image.code_instance(instance_id) else {
            return Ok(Arc::from([]));
        };

        let mut ranges = Vec::<AddressRange<ImageAddress>>::new();
        for line in self.module_image.line_entries_in(instance) {
            if line.range.end <= location.address {
                continue;
            }

            let at = self.module_image.locate(line.range.start);
            if source_for_code_instance(&self.module_image, &at, instance_id)
                .is_some_and(|candidate| !source_line_changed(Some(&source), Some(&candidate)))
            {
                ranges.push(AddressRange {
                    start: line.range.start.max(location.address),
                    end: line.range.end,
                });
            }
        }

        ranges.sort_unstable_by_key(|range| range.start);

        let mut decoder =
            crate::disassembly::decoder_for(self.module_image.target(), AssemblySyntax::Intel)?;
        let modules = self.unwind_modules(inferior);
        let mut targets = Vec::new();
        let mut read_bytes = 0;
        for range in ranges {
            let start = inferior.loaded_module.virtual_address(range.start)?;
            let length = range.end.get().saturating_sub(range.start.get());
            read_bytes += length;
            if read_bytes > MOST_LINE_BYTES {
                break;
            }

            let read = read_logical_memory(
                &self.ptrace,
                pid,
                &inferior.breakpoints,
                start,
                usize::try_from(length).map_err(|_| Error::AddressOverflow)?,
            )?;
            let mut offset = 0;
            while offset < read.bytes.len() {
                let address = start.get() + offset as u64;
                let RawDecode::Instruction {
                    length,
                    flow,
                    references,
                    ..
                } = decoder.decode(address, &read.bytes[offset..], None)
                else {
                    break;
                };

                if matches!(flow, ControlFlow::Call | ControlFlow::IndirectCall) {
                    let target = (flow == ControlFlow::Call)
                        .then(|| {
                            references
                                .iter()
                                .find(|(kind, _)| *kind == InstructionReferenceKind::BranchTarget)
                                .map(|(_, target)| VirtualAddress::new(*target))
                        })
                        .flatten();
                    targets.push(StepTarget {
                        call: VirtualAddress::new(address),
                        target,
                        callee: target
                            .and_then(|target| self.callee_name(inferior, &modules, target)),
                    });
                }

                offset += length;
            }
        }

        Ok(targets.into())
    }

    /// The name of the function a call calls: its debug information's, or
    /// else its symbol's, without a procedure linkage table's `@plt`.
    fn callee_name(
        &self,
        inferior: &Inferior,
        modules: &[super::frames::UnwindModule<'_>],
        target: VirtualAddress,
    ) -> Option<Arc<str>> {
        if let Some(function) =
            locate(self, inferior, target).and_then(|location| location.function)
        {
            return Some(function.name);
        }

        let symbol = describe_address(modules, target).module?.image.symbol?;
        if symbol.offset != 0 {
            return None;
        }

        let name =
            crate::demangle::demangle(&symbol.name).unwrap_or_else(|| symbol.name.to_string());
        Some(name.strip_suffix("@plt").unwrap_or(&name).into())
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Which call a step into is to go into, after checking that it is one
    /// of the line's.
    pub(super) fn chosen_call(
        &self,
        stop_id: StopId,
        pid: Pid,
        call: VirtualAddress,
    ) -> Result<VirtualAddress> {
        if self
            .step_targets(stop_id, pid)?
            .iter()
            .any(|target| target.call == call)
        {
            Ok(call)
        } else {
            Err(Error::NotAStepTarget(call))
        }
    }

    /// For a step into a chosen call stopped at another call instruction,
    /// runs that call to its return. Returns whether it did.
    pub(super) fn pass_unchosen_call(&mut self, pid: Pid) -> Result<bool> {
        if !self.guard_unchosen_call(pid)? {
            return Ok(false);
        }

        self.continue_thread(pid)?;
        Ok(true)
    }

    /// For a step into a chosen call stopped at another call instruction,
    /// guards the call's return address, where the step resumes once the
    /// call's own activation returns. Returns whether it did.
    pub(super) fn guard_unchosen_call(&mut self, pid: Pid) -> Result<bool> {
        let Some(chosen) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, owner, .. }
                    if self.runs_step(*owner, pid) && start.resume_guard.is_none() =>
                {
                    start.into_call
                }
                _ => None,
            })
        else {
            return Ok(false);
        };

        let registers = self.ptrace.registers(pid)?;
        if registers.rip == chosen.get() {
            return Ok(false);
        }

        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let Some((address, stack)) = self.call_return(inferior, pid, &registers)? else {
            return Ok(false);
        };

        let execution = self.active_execution()?;
        self.install_additional_plan_breakpoints(execution, &BTreeSet::from([address]))?;
        if let Some(start) = self.active_step_mut() {
            start.resume_guard = Some(ResumeGuard {
                address,
                stack,
                call: true,
            });
        }

        record!("{pid} runs the call at {:#x} to {address}", registers.rip);

        Ok(true)
    }
}

/// Where an address of the main executable is in its image.
fn locate<P: InspectionOps>(
    controller: &Controller<P>,
    inferior: &Inferior,
    address: VirtualAddress,
) -> Option<crate::ImageLocation> {
    let image = inferior.loaded_module.image_address(address).ok()?;
    Some(controller.module_image.locate(image))
}
