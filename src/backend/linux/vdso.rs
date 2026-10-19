//! The vDSO, the shared object the kernel maps into every process.
//!
//! No file backs it. The kernel copies its whole image, section headers
//! included, into the mapping `/proc/<pid>/maps` names `[vdso]`, whose start
//! the auxiliary vector records as `AT_SYSINFO_EHDR`. Its module is read from
//! the process's memory, or a core dump's, as if that memory were its file.
//!
//! A process may move its vDSO with mremap(2), as a checkpoint restore does,
//! and the breakpoint traps the debugger wrote into it move too. The kernel
//! reports no change to a tracee's mappings short of stopping at every
//! system call, so moves are found by comparing the memory map with the
//! modules at each stop, and when a thread executes a trap at an address
//! the debugger never wrote one (`reconcile_sites`, `rewind_carried_trap`).
//! That works for the vDSO because its name follows it, and for a library
//! because the map names its file. Code that nothing names, such as a
//! JIT's anonymous memory, cannot be followed this way: a trap moved or
//! copied with it is indistinguishable from the program's own `int3`, and
//! is reported as a `SIGTRAP`.

use std::ops::Range;
use std::path::PathBuf;

use nix::unistd::Pid;
use object::LittleEndian;
use object::elf;
use object::read::elf::{FileHeader as _, ProgramHeader as _};

use crate::{LoadBias, MemoryReadCompletion, VirtualAddress};

use super::Controller;
use super::memory::read_logical_memory;
use super::native::InspectionOps;

/// The name `/proc/<pid>/maps` gives the vDSO's mapping, which its module
/// takes as its path. No canonical file path is bracketed.
pub(super) const VDSO_NAME: &str = "[vdso]";

/// The auxiliary-vector entry holding the address of the vDSO's ELF header.
pub(super) const AT_SYSINFO_EHDR: u64 = 33;

/// The most bytes read for one image in memory. Linux's vDSO spans a few
/// pages.
const MAX_IMAGE_SIZE: u64 = 1024 * 1024;

const ELF_HEADER_SIZE: usize = size_of::<elf::FileHeader64<LittleEndian>>();
const PROGRAM_HEADER_SIZE: u64 = size_of::<elf::ProgramHeader64<LittleEndian>>() as u64;
const SECTION_HEADER_SIZE: u64 = size_of::<elf::SectionHeader64<LittleEndian>>() as u64;

/// A module seen mapped in a process: its path and load bias.
type ObservedModule = (PathBuf, LoadBias);

/// An ELF image read from memory as the file it was mapped from.
#[derive(Debug)]
pub(super) struct MemoryImage {
    pub(super) data: Vec<u8>,
    pub(super) load_bias: LoadBias,
}

/// Why memory does not hold a usable ELF image.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum MemoryImageError {
    #[error("memory at {0:#x} is unreadable")]
    Unreadable(u64),

    #[error("{0}")]
    Malformed(&'static str),
}

