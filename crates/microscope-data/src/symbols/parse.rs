use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;

use gimli::{
    AttributeValue, DebuggingInformationEntry, Dwarf, EndianSlice, RunTimeEndian, SectionId, Unit,
    UnitOffset,
};
use object::{
    Architecture, CompressedData, CompressionFormat, Object as _, ObjectSection as _,
    ObjectSymbol as _, SectionKind, SymbolKind, SymbolSection,
};

use super::index::{IndexBuilder, InlineFrame, LineSpan, NO_INDEX, SymbolSpan, SymtabSpan};
use super::{BuildId, ParseBudget, SymbolError, SymbolIndex, SymbolMetadata, demangle_symbol};

const SMALL_DECOMPRESS_CLAIM: u64 = 1024 * 1024;
const MAX_MINI_DEBUGDATA_BYTES: usize = 32 * 1024 * 1024;
const GO120_MAGIC: u32 = 0xffff_fff1;
const GO_HEADER_SIZE: usize = 8 + 8 * 8;
const GO_FUNC_SIZE: usize = 44;

type DwarfReader<'a> = EndianSlice<'a, RunTimeEndian>;

#[derive(Debug, Clone)]
struct RawRange {
    start: u64,
    end: u64,
    depth: u32,
    order: u32,
    chain: Vec<InlineFrame>,
}

pub fn parse_elf(
    bytes: &[u8],
    budget: &ParseBudget,
) -> Result<(SymbolIndex, SymbolMetadata), SymbolError> {
    if bytes.len() > budget.max_input_bytes {
        return Err(SymbolError::BudgetExceeded(
            "ELF file exceeds max input bytes".to_owned(),
        ));
    }

    let file = object::File::parse(bytes).map_err(|e| SymbolError::NotElf(e.to_string()))?;
    if !matches!(file, object::File::Elf32(_) | object::File::Elf64(_)) {
        return Err(SymbolError::NotElf("expected ELF32 or ELF64".to_owned()));
    }

    let raw_build_id = file
        .build_id()
        .map_err(|e| SymbolError::NotElf(e.to_string()))?
        .ok_or(SymbolError::MissingBuildId)?;
    let build_id = BuildId::from_bytes(raw_build_id)?;
    let arch = arch_name(file.architecture()).to_owned();

    let mut builder = IndexBuilder::new(*budget);
    let mut decompressed_used = 0_u64;

    let mut raw_symtab: Vec<(u64, u64, u32)> = Vec::new();
    let has_symtab = collect_elf_symbols(&file, &mut builder, &mut raw_symtab, budget)?;

    if let Some(section) = file.section_by_name(".gnu_debugdata")
        && let Ok(compressed) = section.data()
        && !compressed.is_empty()
    {
        let _ = collect_mini_debugdata(
            compressed,
            &mut builder,
            &mut raw_symtab,
            &mut decompressed_used,
            budget,
        );
    }

    let mut line_spans = Vec::new();
    let mut dwarf_ranges = Vec::new();
    let has_dwarf = collect_dwarf(
        &file,
        &mut builder,
        &mut dwarf_ranges,
        &mut line_spans,
        &mut decompressed_used,
        budget,
    )?;

    let has_gopclntab = collect_gopclntab(
        &file,
        &mut builder,
        &mut raw_symtab,
        &mut decompressed_used,
        budget,
    )?;

    let (inlines, spans) = flatten_dwarf_spans(&dwarf_ranges, budget)?;
    let line_spans = normalize_line_spans(line_spans, budget);
    let symtab_spans = normalize_symtab_spans(raw_symtab, budget);

    let index = builder.finish(
        build_id.clone(),
        arch.clone(),
        has_dwarf,
        has_symtab,
        has_gopclntab,
        inlines,
        spans,
        line_spans,
        symtab_spans,
    );

    let symbol_count = u32::try_from(index.functions().len()).unwrap_or(u32::MAX);
    let range_count = u32::try_from(
        index
            .spans()
            .len()
            .saturating_add(index.symtab_spans().len()),
    )
    .unwrap_or(u32::MAX);

    let meta = SymbolMetadata {
        build_id,
        arch,
        has_dwarf,
        has_symtab,
        has_gopclntab,
        symbol_count,
        range_count,
    };

    Ok((index, meta))
}

