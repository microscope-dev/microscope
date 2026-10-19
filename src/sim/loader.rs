//! Loads golden executables into simulated address spaces as Linux's ELF
//! loader does for a static executable, position-dependent or not, with
//! address randomization off.
//!
//! An [`Image`] is prepared once per binary and shared by every session
//! that runs it; [`Image::load`] gives a session its own address space,
//! sharing the image's pages until the session writes them, and builds the
//! initial stack from the session's arguments.

use std::sync::Arc;

use object::{Object as _, ObjectSegment as _, SegmentFlags};

use super::cpu::{RSP, Registers};
use super::memory::{AddressSpace, Backing, PAGE_BYTES, PAGE_SIZE, Page, Protection};

/// The top of the stack of a process run without address randomization.
pub const STACK_TOP: u64 = 0x7fff_ffff_f000;

/// The size of the stack mapping a new process starts with.
const STACK_SIZE: u64 = 132 * 1024;

/// Where the mmap area begins, growing down, with randomization off and a
/// stack limit under 127 MiB: the top of the address space less the
/// smallest gap Linux leaves for the stack.
const MMAP_BASE: u64 = STACK_TOP - 128 * 1024 * 1024;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;

/// Where the interpreter loaded: zero, without one.
const AT_BASE: u64 = 7;

/// Zero, unless a `binfmt_misc` handler preserved argv[0].
const AT_FLAGS: u64 = 8;
const AT_ENTRY: u64 = 9;

/// `uname`'s machine, `x86_64`.
const AT_PLATFORM: u64 = 15;
const AT_CLKTCK: u64 = 17;

/// Zero, for a program that gains no privileges as it starts.
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

/// x86's `USER_HZ`.
const CLOCK_TICKS_PER_SECOND: u64 = 100;

/// The platform `AT_PLATFORM` names.
const PLATFORM: &[u8] = b"x86_64\0";

/// The `rflags` a new process starts with: interrupts enabled and the
/// reserved bit that always reads as one.
pub const INITIAL_RFLAGS: u64 = 0x202;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("not a loadable ELF executable: {0}")]
    Object(#[from] object::Error),

    #[error("{0}")]
    Unsupported(String),
}

struct Segment {
    start: u64,
    end: u64,

    /// Where the pages that hold some of the file's bytes end. The kernel
    /// maps the rest of the segment anonymously.
    file_end: u64,
    protection: Protection,
    offset: u64,

    /// Page contents from `start`, one per page.
    pages: Vec<Arc<Page>>,
}

/// A static executable prepared for loading.
pub struct Image {
    path: Arc<str>,
    inode: u64,

    /// What the image's addresses must be increased by to name where it
    /// loads: zero for a position-dependent executable.
    bias: u64,
    entry: u64,
    program_headers: u64,
    program_header_count: u64,
    segments: Vec<Segment>,
}

