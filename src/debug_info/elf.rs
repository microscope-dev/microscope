//! ELF symbol tables, normalized into module-image symbols.
//!
//! The static (`.symtab`), dynamic (`.dynsym`), and embedded `MiniDebugInfo`
//! (`.gnu_debugdata`) tables are merged into one catalog. Only defined code
//! symbols in executable sections receive an extent, so only they can name
//! machine code; only sized data symbols in allocated sections name storage.
//! Each PLT stub is named `name@plt` after the function it jumps to, as
//! binutils names them. Where one name has several definitions, symbol
//! versions tell them apart, as `memcpy@GLIBC_2.2.5` does an old `memcpy`.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;

use object::read::elf::{Dyn as _, ElfFile, FileHeader, ProgramHeader as _, SectionHeader as _};
use object::{
    Object, ObjectSection, ObjectSegment, ObjectSymbol, ObjectSymbolTable, SectionFlags,
    SectionKind, SegmentFlags, SymbolFlags, SymbolSection, elf,
};

use crate::{
    AddressRange, EmbeddedSymbolTable, GotSlot, GotTarget, ImageAddress, SectionId, SectionInfo,
    SymbolBinding, SymbolExtent, SymbolExtentProvenance, SymbolId, SymbolInfo, SymbolKind,
    SymbolTableSources, ThreadLocal,
};

/// Bounds the decompressed size of an embedded symbol table so a malformed or
/// hostile module cannot exhaust memory.
const EMBEDDED_TABLE_LIMIT: usize = 64 << 20;

pub struct SymbolTable {
    pub symbols: Vec<SymbolInfo>,
    pub got_slots: Vec<GotSlot>,
    pub sources: SymbolTableSources,
}

/// A section of the module image, identified by its name and address range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageSection<'data> {
    name: &'data [u8],
    start: u64,
    end: u64,
}

struct RawSymbol {
    kind: SymbolKind,
    binding: SymbolBinding,
    exported: bool,
    size: u64,

    /// The image's executable section that defines a code symbol.
    code_section: Option<(u64, u64)>,

    /// The image's allocated section that defines a data symbol.
    storage_section: Option<(u64, u64)>,

    /// Whether a table defines the symbol without a version.
    unversioned: bool,

    /// The versions the dynamic table defines the symbol in, each with
    /// whether it is the name's default version, which the static linker
    /// binds new references to.
    versions: Vec<(Box<[u8]>, bool)>,
}

/// Loads every symbol table of an ELF image. `unwind_functions` are the code
/// ranges the image's call-frame information describes; their starts bound
/// the inferred extents of unsized symbols.
pub fn load_symbols(
    object: &object::File<'_>,
    debug_object: Option<&object::File<'_>>,
    unwind_functions: &[AddressRange<ImageAddress>],
) -> SymbolTable {
    let sections = ImageSections {
        code: image_sections(object, is_code),
        storage: image_sections(object, holds_storage),
    };

    let mut raw = BTreeMap::new();

    collect(object, object.symbols(), &sections, false, &mut raw);

    // A separate debug file keeps the static table its module was stripped
    // of, with the module's section headers, which match by identity as an
    // embedded object's do.
    if let Some(debug_object) = debug_object {
        collect(
            debug_object,
            debug_object.symbols(),
            &sections,
            false,
            &mut raw,
        );
    }

    let embedded_table = object
        .section_by_name(".gnu_debugdata")
        .map_or(EmbeddedSymbolTable::Absent, |section| {
            collect_embedded(object, &section, &sections, &mut raw)
        });

    collect(object, object.dynamic_symbols(), &sections, true, &mut raw);
    collect_versions(object, &mut raw);
    let mut raw = distinguish_versions(raw);
    let got_slots = got_slots(object);
    collect_plt(object, &got_slots, &mut raw);

    SymbolTable {
        symbols: normalize(raw, unwind_functions),
        got_slots,
        sources: SymbolTableSources {
            static_table: object.symbol_table().is_some()
                || debug_object.is_some_and(|debug_object| debug_object.symbol_table().is_some()),
            dynamic_table: object.dynamic_symbol_table().is_some(),
            embedded_table,
            runtime_function_table: EmbeddedSymbolTable::Absent,
        },
    }
}

/// Where each thread's copy of each thread-local variable the image
/// defines is, as the image's code finds it on x86-64.
///
/// A symbol's value is its offset in the image's block of thread-local
/// storage. A library's code reaches a variable through a GOT slot that the
/// loader fills, by an `R_X86_64_TPOFF64` relocation, with the variable's
/// offset from the thread pointer, or else through `__tls_get_addr`, which
/// finds the library's block through the thread's dynamic thread vector.
/// An executable's block lies just below the thread pointer, at the
/// block's size rounded up to its alignment.
pub fn load_thread_locals(
    object: &object::File<'_>,
) -> BTreeMap<Arc<str>, Result<ThreadLocal, Arc<str>>> {
    let object::File::Elf64(elf) = object else {
        return BTreeMap::new();
    };

    if object.architecture() != object::Architecture::X86_64 {
        return BTreeMap::new();
    }

    let endian = elf.endian();
    let mut offsets = BTreeMap::<Arc<str>, Option<u64>>::new();

    for symbol in object.symbols().chain(object.dynamic_symbols()) {
        if symbol.kind() != object::SymbolKind::Tls
            || !matches!(symbol.section(), SymbolSection::Section(_))
        {
            continue;
        }

        let Ok(name) = symbol.name() else {
            continue;
        };

        // Two variables of one name, such as two files' statics, are told
        // apart by neither.
        let address = symbol.address();
        offsets
            .entry(name.into())
            .and_modify(|offset| {
                if *offset != Some(address) {
                    *offset = None;
                }
            })
            .or_insert(Some(address));
    }

    if offsets.is_empty() {
        return BTreeMap::new();
    }

    // The GOT slots the loader fills, by the offset in the block each is
    // for.
    let mut slots = BTreeMap::new();

    for (address, relocation) in object.dynamic_relocations().into_iter().flatten() {
        if relocation.flags()
            != (object::RelocationFlags::Elf {
                r_type: elf::R_X86_64_TPOFF64,
            })
        {
            continue;
        }

        let base = match relocation.target() {
            object::RelocationTarget::Absolute => Some(0),
            object::RelocationTarget::Symbol(index) => object
                .dynamic_symbol_table()
                .and_then(|table| table.symbol_by_index(index).ok())
                .filter(|symbol| matches!(symbol.section(), SymbolSection::Section(_)))
                .map(|symbol| symbol.address()),
            _ => None,
        };

        if let Some(offset) = base.and_then(|base| base.checked_add_signed(relocation.addend())) {
            slots.insert(offset, ImageAddress::new(address));
        }
    }

    let tls = elf
        .elf_program_headers()
        .iter()
        .find(|header| header.p_type(endian) == elf::PT_TLS)
        .map(|header| {
            (
                header.p_vaddr(endian),
                header.p_memsz(endian),
                header.p_align(endian).max(1),
            )
        });
    let executable = is_executable(elf);
    let fixed = |offset: u64| -> Result<ThreadLocal, Arc<str>> {
        if !executable {
            return Ok(ThreadLocal::Dynamic(offset));
        }

        let (start, size, align) = tls.ok_or("the image has no thread-local storage")?;
        if start % align != 0 {
            return Err("the image's thread-local storage is misaligned".into());
        }

        let block = size
            .checked_next_multiple_of(align)
            .and_then(|block| i64::try_from(block).ok())
            .ok_or("the image's thread-local storage is too large")?;
        let offset = i64::try_from(offset).map_err(|_| "its offset is too large")?;

        Ok(ThreadLocal::Offset(offset - block))
    };

    offsets
        .into_iter()
        .map(|(name, offset)| {
            let place = offset.map_or_else(
                || Err("several thread-local variables have its name".into()),
                |offset| {
                    slots
                        .get(&offset)
                        .map_or_else(|| fixed(offset), |slot| Ok(ThreadLocal::Slot(*slot)))
                },
            );

            (name, place)
        })
        .collect()
}

/// Where the thread-local variable that the function at `address` reads
/// is, decoded from the function's own code, for a variable no symbol
/// names, as a stripped library's: V8's `v8::Isolate::GetCurrent` reads
/// `g_current_isolate_`, which only the library's debug package names.
///
/// The function may jump straight to the code that reads it, and may open
/// a frame first. That code is one of the four sequences the x86-64 TLS
/// ABI defines, with the linker's relaxations:
///
/// - general dynamic: `lea rdi, [rip+pair]` with a 0x66 prefix, then a
///   call to `__tls_get_addr`; the pair's second word, or the
///   `R_X86_64_DTPOFF64` relocation that fills it, holds the offset;
/// - local dynamic: the same lea without the prefix, the call, then an
///   access at a displacement from rax, which is the offset;
/// - initial exec: `mov reg, [rip+slot]`, where the loader fills the slot
///   by an `R_X86_64_TPOFF64` relocation, then an fs-relative access;
/// - local exec, an executable's own: an fs-relative access at a fixed
///   displacement.
pub fn accessor_thread_local(
    object: &object::File<'_>,
    address: u64,
) -> Result<ThreadLocal, Arc<str>> {
    let relocations: BTreeMap<u64, object::Relocation> =
        object.dynamic_relocations().into_iter().flatten().collect();
    match tls_access(object, &relocations, address)? {
        TlsAccess::Found(place) => Ok(place),
        // A function that only jumps elsewhere, as an exported wrapper of
        // an internal accessor does.
        TlsAccess::Jump(target) => match tls_access(object, &relocations, target)? {
            TlsAccess::Found(place) => Ok(place),
            TlsAccess::Jump(_) => Err("the accessor jumps more than once".into()),
        },
    }
}