const fn arch_name(arch: Architecture) -> &'static str {
    match arch {
        Architecture::X86_64 => "x86_64",
        Architecture::Aarch64 => "aarch64",
        Architecture::I386 => "i386",
        Architecture::Arm => "arm",
        Architecture::Riscv64 => "riscv64",
        _ => "unknown",
    }
}

fn collect_elf_symbols(
    file: &object::File<'_>,
    builder: &mut IndexBuilder,
    out: &mut Vec<(u64, u64, u32)>,
    budget: &ParseBudget,
) -> Result<bool, SymbolError> {
    let mut section_bounds: BTreeMap<usize, (u64, u64)> = BTreeMap::new();
    for section in file.sections() {
        if section.kind() == SectionKind::Text {
            let start = section.address();
            let end = start.saturating_add(section.size());
            if end > start {
                section_bounds.insert(section.index().0, (start, end));
            }
        }
    }

    let mut had_static = false;
    let mut raw_entries: Vec<(u64, u64, u8, String)> = Vec::new();
    for (priority, sym) in file
        .symbols()
        .map(|s| (0_u8, s))
        .chain(file.dynamic_symbols().map(|s| (1_u8, s)))
    {
        if priority == 0 {
            had_static = true;
        }
        if !matches!(sym.kind(), SymbolKind::Text | SymbolKind::Unknown) {
            continue;
        }
        let SymbolSection::Section(sec_idx) = sym.section() else {
            continue;
        };
        let Some(&(_, sec_end)) = section_bounds.get(&sec_idx.0) else {
            continue;
        };
        let addr = sym.address();
        if addr == 0 || addr >= sec_end {
            continue;
        }
        let Ok(name) = sym.name() else {
            continue;
        };
        if name.is_empty() || name.starts_with("$x") || name.starts_with("$d") {
            continue;
        }
        let size = sym.size();
        raw_entries.push((addr, size, priority, demangle_symbol(name)));
        if raw_entries.len() >= budget.max_spans {
            break;
        }
    }

    raw_entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)).then(b.1.cmp(&a.1)));

    for i in 0..raw_entries.len() {
        let (addr, size, _, ref name) = raw_entries[i];
        let next_addr = raw_entries
            .get(i + 1)
            .map(|e| e.0)
            .filter(|&next| next > addr);
        let end = if size > 0 {
            addr.saturating_add(size)
        } else if let Some(next) = next_addr {
            next
        } else {
            addr.saturating_add(1)
        };
        if end <= addr {
            continue;
        }
        let func_id = builder.intern_function(name, NO_INDEX, 0)?;
        out.push((addr, end, func_id));
    }

    Ok(had_static)
}

fn collect_mini_debugdata(
    compressed: &[u8],
    builder: &mut IndexBuilder,
    out: &mut Vec<(u64, u64, u32)>,
    decompressed_used: &mut u64,
    budget: &ParseBudget,
) -> Result<(), SymbolError> {
    let remaining = budget
        .max_decompressed_bytes
        .saturating_sub(*decompressed_used);
    let cap = usize::try_from(remaining)
        .unwrap_or(MAX_MINI_DEBUGDATA_BYTES)
        .min(MAX_MINI_DEBUGDATA_BYTES);
    if cap == 0 {
        return Ok(());
    }
    let mut reader = std::io::BufReader::new(compressed);
    let mut writer = BoundedWriter::new(cap);
    if lzma_rs::xz_decompress(&mut reader, &mut writer).is_err() {
        return Ok(());
    }
    *decompressed_used = decompressed_used.saturating_add(writer.buf.len() as u64);
    let Ok(mini) = object::File::parse(writer.buf.as_slice()) else {
        return Ok(());
    };
    let _ = collect_elf_symbols(&mini, builder, out, budget)?;
    Ok(())
}

