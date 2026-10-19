//! Simulated address spaces: mapped regions with protections, holding
//! 4 KiB pages that are shared until written.
//!
//! The CPU's accesses obey protections and fault where nothing is mapped;
//! ptrace's ignore protections and fail only where nothing is mapped
//! (K-MEM-1). Pages loaded from a golden image are shared by every session
//! that loads it and copied on a session's first write.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

pub const PAGE_SIZE: u64 = PAGE_BYTES as u64;

pub const PAGE_BYTES: usize = 4096;

/// `si_code` of a fault on unmapped memory.
pub const SEGV_MAPERR: i32 = 1;

/// `si_code` of a fault on memory mapped without the needed permission.
pub const SEGV_ACCERR: i32 = 2;

pub type Page = [u8; PAGE_BYTES];

/// What a mapping lets the CPU do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Protection {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl Protection {
    pub const READ: Self = Self {
        read: true,
        write: false,
        execute: false,
    };

    pub const READ_WRITE: Self = Self {
        read: true,
        write: true,
        execute: false,
    };

    pub const READ_EXECUTE: Self = Self {
        read: true,
        write: false,
        execute: true,
    };
}

/// What backs a mapping, as `/proc/<pid>/maps` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backing {
    File {
        path: Arc<str>,
        inode: u64,
        offset: u64,
    },
    Anonymous,
    Stack,
}

#[derive(Debug, Clone)]
struct Region {
    end: u64,
    protection: Protection,
    backing: Backing,
}

/// Where the user half of the address space ends with 4-level paging.
/// What an access at or past it does depends on the machine: a
/// non-canonical address raises a general-protection fault, one in the
/// kernel's half a page fault or, with linear address space separation, a
/// general-protection fault, and with 5-level paging some are user
/// addresses.
pub const USER_HALF_END: u64 = 1 << 47;

/// Why the CPU could not access memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryFault {
    pub address: u64,

    /// [`SEGV_MAPERR`] or [`SEGV_ACCERR`].
    pub code: i32,
}

/// Why the CPU could not access memory, or why the model cannot say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    Fault(MemoryFault),

    /// What Linux does with an access at `address` depends on what the
    /// model does not know, as `what` says: a model gap.
    Unmodeled {
        address: u64,
        what: &'static str,
    },
}

impl From<MemoryFault> for MemoryError {
    fn from(fault: MemoryFault) -> Self {
        Self::Fault(fault)
    }
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fault(fault) => write!(
                formatter,
                "a fault with code {} at {:#x}",
                fault.code, fault.address
            ),
            Self::Unmodeled { address, what } => write!(formatter, "{what} at {address:#x}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Execute,
}

/// One process's memory.
#[derive(Clone, Default)]
pub struct AddressSpace {
    /// Mapped regions by start address. Regions never overlap.
    regions: BTreeMap<u64, Region>,

    /// Page contents by page number. A mapped page absent here reads as
    /// zeros.
    pages: BTreeMap<u64, Arc<Page>>,
}

impl AddressSpace {
    /// Maps `[start, end)`, both page-aligned, with nothing mapped there yet.
    ///
    /// # Panics
    ///
    /// If the range is not page-aligned or overlaps a mapping.
    pub fn map(&mut self, start: u64, end: u64, protection: Protection, backing: Backing) {
        assert!(
            start.is_multiple_of(PAGE_SIZE) && end.is_multiple_of(PAGE_SIZE) && start < end,
            "unaligned mapping {start:#x}-{end:#x}"
        );
        assert!(
            self.regions
                .range(..end)
                .next_back()
                .is_none_or(|(_, region)| region.end <= start),
            "mapping {start:#x}-{end:#x} overlaps another"
        );

        self.regions.insert(
            start,
            Region {
                end,
                protection,
                backing,
            },
        );
    }