/// What the code of an accessor begins with.
enum TlsAccess {
    Found(ThreadLocal),
    Jump(u64),
}

/// The thread-local variable the code at `address` reads, or where it
/// jumps first.
fn tls_access(
    object: &object::File<'_>,
    relocations: &BTreeMap<u64, object::Relocation>,
    address: u64,
) -> Result<TlsAccess, Arc<str>> {
    use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};

    let bytes = object
        .sections()
        .find_map(|section| {
            let start = section.address();
            let end = start.checked_add(section.size())?;
            if section.kind() != SectionKind::Text || !(start..end).contains(&address) {
                return None;
            }
            let data = section.data().ok()?;
            let at = usize::try_from(address - start).ok()?;
            data.get(at..data.len().min(at + 64))
        })
        .ok_or("the accessor's code is not in the image")?;
    let r_type = |slot: u64| match relocations.get(&slot).map(object::Relocation::flags) {
        Some(object::RelocationFlags::Elf { r_type }) => Some(r_type),
        _ => None,
    };
    let fs_access = |instruction: &iced_x86::Instruction| {
        instruction.segment_prefix() == Register::FS
            && (0..instruction.op_count())
                .any(|operand| instruction.op_kind(operand) == OpKind::Memory)
    };

    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut instructions = std::iter::from_fn(|| decoder.can_decode().then(|| decoder.decode()))
        .filter(|instruction| {
            instruction.mnemonic() != Mnemonic::Endbr64
                && !super::x86_64::is_push_register(instruction, Register::RBP)
                && !super::x86_64::is_register_move(instruction, Register::RBP, Register::RSP)
        });
    let first = instructions.next().ok_or("the accessor has no code")?;

    if first.mnemonic() == Mnemonic::Jmp && first.op0_kind() == OpKind::NearBranch64 {
        return Ok(TlsAccess::Jump(first.near_branch_target()));
    }

    if first.mnemonic() == Mnemonic::Lea
        && first.op0_register() == Register::RDI
        && first.is_ip_rel_memory_operand()
    {
        let pair = first.ip_rel_memory_address();
        if instructions
            .next()
            .is_none_or(|call| call.mnemonic() != Mnemonic::Call)
        {
            return Err("the accessor's lea is not followed by a call".into());
        }
        if r_type(pair) != Some(elf::R_X86_64_DTPMOD64) {
            return Err("the accessor's lea names no module and offset pair".into());
        }
        // Only the general-dynamic sequence's lea carries the prefix.
        let at = usize::try_from(first.ip() - address).unwrap_or(usize::MAX);
        if bytes.get(at) == Some(&0x66) {
            let offset = dynamic_pair_offset(object, relocations, pair + 8)?;
            return Ok(TlsAccess::Found(ThreadLocal::Dynamic(offset)));
        }
        let access = instructions
            .find(|instruction| {
                instruction.op_count() == 2
                    && instruction.op1_kind() == OpKind::Memory
                    && instruction.memory_base() == Register::RAX
                    && instruction.memory_index() == Register::None
            })
            .ok_or("the accessor reads nothing in its module's block")?;
        return Ok(TlsAccess::Found(ThreadLocal::Dynamic(
            access.memory_displacement64(),
        )));
    }

    if first.mnemonic() == Mnemonic::Mov
        && first.op1_kind() == OpKind::Memory
        && first.is_ip_rel_memory_operand()
    {
        let slot = first.ip_rel_memory_address();
        if r_type(slot) != Some(elf::R_X86_64_TPOFF64) {
            return Err("the accessor's slot is not one the loader fills with an offset".into());
        }
        if !instructions.next().is_some_and(|next| fs_access(&next)) {
            return Err("the accessor reads its slot but not through the thread pointer".into());
        }
        return Ok(TlsAccess::Found(ThreadLocal::Slot(ImageAddress::new(slot))));
    }

    if fs_access(&first)
        && first.memory_base() == Register::None
        && first.memory_index() == Register::None
    {
        return Ok(TlsAccess::Found(ThreadLocal::Offset(
            first.memory_displacement64().cast_signed(),
        )));
    }

    Err("the accessor reads no thread-local variable the TLS ABI's way".into())
}

/// The offset a general-dynamic pair's second word, at `slot`, holds once
/// loaded: what its `R_X86_64_DTPOFF64` relocation computes, or, for a
/// variable only its module sees, what the linker wrote there.
fn dynamic_pair_offset(
    object: &object::File<'_>,
    relocations: &BTreeMap<u64, object::Relocation>,
    slot: u64,
) -> Result<u64, Arc<str>> {
    if let Some(relocation) = relocations.get(&slot) {
        if relocation.flags()
            != (object::RelocationFlags::Elf {
                r_type: elf::R_X86_64_DTPOFF64,
            })
        {
            return Err("the pair's offset is relocated some other way".into());
        }
        let base = match relocation.target() {
            object::RelocationTarget::Absolute => 0,
            object::RelocationTarget::Symbol(index) => object
                .dynamic_symbol_table()
                .and_then(|table| table.symbol_by_index(index).ok())
                .filter(|symbol| matches!(symbol.section(), SymbolSection::Section(_)))
                .ok_or("the pair's offset names a variable another module defines")?
                .address(),
            _ => return Err("the pair's offset names no variable".into()),
        };
        return base
            .checked_add_signed(relocation.addend())
            .ok_or_else(|| "the pair's offset overflows".into());
    }

    object
        .sections()
        .find_map(|section| {
            let start = section.address();
            let at = usize::try_from(slot.checked_sub(start)?).ok()?;
            let word = section.data().ok()?.get(at..at.checked_add(8)?)?;
            Some(u64::from_le_bytes(word.try_into().ok()?))
        })
        .ok_or_else(|| "the pair's offset is not in the image".into())
}

/// Whether an ELF image is an executable rather than a library: one the
/// system links as such, or one that names a loader or says it is a
/// position-independent executable.
fn is_executable(elf: &object::read::elf::ElfFile64<'_>) -> bool {
    let endian = elf.endian();
    if elf.elf_header().e_type(endian) == elf::ET_EXEC
        || elf
            .elf_program_headers()
            .iter()
            .any(|header| header.p_type(endian) == elf::PT_INTERP)
    {
        return true;
    }

    elf.elf_section_table()
        .dynamic(endian, elf.data())
        .ok()
        .flatten()
        .is_some_and(|(entries, _)| {
            entries.iter().any(|entry| {
                entry.d_tag(endian) == elf::DT_FLAGS_1
                    && entry.d_val(endian) & u64::from(elf::DF_1_PIE) != 0
            })
        })
}

/// Collects the symbols of a `MiniDebugInfo` section: an xz-compressed ELF
/// object whose static symbol table holds the functions the dynamic table
/// omits.
fn collect_embedded(
    object: &object::File<'_>,
    section: &object::Section<'_, '_>,
    sections: &ImageSections<'_>,
    raw: &mut BTreeMap<(Box<[u8]>, u64), RawSymbol>,
) -> EmbeddedSymbolTable {
    let data = match section
        .data()
        .map_err(|error| Arc::from(format!("the section is unreadable: {error}")))
        .and_then(|compressed| decompress(compressed, EMBEDDED_TABLE_LIMIT))
    {
        Ok(data) => data,
        Err(reason) => return EmbeddedSymbolTable::Unusable { reason },
    };

    match object::File::parse(data.as_slice()) {
        Ok(embedded) if embedded.architecture() == object.architecture() => {
            collect(&embedded, embedded.symbols(), sections, false, raw);
            EmbeddedSymbolTable::Loaded
        }
        Ok(_) => unusable("the embedded object targets another architecture"),
        Err(error) => unusable(&format!("the embedded object is malformed: {error}")),
    }
}

fn unusable(reason: &str) -> EmbeddedSymbolTable {
    EmbeddedSymbolTable::Unusable {
        reason: reason.into(),
    }
}

/// Whether the image has a `PT_TLS` segment. An empty one gives threads no
/// storage, and loaders assign it no TLS module.
pub fn has_thread_local_storage(object: &object::File<'_>) -> bool {
    fn any_tls<Elf: FileHeader>(file: &ElfFile<'_, Elf>) -> bool {
        let endian = file.endian();
        file.elf_program_headers().iter().any(|segment| {
            segment.p_type(endian) == elf::PT_TLS && segment.p_memsz(endian).into() != 0
        })
    }

    match object {
        object::File::Elf32(file) => any_tls(file),
        object::File::Elf64(file) => any_tls(file),
        _ => false,
    }
}

