use std::collections::{BTreeMap, BTreeSet};

use nix::unistd::Pid;

use crate::protocol::{ProcessId, StopId};
use crate::unwind::MemoryReader;
use crate::{
    Error, MemoryRead, MemoryReadCompletion, MemoryReadUnavailableReason, Result, VirtualAddress,
};

use super::native::InspectionOps;
use super::trap::{Lift, Trap};
use super::{
    BREAKPOINT_OPCODE, BreakpointOwner, BreakpointSite, Controller, LinuxError,
    MAX_LOGICAL_MEMORY_READ, MAX_PUBLIC_MEMORY_READ, backend_error, validate_process,
    validate_public_stop,
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
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let pid = inferior.run.stop().ok_or(Error::NotStopped)?.reader();

        let read = read_logical_memory(&self.ptrace, pid, &inferior.breakpoints, address, size)?;
        Ok(MemoryRead {
            revision: self.revision,
            stop_id,
            target: self.main.image.target(),
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
            if site.trap == Trap::Installed {
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
            if site.trap == Trap::Installed {
                let byte = word
                    .get_mut(offset_within(offset)?)
                    .ok_or_else(|| Error::internal("a site in a word's range is in the word"))?;
                *byte = site.original_byte;
            }
        }

        let readable_end = current.saturating_add(readable);
        let selected_start = address.get().max(current);
        let selected_end = end.min(readable_end);

        if selected_start < selected_end {
            let start = offset_within(selected_start - current)?;
            let stop = offset_within(selected_end - current)?;
            let selected = word
                .get(start..stop)
                .ok_or_else(|| Error::internal("a read's selection lies in its word"))?;
            bytes.extend_from_slice(selected);
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
        if site.trap == Trap::Installed
            && let Some(&byte) = bytes.get(offset)
        {
            debug_assert_eq!(
                byte, site.original_byte,
                "a read at {address} shows the trap installed at {site_address}"
            );
        }
    }
}

pub(super) fn write_logical_memory_with(
    sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
    address: VirtualAddress,
    bytes: &[u8],
    mut read_word: impl FnMut(u64) -> Result<u64>,
    mut write_word: impl FnMut(u64, u64) -> Result<()>,
) -> Result<u64> {
    const WORD: u64 = 8;

    let length = bytes.len() as u64;
    let end = address
        .get()
        .checked_add(length)
        .ok_or(Error::AddressOverflow)?;
    let mut word = address.get() & !(WORD - 1);
    let mut written = 0;

    while word < end {
        let Ok(current) = read_word(word) else {
            break;
        };

        let mut physical = current.to_ne_bytes();
        let mut saved = Vec::new();
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

            match sites.get(&VirtualAddress::new(target)) {
                Some(site) => {
                    saved.push((target, user));
                    if site.trap != Trap::Installed {
                        *byte = user;
                    }
                }
                None => *byte = user,
            }
        }

        if write_word(word, u64::from_ne_bytes(physical)).is_err() {
            break;
        }

        for (target, byte) in saved {
            if let Some(site) = sites.get_mut(&VirtualAddress::new(target)) {
                site.original_byte = byte;
            }
        }

        written += count;
        let Some(next) = word.checked_add(WORD) else {
            break;
        };
        word = next;
    }

    if written == 0 && length != 0 {
        return Err(Error::MemoryNotWritable(address));
    }

    Ok(written)
}

pub(super) fn install_site(
    sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
    address: VirtualAddress,
    owner: BreakpointOwner,
    read_word: impl FnOnce(u64) -> Result<u64>,
    write_word: impl FnOnce(u64, u64) -> Result<()>,
) -> Result<()> {
    if let Some(site) = sites.get_mut(&address) {
        site.owners.insert(owner);
        return Ok(());
    }

    let original_byte = patch_byte(address, BREAKPOINT_OPCODE, read_word, write_word)?;
    sites.insert(
        address,
        BreakpointSite {
            original_byte,
            trap: Trap::Installed,
            owners: BTreeSet::from([owner]),
        },
    );

    Ok(())
}

pub(super) fn lift_site(
    sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
    address: VirtualAddress,
    lift: Lift,
    read_word: impl FnOnce(u64) -> Result<u64>,
    write_word: impl FnOnce(u64, u64) -> Result<()>,
) -> Result<()> {
    let site = sites
        .get_mut(&address)
        .ok_or_else(|| Error::internal("a breakpoint site is known"))?;
    if site.trap != Trap::Installed {
        return Ok(());
    }

    patch_byte(address, site.original_byte, read_word, write_word)?;
    site.trap = Trap::Lifted(lift);
    Ok(())
}

pub(super) fn reinstall_site(
    sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
    address: VirtualAddress,
    read_word: impl FnOnce(u64) -> Result<u64>,
    write_word: impl FnOnce(u64, u64) -> Result<()>,
) -> Result<()> {
    let site = sites
        .get_mut(&address)
        .ok_or_else(|| Error::internal("a breakpoint site is known"))?;
    if site.trap == Trap::Installed {
        return Ok(());
    }

    patch_byte(address, BREAKPOINT_OPCODE, read_word, write_word)?;
    site.trap = Trap::Installed;
    Ok(())
}

pub(super) fn remove_site(
    sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
    address: VirtualAddress,
    read_word: impl FnOnce(u64) -> Result<u64>,
    write_word: impl FnOnce(u64, u64) -> Result<()>,
) -> Result<BreakpointSite> {
    let site = sites
        .get(&address)
        .ok_or_else(|| Error::internal("a breakpoint site is known"))?;
    if site.trap == Trap::Installed {
        patch_byte(address, site.original_byte, read_word, write_word)?;
    }

    sites
        .remove(&address)
        .ok_or_else(|| Error::internal("a breakpoint site is known"))
}

pub(super) fn remove_site_owner(
    sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
    address: VirtualAddress,
    owner: BreakpointOwner,
    remove: impl FnOnce(&mut BTreeMap<VirtualAddress, BreakpointSite>) -> Result<BreakpointSite>,
) -> Result<Option<BreakpointSite>> {
    let site = sites
        .get_mut(&address)
        .ok_or_else(|| backend_error(LinuxError::BreakpointSiteMissing(address)))?;
    if !site.owners.contains(&owner) {
        return Err(backend_error(LinuxError::BreakpointOwnerMissing(address)));
    }

    if site.owners.len() > 1 {
        let removed = site.owners.remove(&owner);
        debug_assert!(
            removed,
            "{owner:?} owned the site at {address}, then did not"
        );
        return Ok(None);
    }

    remove(sites).map(Some)
}

pub(super) const fn holding_word(address: u64) -> (u64, WordByte) {
    (address & !7, WordByte((address & 7) as usize))
}

#[derive(Debug, Clone, Copy)]
pub(super) struct WordByte(usize);

impl WordByte {
    pub(super) const fn get(self) -> usize {
        self.0
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "holding_word masks offsets below eight"
    )]
    pub(super) fn of(self, bytes: &mut [u8; 8]) -> &mut u8 {
        debug_assert!(self.0 < 8, "byte {} of a word", self.0);
        &mut bytes[self.0]
    }
}

pub(super) fn patch_byte(
    address: VirtualAddress,
    byte: u8,
    read_word: impl FnOnce(u64) -> Result<u64>,
    write_word: impl FnOnce(u64, u64) -> Result<()>,
) -> Result<u8> {
    let (word, offset) = holding_word(address.get());
    let mut bytes = read_word(word)?.to_ne_bytes();
    let original = std::mem::replace(offset.of(&mut bytes), byte);
    write_word(word, u64::from_ne_bytes(bytes))?;
    Ok(original)
}

impl MemoryReader for PtraceMemory<'_> {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        self.ptrace.read_word(self.pid, address.get()).ok()
    }
}

#[cfg(test)]
mod tests;