impl Image {
    /// Prepares an executable that the simulated kernel names `path` and
    /// whose file has `inode`.
    ///
    /// A static position-independent executable, having no interpreter,
    /// loads into the mmap area as the first thing mapped there (K-EXEC-2):
    /// its whole span ends at [`MMAP_BASE`].
    pub fn new(path: &str, inode: u64, data: &[u8]) -> Result<Self, LoadError> {
        let file = object::File::parse(data)?;
        let object::File::Elf64(elf) = &file else {
            return Err(LoadError::Unsupported("not a 64-bit ELF file".into()));
        };

        let header = elf.elf_header();
        let endian = elf.endian();
        let program_header_offset = header.e_phoff.get(endian);
        let program_header_count = u64::from(header.e_phnum.get(endian));
        let program_header_table = elf
            .elf_program_headers()
            .iter()
            .map(|header| {
                (
                    header.p_type.get(endian),
                    header.p_align.get(endian),
                    header.p_vaddr.get(endian),
                    header.p_memsz.get(endian),
                )
            })
            .collect::<Vec<_>>();
        if program_header_table
            .iter()
            .any(|&(kind, ..)| kind == object::elf::PT_INTERP)
        {
            return Err(LoadError::Unsupported(
                "executables with an interpreter do not load".into(),
            ));
        }

        let loads = program_header_table
            .iter()
            .filter(|&&(kind, ..)| kind == object::elf::PT_LOAD);
        let bias = match file.kind() {
            object::ObjectKind::Executable => 0,
            object::ObjectKind::Dynamic => {
                if loads.clone().any(|&(_, align, ..)| align > PAGE_SIZE) {
                    return Err(LoadError::Unsupported(
                        "segments aligned beyond a page do not load".into(),
                    ));
                }

                let start = loads.clone().map(|&(_, _, address, _)| address).min();
                let end = loads.map(|&(_, _, address, size)| address + size).max();
                let (Some(start), Some(end)) = (start, end) else {
                    return Err(LoadError::Unsupported("nothing to load".into()));
                };

                let span = end.next_multiple_of(PAGE_SIZE) - (start & !(PAGE_SIZE - 1));
                MMAP_BASE - span - (start & !(PAGE_SIZE - 1))
            }
            _ => {
                return Err(LoadError::Unsupported(
                    "only static executables load".into(),
                ));
            }
        };

        let mut segments = Vec::new();
        let mut program_headers = None;
        for segment in file.segments() {
            let SegmentFlags::Elf { p_flags } = segment.flags() else {
                return Err(LoadError::Unsupported("segment without ELF flags".into()));
            };

            let (file_offset, file_size) = segment.file_range();
            let address = segment.address() + bias;
            if address % PAGE_SIZE != file_offset % PAGE_SIZE {
                return Err(LoadError::Unsupported("misaligned segment".into()));
            }

            if (file_offset..file_offset + file_size).contains(&program_header_offset) {
                program_headers = Some(address + program_header_offset - file_offset);
            }

            let prepared = prepare_segment(
                data,
                address,
                segment.size(),
                file_offset,
                file_size,
                p_flags,
            )?;

            // Linux maps a later segment over an earlier one's page.
            if segments
                .iter()
                .any(|other: &Segment| other.start < prepared.end && prepared.start < other.end)
            {
                return Err(LoadError::Unsupported(
                    "segments that share a page do not load".into(),
                ));
            }

            segments.push(prepared);
        }

        Ok(Self {
            path: Arc::from(path),
            inode,
            bias,
            entry: file.entry() + bias,
            program_headers: program_headers
                .ok_or_else(|| LoadError::Unsupported("program headers are not loaded".into()))?,
            program_header_count,
            segments,
        })
    }

    /// Where the program starts, as loaded.
    #[must_use]
    pub const fn entry(&self) -> u64 {
        self.entry
    }

    /// What the image's own addresses, which its debug information and
    /// symbols use, must be increased by to name where it loads.
    #[must_use]
    pub const fn bias(&self) -> u64 {
        self.bias
    }

    /// The byte of code the image has at `address`, as loaded.
    #[must_use]
    pub fn original_byte(&self, address: u64) -> Option<u8> {
        self.code().find_map(|(start, page)| {
            let offset = usize::try_from(address.checked_sub(start)?).ok()?;
            page.get(offset).copied()
        })
    }