/// Returns the image's allocated sections, ordered by address. Empty
/// sections are omitted, as is a thread-local `.tbss`, whose addresses are
/// only a template layout and overlap the sections that follow it.
pub fn load_sections(object: &object::File<'_>) -> Vec<SectionInfo> {
    let mut sections = object
        .sections()
        .filter_map(|section| {
            let SectionFlags::Elf { sh_flags } = section.flags() else {
                return None;
            };

            if sh_flags & u64::from(elf::SHF_ALLOC) == 0
                || section.kind() == object::SectionKind::UninitializedTls
            {
                return None;
            }

            let start = section.address();
            let end = start.checked_add(section.size())?;
            let name = String::from_utf8_lossy(section.name_bytes().ok()?).into_owned();
            (start < end).then_some((
                start,
                end,
                name,
                sh_flags & u64::from(elf::SHF_EXECINSTR) != 0,
                sh_flags & u64::from(elf::SHF_WRITE) != 0,
            ))
        })
        .collect::<Vec<_>>();
    sections.sort();

    // ELF numbers sections with `u32`s, so every section has an id.
    sections
        .into_iter()
        .zip(0..=u32::MAX)
        .map(
            |((start, end, name, executable, writable), index)| SectionInfo {
                id: SectionId::new(index),
                name: name.into(),
                range: AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                },
                executable,
                writable,
            },
        )
        .collect()
}

/// The image's sections that can define symbol extents and storage.
struct ImageSections<'data> {
    code: Vec<ImageSection<'data>>,
    storage: Vec<ImageSection<'data>>,
}

/// Returns the address ranges of an image's executable sections, or of its
/// executable segments when it has no section headers.
pub(super) fn executable_ranges(object: &object::File<'_>) -> Vec<AddressRange<ImageAddress>> {
    let range = |start: u64, size: u64| {
        Some(AddressRange {
            start: ImageAddress::new(start),
            end: ImageAddress::new(start.checked_add(size)?),
        })
    };

    let sections = image_sections(object, is_code)
        .into_iter()
        .filter_map(|section| range(section.start, section.end - section.start))
        .collect::<Vec<_>>();
    if !sections.is_empty() {
        return sections;
    }

    object
        .segments()
        .filter(|segment| {
            matches!(segment.flags(), SegmentFlags::Elf { p_flags } if p_flags & elf::PF_X != 0)
        })
        .filter_map(|segment| range(segment.address(), segment.size()))
        .collect()
}

const fn is_code(flags: u64) -> bool {
    let required = elf::SHF_ALLOC as u64 | elf::SHF_EXECINSTR as u64;
    flags & required == required
}

/// Whether a section can hold the storage of data symbols: allocated and
/// not thread-local.
const fn holds_storage(flags: u64) -> bool {
    flags & elf::SHF_ALLOC as u64 != 0 && flags & elf::SHF_TLS as u64 == 0
}

/// The image's sections whose ELF flags `accept` takes.
fn image_sections<'data>(
    object: &object::File<'data>,
    accept: fn(u64) -> bool,
) -> Vec<ImageSection<'data>> {
    object
        .sections()
        .filter_map(|section| image_section(&section, accept))
        .collect()
}

fn image_section<'data>(
    section: &impl ObjectSection<'data>,
    accept: fn(u64) -> bool,
) -> Option<ImageSection<'data>> {
    let SectionFlags::Elf { sh_flags } = section.flags() else {
        return None;
    };

    if !accept(sh_flags) {
        return None;
    }

    let start = section.address();
    Some(ImageSection {
        name: section.name_bytes().ok()?,
        start,
        end: start.checked_add(section.size())?,
    })
}

/// Decompresses an xz stream into at most `limit` bytes.
fn decompress(compressed: &[u8], limit: usize) -> Result<Vec<u8>, Arc<str>> {
    struct Bounded {
        data: Vec<u8>,
        limit: usize,
    }

    impl io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.data.len().saturating_add(bytes.len()) > self.limit {
                return Err(io::Error::other(format!(
                    "the decompressed table exceeds {} bytes",
                    self.limit
                )));
            }

            self.data.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut output = Bounded {
        data: Vec::new(),
        limit,
    };

    lzma_rs::xz_decompress(&mut io::BufReader::new(compressed), &mut output)
        .map_err(|error| Arc::from(format!("the section is not a valid xz stream: {error}")))?;
    Ok(output.data)
}

fn collect<'data, S>(
    source: &object::File<'data>,
    symbols: impl Iterator<Item = S>,
    image_sections: &ImageSections<'_>,
    exported: bool,
    raw: &mut BTreeMap<(Box<[u8]>, u64), RawSymbol>,
) where
    S: ObjectSymbol<'data>,
{
    for symbol in symbols {
        // Undefined, absolute, and common symbols have no image address.
        let SymbolSection::Section(index) = symbol.section() else {
            continue;
        };

        let SymbolFlags::Elf { st_info, .. } = symbol.flags() else {
            continue;
        };

        let kind = match st_info & 0xf {
            elf::STT_FUNC => SymbolKind::Function,
            elf::STT_GNU_IFUNC => SymbolKind::IndirectFunction,
            elf::STT_OBJECT | elf::STT_COMMON => SymbolKind::Data,
            elf::STT_NOTYPE => SymbolKind::Unknown,
            // Section and file symbols name no entity, and a thread-local
            // symbol's value is an offset into a TLS block, not an address.
            _ => continue,
        };

        let binding = match st_info >> 4 {
            elf::STB_GLOBAL | elf::STB_GNU_UNIQUE => SymbolBinding::Global,
            elf::STB_WEAK => SymbolBinding::Weak,
            elf::STB_LOCAL => SymbolBinding::Local,
            _ => continue,
        };

        let Ok(name) = symbol.name_bytes() else {
            continue;
        };

        if name.is_empty() {
            continue;
        }

        // A symbol from the embedded object refers to that object's copy of
        // the section header table, so sections are matched by identity.
        let defining = source.section_by_index(index).ok();
        // A label names no code, except the process's entry point, which
        // glibc's dynamic loader defines as one: where a stack ends.
        let names_code = matches!(kind, SymbolKind::Function | SymbolKind::IndirectFunction)
            || (kind == SymbolKind::Unknown && entry_label(name));
        let code_section = names_code
            .then_some(defining.as_ref())
            .flatten()
            .and_then(|section| image_section(section, is_code))
            .filter(|section| image_sections.code.contains(section))
            .map(|section| (section.start, section.end));
        let storage_section = (kind == SymbolKind::Data)
            .then_some(defining.as_ref())
            .flatten()
            .and_then(|section| image_section(section, holds_storage))
            .filter(|section| image_sections.storage.contains(section))
            .map(|section| (section.start, section.end));

        // Symbols are keyed by their name bytes so that names which are not
        // UTF-8 stay distinct.
        let entry = raw
            .entry((Box::from(name), symbol.address()))
            .or_insert_with(|| RawSymbol {
                kind,
                binding,
                exported,
                size: symbol.size(),
                code_section,
                storage_section,
                unversioned: false,
                versions: Vec::new(),
            });
        entry.exported |= exported;

        // The dynamic table's versions are read separately.
        entry.unversioned |= !exported;
    }
}

/// Records the versions the dynamic table's version section gives each
/// defined dynamic symbol.
fn collect_versions(object: &object::File<'_>, raw: &mut BTreeMap<(Box<[u8]>, u64), RawSymbol>) {
    let versions = match object {
        object::File::Elf64(elf) => elf
            .elf_section_table()
            .versions(elf.endian(), elf.data())
            .ok()
            .flatten()
            .map(|versions| (elf.endian(), versions)),
        _ => None,
    };

    for symbol in object.dynamic_symbols() {
        let Ok(name) = symbol.name_bytes() else {
            continue;
        };

        let Some(entry) = raw.get_mut(&(Box::from(name), symbol.address())) else {
            continue;
        };

        let version = versions.as_ref().and_then(|(endian, versions)| {
            let index = versions.version_index(*endian, symbol.index());
            if index.is_local() || index.is_global() {
                return None;
            }

            let version = versions.version(index).ok().flatten()?;
            Some((Box::from(version.name()), !index.is_hidden()))
        });

        match version {
            Some(version) => entry.versions.push(version),
            None => entry.unversioned = true,
        }
    }
}

