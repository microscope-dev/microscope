use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;

use gimli::{
    AttributeValue, DebuggingInformationEntry, Dwarf, EndianSlice, RunTimeEndian, SectionId, Unit,
    UnitOffset,
};
use object::{
    Architecture, CompressedData, CompressionFormat, Object as _, ObjectSection as _,
    ObjectSymbol as _, RelocationFlags, SectionFlags, SectionKind, SymbolKind, SymbolSection, elf,
};

use super::index::{IndexBuilder, InlineFrame, LineSpan, NO_INDEX, SymbolSpan, SymtabSpan};
use super::{BuildId, ParseBudget, SymbolError, SymbolIndex, SymbolMetadata, demangle_symbol};

const SMALL_DECOMPRESS_CLAIM: u64 = 1024 * 1024;
const GO120_MAGIC: u32 = 0xffff_fff1;
const GO_HEADER_SIZE: usize = 8 + 8 * 8;
const GO_FUNC_SIZE: usize = 44;

type DwarfReader<'a> = EndianSlice<'a, RunTimeEndian>;

#[derive(Debug, Clone, Copy)]
struct RawRange {
    start: u64,
    end: u64,
    depth: u32,
    order: u32,
    chain: ChainId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ChainId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ChainNode {
    frame: InlineFrame,
    parent: Option<ChainId>,
}

#[derive(Default)]
struct DwarfRanges {
    ranges: Vec<RawRange>,
    chains: InlineChains,
}

#[derive(Default)]
struct InlineChains {
    nodes: Vec<ChainNode>,
    ids: foldhash::HashMap<ChainNode, ChainId>,
}

impl InlineChains {
    fn intern(
        &mut self,
        frame: InlineFrame,
        parent: Option<ChainId>,
    ) -> Result<ChainId, SymbolError> {
        let node = ChainNode { frame, parent };
        if let Some(&id) = self.ids.get(&node) {
            return Ok(id);
        }
        let id = ChainId(
            u32::try_from(self.nodes.len())
                .map_err(|_| SymbolError::BudgetExceeded("inline chain overflow".to_owned()))?,
        );
        assert!(parent.is_none_or(|p| p.0 < id.0));
        self.nodes.push(node);
        self.ids.insert(node, id);
        Ok(id)
    }

    fn frames(&self, id: ChainId) -> impl Iterator<Item = InlineFrame> + '_ {
        std::iter::successors(Some(self.nodes[id.0 as usize]), |node| {
            node.parent.map(|p| self.nodes[p.0 as usize])
        })
        .map(|node| node.frame)
    }
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

