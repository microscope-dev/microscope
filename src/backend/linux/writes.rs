use std::sync::Arc;

use nix::unistd::Pid;

use crate::protocol::{BreakpointSpec, DebuggerEvent, ExecutionId, Reply, StopId, StopReason};
use crate::runtime_model::RuntimeWrite;
use crate::{CodeInstanceId, Error, ModuleId, ProcessId, Result, VirtualAddress};

use super::memory::{read_logical_memory, write_logical_memory_with};
use super::native::LinuxTraceOps;
use super::trap::TrapEvent;
use super::{
    Controller, LinuxError, backend_error, debug_thread_id, process_id, validate_process,
    validate_public_stop, validate_resumable, validate_stopped_thread,
};

pub(super) const MAX_PUBLIC_MEMORY_WRITE: u64 = 64 * 1024;

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn write_memory(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        bytes: &[u8],
    ) -> Result<u64> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let pid = inferior.run.stop().ok_or(Error::NotStopped)?.reader();
        self.write_memory_as(pid, address, bytes)
    }

    pub(super) fn write_for_runtime(
        &mut self,
        writes: &[RuntimeWrite],
    ) -> std::result::Result<(), Arc<str>> {
        let inferior = self
            .session
            .inferior()
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

    pub(super) fn write_memory_as(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        bytes: &[u8],
    ) -> Result<u64> {
        let length = bytes.len() as u64;

        self.views.forget_scans();

        if length > MAX_PUBLIC_MEMORY_WRITE {
            return Err(Error::MemoryWriteTooLarge {
                requested: length,
                maximum: MAX_PUBLIC_MEMORY_WRITE,
            });
        }

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let published = match inferior.run.stop_mut() {
            Some(stop) => {
                stop.caches.after_memory_write();
                true
            }
            None => false,
        };

        let written = write_logical_memory_with(
            &mut inferior.breakpoints,
            address,
            bytes,
            |word| self.ptrace.read_word(pid, word),
            |word, value| self.ptrace.write_word(pid, word, value),
        );

        if published {
            self.bump_revision();
        }

        written
    }

    pub(super) fn write_register(
        &mut self,
        pid: Pid,
        register: crate::RegisterId,
        bytes: &[u8],
    ) -> Result<()> {
        self.views.forget_scans();
        if let Some(stop) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.run.stop_mut())
        {
            stop.caches.after_register_write();
        }

        let mut registers = self.ptrace.registers(pid)?;
        let slot = registers
            .slot(register)
            .ok_or_else(|| backend_error(LinuxError::UnsupportedRegisterWrite))?;
        let mut value = slot.to_le_bytes();
        for (to, from) in value.iter_mut().zip(bytes) {
            *to = *from;
        }
        *slot = u64::from_le_bytes(value);

        let moves = super::arch::is_program_counter(register);
        if moves {
            registers.cancel_syscall_restart();
        }

        self.ptrace.set_registers(pid, registers)?;

        if !moves {
            if let Some(stop) = self
                .session
                .inferior_mut()
                .and_then(|inferior| inferior.run.stop_mut())
            {
                stop.selected_frames.clear();
            }
            self.bump_revision();
        }

        Ok(())
    }

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
                let program_counter = crate::RegisterId::new(super::arch::PROGRAM_COUNTER);
                self.write_register(pid, program_counter, &target.get().to_le_bytes())?;
                self.publish_moved_thread(pid)
            });

        match moved {
            Ok((execution, stopped)) => {
                let _ = reply.send(Ok(execution));
                let _ = self.events.send(stopped);
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn jump_target(
        &self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        spec: BreakpointSpec,
    ) -> Result<VirtualAddress> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        validate_process(inferior, process_id)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_resumable(inferior)?;
        validate_stopped_thread(inferior, pid)?;
        let pc = self.ptrace.registers(pid)?.program_counter();
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

    fn physical_function_at(&self, address: VirtualAddress) -> Option<(ModuleId, CodeInstanceId)> {
        let inferior = self.session.inferior()?;
        let main = (self.main.id, &self.main.image, &inferior.loaded_module);
        std::iter::once(main)
            .chain(
                self.modules
                    .iter()
                    .filter(|(id, _)| **id != self.main.id)
                    .map(|(id, module)| (*id, &module.image, &module.loaded)),
            )
            .find_map(|(id, image, loaded)| {
                let address = loaded.image_address(address).ok()?;
                image.contains_address(address).then_some(())?;
                Some((id, image.locate(address).physical_instance?))
            })
    }

    pub(super) fn publish_moved_thread(
        &mut self,
        pid: Pid,
    ) -> Result<(ExecutionId, DebuggerEvent)> {
        let reason = StopReason::Jump;
        let presentation = self.presentation_for_thread(pid, Some(&reason))?;
        let pc = self.ptrace.registers(pid)?.program_counter();
        let stop_id = self.ptrace.allocate_stop_id()?;
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let execution = ExecutionId::new(inferior.next_execution + 1);
        inferior.next_execution = execution.get();
        inferior.trap_event(pid, TrapEvent::Moved(pc))?;
        let thread = inferior.thread_mut(pid)?;

        thread.retrap = thread.retrap.filter(|trap| *trap == pc);
        thread.reason = Some(reason.clone());
        let stop = inferior.run.stop_mut().ok_or(Error::NotStopped)?;
        stop.id = stop_id;
        stop.triggering_thread = pid;
        stop.reason = reason.clone();
        stop.selected = crate::ExecutionContext::Thread(debug_thread_id(pid));
        stop.selected_thread = Some(pid);
        stop.selected_frames.clear();
        stop.returned = None;
        stop.presentations.insert(pid, presentation);

        stop.caches = super::StopCaches::default();
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
