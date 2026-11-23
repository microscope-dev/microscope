use std::cell::RefCell;

use foldhash::HashMap;
use twox_hash::XxHash3_64;

thread_local! {
    static SYM_ZSTD_DCTX: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
    static SYM_ZSTD_CCTX: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
}

use super::{BuildId, ParseBudget, SymbolError};

pub const INDEX_MAGIC: [u8; 8] = *b"USCOPESY";
pub const INDEX_VERSION: u16 = 1;
pub const NO_INDEX: u32 = u32::MAX;
const HEADER_LEN: usize = 8 + 2 + 2 + 4 + 8 + 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFrame {
    pub name: String,
    pub file: Option<String>,
    pub line: u32,
    pub column: u32,
    pub declared_file: Option<String>,
    pub declared_line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolFunction {
    pub name: u32,
    pub file: u32,
    pub decl_line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineFrame {
    pub function: u32,
    pub call_file: u32,
    pub call_line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolSpan {
    pub start: u64,
    pub end: u64,
    pub inline_start: u32,
    pub inline_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSpan {
    pub start: u64,
    pub end: u64,
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymtabSpan {
    pub start: u64,
    pub end: u64,
    pub function: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolIndex {
    pub(super) build_id: BuildId,
    pub(super) arch: String,
    pub(super) has_dwarf: bool,
    pub(super) has_symtab: bool,
    pub(super) has_gopclntab: bool,
    pub(super) string_data: String,
    pub(super) string_offsets: Vec<u32>,
    pub(super) files: Vec<u32>,
    pub(super) functions: Vec<SymbolFunction>,
    pub(super) inlines: Vec<InlineFrame>,
    pub(super) spans: Vec<SymbolSpan>,
    pub(super) line_spans: Vec<LineSpan>,
    pub(super) symtab_spans: Vec<SymtabSpan>,
}

impl SymbolIndex {
    #[must_use]
    pub const fn build_id(&self) -> &BuildId {
        &self.build_id
    }

    #[must_use]
    pub fn arch(&self) -> &str {
        &self.arch
    }

    #[must_use]
    pub const fn has_dwarf(&self) -> bool {
        self.has_dwarf
    }

    #[must_use]
    pub const fn has_symtab(&self) -> bool {
        self.has_symtab
    }

    #[must_use]
    pub fn find_function_address(&self, name: &str) -> Option<u64> {
        for span in &self.symtab_spans {
            if let Some(func) = self.functions.get(span.function as usize)
                && self.string(func.name) == Some(name)
            {
                return Some(span.start);
            }
        }
        for span in &self.spans {
            let start = span.inline_start as usize;
            let end = start.saturating_add(span.inline_count as usize);
            if let Some(inlines) = self.inlines.get(start..end) {
                for inl in inlines {
                    if let Some(func) = self.functions.get(inl.function as usize)
                        && self.string(func.name) == Some(name)
                    {
                        return Some(span.start);
                    }
                }
            }
        }
        None
    }

    #[must_use]
    pub const fn has_gopclntab(&self) -> bool {
        self.has_gopclntab
    }

    #[must_use]
    pub fn functions(&self) -> &[SymbolFunction] {
        &self.functions
    }

    #[must_use]
    pub fn spans(&self) -> &[SymbolSpan] {
        &self.spans
    }

    #[must_use]
    pub fn line_spans(&self) -> &[LineSpan] {
        &self.line_spans
    }

    #[must_use]
    pub fn symtab_spans(&self) -> &[SymtabSpan] {
        &self.symtab_spans
    }

    #[must_use]
    pub fn string(&self, id: u32) -> Option<&str> {
        if id == NO_INDEX {
            return None;
        }
        let idx = id as usize;
        let &start = self.string_offsets.get(idx)?;
        let &end = self.string_offsets.get(idx + 1)?;
        self.string_data.get(start as usize..end as usize)
    }

    #[must_use]
    pub const fn string_count(&self) -> usize {
        self.string_offsets.len().saturating_sub(1)
    }

    #[must_use]
    pub fn file_path(&self, file_id: u32) -> Option<&str> {
        if file_id == NO_INDEX {
            return None;
        }
        let str_id = *self.files.get(file_id as usize)?;
        self.string(str_id)
    }

    #[must_use]
    pub fn contains_function(&self, name: &str) -> bool {
        self.functions
            .iter()
            .any(|f| self.string(f.name) == Some(name))
    }

    #[must_use]
    pub const fn approx_bytes(&self) -> usize {
        self.string_data
            .len()
            .saturating_add(self.string_offsets.len() * std::mem::size_of::<u32>())
            .saturating_add(self.files.len() * std::mem::size_of::<u32>())
            .saturating_add(self.functions.len() * std::mem::size_of::<SymbolFunction>())
            .saturating_add(self.inlines.len() * std::mem::size_of::<InlineFrame>())
            .saturating_add(self.spans.len() * std::mem::size_of::<SymbolSpan>())
            .saturating_add(self.line_spans.len() * std::mem::size_of::<LineSpan>())
            .saturating_add(self.symtab_spans.len() * std::mem::size_of::<SymtabSpan>())
            .saturating_add(128)
    }

    #[must_use]
    pub fn lookup(&self, address: u64) -> Vec<ResolvedFrame> {
        let leaf_line = self.lookup_line(address);

        if let Some(span) = self.lookup_dwarf_span(address) {
            let start = span.inline_start as usize;
            let end = start.saturating_add(span.inline_count as usize);
            if let Some(chain) = self.inlines.get(start..end)
                && !chain.is_empty()
            {
                let mut out = Vec::with_capacity(chain.len());
                let mut cur_file =
                    leaf_line.and_then(|l| self.file_path(l.file).map(str::to_owned));
                let mut cur_line = leaf_line.map_or(0, |l| l.line);
                let mut cur_col = leaf_line.map_or(0, |l| l.column);

                for frame in chain {
                    if let Some(func) = self.functions.get(frame.function as usize) {
                        let name = self.string(func.name).unwrap_or_default().to_owned();
                        if !name.is_empty() {
                            let declared_file = self.file_path(func.file).map(str::to_owned);
                            out.push(ResolvedFrame {
                                name,
                                file: cur_file.take().or_else(|| declared_file.clone()),
                                line: if cur_line > 0 {
                                    cur_line
                                } else {
                                    func.decl_line
                                },
                                column: cur_col,
                                declared_file,
                                declared_line: func.decl_line,
                            });
                        }
                    }
                    cur_file = self.file_path(frame.call_file).map(str::to_owned);
                    cur_line = frame.call_line;
                    cur_col = 0;
                }
                if !out.is_empty() {
                    return out;
                }
            }
        }

        if let Some(sym) = self.lookup_symtab_span(address)
            && let Some(func) = self.functions.get(sym.function as usize)
        {
            let name = self.string(func.name).unwrap_or_default().to_owned();
            if !name.is_empty() {
                let declared_file = self.file_path(func.file).map(str::to_owned);
                let file = leaf_line
                    .and_then(|l| self.file_path(l.file).map(str::to_owned))
                    .or_else(|| declared_file.clone());
                let line = leaf_line.map_or(func.decl_line, |l| l.line);
                let column = leaf_line.map_or(0, |l| l.column);
                return vec![ResolvedFrame {
                    name,
                    file,
                    line,
                    column,
                    declared_file,
                    declared_line: func.decl_line,
                }];
            }
        }

        Vec::new()
    }

    fn lookup_dwarf_span(&self, address: u64) -> Option<SymbolSpan> {
        let idx = self
            .spans
            .partition_point(|s| s.start <= address)
            .checked_sub(1)?;
        let span = *self.spans.get(idx)?;
        (address >= span.start && address < span.end).then_some(span)
    }

    fn lookup_line(&self, address: u64) -> Option<LineSpan> {
        let idx = self
            .line_spans
            .partition_point(|s| s.start <= address)
            .checked_sub(1)?;
        let span = *self.line_spans.get(idx)?;
        (address >= span.start && address < span.end).then_some(span)
    }

    fn lookup_symtab_span(&self, address: u64) -> Option<SymtabSpan> {
        let idx = self
            .symtab_spans
            .partition_point(|s| s.start <= address)
            .checked_sub(1)?;
        let span = *self.symtab_spans.get(idx)?;
        (address >= span.start && address < span.end).then_some(span)
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut raw = Vec::new();
        write_str(&mut raw, self.build_id.as_str());
        write_str(&mut raw, &self.arch);
        let flags = u8::from(self.has_dwarf)
            | (u8::from(self.has_symtab) << 1)
            | (u8::from(self.has_gopclntab) << 2);
        raw.push(flags);

        let str_count = self.string_count();
        write_varuint(&mut raw, str_count as u64);
        for pair in self.string_offsets.windows(2) {
            let s = &self.string_data[pair[0] as usize..pair[1] as usize];
            write_str(&mut raw, s);
        }

        write_varuint(&mut raw, self.files.len() as u64);
        for &f in &self.files {
            write_varuint(&mut raw, u64::from(f));
        }

        write_varuint(&mut raw, self.functions.len() as u64);
        for f in &self.functions {
            write_varuint(&mut raw, u64::from(f.name));
            write_varuint(&mut raw, u64::from(f.file));
            write_varuint(&mut raw, u64::from(f.decl_line));
        }

        write_varuint(&mut raw, self.inlines.len() as u64);
        for item in &self.inlines {
            write_varuint(&mut raw, u64::from(item.function));
            write_varuint(&mut raw, u64::from(item.call_file));
            write_varuint(&mut raw, u64::from(item.call_line));
        }

        write_varuint(&mut raw, self.spans.len() as u64);
        let mut prev_start = 0_u64;
        for span in &self.spans {
            write_varuint(&mut raw, span.start.saturating_sub(prev_start));
            write_varuint(&mut raw, span.end.saturating_sub(span.start));
            write_varuint(&mut raw, u64::from(span.inline_start));
            write_varuint(&mut raw, u64::from(span.inline_count));
            prev_start = span.start;
        }

        write_varuint(&mut raw, self.line_spans.len() as u64);
        let mut prev_line_start = 0_u64;
        for span in &self.line_spans {
            write_varuint(&mut raw, span.start.saturating_sub(prev_line_start));
            write_varuint(&mut raw, span.end.saturating_sub(span.start));
            write_varuint(&mut raw, u64::from(span.file));
            write_varuint(&mut raw, u64::from(span.line));
            write_varuint(&mut raw, u64::from(span.column));
            prev_line_start = span.start;
        }

        write_varuint(&mut raw, self.symtab_spans.len() as u64);
        let mut prev_sym_start = 0_u64;
        for span in &self.symtab_spans {
            write_varuint(&mut raw, span.start.saturating_sub(prev_sym_start));
            write_varuint(&mut raw, span.end.saturating_sub(span.start));
            write_varuint(&mut raw, u64::from(span.function));
            prev_sym_start = span.start;
        }

        let compressed = SYM_ZSTD_CCTX.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let compressor = if let Some(c) = borrow.as_mut() {
                c
            } else {
                let Ok(c) = zstd::bulk::Compressor::new(3) else {
                    return raw.clone();
                };
                borrow.insert(c)
            };
            compressor.compress(&raw).unwrap_or_else(|_| raw.clone())
        });
        let is_compressed = compressed.len() < raw.len();
        let payload = if is_compressed { &compressed } else { &raw };

        let mut header_prefix = [0_u8; 24];
        header_prefix[0..8].copy_from_slice(&INDEX_MAGIC);
        header_prefix[8..10].copy_from_slice(&INDEX_VERSION.to_le_bytes());
        let codec_flags: u16 = u16::from(is_compressed);
        header_prefix[10..12].copy_from_slice(&codec_flags.to_le_bytes());
        header_prefix[12..16].copy_from_slice(&0_u32.to_le_bytes());
        header_prefix[16..24].copy_from_slice(&(raw.len() as u64).to_le_bytes());
        let checksum = index_checksum(&header_prefix, payload);

        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&header_prefix);
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    pub fn decode(bytes: &[u8], budget: &ParseBudget) -> Result<Self, SymbolError> {
        Self::decode_inner(bytes, budget, true)
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub fn decode_unverified_for_fuzz(
        bytes: &[u8],
        budget: &ParseBudget,
    ) -> Result<Self, SymbolError> {
        Self::decode_inner(bytes, budget, false)
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub fn decode_body_for_fuzz(raw: &[u8], budget: &ParseBudget) -> Result<Self, SymbolError> {
        decode_body(raw, budget)
    }

    fn decode_inner(
        bytes: &[u8],
        budget: &ParseBudget,
        verify_checksum: bool,
    ) -> Result<Self, SymbolError> {
        if bytes.len() < HEADER_LEN {
            return Err(SymbolError::MalformedIndex("truncated header".to_owned()));
        }
        if bytes.len() > budget.max_input_bytes {
            return Err(SymbolError::BudgetExceeded(
                "symbol index exceeds max input size".to_owned(),
            ));
        }
        if bytes[..8] != INDEX_MAGIC {
            return Err(SymbolError::MalformedIndex("bad magic".to_owned()));
        }
        let version = u16::from_le_bytes([bytes[8], bytes[9]]);
        if version != INDEX_VERSION {
            return Err(SymbolError::MalformedIndex(format!(
                "unsupported version {version}"
            )));
        }
        let codec_flags = u16::from_le_bytes([bytes[10], bytes[11]]);
        if codec_flags > 1 {
            return Err(SymbolError::MalformedIndex("unknown flags".to_owned()));
        }
        if bytes[12..16] != [0; 4] {
            return Err(SymbolError::MalformedIndex(
                "non-zero reserved header bytes".to_owned(),
            ));
        }
        let raw_len = u64::from_le_bytes([
            bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
        ]);
        let expected_checksum = u64::from_le_bytes([
            bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
        ]);
        let payload = &bytes[HEADER_LEN..];
        if verify_checksum && index_checksum(&bytes[..24], payload) != expected_checksum {
            return Err(SymbolError::ChecksumMismatch);
        }
        if raw_len > budget.max_decompressed_bytes {
            return Err(SymbolError::BudgetExceeded(
                "symbol index exceeds max decompressed size".to_owned(),
            ));
        }
        if codec_flags == 1
            && raw_len > 1024 * 1024
            && raw_len > (payload.len() as u64).saturating_mul(budget.max_decompression_ratio)
        {
            return Err(SymbolError::BudgetExceeded(
                "symbol index decompression ratio exceeded".to_owned(),
            ));
        }
        let raw_len_usize = usize::try_from(raw_len)
            .map_err(|_| SymbolError::BudgetExceeded("symbol index too large".to_owned()))?;

        let raw_owned: Vec<u8>;
        let raw: &[u8] = if codec_flags == 1 {
            let buf = SYM_ZSTD_DCTX.with(|cell| -> Result<Vec<u8>, SymbolError> {
                let mut borrow = cell.borrow_mut();
                let decompressor = if let Some(d) = borrow.as_mut() {
                    d
                } else {
                    let d = zstd::bulk::Decompressor::new()
                        .map_err(|e| SymbolError::MalformedIndex(format!("zstd header: {e}")))?;
                    borrow.insert(d)
                };
                decompressor
                    .decompress(payload, raw_len_usize)
                    .map_err(|e| SymbolError::MalformedIndex(format!("zstd decode: {e}")))
            })?;
            if buf.len() != raw_len_usize {
                return Err(SymbolError::MalformedIndex(
                    "decompressed length mismatch".to_owned(),
                ));
            }
            raw_owned = buf;
            &raw_owned
        } else {
            if payload.len() != raw_len_usize {
                return Err(SymbolError::MalformedIndex(
                    "uncompressed length mismatch".to_owned(),
                ));
            }
            payload
        };

        decode_body(raw, budget)
    }
}

fn index_checksum(header_prefix: &[u8], payload: &[u8]) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = XxHash3_64::new();
    hasher.write(header_prefix);
    hasher.write(payload);
    hasher.finish()
}

#[expect(
    clippy::too_many_lines,
    reason = "decodes and validates every table of SymbolIndex in one pass under ParseBudget"
)]
fn decode_body(raw: &[u8], budget: &ParseBudget) -> Result<SymbolIndex, SymbolError> {
    let mut cur = Cursor::new(raw);
    let build_id = BuildId::parse(&cur.read_str(256)?)?;
    let arch = cur.read_str(64)?;
    let flags = cur.read_u8()?;
    if (flags & !0x07) != 0 {
        return Err(SymbolError::MalformedIndex(
            "unknown body feature flags".to_owned(),
        ));
    }
    let has_dwarf = (flags & 1) != 0;
    let has_symtab = (flags & 2) != 0;
    let has_gopclntab = (flags & 4) != 0;

    let string_count = cur.read_len(budget.max_functions.saturating_mul(4).max(65_536))?;
    let mut string_data = String::with_capacity(raw.len().min(budget.max_string_bytes) / 2);
    let mut string_offsets = Vec::with_capacity(string_count.min(65_536).saturating_add(1));
    string_offsets.push(0);
    let mut total_str_bytes = 0_usize;
    for _ in 0..string_count {
        let overhead = size_of::<u32>();
        let remaining = budget
            .max_string_bytes
            .checked_sub(total_str_bytes.saturating_add(overhead))
            .ok_or_else(|| SymbolError::BudgetExceeded("string table exceeds budget".to_owned()))?;
        let s = cur.read_str_slice(remaining)?;
        total_str_bytes = total_str_bytes
            .saturating_add(overhead)
            .saturating_add(s.len());
        string_data.push_str(s);
        let end = u32::try_from(string_data.len())
            .map_err(|_| SymbolError::BudgetExceeded("string table exceeds u32".to_owned()))?;
        string_offsets.push(end);
    }

    let file_count = cur.read_len(budget.max_files)?;
    let mut files = Vec::with_capacity(file_count.min(4096));
    for _ in 0..file_count {
        let sid = cur.read_u32()?;
        if sid != NO_INDEX && (sid as usize) >= string_count {
            return Err(SymbolError::MalformedIndex(
                "file string id out of bounds".to_owned(),
            ));
        }
        files.push(sid);
    }

    let func_count = cur.read_len(budget.max_functions)?;
    let mut functions = Vec::with_capacity(func_count.min(4096));
    for _ in 0..func_count {
        let name = cur.read_u32()?;
        let file = cur.read_u32()?;
        let decl_line = cur.read_u32()?;
        if (name as usize) >= string_count {
            return Err(SymbolError::MalformedIndex(
                "function name out of bounds".to_owned(),
            ));
        }
        if file != NO_INDEX && (file as usize) >= files.len() {
            return Err(SymbolError::MalformedIndex(
                "function file out of bounds".to_owned(),
            ));
        }
        functions.push(SymbolFunction {
            name,
            file,
            decl_line,
        });
    }

    let inline_limit = budget
        .max_spans
        .saturating_mul(budget.max_inline_depth.max(1));
    let inline_count = cur.read_len(inline_limit)?;
    let mut inlines = Vec::with_capacity(inline_count.min(4096));
    for _ in 0..inline_count {
        let function = cur.read_u32()?;
        let call_file = cur.read_u32()?;
        let call_line = cur.read_u32()?;
        if (function as usize) >= functions.len() {
            return Err(SymbolError::MalformedIndex(
                "inline function out of bounds".to_owned(),
            ));
        }
        if call_file != NO_INDEX && (call_file as usize) >= files.len() {
            return Err(SymbolError::MalformedIndex(
                "inline call_file out of bounds".to_owned(),
            ));
        }
        inlines.push(InlineFrame {
            function,
            call_file,
            call_line,
        });
    }

    let span_count = cur.read_len(budget.max_spans)?;
    let mut spans = Vec::with_capacity(span_count.min(4096));
    let mut prev_start = 0_u64;
    let mut prev_end = 0_u64;
    for i in 0..span_count {
        let delta = cur.read_varuint()?;
        let len = cur.read_varuint()?;
        let inline_start = cur.read_u32()?;
        let inline_count = cur.read_u32()?;
        let start = prev_start
            .checked_add(delta)
            .ok_or_else(|| SymbolError::MalformedIndex("span start overflow".to_owned()))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| SymbolError::MalformedIndex("span end overflow".to_owned()))?;
        if len == 0 || (i > 0 && start < prev_end) {
            return Err(SymbolError::MalformedIndex(
                "spans must be non-empty, sorted, and non-overlapping".to_owned(),
            ));
        }
        let inline_end = u64::from(inline_start).saturating_add(u64::from(inline_count));
        if inline_end > inlines.len() as u64 {
            return Err(SymbolError::MalformedIndex(
                "span inline range out of bounds".to_owned(),
            ));
        }
        spans.push(SymbolSpan {
            start,
            end,
            inline_start,
            inline_count,
        });
        prev_start = start;
        prev_end = end;
    }

    let line_count = cur.read_len(budget.max_line_spans)?;
    let mut line_spans = Vec::with_capacity(line_count.min(4096));
    let mut line_cursor_start = 0_u64;
    let mut line_cursor_end = 0_u64;
    for i in 0..line_count {
        let delta = cur.read_varuint()?;
        let len = cur.read_varuint()?;
        let file = cur.read_u32()?;
        let line = cur.read_u32()?;
        let column = cur.read_u32()?;
        let start = line_cursor_start
            .checked_add(delta)
            .ok_or_else(|| SymbolError::MalformedIndex("line span start overflow".to_owned()))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| SymbolError::MalformedIndex("line span end overflow".to_owned()))?;
        if len == 0 || (i > 0 && start < line_cursor_end) {
            return Err(SymbolError::MalformedIndex(
                "line spans must be non-empty, sorted, and non-overlapping".to_owned(),
            ));
        }
        if file != NO_INDEX && (file as usize) >= files.len() {
            return Err(SymbolError::MalformedIndex(
                "line span file out of bounds".to_owned(),
            ));
        }
        line_spans.push(LineSpan {
            start,
            end,
            file,
            line,
            column,
        });
        line_cursor_start = start;
        line_cursor_end = end;
    }

    let symtab_count = cur.read_len(budget.max_spans)?;
    let mut symtab_spans = Vec::with_capacity(symtab_count.min(4096));
    let mut sym_cursor_start = 0_u64;
    let mut sym_cursor_end = 0_u64;
    for i in 0..symtab_count {
        let delta = cur.read_varuint()?;
        let len = cur.read_varuint()?;
        let function = cur.read_u32()?;
        let start = sym_cursor_start
            .checked_add(delta)
            .ok_or_else(|| SymbolError::MalformedIndex("symtab span start overflow".to_owned()))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| SymbolError::MalformedIndex("symtab span end overflow".to_owned()))?;
        if len == 0 || (i > 0 && start < sym_cursor_end) {
            return Err(SymbolError::MalformedIndex(
                "symtab spans must be non-empty, sorted, and non-overlapping".to_owned(),
            ));
        }
        if (function as usize) >= functions.len() {
            return Err(SymbolError::MalformedIndex(
                "symtab span function out of bounds".to_owned(),
            ));
        }
        symtab_spans.push(SymtabSpan {
            start,
            end,
            function,
        });
        sym_cursor_start = start;
        sym_cursor_end = end;
    }

    if !cur.is_empty() {
        return Err(SymbolError::MalformedIndex(
            "trailing bytes in symbol index".to_owned(),
        ));
    }

    Ok(SymbolIndex {
        build_id,
        arch,
        has_dwarf,
        has_symtab,
        has_gopclntab,
        string_data,
        string_offsets,
        files,
        functions,
        inlines,
        spans,
        line_spans,
        symtab_spans,
    })
}