/// Spells the versions of the definitions of a name that several
/// addresses share, as binutils does: older versions as `name@VERSION`,
/// then the default version as `name@@VERSION` where the plain name would
/// still name several addresses. The plain name stays where a table
/// defines it without a version, or as the default version.
fn distinguish_versions(
    raw: BTreeMap<(Box<[u8]>, u64), RawSymbol>,
) -> BTreeMap<(Box<[u8]>, u64), RawSymbol> {
    fn respell(
        raw: BTreeMap<(Box<[u8]>, u64), RawSymbol>,
        default: bool,
    ) -> BTreeMap<(Box<[u8]>, u64), RawSymbol> {
        let mut addresses = BTreeMap::<&[u8], usize>::new();
        for (name, _) in raw.keys() {
            *addresses.entry(name).or_default() += 1;
        }

        let shared = addresses
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(name, _)| Box::<[u8]>::from(name))
            .collect::<BTreeSet<_>>();
        let mut spelled = BTreeMap::new();
        let mut add = |key: (Box<[u8]>, u64), symbol: RawSymbol| match spelled.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(symbol);
            }

            // A static table may already hold the versioned spelling.
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let existing: &mut RawSymbol = entry.get_mut();
                existing.exported |= symbol.exported;
                existing.unversioned |= symbol.unversioned;
                existing.versions.extend(symbol.versions);
            }
        };

        for ((name, address), mut symbol) in raw {
            if !shared.contains(&name) {
                add((name, address), symbol);
                continue;
            }

            let separator: &[u8] = if default { b"@@" } else { b"@" };
            let (respelled, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut symbol.versions)
                .into_iter()
                .partition(|(_, is_default)| *is_default == default);

            for (version, _) in respelled {
                let spelling = [&name[..], separator, &version].concat().into_boxed_slice();
                add(
                    (spelling, address),
                    RawSymbol {
                        exported: true,
                        unversioned: true,
                        versions: Vec::new(),
                        ..symbol
                    },
                );
            }

            symbol.versions = kept;
            if symbol.unversioned || !symbol.versions.is_empty() {
                add((name, address), symbol);
            }
        }

        spelled
    }

    respell(respell(raw, false), true)
}

/// Names each PLT stub after the function whose GOT slot its indirect jump
/// reads: the symbol a `JUMP_SLOT` or `GLOB_DAT` relocation fills the slot
/// with, or for an `IRELATIVE` one, the indirect function whose resolver
/// fills it. A stub is one section entry long, and local to the image.
/// The GOT slots the loader fills with functions' addresses: those that
/// relocations name a function for, and those an indirect function of the
/// module's own fills.
fn got_slots(object: &object::File<'_>) -> Vec<GotSlot> {
    if object.architecture() != object::Architecture::X86_64 {
        return Vec::new();
    }

    let mut slots = Vec::new();
    for (slot, relocation) in object.dynamic_relocations().into_iter().flatten() {
        let object::RelocationFlags::Elf { r_type } = relocation.flags() else {
            continue;
        };

        let target = match (r_type, relocation.target()) {
            (
                elf::R_X86_64_JUMP_SLOT | elf::R_X86_64_GLOB_DAT,
                object::RelocationTarget::Symbol(index),
            ) => object
                .dynamic_symbol_table()
                .and_then(|table| table.symbol_by_index(index).ok())
                // A data import's slot holds no function.
                .filter(|symbol| symbol.kind() != object::SymbolKind::Data)
                .and_then(|symbol| symbol.name().ok())
                .filter(|name| !name.is_empty())
                .map(|name| GotTarget::Import(Arc::from(name))),
            (elf::R_X86_64_IRELATIVE, _) => u64::try_from(relocation.addend())
                .ok()
                .map(|resolver| GotTarget::Indirect(ImageAddress::new(resolver))),
            _ => None,
        };

        if let Some(target) = target {
            slots.push(GotSlot {
                address: ImageAddress::new(slot),
                target,
            });
        }
    }

    slots
}

fn collect_plt(
    object: &object::File<'_>,
    slots: &[GotSlot],
    raw: &mut BTreeMap<(Box<[u8]>, u64), RawSymbol>,
) {
    let object::File::Elf64(elf) = object else {
        return;
    };

    let endian = elf.endian();

    // An indirect function is named by its exported symbol first, then by
    // the first of its names.
    let mut resolvers = BTreeMap::<u64, (bool, &[u8])>::new();

    for ((name, address), symbol) in raw.iter() {
        if symbol.kind != SymbolKind::IndirectFunction {
            continue;
        }

        let candidate = (!symbol.exported, &name[..]);
        resolvers
            .entry(*address)
            .and_modify(|best| *best = (*best).min(candidate))
            .or_insert(candidate);
    }

    let targets = slots
        .iter()
        .filter_map(|slot| {
            let name: Box<[u8]> = match &slot.target {
                GotTarget::Import(name) => Box::from(name.as_bytes()),
                GotTarget::Indirect(resolver) => Box::from(resolvers.get(&resolver.get())?.1),
            };

            Some((slot.address.get(), name))
        })
        .collect::<BTreeMap<_, _>>();

    if targets.is_empty() {
        return;
    }

    for section in elf.sections() {
        let Ok(section_name @ (b".plt" | b".plt.sec" | b".plt.got")) = section.name_bytes() else {
            continue;
        };

        let Ok(code) = section.data() else {
            continue;
        };

        let start = section.address();
        let declared = section.elf_section_header().sh_entsize(endian);
        let entry_size = match declared {
            8 | 16 => declared,
            _ if section_name == b".plt.got" => 8,
            _ => 16,
        };

        let Ok(entry_bytes) = usize::try_from(entry_size) else {
            continue;
        };

        // A section past the last address is malformed, and its stubs
        // have no addresses.
        let Some(end) = start.checked_add(code.len() as u64) else {
            continue;
        };

        for (index, entry) in code.chunks_exact(entry_bytes).enumerate() {
            let address = start + index as u64 * entry_size;
            let Some(name) = got_slot(entry, address).and_then(|slot| targets.get(&slot)) else {
                continue;
            };

            let mut stub = name.to_vec();
            stub.extend_from_slice(b"@plt");
            raw.entry((stub.into(), address)).or_insert(RawSymbol {
                kind: SymbolKind::Function,
                binding: SymbolBinding::Local,
                exported: false,
                size: entry_size,
                code_section: Some((start, end)),
                storage_section: None,
                unversioned: true,
                versions: Vec::new(),
            });
        }
    }
}

/// How many words each PLT stub's instructions find pushed since its call,
/// for the stubs whose start `uncovered` says no call-frame information
/// describes, as mold and lld leave them: an interval per instruction,
/// valued by the words pushed before it. A stub runs straight from its
/// start: a jump through its GOT slot either leaves it or, before lazy
/// binding, falls through to the next instruction. A direct jump leaves
/// it, and an instruction a stub has no reason to run ends what is known
/// of it. The `.plt` header is left out: what is pushed when it runs
/// depends on the stub that jumped to it.
pub fn plt_frames(
    object: &object::File<'_>,
    uncovered: impl Fn(ImageAddress) -> bool,
) -> Vec<(AddressRange<ImageAddress>, u32)> {
    /// The most instructions a stub of 16 bytes or fewer runs.
    const MOST_STUB_INSTRUCTIONS: usize = 4;
    let object::File::Elf64(elf) = object else {
        return Vec::new();
    };

    let endian = elf.endian();
    let sections = || {
        elf.sections().filter_map(move |section| {
            let name @ (b".plt" | b".plt.sec" | b".plt.got") = section.name_bytes().ok()? else {
                return None;
            };

            let code = section.data().ok()?;
            let declared = section.elf_section_header().sh_entsize(endian);
            let entry_size: usize = match declared {
                8 | 16 => declared.try_into().ok()?,
                _ if name == b".plt.got" => 8,
                _ => 16,
            };

            let header = usize::from(name == b".plt");

            // A section past the last address is malformed, and its stubs
            // have no addresses.
            section.address().checked_add(code.len() as u64)?;
            Some((section.address(), code, entry_size, header))
        })
    };

    let entries = sections()
        .map(|(_, code, size, _)| code.len() / size)
        .sum::<usize>();
    let mut frames = Vec::new();

    for (start, code, entry_size, header) in sections() {
        for (index, entry) in code.chunks_exact(entry_size).enumerate().skip(header) {
            let address = start + (index * entry_size) as u64;
            if !uncovered(ImageAddress::new(address)) {
                continue;
            }

            if frames.capacity() == 0 {
                frames.reserve_exact(entries * MOST_STUB_INSTRUCTIONS);
            }

            stub_frames(entry, address, &mut frames);
        }
    }

    frames
}

/// Adds the frames of the stub whose code is `code`, at `address`.
fn stub_frames(code: &[u8], address: u64, frames: &mut Vec<(AddressRange<ImageAddress>, u32)>) {
    use iced_x86::{Decoder, DecoderOptions, FlowControl, Mnemonic, OpKind, Register};
    let mut decoder = Decoder::with_ip(64, code, address, DecoderOptions::NONE);
    let mut pushed = 0;
    let first = frames.len();
    let mut jumps = false;

    while decoder.can_decode() {
        let instruction = decoder.decode();
        let range = AddressRange {
            start: ImageAddress::new(instruction.ip()),
            end: ImageAddress::new(instruction.next_ip()),
        };

        let words = match instruction.mnemonic() {
            _ if instruction.is_invalid() => break,
            Mnemonic::Endbr64 | Mnemonic::Nop => 0,
            Mnemonic::Mov
                if instruction.op0_kind() == OpKind::Register
                    && instruction.op0_register().full_register() != Register::RSP
                    && matches!(
                        instruction.op1_kind(),
                        OpKind::Immediate8
                            | OpKind::Immediate16
                            | OpKind::Immediate32
                            | OpKind::Immediate64
                            | OpKind::Immediate32to64
                    ) =>
            {
                0
            }

            Mnemonic::Push
                if matches!(
                    instruction.op0_kind(),
                    OpKind::Immediate8to64 | OpKind::Immediate32to64
                ) || instruction.is_ip_rel_memory_operand() =>
            {
                1
            }
            Mnemonic::Jmp if instruction.is_ip_rel_memory_operand() => {
                frames.push((range, pushed));
                jumps = true;
                continue;
            }
            Mnemonic::Jmp if instruction.flow_control() == FlowControl::UnconditionalBranch => {
                frames.push((range, pushed));
                jumps = true;
                break;
            }
            _ => break,
        };

        frames.push((range, pushed));
        pushed += words;
        if pushed > crate::image::unwind::MOST_STUB_PUSHES {
            jumps = false;
            break;
        }
    }

    // A stub leaves by a jump; code that reaches none is no stub.
    if !jumps {
        frames.truncate(first);
    }
}

