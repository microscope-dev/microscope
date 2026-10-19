//! Object files a just-in-time compiler writes to memory and registers
//! through the GDB JIT interface, as CPython's JIT does with each trace it
//! compiles: an ELF image whose sections lie at the addresses the process
//! runs them at, with the symbols and call-frame information that name and
//! unwind its code. Each is a module at load bias zero.
//!
//! A compiler keeps a list of its objects in its `__jit_debug_descriptor`,
//! which is read when wanted; microscope plants no breakpoint on the
//! function a compiler calls after each change, which would stop the
//! process for every trace. A list read while a compiler changes it is read
//! as far as it is sound, and what could not be read is said.

use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::path::PathBuf;

use object::{Object as _, ObjectSection as _};

/// The symbol of the interface's descriptor:
/// `struct jit_descriptor { uint32_t version; uint32_t action_flag;
/// jit_code_entry *relevant_entry; jit_code_entry *first_entry; }`.
pub const DESCRIPTOR: &str = "__jit_debug_descriptor";

/// The bytes of a descriptor, which change whenever its compiler registers
/// or unregisters an object.
pub const DESCRIPTOR_BYTES: usize = 24;

/// The only version of the interface.
const VERSION: u32 = 1;

/// The bytes of `struct jit_code_entry { jit_code_entry *next_entry;
/// jit_code_entry *prev_entry; const char *symfile_addr;
/// uint64_t symfile_size; }`.
const ENTRY_BYTES: usize = 32;

/// The most objects one descriptor lists that are read, and the largest
/// object read.
const MAX_OBJECTS: usize = 8192;
const MAX_OBJECT_BYTES: u64 = 16 << 20;

/// One registered object: where its image was, and the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitObject {
    pub address: u64,
    pub bytes: Vec<u8>,
}

impl JitObject {
    /// The module's name: where its image lies and a hash of what it
    /// holds, so that a compiler that frees an object and writes another
    /// in its place makes a new module.
    pub fn name(&self) -> PathBuf {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.bytes.hash(&mut hasher);
        PathBuf::from(format!(
            "[jit {:#x} {:016x}]",
            self.address,
            hasher.finish()
        ))
    }

    /// The addresses its allocated sections take, which are its code's.
    pub fn code(&self) -> Option<Range<u64>> {
        let object = object::File::parse(self.bytes.as_slice()).ok()?;
        allocated_range(&object).ok().flatten()
    }
}

/// What the descriptors listed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Registered {
    pub objects: Vec<JitObject>,

    /// Each object, or part of a list, that could not be read, and why.
    pub problems: Vec<String>,
}

/// Appends to `witness` the bytes of the descriptor at `descriptor` and of
/// each entry it lists, which change whenever its compiler registers or
/// unregisters an object: a new object's entry may be where an old one's
/// was, leaving the descriptor's own bytes as they were.
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

/// The little-endian word at `at` in `bytes`.
fn word(bytes: &[u8], at: usize) -> Option<u64> {
    bytes
        .get(at..at + 8)
        .and_then(|word| <[u8; 8]>::try_from(word).ok())
        .map(u64::from_le_bytes)
}

/// The objects the descriptor at `descriptor` lists, reading at most
/// `budget` bytes of them through `read`, which gives all the bytes asked
/// for or none.
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

        // A compiler that has registered nothing may leave it unset.
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

/// The addresses an image's allocated sections take, for an image with no
/// program headers to say where it lies, as a JIT compiler writes one.
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

/// An image as CPython's JIT writes one: no program headers, and code of
/// `size` bytes in a `.text` at `address`, where it runs.
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
