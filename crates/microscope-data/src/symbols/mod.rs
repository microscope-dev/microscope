//! ELF symbol extraction, debug-only stripping, and compact `SymbolIndex` encoding.
//!
//! All parsing and decoding runs under an explicit `ParseBudget` so untrusted
//! binaries and symbol indexes cannot exhaust memory or CPU on a symbol server.

mod extract;
mod index;
mod parse;
#[cfg(test)]
mod tests;

use std::fmt;
use std::fs::File;
use std::io::{self, SeekFrom};
use std::path::{Component, Path};
use std::time::Duration;

use object::{
    Endian as _, Endianness, Object as _, ObjectSection as _, ObjectSymbolTable as _, elf,
};
use serde::{Deserialize, Serialize};

pub use extract::strip_to_debug_elf;
pub use index::{
    INDEX_MAGIC, INDEX_VERSION, LineSpan, ResolvedFrame, SymbolFunction, SymbolIndex, SymbolSpan,
};
pub use parse::parse_elf;

const CPP_RECURSION_LIMIT: u32 = 96;
const MIN_BUILD_ID_HEX_LEN: usize = 4;
const MAX_BUILD_ID_HEX_LEN: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BuildId(String);

impl BuildId {
    pub fn parse(raw: &str) -> Result<Self, SymbolError> {
        let trimmed = raw.trim();
        let len = trimmed.len();
        if !(MIN_BUILD_ID_HEX_LEN..=MAX_BUILD_ID_HEX_LEN).contains(&len) || !len.is_multiple_of(2) {
            return Err(SymbolError::InvalidBuildId);
        }
        if !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SymbolError::InvalidBuildId);
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SymbolError> {
        let min_bytes = MIN_BUILD_ID_HEX_LEN / 2;
        let max_bytes = MAX_BUILD_ID_HEX_LEN / 2;
        if !(min_bytes..=max_bytes).contains(&bytes.len()) {
            return Err(SymbolError::InvalidBuildId);
        }
        let mut hex = String::with_capacity(bytes.len() * 2);
        for &byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        Ok(Self(hex))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let bytes = self.0.as_bytes();
        let mut out = Vec::with_capacity(bytes.len() / 2);
        for pair in bytes.as_chunks::<2>().0 {
            let hi = hex_nibble(pair[0]).unwrap_or(0);
            let lo = hex_nibble(pair[1]).unwrap_or(0);
            out.push((hi << 4) | lo);
        }
        out
    }
}

impl fmt::Display for BuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[must_use]
pub fn is_placeholder_symbol_name(name: &str) -> bool {
    name.starts_with("<unnamed ") && name.ends_with('>')
}

/// Hard resource limits enforced during ELF parsing and `SymbolIndex` decoding.
///
/// Guards against decompression bombs, cyclic DWARF DIE graphs, and oversized
/// symbol tables on both servers (`SERVER`) and fuzzers (`FUZZ`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseBudget {
    pub max_input_bytes: usize,
    pub max_decompressed_bytes: u64,
    pub max_decompression_ratio: u64,
    pub max_functions: usize,
    pub max_spans: usize,
    pub max_line_spans: usize,
    pub max_files: usize,
    pub max_string_bytes: usize,
    pub max_inline_depth: usize,
    pub max_units: usize,
    pub max_dies_per_unit: usize,
    pub max_abbrev_bytes: usize,
    pub max_parse_time: Duration,
}

impl ParseBudget {
    pub const SERVER: Self = Self {
        max_input_bytes: 512 * 1024 * 1024,
        max_decompressed_bytes: 512 * 1024 * 1024,
        max_decompression_ratio: 64,
        max_functions: 2_000_000,
        max_spans: 4_000_000,
        max_line_spans: 8_000_000,
        max_files: 500_000,
        max_string_bytes: 128 * 1024 * 1024,
        max_inline_depth: 64,
        max_units: 200_000,
        max_dies_per_unit: 2_000_000,
        max_abbrev_bytes: 16 * 1024 * 1024,
        max_parse_time: Duration::from_mins(1),
    };

    pub const FUZZ: Self = Self {
        max_input_bytes: 2 * 1024 * 1024,
        max_decompressed_bytes: 4 * 1024 * 1024,
        max_decompression_ratio: 32,
        max_functions: 32_768,
        max_spans: 65_536,
        max_line_spans: 65_536,
        max_files: 16_384,
        max_string_bytes: 4 * 1024 * 1024,
        max_inline_depth: 32,
        max_units: 1_024,
        max_dies_per_unit: 32_768,
        max_abbrev_bytes: 1024 * 1024,
        max_parse_time: Duration::from_secs(1),
    };
}