    /// Installs a whole page's contents, sharing them with whoever else
    /// holds them.
    pub fn share_page(&mut self, address: u64, page: Arc<Page>) {
        assert!(
            address.is_multiple_of(PAGE_SIZE) && self.region(address).is_some(),
            "page {address:#x} is not mapped"
        );
        self.pages.insert(address / PAGE_SIZE, page);
    }

    fn region(&self, address: u64) -> Option<&Region> {
        self.regions
            .range(..=address)
            .next_back()
            .map(|(_, region)| region)
            .filter(|region| address < region.end)
    }

    /// Checks that the CPU may access every byte of `[address, address +
    /// length)`, reporting the first byte it may not.
    ///
    /// Three kinds of access are gaps, since what Linux does depends on
    /// what the model does not know: one reaching past [`USER_HALF_END`];
    /// one where nothing is mapped and the stack is the next mapping
    /// above, which Linux grows the stack down over, as far as its
    /// resource limit and guard gap allow; and a read of memory mapped
    /// writable or executable but not readable, which x86's page tables
    /// cannot refuse, though protection keys refuse reads of
    /// execute-only memory where the CPU has them.
    fn check(&self, address: u64, length: u64, access: Access) -> Result<(), MemoryError> {
        let end = address
            .checked_add(length)
            .filter(|&end| end <= USER_HALF_END)
            .ok_or(MemoryError::Unmodeled {
                address,
                what: "an access past the user half of the address space",
            })?;

        let mut current = address;
        while current < end {
            let Some(region) = self.region(current) else {
                let below_stack = self
                    .regions
                    .range(current..)
                    .next()
                    .is_some_and(|(_, region)| region.backing == Backing::Stack);
                if below_stack {
                    return Err(MemoryError::Unmodeled {
                        address: current,
                        what: "an access the stack would grow over",
                    });
                }

                return Err(MemoryFault {
                    address: current,
                    code: SEGV_MAPERR,
                }
                .into());
            };

            let allowed = match access {
                Access::Read => region.protection.read,
                Access::Write => region.protection.write,
                Access::Execute => region.protection.execute,
            };

            if !allowed {
                if access == Access::Read && (region.protection.write || region.protection.execute)
                {
                    return Err(MemoryError::Unmodeled {
                        address: current,
                        what: "a read of memory mapped writable or executable but not readable",
                    });
                }

                return Err(MemoryFault {
                    address: current,
                    code: SEGV_ACCERR,
                }
                .into());
            }

            current = region.end;
        }

        Ok(())
    }

    /// Whether every byte of the range is mapped, whatever its protection.
    fn mapped(&self, address: u64, length: u64) -> bool {
        let mut current = address;
        let Some(end) = address.checked_add(length) else {
            return false;
        };

        while current < end {
            match self.region(current) {
                Some(region) => current = region.end,
                None => return false,
            }
        }

        true
    }

    /// Copies bytes out without checking anything.
    fn copy_out(&self, address: u64, bytes: &mut [u8]) {
        let mut current = address;
        let mut done = 0;

        while done < bytes.len() {
            let page = current / PAGE_SIZE;
            let offset = usize::try_from(current % PAGE_SIZE).expect("page offset fits usize");
            let count = (bytes.len() - done).min(PAGE_BYTES - offset);

            match self.pages.get(&page) {
                Some(contents) => {
                    bytes[done..done + count].copy_from_slice(&contents[offset..offset + count]);
                }
                None => bytes[done..done + count].fill(0),
            }

            done += count;
            current += count as u64;
        }
    }

    /// Copies bytes in without checking anything, copying shared pages.
    fn copy_in(&mut self, address: u64, bytes: &[u8]) {
        let mut current = address;
        let mut done = 0;

        while done < bytes.len() {
            let page = current / PAGE_SIZE;
            let offset = usize::try_from(current % PAGE_SIZE).expect("page offset fits usize");
            let count = (bytes.len() - done).min(PAGE_BYTES - offset);

            let contents = self
                .pages
                .entry(page)
                .or_insert_with(|| Arc::new([0; PAGE_BYTES]));
            Arc::make_mut(contents)[offset..offset + count]
                .copy_from_slice(&bytes[done..done + count]);
            done += count;
            current += count as u64;
        }
    }

