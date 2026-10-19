//! Target memory access that hides installed breakpoint traps.

use std::collections::BTreeMap;

use nix::unistd::Pid;

use crate::protocol::{ProcessId, StopId};
use crate::unwind::MemoryReader;
use crate::{
    Error, MemoryRead, MemoryReadCompletion, MemoryReadUnavailableReason, Result, VirtualAddress,
};

use super::native::InspectionOps;
use super::{
    BreakpointSite, Controller, LinuxError, MAX_LOGICAL_MEMORY_READ, MAX_PUBLIC_MEMORY_READ,
    backend_error, validate_process, validate_public_stop,
};

impl<P: InspectionOps> Controller<P> {
    pub(super) fn read_memory(
        &self,
        requested_process: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        byte_count: u64,
    ) -> Result<MemoryRead> {
        if byte_count > MAX_PUBLIC_MEMORY_READ {
            return Err(Error::MemoryReadTooLarge {
                requested: byte_count,
                maximum: MAX_PUBLIC_MEMORY_READ,
            });
        }

        address
            .get()
            .checked_add(byte_count)
            .ok_or(Error::AddressOverflow)?;
        let size = usize::try_from(byte_count)
            .map_err(|_| Error::internal("a bounded memory read size fits usize"))?;
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let pid = inferior
            .public_stop
            .as_ref()
            .ok_or(Error::NotStopped)?
            .reader();

        let read = read_logical_memory(&self.ptrace, pid, &inferior.breakpoints, address, size)?;
        Ok(MemoryRead {
            revision: self.revision,
            stop_id,
            target: self.module_image.target(),
            address,
            requested: byte_count,
            bytes: read.bytes.into(),
            completion: read.completion,
        })
    }
}

pub(super) struct PtraceMemory<'a> {
    pub(super) ptrace: &'a dyn InspectionOps,
    pub(super) pid: Pid,
}

pub(super) struct LogicalMemoryRead {
    pub(super) bytes: Vec<u8>,
    pub(super) completion: MemoryReadCompletion,
}

pub(super) enum MemoryAccessError {
    Inaccessible,

    /// Only the first `readable` bytes of the word are accessible, such as
    /// where a core dump's file-backed memory ends inside a word.
    Partial {
        word: u64,
        readable: usize,
    },
    Fatal(Error),
}

pub(super) fn read_logical_memory(
    ptrace: &impl InspectionOps,
    pid: Pid,
    breakpoints: &BTreeMap<VirtualAddress, BreakpointSite>,
    address: VirtualAddress,
    size: usize,
) -> Result<LogicalMemoryRead> {
    read_logical_memory_with(address, size, breakpoints, |current| {
        ptrace.read_memory_word(pid, current)
    })
}

/// Reads `size` bytes at once, as stored, with every installed breakpoint's
/// trap hidden: in one read where the target allows it, or a word at a
/// time, so a read that cannot be made in one says how far it got.
pub(super) fn read_logical_block(
    ptrace: &impl InspectionOps,
    pid: Pid,
    breakpoints: &BTreeMap<VirtualAddress, BreakpointSite>,
    address: VirtualAddress,
    size: usize,
) -> Result<LogicalMemoryRead> {
    let end = address
        .get()
        .checked_add(u64::try_from(size).map_err(|_| Error::AddressOverflow)?)
        .ok_or(Error::AddressOverflow)?;
    if let Some(mut bytes) = ptrace.read_block(pid, address.get(), size) {
        debug_assert_eq!(
            bytes.len(),
            size,
            "a block read at {address} gave {} of {size} bytes",
            bytes.len()
        );
        for (site_address, site) in breakpoints.range(address..VirtualAddress::new(end)) {
            if site.installed {
                let offset = offset_within(site_address.get() - address.get())?;
                *bytes
                    .get_mut(offset)
                    .ok_or_else(|| Error::internal("a block read is shorter than requested"))? =
                    site.original_byte;
            }
        }

        check_traps_hidden(address, &bytes, breakpoints);
        return Ok(LogicalMemoryRead {
            bytes,
            completion: MemoryReadCompletion::Complete,
        });
    }

    let mut bytes = Vec::with_capacity(size);
    while bytes.len() < size {
        let next = address.get() + bytes.len() as u64;
        let chunk = (size - bytes.len()).min(MAX_LOGICAL_MEMORY_READ);
        let read = read_logical_memory(ptrace, pid, breakpoints, VirtualAddress::new(next), chunk)?;
        bytes.extend_from_slice(&read.bytes);
        if let MemoryReadCompletion::Incomplete { .. } = read.completion {
            return Ok(LogicalMemoryRead {
                bytes,
                completion: read.completion,
            });
        }
    }

    Ok(LogicalMemoryRead {
        bytes,
        completion: MemoryReadCompletion::Complete,
    })
}