/// Reads the ELF image whose header is at `start` and which ends by `end`.
///
/// The image must lie in memory as in its file, as the kernel places the
/// vDSO: one loadable segment maps the file's first byte, and every other
/// lies the same distance from its file offset. The file then ends with its
/// segments' contents or its section headers, whichever is last. `read`
/// fills a buffer from memory and fails when any byte is unreadable.
pub(super) fn read_memory_image(
    start: u64,
    end: u64,
    mut read: impl FnMut(u64, &mut [u8]) -> bool,
) -> Result<MemoryImage, MemoryImageError> {
    use MemoryImageError::Malformed;

    let limit = end.saturating_sub(start).min(MAX_IMAGE_SIZE);
    let mut read_prefix = |length: u64| {
        if length > limit {
            return Err(Malformed("the image extends past its memory"));
        }

        let mut data =
            vec![0; usize::try_from(length).map_err(|_| Malformed("the image is too large"))?];
        if read(start, &mut data) {
            Ok(data)
        } else {
            Err(MemoryImageError::Unreadable(start))
        }
    };

    let header_bytes = read_prefix(ELF_HEADER_SIZE as u64)?;
    let header = elf::FileHeader64::<LittleEndian>::parse(header_bytes.as_slice())
        .map_err(|_| Malformed("no 64-bit ELF header"))?;
    let endian = header
        .endian()
        .map_err(|_| Malformed("the image is not little-endian"))?;
    if header.e_type.get(endian) != elf::ET_DYN {
        return Err(Malformed("the image is not a shared object"));
    }

    let program_headers_end = table_end(
        header.e_phoff.get(endian),
        header.e_phnum.get(endian).into(),
        header.e_phentsize.get(endian).into(),
        PROGRAM_HEADER_SIZE,
    )
    .ok_or(Malformed("the program headers are malformed"))?;
    let section_headers_end = table_end(
        header.e_shoff.get(endian),
        header.e_shnum.get(endian).into(),
        header.e_shentsize.get(endian).into(),
        SECTION_HEADER_SIZE,
    )
    .ok_or(Malformed("the section headers are malformed"))?;

    let prefix = read_prefix(program_headers_end.max(ELF_HEADER_SIZE as u64))?;
    let segments = header
        .program_headers(endian, prefix.as_slice())
        .map_err(|_| Malformed("the program headers are malformed"))?
        .iter()
        .filter(|segment| segment.p_type(endian) == elf::PT_LOAD)
        .map(|segment| {
            (
                segment.p_offset(endian),
                segment.p_vaddr(endian),
                segment.p_filesz(endian),
            )
        })
        .collect::<Vec<_>>();
    let link_base = segments
        .iter()
        .find_map(|&(offset, address, _)| (offset == 0).then_some(address))
        .ok_or(Malformed("no loadable segment maps the ELF header"))?;
    let mut size = program_headers_end.max(section_headers_end);

    for &(offset, address, file_size) in &segments {
        if address.checked_sub(offset) != Some(link_base) {
            return Err(Malformed("the image is not laid out as its file"));
        }

        let contents_end = offset
            .checked_add(file_size)
            .ok_or(Malformed("a loadable segment overflows"))?;
        size = size.max(contents_end);
    }

    let load_bias = start
        .checked_sub(link_base)
        .map(LoadBias::new)
        .ok_or(Malformed("the image is linked above its address"))?;
    Ok(MemoryImage {
        data: read_prefix(size)?,
        load_bias,
    })
}

/// The end of a header table of `count` entries at `offset`, or `None` for
/// entries of the wrong size. An absent table ends at zero; a table whose
/// count is held elsewhere, as extended numbering does, is refused.
fn table_end(offset: u64, count: u64, entry_size: u64, expected_size: u64) -> Option<u64> {
    match (offset, count) {
        (0, 0) => Some(0),
        (_, 0) => None,
        _ if entry_size != expected_size => None,
        _ => offset.checked_add(count.checked_mul(entry_size)?),
    }
}

/// The address range of the vDSO in a process's memory map, if one is
/// mapped. Adjacent `[vdso]` lines, as a protection change splits the
/// mapping into, are one range.
pub(super) fn vdso_mapping(maps: &str) -> Option<Range<u64>> {
    let mut mapping: Option<Range<u64>> = None;

    for line in maps.lines() {
        let mut fields = line.splitn(6, char::is_whitespace);
        let range = fields.next();
        if fields.nth(4).map(str::trim_start) != Some(VDSO_NAME) {
            continue;
        }

        let Some((start, end)) = range.and_then(|range| {
            let (start, end) = range.split_once('-')?;
            Some((
                u64::from_str_radix(start, 16).ok()?,
                u64::from_str_radix(end, 16).ok()?,
            ))
        }) else {
            continue;
        };

        match &mut mapping {
            None => mapping = Some(start..end),
            Some(mapping) if mapping.end == start => mapping.end = end,
            Some(_) => break,
        }
    }

    mapping
}

impl<P: InspectionOps> Controller<P> {
    /// The load bias of the registered vDSO module, if it is the image whose
    /// header is at `start`. A memory image's lowest segment holds its header.
    fn registered_vdso(&self, start: u64) -> Option<LoadBias> {
        self.modules.values().find_map(|module| {
            let placed = module.image.path().as_os_str() == VDSO_NAME
                && module
                    .loaded
                    .load_bias
                    .load(module.image.address_range().start)
                    == Some(VirtualAddress::new(start));
            placed.then_some(module.loaded.load_bias)
        })
    }

    /// The vDSO mapped at `mapping`, if any, as an observed module: its path
    /// and load bias and, unless the registered module is already the one
    /// there, its image. The vDSO is read again only once it moves, as after
    /// an exec.
    pub(super) fn observe_vdso(
        &self,
        pid: Pid,
        mapping: Option<&Range<u64>>,
    ) -> Option<(ObservedModule, Option<Vec<u8>>)> {
        let mapping = mapping?;
        let module = |load_bias| (PathBuf::from(VDSO_NAME), load_bias);

        if let Some(load_bias) = self.registered_vdso(mapping.start) {
            return Some((module(load_bias), None));
        }

        let read = self.read_vdso(pid, mapping);

        #[cfg(debug_assertions)]
        if let Err(error) = &read {
            record!("the vDSO at {:#x} is unusable: {error}", mapping.start);
        }

        read.ok()
            .map(|image| (module(image.load_bias), Some(image.data)))
    }