struct BoundedWriter {
    buf: Vec<u8>,
    limit: usize,
}

impl BoundedWriter {
    const fn new(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            limit,
        }
    }
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len().saturating_add(data.len()) > self.limit {
            return Err(std::io::Error::other("mini debugdata exceeded budget"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn read_section_budgeted<'data>(
    file: &object::File<'data>,
    name: &str,
    used: &mut u64,
    budget: &ParseBudget,
) -> Result<Cow<'data, [u8]>, SymbolError> {
    let section = file.section_by_name(name).or_else(|| {
        name.strip_prefix(".debug_")
            .and_then(|rest| file.section_by_name(&format!(".zdebug_{rest}")))
    });
    let Some(section) = section else {
        return Ok(Cow::Borrowed(&[]));
    };
    let data = section
        .compressed_data()
        .map_err(|e| SymbolError::Decompress(format!("{name}: {e}")))?;
    decompress_section(name, data, used, budget)
}

fn decompress_section<'data>(
    name: &str,
    data: CompressedData<'data>,
    used: &mut u64,
    budget: &ParseBudget,
) -> Result<Cow<'data, [u8]>, SymbolError> {
    if data.format == CompressionFormat::None {
        return Ok(Cow::Borrowed(data.data));
    }
    let claimed = data.uncompressed_size;
    let compressed_len = data.data.len() as u64;
    if claimed > SMALL_DECOMPRESS_CLAIM
        && claimed > compressed_len.saturating_mul(budget.max_decompression_ratio)
    {
        return Err(SymbolError::BudgetExceeded(format!(
            "{name} decompression ratio exceeded"
        )));
    }
    let next_used = used.saturating_add(claimed);
    if next_used > budget.max_decompressed_bytes {
        return Err(SymbolError::BudgetExceeded(format!(
            "{name} decompressed size exceeded budget"
        )));
    }
    let size = usize::try_from(claimed)
        .map_err(|_| SymbolError::BudgetExceeded(format!("{name} size overflow")))?;
    let mut out = Vec::new();
    out.try_reserve_exact(size)
        .map_err(|_| SymbolError::BudgetExceeded(format!("{name} allocation")))?;

    match data.format {
        CompressionFormat::Zlib => {
            let mut zlib = flate2::Decompress::new(true);
            let status = zlib
                .decompress_vec(data.data, &mut out, flate2::FlushDecompress::Finish)
                .map_err(|e| SymbolError::Decompress(format!("{name} zlib: {e}")))?;
            if status != flate2::Status::StreamEnd || out.len() != size {
                return Err(SymbolError::Decompress(format!(
                    "{name} zlib stream length mismatch"
                )));
            }
        }
        CompressionFormat::Zstandard => {
            let decoder = zstd::stream::read::Decoder::new(data.data)
                .map_err(|e| SymbolError::Decompress(format!("{name} zstd: {e}")))?;
            decoder
                .take(claimed.saturating_add(1))
                .read_to_end(&mut out)
                .map_err(|e| SymbolError::Decompress(format!("{name} zstd: {e}")))?;
            if out.len() != size {
                return Err(SymbolError::Decompress(format!(
                    "{name} zstd stream length mismatch"
                )));
            }
        }
        CompressionFormat::None => return Ok(Cow::Borrowed(data.data)),
        _ => {
            return Err(SymbolError::Decompress(format!(
                "{name} unsupported compression format"
            )));
        }
    }

    *used = next_used;
    Ok(Cow::Owned(out))
}

