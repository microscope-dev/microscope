//! Copy-on-write paged virtual `AddressSpace` for simulated processes.
//!
//! CPU reads, writes, and fetches enforce mapping permissions (`Protection`),
//! whereas `peek`/`poke` model `ptrace` access that ignores read-only flags
//! (for planting `0xcc` traps) while preserving copy-on-write isolation across
//! forked processes and shared ELF code pages.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

pub const PAGE_SIZE: u64 = PAGE_BYTES as u64;

pub const PAGE_BYTES: usize = 4096;

pub const SEGV_MAPERR: i32 = 1;

pub const SEGV_ACCERR: i32 = 2;

pub type Page = [u8; PAGE_BYTES];

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

pub const USER_HALF_END: u64 = 1 << 47;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryFault {
    pub address: u64,

    pub code: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    Fault(MemoryFault),

    /// An access whose real Linux behavior depends on state the simulator
    /// does not model (a model gap), described by `what`.
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

#[derive(Clone, Default)]
pub struct AddressSpace {
    regions: BTreeMap<u64, Region>,

    pages: BTreeMap<u64, Arc<Page>>,
}

impl AddressSpace {
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

    pub fn read(&self, address: u64, bytes: &mut [u8]) -> Result<(), MemoryError> {
        self.check(address, bytes.len() as u64, Access::Read)?;
        self.copy_out(address, bytes);
        Ok(())
    }

    pub fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), MemoryError> {
        self.check(address, bytes.len() as u64, Access::Write)?;
        self.copy_in(address, bytes);
        Ok(())
    }

    /// Stops at the first non-executable or unmapped page boundary once at
    /// least one byte has been fetched, because `x86_64` only faults if the
    /// decoded instruction actually crosses into that page.
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

    #[must_use]
    pub fn peek(&self, address: u64) -> Option<u64> {
        let bytes = self.peek_bytes(address, 8)?;
        Some(u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
    }

    pub fn poke(&mut self, address: u64, value: u64) -> bool {
        self.poke_bytes(address, &value.to_le_bytes())
    }

    pub fn poke_bytes(&mut self, address: u64, bytes: &[u8]) -> bool {
        let mapped = self.mapped(address, bytes.len() as u64);
        if mapped {
            self.copy_in(address, bytes);
        }

        mapped
    }

    #[must_use]
    pub fn peek_bytes(&self, address: u64, length: usize) -> Option<Vec<u8>> {
        let mut bytes = vec![0; length];
        self.mapped(address, length as u64).then(|| {
            self.copy_out(address, &mut bytes);
            bytes
        })
    }

    #[must_use]
    pub fn peek_page(&self, address: u64) -> Option<&Page> {
        static ZEROS: Page = [0; PAGE_BYTES];
        debug_assert_eq!(address % PAGE_SIZE, 0, "{address:#x} starts no page");
        self.mapped(address, PAGE_SIZE)
            .then(|| self.page(address).map_or(&ZEROS, |page| &**page))
    }

    #[must_use]
    pub fn page(&self, address: u64) -> Option<&Arc<Page>> {
        self.pages.get(&(address / PAGE_SIZE))
    }

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

    #[must_use]
    pub fn executable(&self, address: u64) -> bool {
        self.region(address)
            .is_some_and(|region| region.protection.execute)
    }

    pub fn executable_ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.regions
            .iter()
            .filter(|(_, region)| region.protection.execute)
            .map(|(&start, region)| (start, region.end))
    }

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