impl Default for ParseBudget {
    fn default() -> Self {
        Self::SERVER
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMetadata {
    pub build_id: BuildId,
    pub arch: String,
    pub has_dwarf: bool,
    pub has_symtab: bool,
    pub has_gopclntab: bool,
    pub symbol_count: u32,
    pub range_count: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum SymbolError {
    #[error("invalid build id")]
    InvalidBuildId,
    #[error("not a supported ELF file: {0}")]
    NotElf(String),
    #[error("ELF file has no GNU build id")]
    MissingBuildId,
    #[error("ELF file exceeds parse budget: {0}")]
    BudgetExceeded(String),
    #[error("compressed debug section failed: {0}")]
    Decompress(String),
    #[error("malformed symbol index: {0}")]
    MalformedIndex(String),
    #[error("symbol index checksum mismatch")]
    ChecksumMismatch,
}

pub fn extract_build_id(bytes: &[u8]) -> Result<BuildId, SymbolError> {
    let file = object::File::parse(bytes).map_err(|e| SymbolError::NotElf(e.to_string()))?;
    if !matches!(file, object::File::Elf32(_) | object::File::Elf64(_)) {
        return Err(SymbolError::NotElf("expected ELF32 or ELF64".to_owned()));
    }
    let raw = file
        .build_id()
        .map_err(|e| SymbolError::NotElf(e.to_string()))?
        .ok_or(SymbolError::MissingBuildId)?;
    BuildId::from_bytes(raw)
}

#[must_use]
pub fn has_dwarf_sections(bytes: &[u8]) -> bool {
    let Ok(file) = object::File::parse(bytes) else {
        return false;
    };
    if !matches!(file, object::File::Elf32(_) | object::File::Elf64(_)) {
        return false;
    }
    file.section_by_name(".debug_info")
        .or_else(|| file.section_by_name(".zdebug_info"))
        .is_some_and(|s| s.size() > 0)
}

#[must_use]
pub fn has_debug_or_symtab(bytes: &[u8]) -> bool {
    let Ok(file) = object::File::parse(bytes) else {
        return false;
    };
    if !matches!(file, object::File::Elf32(_) | object::File::Elf64(_)) {
        return false;
    }
    let has_dwarf = file
        .section_by_name(".debug_info")
        .or_else(|| file.section_by_name(".zdebug_info"))
        .is_some_and(|s| s.size() > 0);
    if has_dwarf {
        return true;
    }
    if file
        .symbol_table()
        .is_some_and(|t| t.symbols().next().is_some())
    {
        return true;
    }
    file.section_by_name(".gopclntab")
        .or_else(|| file.section_by_name(".data.rel.ro.gopclntab"))
        .is_some_and(|s| s.size() > 0)
}

#[must_use]
pub fn is_public_system_path(path: &str) -> bool {
    const SYSTEM_PREFIXES: [&str; 10] = [
        "/lib/",
        "/lib64/",
        "/usr/lib/",
        "/usr/lib64/",
        "/usr/libexec/",
        "/usr/bin/",
        "/usr/sbin/",
        "/bin/",
        "/sbin/",
        "/nix/store/",
    ];
    let trimmed = path.trim();
    if trimmed.is_empty() || !trimmed.starts_with('/') || trimmed.contains('\0') {
        return false;
    }
    if Path::new(trimmed)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return false;
    }
    SYSTEM_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

const MAX_NOTE_SCAN_BYTES: u64 = 64 * 1024;
const MAX_ELF_TABLE_ENTRIES: usize = 4096;

pub fn read_elf_build_id_from_file(path: &Path) -> io::Result<Option<BuildId>> {
    let mut file = File::open(path)?;
    read_elf_build_id_from_reader(&mut file)
}

/// Extracts the GNU build ID by seeking only to ELF headers and `PT_NOTE`/`SHT_NOTE` regions.
///
/// Avoids mapping or reading multi-gigabyte binaries just to check whether a
/// symbol server already has their symbols.
#[expect(
    clippy::too_many_lines,
    reason = "handles ELF32/64 program-header and section-header note tables in one streaming pass"
)]
pub fn read_elf_build_id_from_reader<R: io::Read + io::Seek>(
    reader: &mut R,
) -> io::Result<Option<BuildId>> {
    let mut ehdr = [0_u8; 64];
    let n = read_prefix(reader, &mut ehdr)?;
    if n < 52 || ehdr[..4] != [0x7f, b'E', b'L', b'F'] {
        return Ok(None);
    }
    let class = ehdr[4];
    let endian = match ehdr[5] {
        elf::ELFDATA2LSB => Endianness::Little,
        elf::ELFDATA2MSB => Endianness::Big,
        _ => return Ok(None),
    };

    let (phoff, phentsize, phnum, shoff, shentsize, shnum) = match class {
        elf::ELFCLASS64 => {
            if n < 64 {
                return Ok(None);
            }
            (
                endian.read_u64(ehdr[32..40].try_into().unwrap_or([0; 8])),
                usize::from(endian.read_u16(ehdr[54..56].try_into().unwrap_or([0; 2]))),
                usize::from(endian.read_u16(ehdr[56..58].try_into().unwrap_or([0; 2]))),
                endian.read_u64(ehdr[40..48].try_into().unwrap_or([0; 8])),
                usize::from(endian.read_u16(ehdr[58..60].try_into().unwrap_or([0; 2]))),
                usize::from(endian.read_u16(ehdr[60..62].try_into().unwrap_or([0; 2]))),
            )
        }
        elf::ELFCLASS32 => (
            u64::from(endian.read_u32(ehdr[28..32].try_into().unwrap_or([0; 4]))),
            usize::from(endian.read_u16(ehdr[42..44].try_into().unwrap_or([0; 2]))),
            usize::from(endian.read_u16(ehdr[44..46].try_into().unwrap_or([0; 2]))),
            u64::from(endian.read_u32(ehdr[32..36].try_into().unwrap_or([0; 4]))),
            usize::from(endian.read_u16(ehdr[46..48].try_into().unwrap_or([0; 2]))),
            usize::from(endian.read_u16(ehdr[48..50].try_into().unwrap_or([0; 2]))),
        ),
        _ => return Ok(None),
    };

    let mut remaining_budget = MAX_NOTE_SCAN_BYTES;
    if phoff > 0 && phnum > 0 && phnum <= MAX_ELF_TABLE_ENTRIES {
        let min_ent = if class == elf::ELFCLASS64 { 56 } else { 32 };
        if phentsize >= min_ent && phentsize <= 256 {
            let mut ph_buf = vec![0_u8; phnum * phentsize];
            if seek_read_exact(reader, phoff, &mut ph_buf).is_ok() {
                for entry in ph_buf.chunks_exact(phentsize) {
                    let p_type = endian.read_u32(entry[0..4].try_into().unwrap_or([0; 4]));
                    if p_type != elf::PT_NOTE {
                        continue;
                    }
                    let (offset, filesz, align) = if class == elf::ELFCLASS64 {
                        (
                            endian.read_u64(entry[8..16].try_into().unwrap_or([0; 8])),
                            endian.read_u64(entry[32..40].try_into().unwrap_or([0; 8])),
                            endian.read_u64(entry[48..56].try_into().unwrap_or([0; 8])),
                        )
                    } else {
                        (
                            u64::from(endian.read_u32(entry[4..8].try_into().unwrap_or([0; 4]))),
                            u64::from(endian.read_u32(entry[16..20].try_into().unwrap_or([0; 4]))),
                            u64::from(endian.read_u32(entry[28..32].try_into().unwrap_or([0; 4]))),
                        )
                    };
                    if let Some(id) = scan_note_region(
                        reader,
                        offset,
                        filesz,
                        align,
                        endian,
                        &mut remaining_budget,
                    ) {
                        return Ok(Some(id));
                    }
                }
            }
        }
    }

    if shoff > 0 && shnum > 0 && shnum <= MAX_ELF_TABLE_ENTRIES {
        let min_ent = if class == elf::ELFCLASS64 { 64 } else { 40 };
        if shentsize >= min_ent && shentsize <= 256 {
            let mut sh_buf = vec![0_u8; shnum * shentsize];
            if seek_read_exact(reader, shoff, &mut sh_buf).is_ok() {
                for entry in sh_buf.chunks_exact(shentsize) {
                    let sh_type = endian.read_u32(entry[4..8].try_into().unwrap_or([0; 4]));
                    if sh_type != elf::SHT_NOTE {
                        continue;
                    }
                    let (offset, size, align) = if class == elf::ELFCLASS64 {
                        (
                            endian.read_u64(entry[24..32].try_into().unwrap_or([0; 8])),
                            endian.read_u64(entry[32..40].try_into().unwrap_or([0; 8])),
                            endian.read_u64(entry[48..56].try_into().unwrap_or([0; 8])),
                        )
                    } else {
                        (
                            u64::from(endian.read_u32(entry[16..20].try_into().unwrap_or([0; 4]))),
                            u64::from(endian.read_u32(entry[20..24].try_into().unwrap_or([0; 4]))),
                            u64::from(endian.read_u32(entry[32..36].try_into().unwrap_or([0; 4]))),
                        )
                    };
                    if let Some(id) =
                        scan_note_region(reader, offset, size, align, endian, &mut remaining_budget)
                    {
                        return Ok(Some(id));
                    }
                }
            }
        }
    }

    Ok(None)
}

fn read_prefix<R: io::Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn seek_read_exact<R: io::Read + io::Seek>(
    reader: &mut R,
    offset: u64,
    buf: &mut [u8],
) -> io::Result<()> {
    reader.seek(SeekFrom::Start(offset))?;
    reader.read_exact(buf)
}

fn scan_note_region<R: io::Read + io::Seek>(
    reader: &mut R,
    offset: u64,
    size: u64,
    align: u64,
    endian: Endianness,
    budget: &mut u64,
) -> Option<BuildId> {
    if size == 0 || *budget == 0 {
        return None;
    }
    let to_read = size.min(*budget);
    let len = usize::try_from(to_read).ok()?;
    *budget = budget.saturating_sub(to_read);
    let mut buf = vec![0_u8; len];
    if seek_read_exact(reader, offset, &mut buf).is_err() {
        return None;
    }
    let step = if align == 8 { 8_usize } else { 4_usize };
    let mut at = 0_usize;
    while at.saturating_add(12) <= buf.len() {
        let namesz = endian.read_u32(buf[at..at + 4].try_into().ok()?) as usize;
        let descsz = endian.read_u32(buf[at + 4..at + 8].try_into().ok()?) as usize;
        let n_type = endian.read_u32(buf[at + 8..at + 12].try_into().ok()?);
        let name_start = at + 12;
        let name_end = name_start.checked_add(namesz)?;
        let desc_start = align_up_note(name_end, step)?;
        let desc_end = desc_start.checked_add(descsz)?;
        if desc_end > buf.len() {
            break;
        }
        if n_type == elf::NT_GNU_BUILD_ID && buf.get(name_start..name_end) == Some(b"GNU\0") {
            let desc = buf.get(desc_start..desc_end)?;
            if let Ok(bid) = BuildId::from_bytes(desc) {
                return Some(bid);
            }
        }
        at = align_up_note(desc_end, step)?;
    }
    None
}

const fn align_up_note(value: usize, align: usize) -> Option<usize> {
    let rem = value % align;
    if rem == 0 {
        Some(value)
    } else {
        value.checked_add(align - rem)
    }
}

#[must_use]
pub fn demangle_symbol(raw: &str) -> String {
    if let Ok(demangled) = rustc_demangle::try_demangle(raw) {
        return format!("{demangled:#}");
    }
    if raw.starts_with("_Z")
        && let Ok(sym) = cpp_demangle::Symbol::new_with_options(
            raw,
            &cpp_demangle::ParseOptions::default().recursion_limit(CPP_RECURSION_LIMIT),
        )
        && let Ok(demangled) = sym.demangle_with_options(
            &cpp_demangle::DemangleOptions::new().recursion_limit(CPP_RECURSION_LIMIT),
        )
    {
        return demangled;
    }
    raw.to_owned()
}

pub fn zstd_compress(bytes: &[u8], level: i32) -> Result<Vec<u8>, SymbolError> {
    zstd::bulk::compress(bytes, level).map_err(|e| SymbolError::Decompress(e.to_string()))
}

pub fn zstd_decompress_bounded(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>, SymbolError> {
    use std::io::Read as _;
    let decoder = zstd::stream::read::Decoder::new(bytes)
        .map_err(|e| SymbolError::Decompress(e.to_string()))?;
    let mut out = Vec::new();
    let cap = (max_bytes as u64).saturating_add(1);
    decoder
        .take(cap)
        .read_to_end(&mut out)
        .map_err(|e| SymbolError::Decompress(e.to_string()))?;
    if out.len() > max_bytes {
        return Err(SymbolError::BudgetExceeded(
            "decompressed zstd payload exceeded limit".to_owned(),
        ));
    }
    Ok(out)
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_symbol_input(data: &[u8]) {
    let _ = extract_build_id(data);
    let _ = has_debug_or_symtab(data);
    if let Ok((index, _meta)) = parse_elf(data, &ParseBudget::FUZZ) {
        let encoded = index.encode();
        if let Ok(decoded) = SymbolIndex::decode(&encoded, &ParseBudget::FUZZ) {
            assert_eq!(index.build_id(), decoded.build_id());
            assert_eq!(index.functions().len(), decoded.functions().len());
            assert_eq!(index.spans().len(), decoded.spans().len());
            for span in decoded.spans().iter().take(64) {
                let _ = decoded.lookup(span.start);
            }
        }
    }
    let _ = SymbolIndex::decode(data, &ParseBudget::FUZZ);
    let _ = strip_to_debug_elf(data);
}