    /// A read by the CPU, which needs read permission.
    pub fn read(&self, address: u64, bytes: &mut [u8]) -> Result<(), MemoryError> {
        self.check(address, bytes.len() as u64, Access::Read)?;
        self.copy_out(address, bytes);
        Ok(())
    }

    /// A write by the CPU, which needs write permission.
    pub fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), MemoryError> {
        self.check(address, bytes.len() as u64, Access::Write)?;
        self.copy_in(address, bytes);
        Ok(())
    }

    /// Fetches up to `bytes.len()` instruction bytes from executable memory,
    /// returning how many could be fetched, or the error at `address` when
    /// none could.
    pub fn fetch(&self, address: u64, bytes: &mut [u8]) -> Result<usize, MemoryError> {
        let mut count = 0;

        while count < bytes.len() {
            let current = address + count as u64;
            if let Err(fault) = self.check(current, 1, Access::Execute) {
                if count == 0 {
                    return Err(fault);
                }

                break;
            }

            let page_end = (current / PAGE_SIZE + 1) * PAGE_SIZE;
            let take = usize::try_from(page_end - current)
                .expect("page offset fits usize")
                .min(bytes.len() - count);
            self.copy_out(current, &mut bytes[count..count + take]);
            count += take;
        }

        Ok(count)
    }

    /// A `PTRACE_PEEKDATA` word, which ignores protections.
    #[must_use]
    pub fn peek(&self, address: u64) -> Option<u64> {
        let bytes = self.peek_bytes(address, 8)?;
        Some(u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
    }

    /// A `PTRACE_POKEDATA` word, which ignores protections. Returns whether
    /// the word was mapped.
    pub fn poke(&mut self, address: u64, value: u64) -> bool {
        self.poke_bytes(address, &value.to_le_bytes())
    }

    /// Writes bytes anywhere mapped, ignoring protections, as a debugger
    /// or a test does. Returns whether every byte was mapped.
    pub fn poke_bytes(&mut self, address: u64, bytes: &[u8]) -> bool {
        let mapped = self.mapped(address, bytes.len() as u64);
        if mapped {
            self.copy_in(address, bytes);
        }

        mapped
    }

    /// Reads bytes anywhere mapped, ignoring protections.
    #[must_use]
    pub fn peek_bytes(&self, address: u64, length: usize) -> Option<Vec<u8>> {
        let mut bytes = vec![0; length];
        self.mapped(address, length as u64).then(|| {
            self.copy_out(address, &mut bytes);
            bytes
        })
    }

    /// The page starting at `address`, a page boundary, as a debugger
    /// reads it, if it is mapped: a page never written reads as zeros.
    #[must_use]
    pub fn peek_page(&self, address: u64) -> Option<&Page> {
        static ZEROS: Page = [0; PAGE_BYTES];
        debug_assert_eq!(address % PAGE_SIZE, 0, "{address:#x} starts no page");
        self.mapped(address, PAGE_SIZE)
            .then(|| self.page(address).map_or(&ZEROS, |page| &**page))
    }

    /// The contents of the page at `address`, if it was ever written or
    /// shared in.
    #[must_use]
    pub fn page(&self, address: u64) -> Option<&Arc<Page>> {
        self.pages.get(&(address / PAGE_SIZE))
    }

    /// How many bytes from `address`, at most `length`, the kernel copies
    /// with page faults disabled, as perf does: readable bytes in pages
    /// that are present, which the model takes to be those the program
    /// wrote or the loader filled (K-PERF-3).
    #[must_use]
    pub fn present(&self, address: u64, length: u64) -> u64 {
        let mut current = address;
        let end = address.saturating_add(length);

        while current < end {
            let readable = self
                .region(current)
                .is_some_and(|region| region.protection.read);
            if !readable || self.page(current).is_none() {
                break;
            }

            current = (current / PAGE_SIZE + 1) * PAGE_SIZE;
        }

        current.min(end) - address
    }

    /// Whether the CPU may execute `address`.
    #[must_use]
    pub fn executable(&self, address: u64) -> bool {
        self.region(address)
            .is_some_and(|region| region.protection.execute)
    }

    /// The executable regions, as `(start, end)` pairs.
    pub fn executable_ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.regions
            .iter()
            .filter(|(_, region)| region.protection.execute)
            .map(|(&start, region)| (start, region.end))
    }