    /// Reads the vDSO image at `mapping` as the process sees it, without the
    /// debugger's breakpoint traps.
    fn read_vdso(&self, pid: Pid, mapping: &Range<u64>) -> Result<MemoryImage, MemoryImageError> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Err(MemoryImageError::Unreadable(mapping.start));
        };

        read_memory_image(
            mapping.start,
            mapping.end,
            |address, buffer| match read_logical_memory(
                &self.ptrace,
                pid,
                &inferior.breakpoints,
                VirtualAddress::new(address),
                buffer.len(),
            ) {
                Ok(read) if read.completion == MemoryReadCompletion::Complete => {
                    buffer.copy_from_slice(&read.bytes);
                    true
                }
                _ => false,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::linux::core_dump::elf_build_id;
    use object::{Object as _, ObjectSegment as _};

    #[test]
    fn the_vdso_is_found_by_its_name_alone() {
        let maps = concat!(
            "7ffff7fbd000-7ffff7fc1000 r--p 00000000 00:00 0                          [vvar]\n",
            "7ffff7fc1000-7ffff7fc3000 r--p 00000000 00:00 0                          [vvar_vclock]\n",
            "7ffff7fc3000-7ffff7fc4000 r-xp 00000000 00:00 0                          [vdso]\n",
            "7ffff7fc4000-7ffff7fc5000 r--p 00000000 00:00 0                          [vdso]\n",
            "7ffff7fc5000-7ffff7fc6000 r--p 00000000 00:01 7 /opt/[vdso]\n",
            "7ffff7fd0000-7ffff7fd1000 r-xp 00000000 00:00 0                          [vdso]\n",
        );
        assert_eq!(vdso_mapping(maps), Some(0x7fff_f7fc_3000..0x7fff_f7fc_5000));
        assert_eq!(
            vdso_mapping("1000-2000 r-xp 00000000 00:00 0 [stack]\n"),
            None
        );
    }

    /// This process's own vDSO, read through `/proc` as a debugger reads a
    /// tracee's.
    fn own_vdso() -> (Range<u64>, Vec<u8>) {
        use std::os::unix::fs::FileExt as _;

        let maps = std::fs::read_to_string("/proc/self/maps").expect("read maps");
        let mapping = vdso_mapping(&maps).expect("this process maps a vDSO");
        let mut memory = vec![0; usize::try_from(mapping.end - mapping.start).unwrap()];
        std::fs::File::open("/proc/self/mem")
            .expect("open memory")
            .read_exact_at(&mut memory, mapping.start)
            .expect("read the vDSO");
        (mapping, memory)
    }

    /// Reads `memory` as if placed at `base`, where only `readable` bytes
    /// can be read.
    fn reader(memory: &[u8], base: u64, readable: usize) -> impl FnMut(u64, &mut [u8]) -> bool {
        move |address, buffer| {
            let start = usize::try_from(address - base).unwrap();
            memory[..readable]
                .get(start..start + buffer.len())
                .map(|bytes| buffer.copy_from_slice(bytes))
                .is_some()
        }
    }

    fn put(bytes: &mut [u8], offset: usize, value: &[u8]) {
        bytes[offset..offset + value.len()].copy_from_slice(value);
    }

    // Offsets in the ELF64 file and program headers.
    const E_TYPE: usize = 16;
    const E_PHOFF: usize = 32;
    const E_SHOFF: usize = 40;
    const E_PHENTSIZE: usize = 54;
    const E_PHNUM: usize = 56;
    const E_SHNUM: usize = 60;
    const P_OFFSET: usize = 8;
    const P_VADDR: usize = 16;

    #[test]
    fn a_vdso_reads_as_the_file_the_kernel_copied_into_memory() {
        let (mapping, memory) = own_vdso();
        let image = read_memory_image(
            mapping.start,
            mapping.end,
            reader(&memory, mapping.start, memory.len()),
        )
        .expect("the vDSO is an image");

        // The kernel links its vDSO at zero.
        assert_eq!(image.load_bias.get(), mapping.start);
        assert_eq!(image.data, memory[..image.data.len()]);

        // The section headers, past the loadable contents, end the file.
        let header = elf::FileHeader64::<LittleEndian>::parse(image.data.as_slice()).unwrap();
        let sections = header.e_shoff.get(LittleEndian)
            + u64::from(header.e_shnum.get(LittleEndian)) * SECTION_HEADER_SIZE;
        assert_eq!(image.data.len() as u64, sections);
        let object = object::File::parse(image.data.as_slice()).expect("a complete file");
        let loadable = object
            .segments()
            .map(|segment| segment.file_range().0 + segment.file_range().1)
            .max()
            .unwrap();
        assert!(loadable < sections, "{loadable:#x} {sections:#x}");
        assert!(object.section_by_name(".eh_frame").is_some());

        // Its one note segment, aligned to 8 bytes, also holds notes aligned
        // to 4, so the segment finds the build-id the note sections name.
        let build_id = object.build_id().unwrap();
        assert!(build_id.is_some());
        assert_eq!(elf_build_id(image.data.as_slice()), build_id);
    }

    /// Edits a copy of an image, given the offsets of its loadable segment's
    /// program header and another's, and its address.
    type Edit = fn(&mut Vec<u8>, usize, usize, u64);

    #[test]
    fn memory_that_is_no_whole_image_is_refused_for_its_reason() {
        use MemoryImageError::{Malformed, Unreadable};

        let (mapping, memory) = own_vdso();
        let program_headers = usize::try_from(u64::from_le_bytes(
            memory[E_PHOFF..E_PHOFF + 8].try_into().unwrap(),
        ))
        .unwrap();
        let load = (0..usize::from(u16::from_le_bytes(
            memory[E_PHNUM..E_PHNUM + 2].try_into().unwrap(),
        )))
            .map(|index| program_headers + index * size_of::<elf::ProgramHeader64<LittleEndian>>())
            .find(|&header| memory[header..header + 4] == elf::PT_LOAD.to_le_bytes())
            .expect("a loadable segment");
        let other = (0..2)
            .map(|index| program_headers + index * size_of::<elf::ProgramHeader64<LittleEndian>>())
            .find(|&header| header != load)
            .unwrap();

        let cases: [(&str, Edit, MemoryImageError); 8] = [
            (
                "not ELF",
                |bytes, _, _, _| bytes[0] = 0,
                Malformed("no 64-bit ELF header"),
            ),
            (
                "an executable",
                |bytes, _, _, _| put(bytes, E_TYPE, &elf::ET_EXEC.to_le_bytes()),
                Malformed("the image is not a shared object"),
            ),
            (
                "short program headers",
                |bytes, _, _, _| put(bytes, E_PHENTSIZE, &32_u16.to_le_bytes()),
                Malformed("the program headers are malformed"),
            ),
            (
                "extended section numbering",
                |bytes, _, _, _| put(bytes, E_SHNUM, &0_u16.to_le_bytes()),
                Malformed("the section headers are malformed"),
            ),
            (
                "an unmapped header",
                |bytes, load, _, _| put(bytes, load + P_OFFSET, &0x1000_u64.to_le_bytes()),
                Malformed("no loadable segment maps the ELF header"),
            ),
            (
                "segments placed apart from their contents",
                |bytes, load, other, _| {
                    let size = size_of::<elf::ProgramHeader64<LittleEndian>>();
                    let copy = bytes[load..load + size].to_vec();
                    put(bytes, other, &copy);
                    put(bytes, other + P_OFFSET, &0x100_u64.to_le_bytes());
                },
                Malformed("the image is not laid out as its file"),
            ),
            (
                "linked above its address",
                |bytes, load, _, start| put(bytes, load + P_VADDR, &(start + 0x1000).to_le_bytes()),
                Malformed("the image is linked above its address"),
            ),
            (
                "headers past its memory",
                |bytes, _, _, _| put(bytes, E_SHOFF, &0x10_0000_u64.to_le_bytes()),
                Malformed("the image extends past its memory"),
            ),
        ];

        for (name, edit, expected) in cases {
            let mut edited = memory.clone();
            edit(&mut edited, load, other, mapping.start);
            let result = read_memory_image(
                mapping.start,
                mapping.end,
                reader(&edited, mapping.start, edited.len()),
            );
            assert_eq!(result.unwrap_err(), expected, "{name}");
        }

        // Memory that ends early, or that cannot be read, holds no image.
        let image = read_memory_image(
            mapping.start,
            mapping.end,
            reader(&memory, mapping.start, memory.len()),
        )
        .unwrap();
        let size = image.data.len();
        assert_eq!(
            read_memory_image(
                mapping.start,
                mapping.start + size as u64 - 1,
                reader(&memory, mapping.start, memory.len()),
            )
            .unwrap_err(),
            Malformed("the image extends past its memory")
        );
        assert_eq!(
            read_memory_image(
                mapping.start,
                mapping.end,
                reader(&memory, mapping.start, size - 1),
            )
            .unwrap_err(),
            Unreadable(mapping.start)
        );
    }
}