pub(super) struct IndexBuilder {
    string_data: String,
    string_offsets: Vec<u32>,
    string_map: HashMap<String, u32>,
    string_bytes: usize,
    files: Vec<u32>,
    file_map: HashMap<u32, u32>,
    functions: Vec<SymbolFunction>,
    function_map: HashMap<(u32, u32, u32), u32>,
    budget: ParseBudget,
}

impl IndexBuilder {
    pub(super) fn new(budget: ParseBudget) -> Self {
        Self {
            string_data: String::new(),
            string_offsets: vec![0],
            string_map: HashMap::default(),
            string_bytes: 0,
            files: Vec::new(),
            file_map: HashMap::default(),
            functions: Vec::new(),
            function_map: HashMap::default(),
            budget,
        }
    }

    pub(super) fn intern_string(&mut self, text: &str) -> Result<u32, SymbolError> {
        if let Some(&id) = self.string_map.get(text) {
            return Ok(id);
        }
        let next_bytes = self.string_bytes.saturating_add(text.len());
        if next_bytes > self.budget.max_string_bytes {
            return Err(SymbolError::BudgetExceeded(
                "symbol string table exceeded budget".to_owned(),
            ));
        }
        let id = u32::try_from(self.string_offsets.len().saturating_sub(1))
            .map_err(|_| SymbolError::BudgetExceeded("too many symbol strings".to_owned()))?;
        self.string_data.push_str(text);
        let end = u32::try_from(self.string_data.len()).map_err(|_| {
            SymbolError::BudgetExceeded("symbol string table exceeds u32".to_owned())
        })?;
        self.string_offsets.push(end);
        self.string_map.insert(text.to_owned(), id);
        self.string_bytes = next_bytes;
        Ok(id)
    }

