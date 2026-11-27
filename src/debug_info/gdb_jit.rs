//! Reader for the GDB JIT compilation interface (`__jit_debug_descriptor`).
//!
//! Walks the inferior's linked list of `jit_code_entry` structs to discover
//! in-memory ELF images registered by JIT compilers, using cheap header outlines
//! to avoid re-reading unchanged objects across stops.

use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::path::PathBuf;

use object::{Object as _, ObjectSection as _};

pub const DESCRIPTOR: &str = "__jit_debug_descriptor";

pub const DESCRIPTOR_BYTES: usize = 24;

const VERSION: u32 = 1;

const ENTRY_BYTES: usize = 32;

const MAX_OBJECTS: usize = 8192;
const MAX_OBJECT_BYTES: u64 = 16 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitObject {
    pub address: u64,
    pub bytes: Vec<u8>,
}

impl JitObject {
    pub fn name(&self) -> PathBuf {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.bytes.hash(&mut hasher);
        PathBuf::from(format!(
            "[jit {:#x} {:016x}]",
            self.address,
            hasher.finish()
        ))
    }

    pub fn code(&self) -> Option<Range<u64>> {
        let object = object::File::parse(self.bytes.as_slice()).ok()?;
        allocated_range(&object).ok().flatten()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Registered {
    pub objects: Vec<JitObject>,

    pub known: std::collections::BTreeMap<u64, Vec<u8>>,
    pub kept: Vec<u64>,

    pub problems: Vec<String>,
}

/// Captures the descriptor and entry-header bytes to detect JIT list changes
/// between stops without reading the full ELF payloads.
pub fn witness(
    descriptor: u64,
    mut read: impl FnMut(u64, usize) -> Option<Vec<u8>>,
    witness: &mut Vec<u8>,
) {
    let Some(header) = read(descriptor, DESCRIPTOR_BYTES) else {
        return;
    };
    let mut next = word(&header, 16).unwrap_or(0);
    witness.extend(header);
    let mut seen = std::collections::BTreeSet::new();

    while next != 0 && seen.len() < MAX_OBJECTS && seen.insert(next) {
        let Some(entry) = read(next, ENTRY_BYTES) else {
            return;
        };
        next = word(&entry, 0).unwrap_or(0);
        witness.extend(entry);
    }
}

fn word(bytes: &[u8], at: usize) -> Option<u64> {
    bytes
        .get(at..at + 8)
        .and_then(|word| <[u8; 8]>::try_from(word).ok())
        .map(u64::from_le_bytes)
}

pub fn registered(
    descriptor: u64,
    budget: &mut u64,
    mut read: impl FnMut(u64, usize) -> Option<Vec<u8>>,
    registered: &mut Registered,
) {
    let mut problem = |text: String| registered.problems.push(text);

    let Some(header) = read(descriptor, DESCRIPTOR_BYTES) else {
        problem(format!(
            "the JIT descriptor at {descriptor:#x} is unreadable"
        ));
        return;
    };
    let version = header
        .get(..4)
        .and_then(|version| <[u8; 4]>::try_from(version).ok())
        .map(u32::from_le_bytes);

    match version {
        Some(VERSION) => {}

        // Version 0 with a null head pointer is a zero-initialized `.bss`
        // descriptor before any JIT code has been registered.
        Some(0) if word(&header, 16) == Some(0) => return,
        _ => {
            problem(format!(
                "the JIT descriptor at {descriptor:#x} has version {version:?}"
            ));
            return;
        }
    }

    let mut objects = Vec::new();
    let mut next = word(&header, 16).unwrap_or(0);
    let mut seen = std::collections::BTreeSet::new();

    while next != 0 {
        if seen.len() == MAX_OBJECTS || !seen.insert(next) {
            problem(format!(
                "the JIT descriptor at {descriptor:#x} lists more than {MAX_OBJECTS} objects, or \
                 loops"
            ));
            break;
        }

        let Some(entry) = read(next, ENTRY_BYTES) else {
            problem(format!("the JIT entry at {next:#x} is unreadable"));
            break;
        };
        next = word(&entry, 0).unwrap_or(0);
        let (Some(address), Some(size)) = (word(&entry, 16), word(&entry, 24)) else {
            break;
        };

        if let Some(known) = registered.known.get(&address)
            && outline(size, |at, size| read(address.checked_add(at)?, size)).as_ref()
                == Some(known)
        {
            registered.kept.push(address);
            continue;
        }

        if size == 0 || size > MAX_OBJECT_BYTES || size > *budget {
            problem(format!(
                "the JIT object at {address:#x} has {size} bytes, of {budget} left to read"
            ));
            continue;
        }

        let bytes = usize::try_from(size)
            .ok()
            .and_then(|size| read(address, size))
            .filter(|bytes| bytes.starts_with(b"\x7fELF"));
        let Some(bytes) = bytes else {
            problem(format!(
                "the JIT object at {address:#x} is unreadable or no ELF image"
            ));
            continue;
        };

        *budget -= size;
        objects.push(JitObject { address, bytes });
    }

    registered.objects.extend(objects);
}

/// Builds a compact fingerprint (size, ELF header, and section header table)
/// used to recognize previously loaded JIT ELF images without copying them again.
pub fn outline(size: u64, mut read: impl FnMut(u64, usize) -> Option<Vec<u8>>) -> Option<Vec<u8>> {
    const HEADER: usize = 64;
    let mut outline = size.to_le_bytes().to_vec();
    let header = read(0, HEADER)?;
    let half = |at: usize| {
        header
            .get(at..at + 2)
            .and_then(|half| <[u8; 2]>::try_from(half).ok())
            .map(u16::from_le_bytes)
    };
    let sections = word(&header, 0x28)?;
    let length = usize::from(half(0x3a)?) * usize::from(half(0x3c)?);
    if sections.checked_add(length as u64)? > size {
        return None;
    }
    outline.extend_from_slice(&header);
    outline.extend(read(sections, length)?);
    Some(outline)
}

pub(super) fn allocated_range(
    object: &object::File<'_>,
) -> Result<Option<Range<u64>>, gimli::Error> {
    let mut held: Option<Range<u64>> = None;
    let allocated = object.sections().filter(|section| {
        matches!(section.flags(), object::SectionFlags::Elf { sh_flags }
            if sh_flags & u64::from(object::elf::SHF_ALLOC) != 0)
            && section.size() != 0
    });

    for section in allocated {
        let start = section.address();
        let end = start
            .checked_add(section.size())
            .ok_or(gimli::Error::AddressOverflow)?;
        held = Some(held.map_or(start..end, |held| held.start.min(start)..held.end.max(end)));
    }

    Ok(held)
}

#[cfg(test)]
pub fn test_image(address: u64, size: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut writer = object::write::elf::Writer::new(object::Endianness::Little, true, &mut bytes);
    let text = writer.add_section_name(b".text");
    writer.reserve_file_header();
    writer.reserve_null_section_index();
    writer.reserve_section_index();
    let code = writer.reserve(size, 16);
    writer.reserve_shstrtab_section_index();
    writer.reserve_shstrtab();
    writer.reserve_section_headers();

    writer
        .write_file_header(&object::write::elf::FileHeader {
            os_abi: 0,
            abi_version: 0,
            e_type: object::elf::ET_DYN,
            e_machine: object::elf::EM_X86_64,
            e_entry: 0,
            e_flags: 0,
        })
        .expect("a header");
    writer.write_align(16);
    writer.write(&vec![0xcc; size]);
    writer.write_shstrtab();
    writer.write_null_section_header();
    writer.write_section_header(&object::write::elf::SectionHeader {
        name: Some(text),
        sh_type: object::elf::SHT_PROGBITS,
        sh_flags: u64::from(object::elf::SHF_ALLOC | object::elf::SHF_EXECINSTR),
        sh_addr: address,
        sh_offset: code as u64,
        sh_size: size as u64,
        sh_link: 0,
        sh_info: 0,
        sh_addralign: 16,
        sh_entsize: 0,
    });
    writer.write_shstrtab_section_header();

    bytes
}

#[cfg(test)]
mod tests;