fn collect_dwarf(
    file: &object::File<'_>,
    builder: &mut IndexBuilder,
    out_ranges: &mut Vec<RawRange>,
    out_lines: &mut Vec<LineSpan>,
    decompressed_used: &mut u64,
    budget: &ParseBudget,
) -> Result<bool, SymbolError> {
    let debug_info = read_section_budgeted(file, ".debug_info", decompressed_used, budget)?;
    if debug_info.is_empty() {
        return Ok(false);
    }

    let endian = if file.is_little_endian() {
        RunTimeEndian::Little
    } else {
        RunTimeEndian::Big
    };

    let section_names = [
        SectionId::DebugAbbrev,
        SectionId::DebugAddr,
        SectionId::DebugAranges,
        SectionId::DebugInfo,
        SectionId::DebugLine,
        SectionId::DebugLineStr,
        SectionId::DebugRanges,
        SectionId::DebugRngLists,
        SectionId::DebugStr,
        SectionId::DebugStrOffsets,
    ];

    let mut owned_sections: BTreeMap<SectionId, Cow<'_, [u8]>> = BTreeMap::new();
    owned_sections.insert(SectionId::DebugInfo, debug_info);
    for id in section_names {
        if id == SectionId::DebugInfo {
            continue;
        }
        let bytes = read_section_budgeted(file, id.name(), decompressed_used, budget)?;
        owned_sections.insert(id, bytes);
    }

    let dwarf = Dwarf::load(|id| -> Result<DwarfReader<'_>, SymbolError> {
        let slice = owned_sections
            .get(&id)
            .map_or(&[][..], std::convert::AsRef::as_ref);
        Ok(EndianSlice::new(slice, endian))
    })?;

    let mut units = dwarf.units();
    let mut unit_count = 0_usize;
    let mut order = 0_u32;

    while let Ok(Some(header)) = units.next() {
        unit_count += 1;
        if unit_count > budget.max_units {
            break;
        }
        let Ok(unit) = dwarf.unit(header) else {
            continue;
        };

        let unit_files = parse_unit_lines(&dwarf, &unit, builder, out_lines, budget)?;
        parse_unit_functions(
            &dwarf,
            &unit,
            &unit_files,
            builder,
            out_ranges,
            &mut order,
            budget,
        )?;
    }

    Ok(true)
}

fn parse_unit_lines(
    dwarf: &Dwarf<DwarfReader<'_>>,
    unit: &Unit<DwarfReader<'_>>,
    builder: &mut IndexBuilder,
    out_lines: &mut Vec<LineSpan>,
    budget: &ParseBudget,
) -> Result<Vec<u32>, SymbolError> {
    let Some(program) = unit.line_program.clone() else {
        return Ok(Vec::new());
    };
    let header = program.header();
    let comp_dir = unit
        .comp_dir
        .as_ref()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let file_entries = header.file_names();
    let mut unit_files = Vec::with_capacity(file_entries.len() + 1);
    if header.version() < 5 {
        unit_files.push(NO_INDEX);
    }
    for entry in file_entries {
        let path = resolve_line_file_path(dwarf, unit, header, entry, &comp_dir);
        let fid = if path.is_empty() {
            NO_INDEX
        } else {
            builder.intern_file(&path)?
        };
        unit_files.push(fid);
    }

    let mut rows = program.rows();
    let mut prev: Option<(u64, u32, u32, u32)> = None;
    let mut row_guard = 0_usize;
    while let Ok(Some((_, row))) = rows.next_row() {
        row_guard += 1;
        if row_guard > budget.max_line_spans || out_lines.len() >= budget.max_line_spans {
            break;
        }
        let addr = row.address();
        if let Some((p_addr, p_file, p_line, p_col)) = prev
            && addr > p_addr
            && p_file != NO_INDEX
            && p_line > 0
        {
            out_lines.push(LineSpan {
                start: p_addr,
                end: addr,
                file: p_file,
                line: p_line,
                column: p_col,
            });
        }
        if row.end_sequence() {
            prev = None;
        } else {
            let fidx = usize::try_from(row.file_index()).unwrap_or(usize::MAX);
            let file_id = unit_files.get(fidx).copied().unwrap_or(NO_INDEX);
            let line = row
                .line()
                .map_or(0, |l| u32::try_from(l.get()).unwrap_or(u32::MAX));
            let col = match row.column() {
                gimli::ColumnType::LeftEdge => 0,
                gimli::ColumnType::Column(c) => u32::try_from(c.get()).unwrap_or(u32::MAX),
            };
            prev = Some((addr, file_id, line, col));
        }
    }

    Ok(unit_files)
}