    pub(super) fn intern_file(&mut self, path: &str) -> Result<u32, SymbolError> {
        let str_id = self.intern_string(path)?;
        if let Some(&id) = self.file_map.get(&str_id) {
            return Ok(id);
        }
        if self.files.len() >= self.budget.max_files {
            return Err(SymbolError::BudgetExceeded(
                "symbol file table exceeded budget".to_owned(),
            ));
        }
        let id = u32::try_from(self.files.len())
            .map_err(|_| SymbolError::BudgetExceeded("too many symbol files".to_owned()))?;
        self.files.push(str_id);
        self.file_map.insert(str_id, id);
        Ok(id)
    }

    pub(super) fn intern_function(
        &mut self,
        name: &str,
        file: u32,
        decl_line: u32,
    ) -> Result<u32, SymbolError> {
        let name_id = self.intern_string(name)?;
        let key = (name_id, file, decl_line);
        if let Some(&id) = self.function_map.get(&key) {
            return Ok(id);
        }
        if self.functions.len() >= self.budget.max_functions {
            return Err(SymbolError::BudgetExceeded(
                "symbol function table exceeded budget".to_owned(),
            ));
        }
        let id = u32::try_from(self.functions.len())
            .map_err(|_| SymbolError::BudgetExceeded("too many symbol functions".to_owned()))?;
        self.functions.push(SymbolFunction {
            name: name_id,
            file,
            decl_line,
        });
        self.function_map.insert(key, id);
        Ok(id)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "assembles finalized non-overlapping tables into SymbolIndex"
    )]
    pub(super) fn finish(
        self,
        build_id: BuildId,
        arch: String,
        has_dwarf: bool,
        has_symtab: bool,
        has_gopclntab: bool,
        inlines: Vec<InlineFrame>,
        spans: Vec<SymbolSpan>,
        line_spans: Vec<LineSpan>,
        symtab_spans: Vec<SymtabSpan>,
    ) -> SymbolIndex {
        SymbolIndex {
            build_id,
            arch,
            has_dwarf,
            has_symtab,
            has_gopclntab,
            string_data: self.string_data,
            string_offsets: self.string_offsets,
            files: self.files,
            functions: self.functions,
            inlines,
            spans,
            line_spans,
            symtab_spans,
        }
    }
}