/// The GOT slot a PLT stub's `jmp *slot(%rip)` reads, past any `endbr64`
/// or other instructions before it.
fn got_slot(code: &[u8], address: u64) -> Option<u64> {
    use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind};
    let mut decoder = Decoder::with_ip(64, code, address, DecoderOptions::NONE);
    while decoder.can_decode() {
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            return None;
        }

        if instruction.mnemonic() == Mnemonic::Jmp
            && instruction.op0_kind() == OpKind::Memory
            && instruction.is_ip_rel_memory_operand()
        {
            return Some(instruction.ip_rel_memory_address());
        }
    }

    None
}

fn normalize(
    raw: BTreeMap<(Box<[u8]>, u64), RawSymbol>,
    unwind_functions: &[AddressRange<ImageAddress>],
) -> Vec<SymbolInfo> {
    let code_starts = raw
        .iter()
        .filter(|(_, symbol)| symbol.code_section.is_some())
        .map(|((_, address), _)| *address)
        .collect::<BTreeSet<_>>();
    let mut unwind_ends = BTreeMap::<u64, u64>::new();
    for range in unwind_functions {
        let end = unwind_ends
            .entry(range.start.get())
            .or_insert_with(|| range.end.get());
        *end = (*end).min(range.end.get());
    }

    let mut symbols = raw
        .into_iter()
        .map(|((name, address), symbol)| {
            let extent = symbol.code_section.and_then(|section| {
                extent(address, symbol.size, section, &code_starts, &unwind_ends)
            });
            let storage = symbol
                .storage_section
                .and_then(|section| storage(address, symbol.size, section));
            let spelling = Arc::<str>::from(String::from_utf8_lossy(&name));
            (address, spelling, name, symbol, (extent, storage))
        })
        .collect::<Vec<_>>();

    // Lossily spelled names may repeat at one address, so their bytes break
    // the tie and keep the order deterministic.
    symbols.sort_by(|left, right| (left.0, &left.1, &left.2).cmp(&(right.0, &right.1, &right.2)));

    // A table of more symbols than `u32` ids number would take a file of
    // some hundred gigabytes.
    debug_assert!(
        u32::try_from(symbols.len()).is_ok(),
        "{} symbols",
        symbols.len()
    );

    symbols
        .into_iter()
        .zip(0..=u32::MAX)
        .map(
            |((address, name, _, symbol, (extent, storage)), index)| SymbolInfo {
                id: SymbolId::new(index),
                address: ImageAddress::new(address),
                kind: symbol.kind,
                binding: symbol.binding,
                exported: symbol.exported,
                role: super::roles::symbol_role(&name),
                name,
                extent,
                storage,
            },
        )
        .collect()
}

/// Whether an untyped symbol of this name is the label a process starts
/// at, which names the code after it as a function would.
fn entry_label(name: &[u8]) -> bool {
    super::roles::symbol_role(&String::from_utf8_lossy(name)) == crate::CodeRole::Outermost
}

/// Determines the storage a data symbol of `size` bytes names in the
/// allocated section `[section.0, section.1)`: empty for an unsized symbol,
/// and none when the symbol or its declared size lies outside the section.
fn storage(
    address: u64,
    size: u64,
    (section_start, section_end): (u64, u64),
) -> Option<AddressRange<ImageAddress>> {
    let end = address.checked_add(size)?;
    (section_start <= address && address < section_end && end <= section_end).then_some(
        AddressRange {
            start: ImageAddress::new(address),
            end: ImageAddress::new(end),
        },
    )
}

/// Determines the code named by a symbol in the executable section
/// `[section.0, section.1)`.
///
/// A declared extent must lie within the section. An unsized symbol extends
/// to the first evidence of other code: the next code symbol, the next
/// unwind-table function, or the section end. An unwind-table function that
/// begins at the symbol also bounds it by its own end.
fn extent(
    address: u64,
    size: u64,
    (section_start, section_end): (u64, u64),
    code_starts: &BTreeSet<u64>,
    unwind_ends: &BTreeMap<u64, u64>,
) -> Option<SymbolExtent> {
    if address < section_start || address >= section_end {
        return None;
    }

    let (end, provenance) = if size == 0 {
        let after = address.checked_add(1)?;
        let mut end = section_end;
        end = end.min(code_starts.range(after..).next().copied().unwrap_or(end));
        end = end.min(
            unwind_ends
                .range(after..)
                .next()
                .map_or(end, |(start, _)| *start),
        );
        end = end.min(unwind_ends.get(&address).copied().unwrap_or(end));
        (end, SymbolExtentProvenance::Inferred)
    } else {
        let end = address.checked_add(size)?;
        if end > section_end {
            return None;
        }

        (end, SymbolExtentProvenance::Declared)
    };

    (address < end).then(|| SymbolExtent {
        range: AddressRange {
            start: ImageAddress::new(address),
            end: ImageAddress::new(end),
        },
        provenance,
    })
}

/// Normalizes the symbol tables of either a raw ELF file or an object
/// synthesized from structured input, then checks catalog and lookup
/// invariants at every symbol boundary and at input-chosen addresses.
#[cfg(feature = "fuzzing")]
#[allow(
    clippy::expect_used,
    reason = "a fuzz harness reports a bug by panicking"
)]
pub(super) fn fuzz(data: &[u8]) {
    let (bytes, unwind) = if data.starts_with(b"\x7fELF") {
        (data.to_vec(), Vec::new())
    } else {
        fuzz_object(data)
    };

    let Ok(object) = object::File::parse(bytes.as_slice()) else {
        return;
    };

    let table = load_symbols(&object, None, &unwind);
    let sections = image_sections(&object, is_code);
    let storage_sections = image_sections(&object, holds_storage);

    for (index, symbol) in table.symbols.iter().enumerate() {
        assert_eq!(
            symbol.id,
            SymbolId::new(u32::try_from(index).expect("count"))
        );
        let _ = crate::demangle::demangle(&symbol.name);

        if let Some(storage) = symbol.storage {
            assert_eq!(symbol.kind, SymbolKind::Data);
            assert_eq!(storage.start, symbol.address);
            assert!(storage.start <= storage.end);
            assert!(storage_sections.iter().any(|section| {
                section.start <= storage.start.get() && storage.end.get() <= section.end
            }));
        }

        let Some(extent) = symbol.extent else {
            continue;
        };

        assert!(
            matches!(
                symbol.kind,
                SymbolKind::Function | SymbolKind::IndirectFunction
            ) || symbol.role == crate::CodeRole::Outermost
        );
        assert_eq!(extent.range.start, symbol.address);
        assert!(extent.range.start < extent.range.end);
        assert!(sections.iter().any(|section| {
            section.start <= extent.range.start.get() && extent.range.end.get() <= section.end
        }));
    }

    assert!(
        table.symbols.windows(2).all(|pair| {
            let left = (pair[0].address, &pair[0].name);
            let right = (pair[1].address, &pair[1].name);
            left < right || (left == right && pair[0].name.contains(char::REPLACEMENT_CHARACTER))
        }),
        "the catalog is ordered and only lossily spelled names repeat"
    );

    let symbols = table.symbols.clone();
    let image = crate::ModuleImage::new(
        std::path::PathBuf::from("/fuzz"),
        crate::TargetDescription {
            architecture: crate::Architecture::X86_64,
            byte_order: crate::ByteOrder::Little,
            pointer_width: crate::PointerWidth::Bits64,
        },
        AddressRange {
            start: ImageAddress::new(0),
            end: ImageAddress::new(u64::MAX),
        },
        crate::model::ModuleMetadata {
            symbols: table.symbols,
            symbol_sources: table.sources,
            got_slots: table.got_slots,
            sections: load_sections(&object),
            thread_local_storage: has_thread_local_storage(&object),
            thread_locals: load_thread_locals(&object),
            ..crate::model::ModuleMetadata::default()
        },
    );

    fuzz_lookups(&image, &symbols, data);
}