fn resolve_line_file_path(
    dwarf: &Dwarf<DwarfReader<'_>>,
    unit: &Unit<DwarfReader<'_>>,
    header: &gimli::LineProgramHeader<DwarfReader<'_>>,
    entry: &gimli::FileEntry<DwarfReader<'_>>,
    comp_dir: &str,
) -> String {
    let name = dwarf
        .attr_string(unit, entry.path_name())
        .ok()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.is_empty() {
        return String::new();
    }
    if name.starts_with('/') {
        return name;
    }
    let dir = entry
        .directory(header)
        .and_then(|attr| dwarf.attr_string(unit, attr).ok())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    if dir.starts_with('/') {
        format!("{}/{name}", dir.trim_end_matches('/'))
    } else if !dir.is_empty() && !comp_dir.is_empty() {
        format!(
            "{}/{}/{name}",
            comp_dir.trim_end_matches('/'),
            dir.trim_matches('/')
        )
    } else if !dir.is_empty() {
        format!("{}/{name}", dir.trim_end_matches('/'))
    } else if !comp_dir.is_empty() {
        format!("{}/{name}", comp_dir.trim_end_matches('/'))
    } else {
        name
    }
}

#[derive(Clone)]
struct ResolvedDieName {
    name: String,
    decl_file: u32,
    decl_line: u32,
}

fn parse_unit_functions(
    dwarf: &Dwarf<DwarfReader<'_>>,
    unit: &Unit<DwarfReader<'_>>,
    unit_files: &[u32],
    builder: &mut IndexBuilder,
    out_ranges: &mut Vec<RawRange>,
    order: &mut u32,
    budget: &ParseBudget,
) -> Result<(), SymbolError> {
    let mut name_cache: BTreeMap<usize, ResolvedDieName> = BTreeMap::new();
    let mut entries = unit.entries();
    let mut stack: Vec<(isize, InlineFrame)> = Vec::new();
    let mut die_count = 0_usize;

    while let Ok(Some(entry)) = entries.next_dfs() {
        die_count += 1;
        if die_count > budget.max_dies_per_unit || out_ranges.len() >= budget.max_spans {
            break;
        }
        let depth = entry.depth();
        while stack.last().is_some_and(|(d, _)| *d >= depth) {
            stack.pop();
        }

        let tag = entry.tag();
        if tag != gimli::DW_TAG_subprogram && tag != gimli::DW_TAG_inlined_subroutine {
            continue;
        }
        if stack.len() >= budget.max_inline_depth {
            continue;
        }

        let resolved = resolve_die_info(dwarf, unit, entry, unit_files, &mut name_cache, 0);
        if resolved.name.is_empty() {
            continue;
        }

        let func_id =
            builder.intern_function(&resolved.name, resolved.decl_file, resolved.decl_line)?;
        let (call_file, call_line) = if tag == gimli::DW_TAG_inlined_subroutine {
            let cf = entry
                .attr_value(gimli::DW_AT_call_file)
                .and_then(|v| v.udata_value())
                .and_then(|idx| usize::try_from(idx).ok())
                .and_then(|idx| unit_files.get(idx).copied())
                .unwrap_or(NO_INDEX);
            let cl = entry
                .attr_value(gimli::DW_AT_call_line)
                .and_then(|v| v.udata_value())
                .map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX));
            (cf, cl)
        } else {
            (NO_INDEX, 0)
        };

        let frame = InlineFrame {
            function: func_id,
            call_file,
            call_line,
        };
        stack.push((depth, frame));

        if let Ok(mut ranges) = dwarf.die_ranges(unit, entry) {
            let chain_innermost_first: Vec<InlineFrame> =
                stack.iter().rev().map(|(_, f)| *f).collect();
            let chain_depth = u32::try_from(chain_innermost_first.len()).unwrap_or(u32::MAX);
            while let Ok(Some(range)) = ranges.next() {
                if range.end <= range.begin || range.begin == 0 {
                    continue;
                }
                if out_ranges.len() >= budget.max_spans {
                    break;
                }
                *order = order.saturating_add(1);
                out_ranges.push(RawRange {
                    start: range.begin,
                    end: range.end,
                    depth: chain_depth,
                    order: *order,
                    chain: chain_innermost_first.clone(),
                });
            }
        }
    }

    Ok(())
}