#[inline]
fn write_varuint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

#[inline]
fn write_str(out: &mut Vec<u8>, text: &str) {
    write_varuint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    const fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    #[inline]
    fn read_u8(&mut self) -> Result<u8, SymbolError> {
        let byte = *self
            .data
            .get(self.pos)
            .ok_or_else(|| SymbolError::MalformedIndex("unexpected EOF".to_owned()))?;
        self.pos += 1;
        Ok(byte)
    }

    #[cold]
    #[inline(never)]
    fn read_varuint_slow(&mut self) -> Result<u64, SymbolError> {
        let mut result = 0_u64;
        let mut shift = 0_u32;
        for _ in 0..10 {
            let byte = self.read_u8()?;
            let low = u64::from(byte & 0x7f);
            if shift == 63 && low > 1 {
                return Err(SymbolError::MalformedIndex("varint overflow".to_owned()));
            }
            result |= low << shift;
            if byte < 0x80 {
                return Ok(result);
            }
            shift += 7;
        }
        Err(SymbolError::MalformedIndex("varint too long".to_owned()))
    }

    #[inline]
    fn read_varuint(&mut self) -> Result<u64, SymbolError> {
        let pos = self.pos;
        let Some(&b0) = self.data.get(pos) else {
            return Err(SymbolError::MalformedIndex("unexpected EOF".to_owned()));
        };
        if b0 < 0x80 {
            self.pos = pos + 1;
            return Ok(u64::from(b0));
        }
        let Some(&b1) = self.data.get(pos + 1) else {
            return Err(SymbolError::MalformedIndex("unexpected EOF".to_owned()));
        };
        if b1 < 0x80 {
            self.pos = pos + 2;
            return Ok(u64::from(b0 & 0x7f) | (u64::from(b1) << 7));
        }
        self.read_varuint_slow()
    }

    #[inline]
    fn read_u32(&mut self) -> Result<u32, SymbolError> {
        let val = self.read_varuint()?;
        u32::try_from(val).map_err(|_| SymbolError::MalformedIndex("u32 overflow".to_owned()))
    }

    #[inline]
    fn read_len(&mut self, max: usize) -> Result<usize, SymbolError> {
        let val = self.read_varuint()?;
        let len = usize::try_from(val)
            .map_err(|_| SymbolError::BudgetExceeded("length overflow".to_owned()))?;
        if len > max {
            return Err(SymbolError::BudgetExceeded(
                "decoded table length exceeds budget".to_owned(),
            ));
        }
        Ok(len)
    }

    #[inline]
    fn read_str_slice(&mut self, max_len: usize) -> Result<&'a str, SymbolError> {
        let len = self.read_len(max_len)?;
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| SymbolError::MalformedIndex("string slice overflow".to_owned()))?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| SymbolError::MalformedIndex("truncated string".to_owned()))?;
        self.pos = end;
        std::str::from_utf8(slice)
            .map_err(|_| SymbolError::MalformedIndex("non-UTF-8 string".to_owned()))
    }

    fn read_str(&mut self, max_len: usize) -> Result<String, SymbolError> {
        self.read_str_slice(max_len).map(str::to_owned)
    }
}
