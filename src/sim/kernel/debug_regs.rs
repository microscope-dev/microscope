//! Debug registers, as Linux keeps them for a traced thread (K-DR-*).
//!
//! Each thread has four slots, each backed by a perf hardware breakpoint
//! once the tracer writes its address, the DR7 the tracer last wrote, and
//! a virtual DR6.
//!
//! - Writing a slot's address reserves a hardware breakpoint even while
//!   disabled, which fails with `ENOSPC` once others hold every slot.
//! - DR7 writes are transactional: one a slot refuses leaves every slot as
//!   it was.
//! - A new thread starts with no slots, but reads back its creator's DR7.
//! - DR6 changes only at debug exceptions, which reset it to the
//!   single-step bit and then set a bit per slot that hit.

use nix::errno::Errno;

use crate::sim::cpu::{Access, Accesses};

/// DR6's single-step bit.
pub const DR_STEP: u64 = 1 << 14;

/// The DR6 bits that read as set with no condition recorded.
pub const DR6_RESERVED: u64 = 0xffff_0ff0;

/// The DR7 bits a write ignores.
const DR7_RESERVED: u64 = 0xfc00;

/// The first address no user hardware breakpoint may cover.
const USER_LIMIT: u64 = 0x7fff_ffff_f000;

const SLOTS: usize = 4;

/// What a slot's accesses are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Execute,
    Write,
    ReadWrite,
}

/// A slot's hardware breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Breakpoint {
    address: u64,
    length: u64,
    kind: Kind,
    enabled: bool,
}

impl Breakpoint {
    /// Linux's checks of a breakpoint's fields.
    fn validate(self) -> Result<Self, Errno> {
        let aligned = self.address.is_multiple_of(self.length);
        let user = self
            .address
            .checked_add(self.length)
            .is_some_and(|end| end <= USER_LIMIT);
        let shape = self.kind != Kind::Execute || self.length == 1;

        if aligned && user && shape {
            Ok(self)
        } else {
            Err(Errno::EINVAL)
        }
    }

    const fn matches(self, access: Access) -> bool {
        let kind = match self.kind {
            Kind::Execute => false,
            Kind::Write => access.write,
            Kind::ReadWrite => true,
        };

        self.enabled
            && kind
            && access.address < self.address + self.length
            && self.address < access.address + access.size
    }
}

/// One slot's fields in DR7: whether it is enabled, and its length and
/// kind, or `EINVAL` for a kind x86 has no user breakpoint for.
const fn decode(dr7: u64, slot: usize) -> Result<(bool, u64, Kind), Errno> {
    let enabled = (dr7 >> (2 * slot)) & 3 != 0;
    let fields = dr7 >> (16 + 4 * slot);
    let kind = match fields & 3 {
        0 => Kind::Execute,
        1 => Kind::Write,
        3 => Kind::ReadWrite,
        _ => return Err(Errno::EINVAL),
    };

    let length = match (fields >> 2) & 3 {
        0 => 1,
        1 => 2,
        2 => 8,
        _ => 4,
    };

    Ok((enabled, length, kind))
}

/// One thread's debug registers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugRegisters {
    slots: [Option<Breakpoint>; SLOTS],

    /// The DR7 the tracer last wrote, which reads back.
    dr7: u64,

    /// DR6 as Linux virtualizes it: the bits that differ from idle.
    dr6: u64,
}

impl DebugRegisters {
    /// A new thread's registers: no slots, and the DR7 its creator reads.
    #[must_use]
    pub const fn inherited(&self) -> Self {
        Self {
            slots: [None; SLOTS],
            dr7: self.dr7,
            dr6: 0,
        }
    }

    /// How many hardware breakpoints the thread holds.
    fn reserved(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// `PTRACE_PEEKUSER` of debug register `index`.
    pub fn peek(&self, index: usize) -> u64 {
        match index {
            0..SLOTS => self.slots[index].map_or(0, |slot| slot.address),
            6 => self.dr6 ^ DR6_RESERVED,
            7 => self.dr7,
            _ => 0,
        }
    }

    /// `PTRACE_POKEUSER` of debug register `index`, with `others` slots
    /// held by other users of the thread's hardware breakpoints.
    pub fn poke(&mut self, index: usize, value: u64, others: usize) -> Result<(), Errno> {
        match index {
            0..SLOTS => {
                let slot = match self.slots[index] {
                    Some(slot) => Breakpoint {
                        address: value,
                        ..slot
                    },
                    None if self.reserved() + others >= SLOTS => return Err(Errno::ENOSPC),
                    None => Breakpoint {
                        address: value,
                        length: 1,
                        kind: Kind::Write,
                        enabled: false,
                    },
                };

                self.slots[index] = Some(slot.validate()?);
                Ok(())
            }
            6 => {
                self.dr6 = value ^ DR6_RESERVED;
                Ok(())
            }
            7 => self.write_dr7(value & !DR7_RESERVED, others),
            _ => Err(Errno::EIO),
        }
    }

    /// Writes DR7, or leaves every slot as it was if one refuses.
    fn write_dr7(&mut self, dr7: u64, others: usize) -> Result<(), Errno> {
        let before = self.slots;

        for index in 0..SLOTS {
            let applied =
                decode(dr7, index).and_then(|(enabled, length, kind)| match self.slots[index] {
                    None if !enabled => Ok(None),
                    None if self.reserved() + others >= SLOTS => Err(Errno::ENOSPC),
                    None => Breakpoint {
                        address: 0,
                        length,
                        kind,
                        enabled,
                    }
                    .validate()
                    .map(Some),
                    Some(slot) => Breakpoint {
                        length,
                        kind,
                        enabled,
                        ..slot
                    }
                    .validate()
                    .map(Some),
                });

            match applied {
                Ok(slot) => self.slots[index] = slot,
                Err(errno) => {
                    self.slots = before;
                    return Err(errno);
                }
            }
        }

        self.dr7 = dr7;
        Ok(())
    }

    /// Whether an enabled slot breaks on instructions, which the
    /// simulation does not model.
    #[must_use]
    pub fn breaks_on_execution(&self) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|slot| slot.enabled && slot.kind == Kind::Execute)
    }

    /// The slots an instruction's accesses hit, as DR6's B0 to B3.
    #[must_use]
    pub fn hits(&self, accesses: &Accesses) -> u64 {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.map(|slot| (index, slot)))
            .filter(|(_, slot)| accesses.iter().any(|access| slot.matches(access)))
            .fold(0, |hits, (index, _)| hits | 1 << index)
    }

    /// Records a debug exception: a single step, slots that hit, or both.
    pub const fn debug_exception(&mut self, single_step: bool, hits: u64) {
        self.dr6 = if single_step { DR_STEP } else { 0 } | hits;
    }
}