/// A byte offset inside a word or read that the caller has bounded.
fn offset_within(offset: u64) -> Result<usize> {
    usize::try_from(offset).map_err(|_| Error::internal("a byte offset within a read fits usize"))
}

pub(super) fn read_logical_memory_with(
    address: VirtualAddress,
    size: usize,
    breakpoints: &BTreeMap<VirtualAddress, BreakpointSite>,
    mut read_word: impl FnMut(u64) -> std::result::Result<u64, MemoryAccessError>,
) -> Result<LogicalMemoryRead> {
    if size > MAX_LOGICAL_MEMORY_READ {
        return Err(backend_error(LinuxError::MemoryReadTooLarge {
            size,
            maximum: MAX_LOGICAL_MEMORY_READ,
        }));
    }

    if size == 0 {
        return Ok(LogicalMemoryRead {
            bytes: Vec::new(),
            completion: MemoryReadCompletion::Complete,
        });
    }

    let end = address
        .get()
        .checked_add(size as u64)
        .ok_or(Error::AddressOverflow)?;
    let mut bytes = Vec::with_capacity(size);
    let word_size = u64::from(u64::BITS / 8);
    let mut current = address.get() & !(word_size - 1);

    while current < end {
        let (mut word, readable) = match read_word(current) {
            Ok(word) => (word.to_le_bytes(), word_size),
            Err(MemoryAccessError::Partial { word, readable }) => (
                word.to_le_bytes(),
                u64::try_from(readable).unwrap_or(u64::MAX).min(word_size),
            ),
            Err(MemoryAccessError::Inaccessible) => (0_u64.to_le_bytes(), 0),
            Err(MemoryAccessError::Fatal(error)) => return Err(error),
        };

        let last_word_address = current.saturating_add(word_size - 1);

        for (&site_address, site) in
            breakpoints.range(VirtualAddress::new(current)..=VirtualAddress::new(last_word_address))
        {
            let offset = site_address.get() - current;
            if site.installed {
                word[offset_within(offset)?] = site.original_byte;
            }
        }

        let readable_end = current.saturating_add(readable);
        let selected_start = address.get().max(current);
        let selected_end = end.min(readable_end);

        if selected_start < selected_end {
            let start = offset_within(selected_start - current)?;
            let stop = offset_within(selected_end - current)?;
            bytes.extend_from_slice(&word[start..stop]);
        }

        if readable < word_size && readable_end < end {
            return Ok(LogicalMemoryRead {
                completion: MemoryReadCompletion::Incomplete {
                    next_address: VirtualAddress::new(address.get().max(readable_end)),
                    reason: MemoryReadUnavailableReason::Inaccessible,
                },
                bytes,
            });
        }

        if current.saturating_add(word_size) >= end {
            break;
        }

        current = current
            .checked_add(word_size)
            .ok_or(Error::AddressOverflow)?;
    }

    check_traps_hidden(address, &bytes, breakpoints);
    Ok(LogicalMemoryRead {
        bytes,
        completion: MemoryReadCompletion::Complete,
    })
}

/// Panics in development builds if `bytes`, read at `address`, show the
/// trap of an installed breakpoint rather than the byte it replaced.
fn check_traps_hidden(
    address: VirtualAddress,
    bytes: &[u8],
    breakpoints: &BTreeMap<VirtualAddress, BreakpointSite>,
) {
    if !cfg!(debug_assertions) || bytes.is_empty() {
        return;
    }

    let end = VirtualAddress::new(address.get().saturating_add(bytes.len() as u64));
    for (&site_address, site) in breakpoints.range(address..end) {
        let offset = usize::try_from(site_address.get() - address.get()).unwrap_or(usize::MAX);
        if site.installed
            && let Some(&byte) = bytes.get(offset)
        {
            debug_assert_eq!(
                byte, site.original_byte,
                "a read at {address} shows the trap installed at {site_address}"
            );
        }
    }
}

impl MemoryReader for PtraceMemory<'_> {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        self.ptrace.read_word(self.pid, address.get()).ok()
    }
}