fn resolve_die_info(
    dwarf: &Dwarf<DwarfReader<'_>>,
    unit: &Unit<DwarfReader<'_>>,
    entry: &DebuggingInformationEntry<DwarfReader<'_>>,
    unit_files: &[u32],
    cache: &mut BTreeMap<usize, ResolvedDieName>,
    recursion: u8,
) -> ResolvedDieName {
    let offset = entry.offset().0;
    if let Some(cached) = cache.get(&offset) {
        return cached.clone();
    }
    if recursion >= 8 {
        return ResolvedDieName {
            name: String::new(),
            decl_file: NO_INDEX,
            decl_line: 0,
        };
    }

    let mut name = attr_string_value(dwarf, unit, entry, gimli::DW_AT_linkage_name)
        .or_else(|| attr_string_value(dwarf, unit, entry, gimli::DW_AT_MIPS_linkage_name))
        .map(|s| demangle_symbol(&s))
        .or_else(|| attr_string_value(dwarf, unit, entry, gimli::DW_AT_name))
        .unwrap_or_default();

    let mut decl_file = entry
        .attr_value(gimli::DW_AT_decl_file)
        .and_then(|v| v.udata_value())
        .and_then(|idx| usize::try_from(idx).ok())
        .and_then(|idx| unit_files.get(idx).copied())
        .unwrap_or(NO_INDEX);
    let mut decl_line = entry
        .attr_value(gimli::DW_AT_decl_line)
        .and_then(|v| v.udata_value())
        .map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX));

    if name.is_empty() || decl_file == NO_INDEX {
        for ref_attr in [gimli::DW_AT_abstract_origin, gimli::DW_AT_specification] {
            if let Some(AttributeValue::UnitRef(UnitOffset(target))) = entry.attr_value(ref_attr)
                && target != offset
                && let Ok(target_entry) = unit.entry(UnitOffset(target))
            {
                let parent =
                    resolve_die_info(dwarf, unit, &target_entry, unit_files, cache, recursion + 1);
                if name.is_empty() {
                    name = parent.name;
                }
                if decl_file == NO_INDEX {
                    decl_file = parent.decl_file;
                }
                if decl_line == 0 {
                    decl_line = parent.decl_line;
                }
            }
        }
    }

    let resolved = ResolvedDieName {
        name,
        decl_file,
        decl_line,
    };
    cache.insert(offset, resolved.clone());
    resolved
}

fn attr_string_value(
    dwarf: &Dwarf<DwarfReader<'_>>,
    unit: &Unit<DwarfReader<'_>>,
    entry: &DebuggingInformationEntry<DwarfReader<'_>>,
    attr: gimli::DwAt,
) -> Option<String> {
    let val = entry.attr_value(attr)?;
    let s = dwarf.attr_string(unit, val).ok()?;
    Some(s.to_string_lossy().into_owned())
}