    let mut line_spans = Vec::new();
    let mut dwarf_ranges = DwarfRanges::default();
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

fn is_executable_section(section: &object::Section<'_, '_>) -> bool {
    if section.kind() == SectionKind::Text {
        return true;
    }
    matches!(
        section.flags(),
        SectionFlags::Elf { sh_flags } if sh_flags & u64::from(elf::SHF_EXECINSTR) != 0
    )
}

fn collect_elf_symbols(
    file: &object::File<'_>,
    builder: &mut IndexBuilder,
    out: &mut Vec<(u64, u64, u32)>,
    budget: &ParseBudget,
) -> Result<bool, SymbolError> {
    let mut section_bounds: BTreeMap<usize, (u64, u64)> = BTreeMap::new();
    for section in file.sections() {
        if is_executable_section(&section) {
            let start = section.address();
            let end = start.saturating_add(section.size());
            if end > start {
                section_bounds.insert(section.index().0, (start, end));
            }
        }
    }

    let mut had_static = false;
    let mut raw_entries: Vec<(u64, u64, u8, String)> = Vec::new();
    let mut raw_name_bytes = 0_usize;
    for (priority, sym) in file
        .symbols()
        .map(|s| (0_u8, s))
        .chain(file.dynamic_symbols().map(|s| (1_u8, s)))
    {
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
        if priority == 0 {
            had_static = true;
        }
        let size = sym.size();
        let mut demangled = demangle_symbol(name);
        if demangled.len() > 65_536 {
            let mut end = 65_536;
            while !demangled.is_char_boundary(end) {
                end -= 1;
            }
            demangled.truncate(end);
        }
        raw_name_bytes = raw_name_bytes
            .saturating_add(demangled.len())
            .saturating_add(size_of::<String>());
        if raw_name_bytes > budget.max_string_bytes.saturating_mul(2) {
            break;
        }
        raw_entries.push((addr, size, priority, demangled));
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
    let (codec, decoder): (&str, Box<dyn std::io::Read + '_>) = match data.format {
        CompressionFormat::Zlib => ("zlib", Box::new(flate2::read::ZlibDecoder::new(data.data))),
        CompressionFormat::Zstandard => (
            "zstd",
            Box::new(
                zstd::stream::read::Decoder::new(data.data)
                    .map_err(|e| SymbolError::Decompress(format!("{name} zstd: {e}")))?,
            ),
        ),
        _ => {
            return Err(SymbolError::Decompress(format!(
                "{name} unsupported compression format"
            )));
        }
    };

    let mut out = Vec::new();
    out.try_reserve_exact(size)
        .map_err(|_| SymbolError::BudgetExceeded(format!("{name} allocation")))?;
    decoder
        .take(claimed.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| SymbolError::Decompress(format!("{name} {codec}: {e}")))?;
    if out.len() != size {
        return Err(SymbolError::Decompress(format!(
            "{name} {codec} stream length mismatch"
        )));
    }

    *used = next_used;
    Ok(Cow::Owned(out))
}

fn collect_dwarf(
    file: &object::File<'_>,
    builder: &mut IndexBuilder,
    out_ranges: &mut DwarfRanges,
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
    out: &mut DwarfRanges,
    order: &mut u32,
    budget: &ParseBudget,
) -> Result<(), SymbolError> {
    let mut name_cache: BTreeMap<usize, ResolvedDieName> = BTreeMap::new();
    let mut entries = unit.entries();
    let mut stack: Vec<(isize, ChainId)> = Vec::new();
    let mut die_count = 0_usize;

    while let Ok(Some(entry)) = entries.next_dfs() {
        die_count += 1;
        if die_count > budget.max_dies_per_unit || out.ranges.len() >= budget.max_spans {
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
                .and_then(attr_file_index)
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
        let chain = out.chains.intern(frame, stack.last().map(|&(_, id)| id))?;
        stack.push((depth, chain));

        if let Ok(mut ranges) = dwarf.die_ranges(unit, entry) {
            let chain_depth = u32::try_from(stack.len()).unwrap_or(u32::MAX);
            while let Ok(Some(range)) = ranges.next() {
                if range.end <= range.begin || range.begin == 0 {
                    continue;
                }
                if out.ranges.len() >= budget.max_spans {
                    break;
                }
                *order = order.saturating_add(1);
                out.ranges.push(RawRange {
                    start: range.begin,
                    end: range.end,
                    depth: chain_depth,
                    order: *order,
                    chain,
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
        .and_then(attr_file_index)
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

fn attr_file_index(val: AttributeValue<DwarfReader<'_>>) -> Option<usize> {
    match val {
        AttributeValue::FileIndex(idx) => usize::try_from(idx).ok(),
        other => other
            .udata_value()
            .and_then(|idx| usize::try_from(idx).ok()),
    }
}

fn find_go_moduledata_text(
    file: &object::File<'_>,
    table_addr: u64,
    nfunc: u64,
    funcname_off: u64,
    pcln_off: u64,
    first_entry: u64,
    ptr_size: usize,
) -> Option<u64> {
    const MODULE_PC_HEADER: u64 = 0;
    const MODULE_FUNCNAMETAB: u64 = 1;
    const MODULE_FTAB: u64 = 16;
    const MODULE_FTAB_LENGTH: u64 = 17;
    const MODULE_MINPC: u64 = 20;
    const MODULE_TEXT: u64 = 22;

    if ptr_size != 8 || table_addr == 0 {
        return None;
    }

    let funcnametab = table_addr.checked_add(funcname_off)?;
    let ftab = table_addr.checked_add(pcln_off)?;
    let expected_ftab_len = nfunc.checked_add(1)?;

    let writable = u64::from(elf::SHF_ALLOC | elf::SHF_WRITE);
    let sections: Vec<(u64, &[u8])> = file
        .sections()
        .filter(|sec| {
            matches!(
                sec.flags(),
                SectionFlags::Elf { sh_flags } if sh_flags & writable == writable
            )
        })
        .filter_map(|sec| Some((sec.address(), sec.data().ok()?)))
        .collect();
    let relative: BTreeMap<u64, u64> = file
        .dynamic_relocations()
        .into_iter()
        .flatten()
        .filter(|(_, rel)| {
            matches!(
                rel.flags(),
                RelocationFlags::Elf { r_type }
                    if r_type == elf::R_X86_64_RELATIVE || r_type == elf::R_AARCH64_RELATIVE
            )
        })
        .filter_map(|(addr, rel)| Some((addr, u64::try_from(rel.addend()).ok()?)))
        .collect();

    let read_word = |addr: u64| -> Option<u64> {
        if let Some(&val) = relative.get(&addr) {
            return Some(val);
        }
        let (start, data) = sections
            .iter()
            .find(|(s, d)| *s <= addr && addr - *s < d.len() as u64)?;
        let off = usize::try_from(addr - *start).ok()?;
        read_word_le(data, off, 8)
    };

    let mut candidates: Vec<u64> = relative
        .iter()
        .filter(|(_, val)| **val == table_addr)
        .map(|(addr, _)| *addr)
        .collect();
    for (start, data) in &sections {
        let align_off = usize::try_from((8 - (*start % 8)) % 8).unwrap_or(0);
        let mut off = align_off;
        while off.saturating_add(8) <= data.len() {
            if read_word_le(data, off, 8) == Some(table_addr)
                && let Some(addr) = start.checked_add(off as u64)
            {
                candidates.push(addr);
            }
            off += 8;
        }
    }
    candidates.sort_unstable();
    candidates.dedup();

    candidates.into_iter().find_map(|module| {
        let field = |idx: u64| read_word(module.checked_add(idx.checked_mul(8)?)?);
        let text = field(MODULE_TEXT)?;
        let valid = field(MODULE_PC_HEADER) == Some(table_addr)
            && field(MODULE_FUNCNAMETAB) == Some(funcnametab)
            && field(MODULE_FTAB) == Some(ftab)
            && field(MODULE_FTAB_LENGTH) == Some(expected_ftab_len)
            && field(MODULE_MINPC) == text.checked_add(first_entry);
        valid.then_some(text)
    })
}

pub fn resolve_gopclntab_text_start(file: &object::File<'_>) -> Option<u64> {
    let sec_name = if file.section_by_name(".gopclntab").is_some() {
        ".gopclntab"
    } else if file.section_by_name(".data.rel.ro.gopclntab").is_some() {
        ".data.rel.ro.gopclntab"
    } else {
        return None;
    };
    let sec = file.section_by_name(sec_name)?;
    let table_addr = sec.address();
    let mut used = 0_u64;
    let budget = ParseBudget::SERVER;
    let data = read_section_budgeted(file, sec_name, &mut used, &budget).ok()?;
    if data.len() < GO_HEADER_SIZE {
        return None;
    }
    let magic = read_u32_le(&data, 0)?;
    if magic != GO120_MAGIC || data[4] != 0 || data[5] != 0 {
        return None;
    }
    let ptr_size = usize::from(data[7]);
    if ptr_size != 8 && ptr_size != 4 {
        return None;
    }
    let nfunc_u64 = read_word_le(&data, 8, ptr_size)?;
    let header_text_start = read_word_le(&data, 8 + 2 * ptr_size, ptr_size)?;
    let funcname_off_u64 = read_word_le(&data, 8 + 3 * ptr_size, ptr_size)?;
    let pcln_off_u64 = read_word_le(&data, 8 + 7 * ptr_size, ptr_size)?;
    let nfunc = usize::try_from(nfunc_u64).ok()?;
    let pcln_off = usize::try_from(pcln_off_u64).ok()?;
    if nfunc == 0 || pcln_off >= data.len() {
        return None;
    }
    let ft_bytes = nfunc.checked_add(1)?.checked_mul(8)?;
    let ftab = data.get(pcln_off..pcln_off.checked_add(ft_bytes)?)?;
    let first_entry = u64::from(read_u32_le(ftab, 0)?);
    let last_end = u64::from(read_u32_le(ftab, nfunc * 8)?);

    let exec_ranges: Vec<(u64, u64)> = file
        .sections()
        .filter(is_executable_section)
        .filter_map(|s| {
            let start = s.address();
            let end = start.checked_add(s.size())?;
            (end > start).then_some((start, end))
        })
        .collect();
    if exec_ranges.is_empty() {
        return None;
    }
    let fits_exec = |base: u64| -> bool {
        let Some(min_pc) = base.checked_add(first_entry) else {
            return false;
        };
        let Some(max_pc) = base.checked_add(last_end) else {
            return false;
        };
        max_pc > min_pc && exec_ranges.iter().any(|&(s, e)| s <= min_pc && max_pc <= e)
    };

    let embedded_text = file
        .section_by_name(super::extract::GO_TEXT_SECTION_NAME)
        .and_then(|s| s.data().ok())
        .filter(|d| d.len() == 8)
        .and_then(|d| d.try_into().ok().map(u64::from_le_bytes));
    let runtime_text = file
        .symbols()
        .find(|sym| sym.name_bytes().ok() == Some(b"runtime.text"))
        .map(|sym| sym.address());
    let moduledata_text = find_go_moduledata_text(
        file,
        table_addr,
        nfunc_u64,
        funcname_off_u64,
        pcln_off_u64,
        first_entry,
        ptr_size,
    );
    let text_section_addr = file
        .section_by_name(".text")
        .filter(is_executable_section)
        .map(|s| s.address());

    embedded_text
        .filter(|&b| fits_exec(b))
        .or_else(|| moduledata_text.filter(|&b| fits_exec(b)))
        .or_else(|| runtime_text.filter(|&b| fits_exec(b)))
        .or_else(|| {
            (header_text_start != 0 && fits_exec(header_text_start)).then_some(header_text_start)
        })
        .or_else(|| text_section_addr.filter(|&b| fits_exec(b)))
}

#[expect(
    clippy::too_many_lines,
    reason = "parses Go 1.16/1.18/1.20+ pclntab header, resolves textStart via embedded/moduledata/symtab/.text, and indexes function ranges"
)]
fn collect_gopclntab(
    file: &object::File<'_>,
    builder: &mut IndexBuilder,
    out: &mut Vec<(u64, u64, u32)>,
    decompressed_used: &mut u64,
    budget: &ParseBudget,
) -> Result<bool, SymbolError> {
    let sec_name = if file.section_by_name(".gopclntab").is_some() {
        ".gopclntab"
    } else if file.section_by_name(".data.rel.ro.gopclntab").is_some() {
        ".data.rel.ro.gopclntab"
    } else {
        return Ok(false);
    };
    let table_addr = file
        .section_by_name(sec_name)
        .map_or(0, |sec| sec.address());
    let data = read_section_budgeted(file, sec_name, decompressed_used, budget)?;
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
    let nfunc_u64 = read_word_le(&data, 8, ptr_size).unwrap_or(0);
    let header_text_start = read_word_le(&data, 8 + 2 * ptr_size, ptr_size).unwrap_or(0);
    let funcname_off_u64 = read_word_le(&data, 8 + 3 * ptr_size, ptr_size).unwrap_or(0);
    let pcln_off_u64 = read_word_le(&data, 8 + 7 * ptr_size, ptr_size).unwrap_or(0);

    let (Ok(nfunc), Ok(funcname_off), Ok(pcln_off)) = (
        usize::try_from(nfunc_u64),
        usize::try_from(funcname_off_u64),
        usize::try_from(pcln_off_u64),
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

    let exec_ranges: Vec<(u64, u64)> = file
        .sections()
        .filter(is_executable_section)
        .filter_map(|sec| {
            let start = sec.address();
            let end = start.checked_add(sec.size())?;
            (end > start).then_some((start, end))
        })
        .collect();
    if exec_ranges.is_empty() {
        return Ok(false);
    }

    let first_entry = u64::from(read_u32_le(ftab, 0).unwrap_or(0));
    let last_end = u64::from(read_u32_le(ftab, nfunc * 8).unwrap_or(0));
    let fits_exec = |base: u64| -> bool {
        let Some(min_pc) = base.checked_add(first_entry) else {
            return false;
        };
        let Some(max_pc) = base.checked_add(last_end) else {
            return false;
        };
        max_pc > min_pc && exec_ranges.iter().any(|&(s, e)| s <= min_pc && max_pc <= e)
    };

    let embedded_text = file
        .section_by_name(super::extract::GO_TEXT_SECTION_NAME)
        .and_then(|sec| sec.data().ok())
        .filter(|d| d.len() == 8)
        .and_then(|d| d.try_into().ok().map(u64::from_le_bytes));
    let runtime_text = file
        .symbols()
        .find(|sym| sym.name_bytes().ok() == Some(b"runtime.text"))
        .map(|sym| sym.address());
    let moduledata_text = find_go_moduledata_text(
        file,
        table_addr,
        nfunc_u64,
        funcname_off_u64,
        pcln_off_u64,
        first_entry,
        ptr_size,
    );
    let text_section_addr = file
        .section_by_name(".text")
        .filter(is_executable_section)
        .map(|sec| sec.address());

    let Some(text_start) = embedded_text
        .filter(|&b| fits_exec(b))
        .or_else(|| moduledata_text.filter(|&b| fits_exec(b)))
        .or_else(|| runtime_text.filter(|&b| fits_exec(b)))
        .or_else(|| {
            (header_text_start != 0 && fits_exec(header_text_start)).then_some(header_text_start)
        })
        .or_else(|| text_section_addr.filter(|&b| fits_exec(b)))
    else {
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
        if end > start && exec_ranges.iter().any(|&(s, e)| s <= start && end <= e) {
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

fn flatten_dwarf_spans(
    dwarf: &DwarfRanges,
    budget: &ParseBudget,
) -> Result<(Vec<InlineFrame>, Vec<SymbolSpan>), SymbolError> {
    let ranges = &dwarf.ranges;
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
    let mut materialized: foldhash::HashMap<ChainId, (u32, u32)> = foldhash::HashMap::default();
    let mut spans: Vec<SymbolSpan> = Vec::new();

    let Some(&(mut prev_addr, _, _)) = events.first() else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut i = 0_usize;

    while i < events.len() {
        let addr = events[i].0;
        if addr > prev_addr
            && let Some(&(_, _, winner_idx)) = active.last()
        {
            let chain = ranges[winner_idx].chain;
            let (inline_start, inline_count) = if let Some(&cached) = materialized.get(&chain) {
                cached
            } else {
                let start = inlines.len();
                inlines.extend(dwarf.chains.frames(chain));
                let span = (
                    u32::try_from(start).map_err(|_| {
                        SymbolError::BudgetExceeded("inline table overflow".to_owned())
                    })?,
                    u32::try_from(inlines.len() - start).unwrap_or(u32::MAX),
                );
                assert_eq!(span.1, ranges[winner_idx].depth);
                materialized.insert(chain, span);
                span
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