    /// Every executable page the image maps, with its address, as loaded.
    pub fn code(&self) -> impl Iterator<Item = (u64, &Arc<Page>)> + '_ {
        self.segments
            .iter()
            .filter(|segment| segment.protection.execute)
            .flat_map(|segment| {
                segment
                    .pages
                    .iter()
                    .enumerate()
                    .map(move |(index, page)| (segment.start + index as u64 * PAGE_SIZE, page))
            })
    }

    /// A new address space running the image with `arguments` after the
    /// program's name, and the registers it starts with. `random` fills
    /// the sixteen bytes `AT_RANDOM` names.
    #[must_use]
    pub fn load(&self, arguments: &[String], random: [u8; 16]) -> (AddressSpace, Registers) {
        let mut space = AddressSpace::default();
        for segment in &self.segments {
            if segment.file_end > segment.start {
                let backing = Backing::File {
                    path: Arc::clone(&self.path),
                    inode: self.inode,
                    offset: segment.offset,
                };

                space.map(segment.start, segment.file_end, segment.protection, backing);
            }

            if segment.end > segment.file_end {
                space.map(
                    segment.file_end,
                    segment.end,
                    segment.protection,
                    Backing::Anonymous,
                );
            }

            for (index, page) in segment.pages.iter().enumerate() {
                space.share_page(segment.start + index as u64 * PAGE_SIZE, Arc::clone(page));
            }
        }

        space.map(
            STACK_TOP - STACK_SIZE,
            STACK_TOP,
            Protection::READ_WRITE,
            Backing::Stack,
        );
        let stack_pointer = self.build_stack(&mut space, arguments, random);
        let mut registers = Registers {
            rip: self.entry,
            rflags: INITIAL_RFLAGS,
            ..Registers::default()
        };

        registers.general[RSP] = stack_pointer;

        (space, registers)
    }

    /// Lays out the initial stack as Linux does (K-EXEC-3), top down: a
    /// zero word, the executable's name, the argument strings, the last
    /// highest, then from the next 16-byte boundary down the platform's
    /// name and the random bytes, and below them argc, argv, an empty
    /// environment, and the auxiliary vector, with argc 16-byte aligned.
    /// The vector has Linux's entries, in Linux's order, but for those
    /// whose values depend on the machine, the user, or the kernel's
    /// version, none of which the model has and no golden program reads.
    /// Returns the stack pointer, which points at argc.
    fn build_stack(&self, space: &mut AddressSpace, arguments: &[String], random: [u8; 16]) -> u64 {
        let push = |space: &mut AddressSpace, top: &mut u64, bytes: &[u8]| {
            *top -= bytes.len() as u64;
            assert!(space.write(*top, bytes).is_ok(), "the stack is writable");
            *top
        };

        let terminated = |text: &str| {
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0);
            bytes
        };

        let mut top = STACK_TOP - 8;
        let name = terminated(&self.path);
        let executable_name = push(space, &mut top, &name);
        let mut argv = arguments
            .iter()
            .rev()
            .map(|argument| push(space, &mut top, &terminated(argument)))
            .collect::<Vec<_>>();
        argv.push(push(space, &mut top, &name));
        argv.reverse();

        top &= !15;
        let platform = push(space, &mut top, PLATFORM);
        let random_address = push(space, &mut top, &random);

        let mut words = vec![argv.len() as u64];
        words.extend(&argv);
        words.extend([0, 0]);
        words.extend([
            AT_PAGESZ,
            PAGE_SIZE,
            AT_CLKTCK,
            CLOCK_TICKS_PER_SECOND,
            AT_PHDR,
            self.program_headers,
            AT_PHENT,
            56,
            AT_PHNUM,
            self.program_header_count,
            AT_BASE,
            0,
            AT_FLAGS,
            0,
            AT_ENTRY,
            self.entry,
            AT_SECURE,
            0,
            AT_RANDOM,
            random_address,
            AT_EXECFN,
            executable_name,
            AT_PLATFORM,
            platform,
            AT_NULL,
            0,
        ]);

        let size = words.len() as u64 * 8;
        let stack_pointer = (top - size) & !15;
        let bytes = words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect::<Vec<_>>();

        assert!(
            space.write(stack_pointer, &bytes).is_ok(),
            "the stack is writable"
        );

        stack_pointer
    }
}