fn collect_gopclntab(
    file: &object::File<'_>,
    builder: &mut IndexBuilder,
    out: &mut Vec<(u64, u64, u32)>,
    decompressed_used: &mut u64,
    budget: &ParseBudget,
) -> Result<bool, SymbolError> {
    let data = read_section_budgeted(file, ".gopclntab", decompressed_used, budget)?;
    if data.len() < GO_HEADER_SIZE {
        return Ok(false);
    }
    let magic = read_u32_le(&data, 0).unwrap_or(0);
    if magic != GO120_MAGIC || data[4] != 0 || data[5] != 0 {
        return Ok(false);
    }
    let ptr_size = usize::from(data[7]);
    if ptr_size != 8 && ptr_size != 4 {
        return Ok(false);
    }
    let nfunc = read_word_le(&data, 8, ptr_size).unwrap_or(0);
    let text_start = read_word_le(&data, 8 + 2 * ptr_size, ptr_size).unwrap_or(0);
    let funcname_off = read_word_le(&data, 8 + 3 * ptr_size, ptr_size).unwrap_or(0);
    let pcln_off = read_word_le(&data, 8 + 7 * ptr_size, ptr_size).unwrap_or(0);

    let (Ok(nfunc), Ok(funcname_off), Ok(pcln_off)) = (
        usize::try_from(nfunc),
        usize::try_from(funcname_off),
        usize::try_from(pcln_off),
    ) else {
        return Ok(false);
    };
    if nfunc == 0 || nfunc > budget.max_functions || pcln_off >= data.len() {
        return Ok(false);
    }
    let funcnametab = data.get(funcname_off..).unwrap_or(&[]);
    let ft_entries = nfunc.saturating_add(1);
    let Some(ft_bytes) = ft_entries.checked_mul(8) else {
        return Ok(false);
    };
    let Some(ftab) = data.get(pcln_off..pcln_off.saturating_add(ft_bytes)) else {
        return Ok(false);
    };

    let mut added = false;
    for i in 0..nfunc {
        let off = i * 8;
        let entry_off = u64::from(read_u32_le(ftab, off).unwrap_or(0));
        let next_off = u64::from(read_u32_le(ftab, off + 8).unwrap_or(0));
        let func_info_off = read_u32_le(ftab, off + 4).unwrap_or(0) as usize;
        if next_off <= entry_off {
            continue;
        }
        let info_pos = pcln_off.saturating_add(func_info_off);
        if info_pos.saturating_add(GO_FUNC_SIZE) > data.len() {
            continue;
        }
        let name_off = read_u32_le(&data, info_pos + 4).unwrap_or(u32::MAX) as usize;
        let Some(name_slice) = funcnametab.get(name_off..) else {
            continue;
        };
        let end_nul = name_slice
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_slice.len())
            .min(1024);
        let Ok(name) = std::str::from_utf8(&name_slice[..end_nul]) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let start = text_start.saturating_add(entry_off);
        let end = text_start.saturating_add(next_off);
        if end > start {
            let func_id = builder.intern_function(name, NO_INDEX, 0)?;
            out.push((start, end, func_id));
            added = true;
        }
    }

    Ok(added)
}