/// Checks lookups at every extent boundary and at input-chosen addresses: a
/// result contains its address and outranks every other containing extent,
/// and no address inside an extent goes unnamed.
#[cfg(feature = "fuzzing")]
#[allow(
    clippy::expect_used,
    reason = "a fuzz harness reports a bug by panicking"
)]
fn fuzz_lookups(image: &crate::ModuleImage, symbols: &[SymbolInfo], data: &[u8]) {
    let probes = symbols
        .iter()
        .filter_map(|symbol| symbol.extent)
        .flat_map(|extent| {
            let (start, end) = (extent.range.start.get(), extent.range.end.get());
            [start, end - 1, end, start.wrapping_sub(1)]
        })
        .chain(
            data.as_chunks::<8>()
                .0
                .iter()
                .take(64)
                .map(|chunk| u64::from_le_bytes(*chunk) % 0x200),
        );

    for address in probes {
        let address = ImageAddress::new(address);
        let containing = symbols
            .iter()
            .filter(|symbol| {
                symbol
                    .extent
                    .is_some_and(|extent| extent.range.contains(address))
            })
            .collect::<Vec<_>>();

        assert_description(image, symbols, address);
        let Some(found) = image.symbolize(address) else {
            assert!(containing.is_empty(), "an extent contains {address:#x}");
            continue;
        };

        let found = image
            .symbol(found.symbol)
            .expect("found symbols are cataloged");
        let extent = found.extent().expect("found symbols name code");
        assert!(extent.range.contains(address));

        for other in containing {
            let other = other.extent.expect("containing");
            assert!(
                (extent.provenance, std::cmp::Reverse(extent.range.start))
                    <= (other.provenance, std::cmp::Reverse(other.range.start)),
                "{found:?} outranks {other:?} at {address:#x}"
            );
        }
    }
}

/// Checks that a description names its address's section and prefers a
/// code symbol, naming data only through declared storage.
#[cfg(feature = "fuzzing")]
#[allow(
    clippy::expect_used,
    reason = "a fuzz harness reports a bug by panicking"
)]
fn assert_description(image: &crate::ModuleImage, symbols: &[SymbolInfo], address: ImageAddress) {
    let description = image.describe(address);
    assert_eq!(description.address, address);
    if let Some(section) = &description.section {
        let info = image.section(section.section).expect("cataloged section");
        assert!(info.range.contains(address));
        assert_eq!(section.offset, address.get() - info.range.start.get());
    } else {
        assert!(
            image
                .sections()
                .iter()
                .all(|section| !section.range.contains(address))
        );
    }

    let code = image.symbolize(address);
    let names = |symbol: &SymbolInfo| {
        symbol.storage.is_some_and(|storage| {
            storage.contains(address) || (storage.is_empty() && storage.start == address)
        })
    };

    match &description.symbol {
        Some(symbol) if code.is_none() => {
            let symbol = image.symbol(symbol.symbol).expect("cataloged symbol");
            assert!(names(&symbol.info()));
        }
        Some(symbol) => assert_eq!(Some(symbol), code.as_ref()),
        None => assert!(code.is_none() && !symbols.iter().any(names)),
    }
}

/// Synthesizes a relocatable x86-64 object: one byte sizes the executable
/// section, and each following six-byte record defines a symbol or a
/// call-frame entry from a small pool of colliding names.
#[cfg(feature = "fuzzing")]
fn fuzz_object(data: &[u8]) -> (Vec<u8>, Vec<AddressRange<ImageAddress>>) {
    use object::write;

    const NAMES: [&[u8]; 8] = [
        b"a",
        b"b",
        b"_a",
        b"__b",
        b"main",
        b"_ZN3foo3barEv",
        b"\xffa",
        b"\xfea",
    ];
    let mut object = write::Object::new(
        object::BinaryFormat::Elf,
        object::Architecture::X86_64,
        object::Endianness::Little,
    );
    let text = object.add_section(Vec::new(), b".text".to_vec(), object::SectionKind::Text);
    let data_section = object.add_section(Vec::new(), b".data".to_vec(), object::SectionKind::Data);
    let text_size = usize::from(data.first().copied().unwrap_or(0)) * 2;
    object.append_section_data(text, &vec![0xcc; text_size], 1);
    object.append_section_data(data_section, &[0; 16], 1);

    let mut unwind = Vec::new();

    for record in data.get(1..).unwrap_or_default().as_chunks::<6>().0 {
        let value = u64::from(u16::from_le_bytes([record[2], record[3]]) % 0x200);
        let size = u64::from(record[4]) % 0x40;

        if record[0] & 0x80 != 0 {
            if let Some(end) = value.checked_add(size) {
                unwind.push(AddressRange {
                    start: ImageAddress::new(value),
                    end: ImageAddress::new(end),
                });
            }

            continue;
        }

        let st_type = [
            elf::STT_FUNC,
            elf::STT_GNU_IFUNC,
            elf::STT_OBJECT,
            elf::STT_NOTYPE,
            elf::STT_TLS,
        ][usize::from(record[1] % 5)];
        let st_bind =
            [elf::STB_GLOBAL, elf::STB_WEAK, elf::STB_LOCAL][usize::from(record[1] / 5 % 3)];
        let section = match record[0] % 4 {
            0 | 1 => write::SymbolSection::Section(text),
            2 => write::SymbolSection::Section(data_section),
            _ => write::SymbolSection::Absolute,
        };

        object.add_symbol(write::Symbol {
            name: NAMES[usize::from(record[5]) % NAMES.len()].to_vec(),
            value,
            size: if record[0] & 0x40 != 0 {
                u64::MAX - size
            } else {
                size
            },
            kind: object::SymbolKind::Unknown,
            scope: if st_bind == elf::STB_LOCAL {
                object::SymbolScope::Compilation
            } else {
                object::SymbolScope::Dynamic
            },
            weak: st_bind == elf::STB_WEAK,
            section,
            flags: object::SymbolFlags::Elf {
                st_info: (st_bind << 4) | st_type,
                st_other: 0,
            },
        });
    }

    (object.write().unwrap_or_default(), unwind)
}

#[cfg(test)]
mod tests {
    use object::write;
    use object::{Architecture, BinaryFormat, Endianness};

    use super::*;

    /// Each accessor of the TLS fixture reads, by the model its code
    /// uses, the variable the symbols place where the decode does.
    #[test]
    fn accessors_are_decoded_to_their_variables_under_every_tls_model() {
        let fixture = |name: &str| {
            std::fs::read(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("build/test-programs")
                    .join(name),
            )
            .expect("the fixture; run `just build-test-programs`")
        };
        let library = fixture("libtls-accessors.so");
        let program = fixture("tls-accessors");
        for (data, accessor, variable) in [
            (&library, "tls_general_get", "tls_general"),
            (&library, "tls_hidden_get", "tls_hidden"),
            // Local dynamic: the first variable it reads.
            (&library, "tls_local_get", "tls_local_first"),
            (&library, "tls_initial_get", "tls_initial"),
            (&program, "tls_program_get", "tls_program"),
        ] {
            let object = object::File::parse(data.as_slice()).expect("an ELF file");
            let address = object
                .symbols()
                .find(|symbol| symbol.name() == Ok(accessor))
                .expect("the accessor")
                .address();
            let placed = load_thread_locals(&object);
            assert_eq!(
                accessor_thread_local(&object, address),
                placed[variable].clone(),
                "{accessor}"
            );
        }

        // The library's code reaches each variable but the initial-exec one
        // only through its module's block.
        let object = object::File::parse(library.as_slice()).expect("an ELF file");
        let placed = load_thread_locals(&object);
        for (name, offset) in [("tls_general", 0x20), ("tls_hidden", 0x18)] {
            assert_eq!(placed[name], Ok(ThreadLocal::Dynamic(offset)), "{name}");
        }
        assert!(matches!(placed["tls_initial"], Ok(ThreadLocal::Slot(_))));

        // A function that reads no thread-local variable is refused.
        let set = object
            .symbols()
            .find(|symbol| symbol.name() == Ok("tls_set"))
            .expect("tls_set")
            .address();
        assert!(accessor_thread_local(&object, set).is_err());
    }

    /// One symbol of a synthesized relocatable object.
    struct Spec {
        name: &'static [u8],
        value: u64,
        size: u64,
        st_type: u8,
        st_bind: u8,
        section: write::SymbolSection,
    }

    impl Spec {
        fn new(name: &'static str, section: write::SymbolSection) -> Self {
            Self::named(name.as_bytes(), section)
        }

        const fn named(name: &'static [u8], section: write::SymbolSection) -> Self {
            Self {
                name,
                value: 0,
                size: 0,
                st_type: elf::STT_FUNC,
                st_bind: elf::STB_GLOBAL,
                section,
            }
        }

        const fn at(mut self, value: u64, size: u64) -> Self {
            self.value = value;
            self.size = size;
            self
        }

        const fn typed(mut self, st_type: u8, st_bind: u8) -> Self {
            self.st_type = st_type;
            self.st_bind = st_bind;
            self
        }
    }

    struct Sections {
        text: write::SectionId,
        data: write::SectionId,
        tls: write::SectionId,
    }

