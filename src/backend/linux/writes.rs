//! Changing memory, variables, and registers at a stop.

use std::sync::Arc;

use nix::unistd::Pid;

use crate::protocol::{BreakpointSpec, DebuggerEvent, ExecutionId, Reply, StopId, StopReason};
use crate::runtime_model::RuntimeWrite;
use crate::{CodeInstanceId, Error, ModuleId, ProcessId, Result, VirtualAddress};

use super::memory::read_logical_memory;
use super::native::LinuxTraceOps;
use super::{
    Controller, LinuxError, backend_error, debug_thread_id, process_id, validate_process,
    validate_public_stop, validate_resumable, validate_stopped_thread,
};

/// The most bytes one request writes.
pub(super) const MAX_PUBLIC_MEMORY_WRITE: u64 = 64 * 1024;
const WORD: u64 = 8;

impl<P: LinuxTraceOps> Controller<P> {
    /// Writes bytes into the stopped process and returns how many were
    /// written: all of them, or those before memory that cannot be written.
    /// Installed breakpoint traps stay in place; the bytes they hide become
    /// the bytes written there.
    pub(super) fn write_memory(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        bytes: &[u8],
    ) -> Result<u64> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let pid = inferior
            .public_stop
            .as_ref()
            .ok_or(Error::NotStopped)?
            .reader();
        self.write_memory_as(pid, address, bytes)
    }

    /// Checks all expected bytes before making a runtime's writes, while
    /// every thread is stopped. A kernel write failure can leave earlier
    /// bytes changed; the error reports that partial progress.
    pub(super) fn write_for_runtime(
        &mut self,
        writes: &[RuntimeWrite],
    ) -> std::result::Result<(), Arc<str>> {
        let inferior = self
            .inferior
            .as_ref()
            .ok_or_else(|| Arc::<str>::from("the process is gone"))?;
        if !inferior
            .threads
            .iter()
            .all(|(&pid, thread)| inferior.settled(pid, thread))
        {
            return Err("the process is running".into());
        }

        let pid = inferior.memory_thread();

        for write in writes {
            if write.expected.len() != write.bytes.len() {
                return Err(format!(
                    "the write at {} replaces {} bytes with {}",
                    write.address,
                    write.expected.len(),
                    write.bytes.len()
                )
                .into());
            }

            let held = read_logical_memory(
                &self.ptrace,
                pid,
                &inferior.breakpoints,
                write.address,
                write.expected.len(),
            )
            .map_err(|error| -> Arc<str> {
                format!("the memory at {} cannot be read: {error}", write.address).into()
            })?;
            if held.bytes != write.expected {
                return Err(format!(
                    "the memory at {} no longer holds what the runtime read",
                    write.address
                )
                .into());
            }
        }

        for (made, write) in writes.iter().enumerate() {
            record!(
                "a runtime writes {} bytes at {}",
                write.bytes.len(),
                write.address
            );
            let written = self
                .write_memory_as(pid, write.address, &write.bytes)
                .map_err(|error| error.to_string());
            if written != Ok(write.bytes.len() as u64) {
                let error =
                    written.map_or_else(|error| error, |count| format!("{count} bytes were"));
                return Err(format!(
                    "the memory at {} could not be written ({error}), after {made} of the \
                     runtime's {} writes were made",
                    write.address,
                    writes.len()
                )
                .into());
            }
        }

        Ok(())
    }

    /// Writes memory through a stopped thread; see [`Self::write_memory`].
    pub(super) fn write_memory_as(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        bytes: &[u8],
    ) -> Result<u64> {
        let length = bytes.len() as u64;

        // Scans resumed after a write could follow links it changed, and
        // a task may no longer be where its runtime kept it.
        self.views.forget_scans();

        if let Some(stop) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.public_stop.as_ref())
        {
            stop.task_locators.borrow_mut().clear();
            stop.task_sets.borrow_mut().take();
            stop.interpreted.borrow_mut().clear();
            stop.memory.forget();
        }

        if length > MAX_PUBLIC_MEMORY_WRITE {
            return Err(Error::MemoryWriteTooLarge {
                requested: length,
                maximum: MAX_PUBLIC_MEMORY_WRITE,
            });
        }

        let end = address
            .get()
            .checked_add(length)
            .ok_or(Error::AddressOverflow)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let mut word = address.get() & !(WORD - 1);
        let mut written = 0;

        while word < end {
            let Ok(current) = self.ptrace.read_word(pid, word) else {
                break;
            };

            let mut physical = current.to_ne_bytes();
            let mut hidden = Vec::new();
            let mut count = 0;

            for (offset, byte) in physical.iter_mut().enumerate() {
                let target = word + offset as u64;
                if !(address.get()..end).contains(&target) {
                    continue;
                }

                let user = usize::try_from(target - address.get())
                    .ok()
                    .and_then(|offset| bytes.get(offset))
                    .copied()
                    .ok_or_else(|| Error::internal("a write covers the bytes it stores"))?;
                count += 1;

                match inferior.breakpoints.get(&VirtualAddress::new(target)) {
                    Some(site) if site.installed => hidden.push((target, user)),
                    _ => *byte = user,
                }
            }

            if self
                .ptrace
                .write_word(pid, word, u64::from_ne_bytes(physical))
                .is_err()
            {
                break;
            }

            for (target, byte) in hidden {
                if let Some(site) = inferior.breakpoints.get_mut(&VirtualAddress::new(target)) {
                    site.original_byte = byte;
                }
            }

            written += count;
            word += WORD;
        }

        if written == 0 && length != 0 {
            return Err(Error::MemoryNotWritable(address));
        }

        Ok(written)
    }

    /// Writes the low bytes of a register of the innermost frame. A thread
    /// whose program counter is written is no longer where a system call
    /// it was in would restart, which the kernel would otherwise do by
    /// moving it back.
    pub(super) fn write_register(
        &self,
        pid: Pid,
        register: crate::RegisterId,
        bytes: &[u8],
    ) -> Result<()> {
        self.views.forget_scans();
        let mut registers = self.ptrace.registers(pid)?;
        let slot = super::registers::x86_64_register_slot(&mut registers, register)
            .ok_or_else(|| backend_error(LinuxError::UnsupportedRegisterWrite))?;
        let mut value = slot.to_le_bytes();
        value[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
        *slot = u64::from_le_bytes(value);

        if super::registers::is_program_counter(register) {
            registers.orig_rax = u64::MAX;
        }

        self.ptrace.set_registers(pid, registers)
    }

    /// Moves a stopped thread, without running it, to resume at the one
    /// location `spec` resolves to in the function it is stopped in, and
    /// publishes the stop again there.
    pub(super) fn jump(
        &mut self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        spec: BreakpointSpec,
        reply: Reply<ExecutionId>,
    ) {
        let moved = self
            .jump_target(process_id, stop_id, pid, spec)
            .and_then(|target| {
                record!("jump {pid} to {target}");
                let program_counter = crate::RegisterId::new(super::registers::PROGRAM_COUNTER);
                self.write_register(pid, program_counter, &target.get().to_le_bytes())?;
                self.publish_moved_thread(pid)
            });

        match moved {
            // Acknowledge the jump before publishing the stop it made.
            Ok((execution, stopped)) => {
                let _ = reply.send(Ok(execution));
                let _ = self.events.send(stopped);
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    /// Where a jump moves a thread: the one address `spec` resolves to in
    /// the code of the function the thread is stopped in. Leaving the
    /// function would leave its frame for another's, so only an
    /// assignment of the program counter does that.
    fn jump_target(
        &self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        spec: BreakpointSpec,
    ) -> Result<VirtualAddress> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_process(inferior, process_id)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_resumable(inferior)?;
        validate_stopped_thread(inferior, pid)?;
        let pc = VirtualAddress::new(self.ptrace.registers(pid)?.rip);
        let function = self
            .physical_function_at(pc)
            .ok_or(Error::JumpWithoutFunction)?;
        let described = Arc::<str>::from(spec.to_string());
        let targets = self
            .advance_targets(spec)?
            .into_iter()
            .filter(|target| self.physical_function_at(*target) == Some(function))
            .collect::<Vec<_>>();

        match targets.as_slice() {
            [target] => Ok(*target),
            [] => Err(Error::JumpOutsideFunction(described)),
            _ => Err(Error::AmbiguousJump(described)),
        }
    }

    /// The module and out-of-line code instance holding an address.
    fn physical_function_at(&self, address: VirtualAddress) -> Option<(ModuleId, CodeInstanceId)> {
        let inferior = self.inferior.as_ref()?;
        let main = (
            ModuleId::new(0),
            &self.module_image,
            &inferior.loaded_module,
        );
        std::iter::once(main)
            .chain(
                self.modules
                    .iter()
                    .filter(|(id, _)| **id != ModuleId::new(0))
                    .map(|(id, module)| (*id, &module.image, &module.loaded)),
            )
            .find_map(|(id, image, loaded)| {
                let address = loaded.image_address(address).ok()?;
                image.contains_address(address).then_some(())?;
                Some((id, image.locate(address).physical_instance?))
            })
    }

    /// Publishes the stop again after a thread was moved to resume
    /// elsewhere: under a new stop, as everything a client read from the
    /// old one, from frames to values, may now differ. A thread that
    /// reported a breakpoint's trap and stands there still steps over it as
    /// it resumes; one moved anywhere else has not arrived at a breakpoint
    /// where it lands, which reports it as it resumes, as gdb does.
    pub(super) fn publish_moved_thread(
        &mut self,
        pid: Pid,
    ) -> Result<(ExecutionId, DebuggerEvent)> {
        let reason = StopReason::Jump;
        let presentation = self.presentation_for_thread(pid, Some(&reason))?;
        let pc = VirtualAddress::new(self.ptrace.registers(pid)?.rip);
        let stop_id = self.ptrace.allocate_stop_id()?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let execution = ExecutionId::new(inferior.next_execution + 1);
        inferior.next_execution = execution.get();
        let thread = inferior.thread_mut(pid)?;
        thread.stopped_at_breakpoint = thread.stopped_at_breakpoint.filter(|trap| *trap == pc);
        thread.reason = Some(reason.clone());
        let stop = inferior.public_stop.as_mut().ok_or(Error::NotStopped)?;
        stop.id = stop_id;
        stop.triggering_thread = pid;
        stop.reason = reason.clone();
        stop.selected = crate::ExecutionContext::Thread(debug_thread_id(pid));
        stop.selected_thread = Some(pid);
        stop.selected_frames.clear();
        stop.returned = None;
        stop.presentations.insert(pid, presentation);
        stop.task_locators.borrow_mut().clear();
        stop.task_sets.borrow_mut().take();
        stop.interpreted.borrow_mut().clear();
        stop.memory.forget();
        let process_id = process_id(inferior.tgid);
        self.bump_revision();

        Ok((
            execution,
            DebuggerEvent::InferiorStopped {
                revision: self.revision,
                process_id,
                execution_id: Some(execution),
                stop_id,
                thread_id: debug_thread_id(pid),
                reason,
            },
        ))
    }
}