fn read_u32_le(data: &[u8], offset: usize) -> Option<u32> {
    let b = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_word_le(data: &[u8], offset: usize, ptr_size: usize) -> Option<u64> {
    if ptr_size == 8 {
        let b = data.get(offset..offset.checked_add(8)?)?;
        Some(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    } else {
        read_u32_le(data, offset).map(u64::from)
    }
}

type ChainInternKey = Vec<(u32, u32, u32)>;

fn flatten_dwarf_spans(
    ranges: &[RawRange],
    budget: &ParseBudget,
) -> Result<(Vec<InlineFrame>, Vec<SymbolSpan>), SymbolError> {
    if ranges.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut events: Vec<(u64, bool, usize)> = Vec::with_capacity(ranges.len() * 2);
    for (idx, r) in ranges.iter().enumerate() {
        if r.end > r.start {
            events.push((r.start, true, idx));
            events.push((r.end, false, idx));
        }
    }
    events.sort_unstable_by_key(|&(addr, is_start, idx)| (addr, is_start, idx));

    let mut active: BTreeSet<(u32, u32, usize)> = BTreeSet::new();
    let mut inlines: Vec<InlineFrame> = Vec::new();
    let mut chain_intern: BTreeMap<ChainInternKey, (u32, u32)> = BTreeMap::new();
    let mut spans: Vec<SymbolSpan> = Vec::new();

    let mut i = 0_usize;
    let mut prev_addr = events[0].0;

    while i < events.len() {
        let addr = events[i].0;
        if addr > prev_addr
            && let Some(&(_, _, winner_idx)) = active.last()
        {
            let chain = &ranges[winner_idx].chain;
            let key: Vec<(u32, u32, u32)> = chain
                .iter()
                .map(|f| (f.function, f.call_file, f.call_line))
                .collect();
            let (inline_start, inline_count) = if let Some(&cached) = chain_intern.get(&key) {
                cached
            } else {
                let start = u32::try_from(inlines.len())
                    .map_err(|_| SymbolError::BudgetExceeded("inline table overflow".to_owned()))?;
                let count = u32::try_from(chain.len()).unwrap_or(u32::MAX);
                inlines.extend_from_slice(chain);
                chain_intern.insert(key, (start, count));
                (start, count)
            };

            if let Some(last) = spans.last_mut()
                && last.end == prev_addr
                && last.inline_start == inline_start
                && last.inline_count == inline_count
            {
                last.end = addr;
            } else if spans.len() < budget.max_spans {
                spans.push(SymbolSpan {
                    start: prev_addr,
                    end: addr,
                    inline_start,
                    inline_count,
                });
            }
        }

        while i < events.len() && events[i].0 == addr {
            let (_, is_start, idx) = events[i];
            let entry = (ranges[idx].depth, ranges[idx].order, idx);
            if is_start {
                active.insert(entry);
            } else {
                active.remove(&entry);
            }
            i += 1;
        }
        prev_addr = addr;
    }

    Ok((inlines, spans))
}

fn normalize_line_spans(mut spans: Vec<LineSpan>, budget: &ParseBudget) -> Vec<LineSpan> {
    spans.retain(|s| s.end > s.start && s.file != NO_INDEX && s.line > 0);
    spans.sort_unstable_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut out: Vec<LineSpan> = Vec::with_capacity(spans.len().min(budget.max_line_spans));
    for mut s in spans {
        if out.len() >= budget.max_line_spans {
            break;
        }
        if let Some(last) = out.last_mut() {
            if s.start < last.end {
                if s.end <= last.end {
                    continue;
                }
                s.start = last.end;
            }
            let same_location =
                last.file == s.file && last.line == s.line && last.column == s.column;
            if last.end == s.start && same_location {
                last.end = s.end;
                continue;
            }
        }
        out.push(s);
    }
    out
}

fn normalize_symtab_spans(mut raw: Vec<(u64, u64, u32)>, budget: &ParseBudget) -> Vec<SymtabSpan> {
    raw.retain(|&(start, end, _)| end > start);
    raw.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut out: Vec<SymtabSpan> = Vec::with_capacity(raw.len().min(budget.max_spans));
    for (mut start, end, function) in raw {
        if out.len() >= budget.max_spans {
            break;
        }
        if let Some(last) = out.last_mut() {
            if start < last.end {
                if end <= last.end {
                    continue;
                }
                start = last.end;
            }
            if last.end == start && last.function == function {
                last.end = end;
                continue;
            }
        }
        out.push(SymtabSpan {
            start,
            end,
            function,
        });
    }
    out
}