/// Reads one segment's pages as the kernel maps them: whole pages of the
/// file from the segment's page-aligned offset. Where a segment reaches
/// past its file contents, the kernel zeroes the rest of the last file
/// page, which succeeds only if the segment is writable.
fn prepare_segment(
    data: &[u8],
    address: u64,
    memory_size: u64,
    file_offset: u64,
    file_size: u64,
    flags: u32,
) -> Result<Segment, LoadError> {
    let protection = Protection {
        read: flags & object::elf::PF_R != 0,
        write: flags & object::elf::PF_W != 0,
        execute: flags & object::elf::PF_X != 0,
    };

    let start = address & !(PAGE_SIZE - 1);
    let end = (address + memory_size).next_multiple_of(PAGE_SIZE);
    let offset = file_offset & !(PAGE_SIZE - 1);
    let file_end = address + file_size;
    let mut pages = Vec::new();

    for page_address in (start..end).step_by(PAGE_BYTES) {
        let mut page = [0; PAGE_BYTES];
        if file_size > 0 && page_address < file_end {
            let from = usize::try_from(offset + (page_address - start))
                .map_err(|_| LoadError::Unsupported("segment offset overflows".into()))?;
            let available = data.len().saturating_sub(from).min(page.len());
            page[..available].copy_from_slice(&data[from..from + available]);
            if protection.write && memory_size > file_size && page_address + PAGE_SIZE > file_end {
                let keep = usize::try_from(file_end - page_address).expect("within one page");
                page[keep..].fill(0);
            }
        }

        pages.push(Arc::new(page));
    }

    Ok(Segment {
        start,
        end,
        file_end: if file_size == 0 {
            start
        } else {
            file_end.next_multiple_of(PAGE_SIZE).min(end)
        },
        protection,
        offset,
        pages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A static executable of `size` bytes, each 0xaa past its headers,
    /// with one `PT_LOAD` header per `(offset, address, file size, memory
    /// size, flags)`.
    fn executable(size: usize, loads: &[(u64, u64, u64, u64, u32)]) -> Vec<u8> {
        let mut data = vec![0xaa; size];
        let mut header = vec![0; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = 2; // 64-bit
        header[5] = 1; // little-endian
        header[6] = 1; // version
        header[16..18].copy_from_slice(&2_u16.to_le_bytes()); // ET_EXEC
        header[18..20].copy_from_slice(&62_u16.to_le_bytes()); // x86-64
        header[20..24].copy_from_slice(&1_u32.to_le_bytes());
        header[24..32].copy_from_slice(&loads[0].1.to_le_bytes()); // entry
        header[32..40].copy_from_slice(&64_u64.to_le_bytes()); // phoff
        header[52..54].copy_from_slice(&64_u16.to_le_bytes());
        header[54..56].copy_from_slice(&56_u16.to_le_bytes());
        let count = u16::try_from(loads.len()).expect("a few headers");
        header[56..58].copy_from_slice(&count.to_le_bytes());
        header[58..60].copy_from_slice(&64_u16.to_le_bytes());

        for &(offset, address, file_size, memory_size, flags) in loads {
            header.extend(object::elf::PT_LOAD.to_le_bytes());
            header.extend(flags.to_le_bytes());
            for word in [offset, address, address, file_size, memory_size, PAGE_SIZE] {
                header.extend(word.to_le_bytes());
            }
        }

        data[..header.len()].copy_from_slice(&header);
        data
    }

    const R: u32 = object::elf::PF_R;
    const RW: u32 = object::elf::PF_R | object::elf::PF_W;
    const RX: u32 = object::elf::PF_R | object::elf::PF_X;

    /// Linux maps segments that share a page over each other, which the
    /// model does not: such an executable is refused, not loaded wrong.
    #[test]
    fn segments_sharing_a_page_do_not_load() {
        let data = executable(
            0x2000,
            &[
                (0, 0x40_0000, 0x200, 0x200, RX),
                (0x200, 0x40_0200, 0x10, 0x20, RW),
            ],
        );

        let result = Image::new("/shared", 1, &data);
        assert!(
            matches!(&result, Err(LoadError::Unsupported(why)) if why.contains("share a page")),
            "{:?}",
            result.err()
        );
    }

    /// The kernel zeroes the rest of a writable segment's last file page
    /// only when the segment has bytes beyond the file's; otherwise the
    /// page holds the file's next bytes, as a read-only segment's does. A
    /// segment without file bytes maps none of the file, wherever its
    /// offset points.
    #[test]
    fn only_a_segment_reaching_past_its_file_bytes_has_its_page_zeroed() {
        let data = executable(
            0x4000,
            &[
                (0, 0x40_0000, 0x200, 0x200, RX),
                (0x1000, 0x40_1000, 0x10, 0x10, RW),
                (0x2000, 0x40_2000, 0x10, 0x20, RW),
                (0x3000, 0x40_3000, 0x10, 0x10, R),
                (0x3010, 0x40_4010, 0, 0x20, RW),
            ],
        );

        let image = Image::new("/tails", 1, &data).expect("an executable");
        let (space, _) = image.load(&[], [0; 16]);
        let byte = |address| space.peek_bytes(address, 1).expect("mapped")[0];

        assert_eq!(byte(0x40_100f), 0xaa);
        assert_eq!(
            byte(0x40_1010),
            0xaa,
            "a writable segment of file bytes alone"
        );
        assert_eq!(byte(0x40_2010), 0, "a writable segment with more");
        assert_eq!(byte(0x40_2fff), 0);
        assert_eq!(byte(0x40_3010), 0xaa, "a read-only segment");
        assert_eq!(byte(0x40_4000), 0, "a segment without file bytes");
        assert_eq!(byte(0x40_4010), 0);
    }
}