    /// Reads bytes as the kernel would for a system call: all of them, or
    /// why some are not readable.
    pub fn read_user(&self, address: u64, length: u64) -> Result<Vec<u8>, MemoryError> {
        self.check(address, length, Access::Read)?;
        let length = usize::try_from(length).map_err(|_| MemoryError::Unmodeled {
            address,
            what: "a read longer than the host can hold",
        })?;
        let mut bytes = vec![0; length];
        self.copy_out(address, &mut bytes);
        Ok(bytes)
    }

    /// The address space as `/proc/<pid>/maps` describes it.
    #[must_use]
    pub fn maps(&self) -> String {
        let mut text = String::new();
        for (&start, region) in &self.regions {
            let permissions = format!(
                "{}{}{}p",
                if region.protection.read { 'r' } else { '-' },
                if region.protection.write { 'w' } else { '-' },
                if region.protection.execute { 'x' } else { '-' },
            );
            let (offset, inode, name) = match &region.backing {
                Backing::File {
                    path,
                    inode,
                    offset,
                } => (*offset, *inode, path.as_ref()),
                Backing::Anonymous => (0, 0, ""),
                Backing::Stack => (0, 0, "[stack]"),
            };

            let device = if inode == 0 { "00:00" } else { "00:2a" };
            let line_start = text.len();
            let _ = write!(
                text,
                "{start:08x}-{:08x} {permissions} {offset:08x} {device} {inode} ",
                region.end
            );

            if !name.is_empty() {
                // The kernel starts names at column 73.
                let width = text.len() - line_start;
                text.extend(std::iter::repeat_n(' ', 73_usize.saturating_sub(width)));
                text.push_str(name);
            }

            text.push('\n');
        }

        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CPU obeys protections while ptrace ignores them, both fail where
    /// nothing is mapped, and a write copies a shared page instead of
    /// changing it for everyone who shares it.
    #[test]
    fn protections_bind_the_cpu_but_not_ptrace() {
        let mut space = AddressSpace::default();
        let code = 0x40_1000;
        space.map(
            code,
            code + PAGE_SIZE,
            Protection::READ_EXECUTE,
            Backing::Anonymous,
        );

        let shared = Arc::new([0x90; PAGE_BYTES]);
        space.share_page(code, Arc::clone(&shared));

        let mut byte = [0];
        space.read(code + 5, &mut byte).expect("readable code");
        assert_eq!(byte, [0x90]);
        assert_eq!(
            space.write(code + 5, &[0xcc]),
            Err(MemoryError::Fault(MemoryFault {
                address: code + 5,
                code: SEGV_ACCERR
            }))
        );
        assert!(space.poke(code, 0xcc));
        assert_eq!(space.peek(code), Some(0xcc));
        assert_eq!(shared[0], 0x90, "the shared page was copied, not changed");

        let unmapped = code + PAGE_SIZE;
        assert_eq!(
            space.peek(unmapped - 4),
            None,
            "a word that crosses into nothing"
        );
        assert_eq!(
            space.read(unmapped, &mut byte),
            Err(MemoryError::Fault(MemoryFault {
                address: unmapped,
                code: SEGV_MAPERR
            }))
        );
        let mut fetched = [0; 15];
        assert_eq!(space.fetch(unmapped - 3, &mut fetched), Ok(3));
        assert_eq!(
            space.fetch(unmapped, &mut fetched),
            Err(MemoryError::Fault(MemoryFault {
                address: unmapped,
                code: SEGV_MAPERR
            }))
        );
    }
}
