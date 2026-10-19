//! Perf samples (K-PERF-*): what the kernel writes when it samples a
//! thread in user code, for the events microscope's profiler opens, byte for
//! byte as the recorder's decoder reads them.
//!
//! The kernel takes no sample of its own accord. A probe arms an execute
//! breakpoint, which samples every thread of a process each time it
//! reaches an address, as `PERF_TYPE_BREAKPOINT` does; the world samples
//! threads where they are, as a clock event's tick would find them.

use super::{Kernel, Tgid, Tid};
use crate::sim::cpu::{RBP, RSP, Registers};
use crate::sim::memory::AddressSpace;

/// `PERF_RECORD_SAMPLE`, and the `misc` of one taken in user code.
const RECORD_SAMPLE: u32 = 9;
const MISC_USER: u16 = 2;

/// The callchain's mark before the user addresses.
const CONTEXT_USER: u64 = (-512_i64).cast_unsigned();

/// `PERF_SAMPLE_REGS_ABI_64`.
const ABI_64: u64 = 2;

/// The most addresses a chain holds: the events' `sample_max_stack`.
const MAX_STACK: usize = 127;

/// The end of user memory, `TASK_SIZE_MAX`.
const TASK_SIZE: u64 = 0x7fff_ffff_f000;

/// An execute breakpoint sampling every thread of one process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakpoint {
    pub tgid: Tgid,
    pub address: u64,
    pub stack_bytes: u32,

    /// The records written so far, as a ring holds them.
    pub records: Vec<u8>,
}

impl Kernel {
    /// The record of a sample of `tid` where it is now, in user code,
    /// copying at most `stack_bytes` of its stack, which is a multiple of
    /// eight.
    ///
    /// K-PERF-1: the sample's fields are in the order of the events'
    /// `sample_type`. Its chain is `PERF_CONTEXT_USER`, the instruction
    /// pointer, and the frame-pointer walk (K-PERF-2); its registers are
    /// the thread's, in perf's numbering; its stack is K-PERF-3's copy.
    pub fn sample(&mut self, tid: Tid, stack_bytes: u32) -> Vec<u8> {
        let thread = &self.threads[&tid];
        let space = &self.processes[&thread.tgid].space;
        let registers = &thread.registers;
        let mut chain = vec![CONTEXT_USER, registers.rip];
        walk_frames(space, registers.general[RBP], &mut chain);
        if chain.len() > MAX_STACK {
            self.gap("a frame-pointer walk as deep as a chain holds");
            return Vec::new();
        }

        let thread = &self.threads[&tid];
        let space = &self.processes[&thread.tgid].space;
        let registers = &thread.registers;

        let mut words = vec![
            0,
            u64::from(thread.tgid.id().cast_unsigned()) | u64::from(tid.cast_unsigned()) << 32,
            thread.retired,
            0,
            1,
            chain.len() as u64,
        ];
        words.extend(&chain);
        words.push(ABI_64);
        words.extend(perf_registers(registers));

        let header = 8 + 8 * words.len();
        let sp = registers.general[RSP];
        let size = stack_size(stack_bytes, header, sp);
        let copied = space.present(sp, size);
        let mut stack = space
            .peek_bytes(sp, usize::try_from(copied).expect("a copy fits"))
            .unwrap_or_default();

        #[cfg(test)]
        if self.sabotage == Some(super::Sabotage::SkewSampledReturnAddresses) {
            skew_return_addresses(&thread.shadow, sp, &mut stack);
        }

        stack.resize(usize::try_from(size).expect("a copy fits"), 0);

        let length = header + 16 + stack.len();
        let mut record = Vec::with_capacity(length);
        record.extend(RECORD_SAMPLE.to_le_bytes());
        record.extend(MISC_USER.to_le_bytes());
        record.extend(u16::try_from(length).expect("a sample fits").to_le_bytes());
        for word in words {
            record.extend(word.to_le_bytes());
        }

        record.extend(size.to_le_bytes());
        record.extend(&stack);
        record.extend(copied.to_le_bytes());
        record
    }

    /// Samples `tid` if an armed breakpoint covers the instruction it is
    /// about to execute (K-PERF-4).
    pub(super) fn sample_breakpoint(&mut self, tid: Tid) {
        let thread = &self.threads[&tid];
        let Some(breakpoint) = &self.perf else {
            return;
        };

        if breakpoint.tgid != thread.tgid || breakpoint.address != thread.registers.rip {
            return;
        }

        let record = self.sample(tid, breakpoint.stack_bytes);
        if let Some(breakpoint) = &mut self.perf {
            breakpoint.records.extend(record);
        }
    }
}

/// K-PERF-2: the kernel's walk from `link`: while the two words at the
/// link lie below the end of user memory in present pages, the upper is
/// the next return address and the lower the next link. A walk that fills
/// the chain stops there, which no probe pins.
fn walk_frames(space: &AddressSpace, mut link: u64, chain: &mut Vec<u64>) {
    while chain.len() - 1 < MAX_STACK {
        if link.checked_add(16).is_none_or(|end| end > TASK_SIZE) || space.present(link, 16) < 16 {
            break;
        }

        let (Some(next), Some(return_address)) = (space.peek(link), space.peek(link + 8)) else {
            break;
        };

        chain.push(return_address);
        link = next;
    }
}

/// The registers [`decode::REGISTERS`] asks for, in perf's order: ax, bx,
/// cx, dx, si, di, bp, sp, ip, flags, then r8 to r15.
pub fn perf_registers(registers: &Registers) -> impl Iterator<Item = u64> + '_ {
    let general = &registers.general;
    [
        general[0],
        general[3],
        general[1],
        general[2],
        general[6],
        general[7],
        general[5],
        general[4],
        registers.rip,
        registers.rflags,
    ]
    .into_iter()
    .chain(general[8..].iter().copied())
}

/// K-PERF-3: the bytes of stack a sample asks for: what the event asks,
/// no more than reach the end of user memory, and, where the record would
/// pass 64 KiB, what still fits, rounded up to a word.
fn stack_size(asked: u32, header: usize, sp: u64) -> u64 {
    let header = header + 16;
    let fits = u64::from(u16::MAX).min(TASK_SIZE.saturating_sub(sp));
    let size = u64::from(asked).min(fits);
    if header as u64 + size > u64::from(u16::MAX) {
        (u64::from(u16::MAX) - header as u64 - 8).next_multiple_of(8)
    } else {
        size
    }
}

/// Reports the return address in each slot a call pushed one, within the
/// copy, as the next address.
#[cfg(test)]
fn skew_return_addresses(shadow: &super::shadow::Shadow, sp: u64, stack: &mut [u8]) {
    for call in &shadow.calls {
        let Some(offset) = call.slot.checked_sub(sp) else {
            continue;
        };

        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        if let Some(word) = stack.get_mut(offset..offset.saturating_add(8))
            && word.len() == 8
        {
            let value = u64::from_le_bytes((&*word).try_into().expect("eight bytes"));
            word.copy_from_slice(&value.wrapping_add(1).to_le_bytes());
        }
    }
}