    /// Writes an x86-64 relocatable object with a 0x40-byte executable
    /// section, a data section, and a thread-local section. Relocatable
    /// sections all begin at address zero.
    fn object(
        architecture: Architecture,
        symbols: impl FnOnce(&Sections) -> Vec<Spec>,
        extra: &[(&str, Vec<u8>)],
    ) -> Vec<u8> {
        let mut object = write::Object::new(BinaryFormat::Elf, architecture, Endianness::Little);
        let sections = Sections {
            text: object.add_section(Vec::new(), b".text".to_vec(), object::SectionKind::Text),
            data: object.add_section(Vec::new(), b".data".to_vec(), object::SectionKind::Data),
            tls: object.add_section(Vec::new(), b".tdata".to_vec(), object::SectionKind::Tls),
        };

        object.append_section_data(sections.text, &[0xcc; 0x40], 16);
        object.append_section_data(sections.data, &[0; 0x10], 8);
        object.append_section_data(sections.tls, &[0; 0x10], 8);

        for (name, data) in extra {
            let section = object.add_section(
                Vec::new(),
                name.as_bytes().to_vec(),
                object::SectionKind::Other,
            );
            object.append_section_data(section, data, 1);
        }

        for spec in symbols(&sections) {
            object.add_symbol(write::Symbol {
                name: spec.name.to_vec(),
                value: spec.value,
                size: spec.size,
                kind: object::SymbolKind::Unknown,
                scope: if spec.st_bind == elf::STB_LOCAL {
                    object::SymbolScope::Compilation
                } else {
                    object::SymbolScope::Dynamic
                },
                weak: spec.st_bind == elf::STB_WEAK,
                section: spec.section,
                flags: object::SymbolFlags::Elf {
                    st_info: (spec.st_bind << 4) | spec.st_type,
                    st_other: 0,
                },
            });
        }

        object.write().expect("write test object")
    }

    fn load(data: &[u8], unwind: &[(u64, u64)]) -> SymbolTable {
        let object = object::File::parse(data).expect("parse test object");
        let unwind = unwind
            .iter()
            .map(|&(start, end)| AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            })
            .collect::<Vec<_>>();

