//! `x86_64` hardware debug registers (`DR0`-`DR3`, `DR6`, `DR7`) for
//! watchpoints and single-step status.
//!
//! Models Linux's alignment/user-limit validation, `ENOSPC` slot contention
//! with other perf/debug users, and `DR6`'s active-high reserved bits
//! (`DR6_RESERVED`).

use nix::errno::Errno;

use crate::sim::cpu::{Access, Accesses};

pub const DR_STEP: u64 = 1 << 14;

pub const DR6_RESERVED: u64 = 0xffff_0ff0;

const DR7_RESERVED: u64 = 0xfc00;

const USER_LIMIT: u64 = 0x7fff_ffff_f000;

const SLOTS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Execute,
    Write,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Breakpoint {
    address: u64,
    length: u64,
    kind: Kind,
    enabled: bool,
}

impl Breakpoint {
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugRegisters {
    slots: [Option<Breakpoint>; SLOTS],

    dr7: u64,

    dr6: u64,
}

impl DebugRegisters {
    #[must_use]
    pub const fn inherited(&self) -> Self {
        Self {
            slots: [None; SLOTS],
            dr7: self.dr7,
            dr6: 0,
        }
    }

    fn reserved(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    pub fn peek(&self, index: usize) -> u64 {
        match index {
            0..SLOTS => self.slots[index].map_or(0, |slot| slot.address),
            6 => self.dr6 ^ DR6_RESERVED,
            7 => self.dr7,
            _ => 0,
        }
    }

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

    #[must_use]
    pub fn breaks_on_execution(&self) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|slot| slot.enabled && slot.kind == Kind::Execute)
    }

    #[must_use]
    pub fn hits(&self, accesses: &Accesses) -> u64 {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.map(|slot| (index, slot)))
            .filter(|(_, slot)| accesses.iter().any(|access| slot.matches(access)))
            .fold(0, |hits, (index, _)| hits | 1 << index)
    }

    pub const fn debug_exception(&mut self, single_step: bool, hits: u64) {
        self.dr6 = if single_step { DR_STEP } else { 0 } | hits;
    }
}