        load_symbols(&object, None, &unwind)
    }

    type Summary<'a> = (&'a str, SymbolKind, SymbolBinding, Option<(u64, u64)>);

    fn summary(table: &SymbolTable) -> Vec<Summary<'_>> {
        table
            .symbols
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_ref(),
                    symbol.kind,
                    symbol.binding,
                    symbol
                        .extent
                        .map(|extent| (extent.range.start.get(), extent.range.end.get())),
                )
            })
            .collect()
    }

    fn xz(data: &[u8]) -> Vec<u8> {
        let mut compressed = Vec::new();
        lzma_rs::xz_compress(&mut io::BufReader::new(data), &mut compressed).expect("compress");
        compressed
    }

    #[test]
    fn only_addressable_symbols_are_cataloged_and_only_trustworthy_code_has_extents() {
        use SymbolBinding::{Global, Local, Weak};
        use SymbolKind::{Data, Function, IndirectFunction, Unknown};
        use write::SymbolSection::{Absolute, Section, Undefined};

        let data = object(
            Architecture::X86_64,
            |sections| {
                let text = Section(sections.text);
                vec![
                    Spec::new("sized", text).at(0x00, 0x10),
                    Spec::new("unsized", text).at(0x10, 0),
                    // A label inside a function is cataloged but names no code.
                    Spec::new("label", text)
                        .at(0x18, 0)
                        .typed(elf::STT_NOTYPE, elf::STB_LOCAL),
                    Spec::new("local", text)
                        .at(0x20, 0x08)
                        .typed(elf::STT_FUNC, elf::STB_LOCAL),
                    Spec::new("resolver", text)
                        .at(0x28, 0x04)
                        .typed(elf::STT_GNU_IFUNC, elf::STB_WEAK),
                    Spec::new("overflowing", text).at(0x30, u64::MAX),
                    Spec::new("past_section", text).at(0x38, 0x10),
                    Spec::new("unsized_last", text).at(0x3c, 0),
                    // A function symbol outside executable code names no code.
                    Spec::new("misplaced", Section(sections.data)).at(0x0, 0x4),
                    Spec::new("object", Section(sections.data))
                        .at(0x8, 0x8)
                        .typed(elf::STT_OBJECT, elf::STB_GNU_UNIQUE),
                    // These have no image address.
                    Spec::new("thread_local", Section(sections.tls))
                        .at(0x8, 0x8)
                        .typed(elf::STT_TLS, elf::STB_GLOBAL),
                    Spec::new("undefined", Undefined).at(0x20, 0),
                    Spec::new("absolute", Absolute)
                        .at(0x20, 0)
                        .typed(elf::STT_OBJECT, elf::STB_GLOBAL),
                    Spec::new("section", text).typed(elf::STT_SECTION, elf::STB_LOCAL),
                    Spec::new("file.c", Absolute).typed(elf::STT_FILE, elf::STB_LOCAL),
                ]
            },
            &[],
        );

        let table = load(&data, &[]);
        assert_eq!(
            summary(&table),
            [
                ("misplaced", Function, Global, None),
                ("sized", Function, Global, Some((0x00, 0x10))),
                ("object", Data, Global, None),
                // Unsized code ends at the next code symbol, not at a label.
                ("unsized", Function, Global, Some((0x10, 0x20))),
                ("label", Unknown, Local, None),
                ("local", Function, Local, Some((0x20, 0x28))),
                ("resolver", IndirectFunction, Weak, Some((0x28, 0x2c))),
                ("overflowing", Function, Global, None),
                ("past_section", Function, Global, None),
                ("unsized_last", Function, Global, Some((0x3c, 0x40))),
            ]
        );

        // Identifiers are dense in address-then-name order.
        assert!(table.symbols.iter().enumerate().all(|(index, symbol)| {
            symbol.id == SymbolId::new(u32::try_from(index).expect("test symbol count"))
        }));
        assert_eq!(
            table.sources,
            SymbolTableSources {
                static_table: true,
                dynamic_table: false,
                embedded_table: EmbeddedSymbolTable::Absent,
                runtime_function_table: EmbeddedSymbolTable::Absent,
            }
        );
        let provenance = |name: &str| {
            table
                .symbols
                .iter()
                .find(|symbol| symbol.name.as_ref() == name)
                .and_then(|symbol| symbol.extent)
                .map(|extent| extent.provenance)
        };

        assert_eq!(provenance("sized"), Some(SymbolExtentProvenance::Declared));
        assert_eq!(
            provenance("unsized"),
            Some(SymbolExtentProvenance::Inferred)
        );

        // Call-frame entries are evidence of function boundaries: one that
        // begins at an unsized symbol ends it, and one that begins later caps
        // it even where no symbol names that code.
        let table = load(&data, &[(0x10, 0x14), (0x3c, 0x3e)]);
        assert_eq!(summary(&table)[3].3, Some((0x10, 0x14)));
        assert_eq!(summary(&table)[9].3, Some((0x3c, 0x3e)));
        let table = load(&data, &[(0x0, 0x10), (0x18, 0x20)]);
        assert_eq!(summary(&table)[3].3, Some((0x10, 0x18)));
    }

    /// A process's entry point may be a bare label, as glibc's dynamic
    /// loader's `_start` is, with no type, size, or call-frame entry. It
    /// still names the entry code, up to the next function, so a stack of
    /// a program starting up ends there complete. Other labels still name
    /// no code.
    #[test]
    fn an_untyped_entry_point_names_its_code() {
        use write::SymbolSection::Section;

        let data = object(
            Architecture::X86_64,
            |sections| {
                let text = Section(sections.text);
                vec![
                    Spec::new("_start", text)
                        .at(0x00, 0)
                        .typed(elf::STT_NOTYPE, elf::STB_LOCAL),
                    Spec::new("_dl_start_user", text)
                        .at(0x08, 0)
                        .typed(elf::STT_NOTYPE, elf::STB_LOCAL),
                    Spec::new("_dl_start", text)
                        .at(0x20, 0x10)
                        .typed(elf::STT_FUNC, elf::STB_LOCAL),
                ]
            },
            &[],
        );

        let table = load(&data, &[]);
        assert_eq!(
            summary(&table),
            [
                (
                    "_start",
                    SymbolKind::Unknown,
                    SymbolBinding::Local,
                    Some((0x00, 0x20))
                ),
                (
                    "_dl_start_user",
                    SymbolKind::Unknown,
                    SymbolBinding::Local,
                    None
                ),
                (
                    "_dl_start",
                    SymbolKind::Function,
                    SymbolBinding::Local,
                    Some((0x20, 0x30))
                ),
            ]
        );
        assert_eq!(table.symbols[0].role, crate::CodeRole::Outermost);
    }

    #[test]
    fn data_symbols_name_storage_only_within_allocated_non_thread_local_sections() {
        use write::SymbolSection::Section;

        let mut object =
            write::Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = object.add_section(Vec::new(), b".text".to_vec(), object::SectionKind::Text);
        let data = object.add_section(Vec::new(), b".data".to_vec(), object::SectionKind::Data);
        let tdata = object.add_section(Vec::new(), b".tdata".to_vec(), object::SectionKind::Tls);
        let tbss = object.add_section(
            Vec::new(),
            b".tbss".to_vec(),
            object::SectionKind::UninitializedTls,
        );
        let note = object.add_section(Vec::new(), b".note".to_vec(), object::SectionKind::Other);
        object.append_section_data(text, &[0xcc; 0x10], 16);
        object.append_section_data(data, &[0; 0x10], 8);
        object.append_section_data(tdata, &[0; 0x10], 8);
        object.append_section_bss(tbss, 0x10, 8);
        object.append_section_data(note, &[0; 0x10], 1);

        for (name, section, value, size) in [
            ("object", data, 0x8, 0x8),
            ("unsized", data, 0x4, 0),
            ("overflowing", data, 0xc, 0x8),
            ("at_end", data, 0x10, 0),
            ("thread_template", tdata, 0x0, 0x8),
            ("unallocated", note, 0x0, 0x8),
        ] {
            object.add_symbol(write::Symbol {
                name: name.as_bytes().to_vec(),
                value,
                size,
                kind: object::SymbolKind::Unknown,
                scope: object::SymbolScope::Dynamic,
                weak: false,
                section: Section(section),
                flags: object::SymbolFlags::Elf {
                    st_info: (elf::STB_GLOBAL << 4) | elf::STT_OBJECT,
                    st_other: 0,
                },
            });
        }

        let bytes = object.write().expect("write test object");
        let object = object::File::parse(bytes.as_slice()).expect("parse test object");

        let storage = load_symbols(&object, None, &[])
            .symbols
            .into_iter()
            .map(|symbol| {
                (
                    symbol.name.to_string(),
                    symbol
                        .storage
                        .map(|range| (range.start.get(), range.end.get())),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let expected = [
            ("at_end", None),
            ("object", Some((0x8, 0x10))),
            ("overflowing", None),
            ("thread_template", None),
            ("unallocated", None),
            ("unsized", Some((0x4, 0x4))),
        ]
        .map(|(name, range)| (name.to_owned(), range));
        assert_eq!(storage, BTreeMap::from(expected));

        // Relocatable sections all begin at zero; only the thread-local
        // template that occupies no addresses and the unallocated note are
        // left out.
        let sections = load_sections(&object)
            .into_iter()
            .map(|section| {
                (
                    section.name.to_string(),
                    section.range.end.get(),
                    section.executable,
                    section.writable,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sections,
            [
                (".data".to_owned(), 0x10, false, true),
                (".tdata".to_owned(), 0x10, false, true),
                (".text".to_owned(), 0x10, true, false),
            ]
        );
    }

    #[test]
    fn names_that_are_not_utf8_stay_distinct_when_their_spellings_collide() {
        use SymbolBinding::{Global, Local};
        use SymbolKind::Function;
        use write::SymbolSection::Section;

        let data = object(
            Architecture::X86_64,
            |sections| {
                vec![
                    Spec::named(b"\xffa", Section(sections.text))
                        .at(0x0, 0x10)
                        .typed(elf::STT_FUNC, elf::STB_LOCAL),
                    Spec::named(b"\xfea", Section(sections.text)).at(0x0, 0x10),
                ]
            },
            &[],
        );
        assert_eq!(
            summary(&load(&data, &[])),
            [
                ("\u{fffd}a", Function, Global, Some((0x0, 0x10))),
                ("\u{fffd}a", Function, Local, Some((0x0, 0x10))),
            ]
        );
    }

    #[test]
    fn embedded_symbol_tables_merge_or_explain_why_they_are_unusable() {
        let embedded_text = |name: &'static str, architecture| {
            object(
                architecture,
                |sections| {
                    vec![
                        Spec::new(name, write::SymbolSection::Section(sections.text))
                            .at(0x20, 0x08)
                            .typed(elf::STT_FUNC, elf::STB_LOCAL),
                    ]
                },
                &[],
            )
        };

        let with_embedded = |payload: Vec<u8>| {
            object(
                Architecture::X86_64,
                |sections| {
                    vec![
                        Spec::new("exported", write::SymbolSection::Section(sections.text))
                            .at(0x0, 0x10),
                    ]
                },
                &[(".gnu_debugdata", payload)],
            )
        };

        let table = load(
            &with_embedded(xz(&embedded_text("hidden", Architecture::X86_64))),
            &[],
        );
        assert_eq!(table.sources.embedded_table, EmbeddedSymbolTable::Loaded);
        assert_eq!(
            summary(&table),
            [
                (
                    "exported",
                    SymbolKind::Function,
                    SymbolBinding::Global,
                    Some((0x0, 0x10))
                ),
                (
                    "hidden",
                    SymbolKind::Function,
                    SymbolBinding::Local,
                    Some((0x20, 0x28))
                ),
            ]
        );

        let unusable = |payload: Vec<u8>| {
            let table = load(&with_embedded(payload), &[]);
            assert_eq!(
                table.symbols.len(),
                1,
                "only the image's own symbol remains"
            );
            match table.sources.embedded_table {
                EmbeddedSymbolTable::Unusable { reason } => reason,
                other => panic!("expected an unusable table, got {other:?}"),
            }
        };

        assert!(unusable(b"not xz".to_vec()).contains("xz stream"));
        assert!(unusable(xz(b"not an object")).contains("malformed"));
        assert!(
            unusable(xz(&embedded_text("foreign", Architecture::Aarch64))).contains("architecture")
        );

        let large = vec![0_u8; 0x1000];
        assert!(decompress(&xz(&large), large.len()).is_ok());
        assert!(
            decompress(&xz(&large), large.len() - 1)
                .expect_err("over the limit")
                .contains("exceeds")
        );
    }

    /// Each instruction of a PLT stub finds pushed what the stub pushed
    /// before it, whichever linker wrote it; code no stub runs is no stub.
    #[test]
    fn plt_stubs_count_the_words_they_push() {
        let frames = |code: &[u8]| {
            let mut frames = Vec::new();
            stub_frames(code, 0x1000, &mut frames);
            frames
                .into_iter()
                .map(|(range, pushed)| {
                    (range.start.get() - 0x1000, range.end.get() - 0x1000, pushed)
                })
                .collect::<Vec<_>>()
        };

        let jump_through_got = [0xff, 0x25, 0, 0, 0, 0];
        let push_index = [0x68, 1, 0, 0, 0];
        let jump_to_header = [0xe9, 0, 0, 0, 0];
        let endbr64 = [0xf3, 0x0f, 0x1e, 0xfa];

        // GNU ld and lld: through the GOT, or before binding on to the
        // header with the stub's index pushed.
        assert_eq!(
            frames(&[&jump_through_got[..], &push_index, &jump_to_header].concat()),
            [(0, 6, 0), (6, 11, 0), (11, 16, 1)]
        );

        // GNU ld's lazy half of an IBT stub, and its `.plt.sec` half.
        assert_eq!(
            frames(&[&endbr64[..], &push_index, &[0xf2], &jump_to_header].concat()),
            [(0, 4, 0), (4, 9, 0), (9, 15, 1)]
        );
        assert_eq!(
            frames(&[&endbr64[..], &[0xf2], &jump_through_got, &[0x66, 0x90]].concat()),
            [(0, 4, 0), (4, 11, 0), (11, 13, 0)]
        );

        // mold: the index in r11, and padding no stub runs.
        assert_eq!(
            frames(&[&[0x41, 0xbb, 1, 0, 0, 0][..], &jump_through_got, &[0xcc; 4]].concat()),
            [(0, 6, 0), (6, 12, 0)]
        );

        // `.plt.got`.
        assert_eq!(
            frames(&[&jump_through_got[..], &[0x66, 0x90]].concat()),
            [(0, 6, 0), (6, 8, 0)]
        );

        // What moves the stack otherwise, or never jumps, is no stub.
        assert_eq!(
            frames(&[&[0x48, 0x83, 0xec, 0x08][..], &jump_through_got].concat()),
            []
        );
        assert_eq!(frames(&[&[0x41, 0x53][..], &jump_through_got].concat()), []);
        assert_eq!(frames(&[&endbr64[..], &[0xcc; 12]].concat()), []);
        assert_eq!(
            frames(&[&push_index[..], &push_index, &push_index, &jump_to_header].concat()),
            [],
            "more pushes than any stub makes"
        );
    }

    /// A malformed section whose code runs past the last address holds no
    /// stub, rather than overflowing the address of one.
    #[test]
    fn a_plt_past_the_last_address_has_no_stubs() {
        // endbr64; jmp *0(%rip); nopw 0(%rax,%rax): one 16-byte stub.
        const STUB: [u8; 16] = [
            0xf3, 0x0f, 0x1e, 0xfa, 0xff, 0x25, 0, 0, 0, 0, 0x66, 0x0f, 0x1f, 0x44, 0, 0,
        ];
        let mut data = object(
            Architecture::X86_64,
            |_| Vec::new(),
            &[(".plt.sec", STUB.repeat(2))],
        );
        let index = object::File::parse(&*data)
            .expect("parse test object")
            .section_by_name(".plt.sec")
            .expect("the stubs' section")
            .index()
            .0;
        let word = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().expect("a word"));
        let headers = usize::try_from(word(0x28)).expect("a header offset");
        let address = headers + index * 64 + 0x10;
        data[address..address + 8].copy_from_slice(&(u64::MAX - 8).to_le_bytes());

        let object = object::File::parse(&*data).expect("parse test object");
        assert_eq!(plt_frames(&object, |_| true), []);
    }
}
