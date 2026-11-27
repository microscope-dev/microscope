//! Parser for Go's `.gopclntab` function, line, inline, and stack-delta tables.
//!
//! Supports the Go 1.20+ (`0xffff_fff1`) 64-bit little-endian format and can locate
//! tables in stripped or PIE binaries by scanning for `runtime.moduledata` when
//! section names or `runtime.text` are absent.

mod metadata;
mod unwind;

pub use metadata::{Catalog, complete as complete_metadata};
pub use unwind::{GoUnwind, frame_saves};

use crate::image::unwind::GoTableFacts;
use std::ops::Range;
use std::sync::Arc;

const GO120_MAGIC: u32 = 0xffff_fff1;

const HEADER_SIZE: usize = 8 + 8 * 8;

const FUNC_SIZE: usize = 44;

const INLINED_CALL_SIZE: usize = 16;

const PCDATA_UNSAFE_POINT: usize = 0;

const PCDATA_INLINE_TREE_INDEX: usize = 2;

const FUNCDATA_INLINE_TREE: usize = 3;

const FLAG_TOP_FRAME: u8 = 1 << 0;

const FLAG_SP_WRITE: u8 = 1 << 1;

const FLAG_ASM: u8 = 1 << 2;

const MAX_VARINT_BYTES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PclntabError {
    #[error("the Go function table is truncated: {0}")]
    Truncated(&'static str),

    #[error("the Go function table was written by {0}, whose format is not supported")]
    UnsupportedRelease(&'static str),

    #[error("the Go function table's magic number {0:#010x} is unknown")]
    UnknownMagic(u32),

    #[error("the Go function table is malformed: {0}")]
    Malformed(Arc<str>),

    #[error("the Go function table's code addresses are relative to an unknown text start")]
    TextStartUnknown,
}

type Result<T> = std::result::Result<T, PclntabError>;

fn malformed(reason: impl Into<Arc<str>>) -> PclntabError {
    PclntabError::Malformed(reason.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialFunction {
    Abort,
    AsmCgoCall,
    AsyncPreempt,
    CgoCallback,
    CoroStart,
    DebugCallV2,
    GcBgMarkWorker,
    Goexit,
    Gogo,
    Gopanic,
    HandleAsyncEvent,
    Mcall,
    Morestack,
    Mstart,
    Panicwrap,
    Rt0Go,
    RuntimeMain,
    RunFinalizers,
    RunCleanups,
    Sigpanic,
    Systemstack,
    SystemstackSwitch,

    Wrapper,
}

/// Version-specific mapping for Go's internal `abi.FuncID` enum.
///
/// Go renumbers `FuncID` across releases, so `SpecialFunction` is only decoded
/// when `.go.buildinfo` matches a release with a verified table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FuncIdNumbering {
    Go125,
}

impl FuncIdNumbering {
    const fn of(release: GoRelease) -> Option<Self> {
        match release {
            GoRelease {
                major: 1,
                minor: 25..=27,
            } => Some(Self::Go125),
            _ => None,
        }
    }

    const fn special(self, id: u8) -> Option<SpecialFunction> {
        use SpecialFunction as S;
        match self {
            Self::Go125 => Some(match id {
                1 => S::Abort,
                2 => S::AsmCgoCall,
                3 => S::AsyncPreempt,
                4 => S::CgoCallback,
                5 => S::CoroStart,
                6 => S::DebugCallV2,
                7 => S::GcBgMarkWorker,
                8 => S::Goexit,
                9 => S::Gogo,
                10 => S::Gopanic,
                11 => S::HandleAsyncEvent,
                12 => S::Mcall,
                13 => S::Morestack,
                14 => S::Mstart,
                15 => S::Panicwrap,
                16 => S::Rt0Go,
                17 => S::RuntimeMain,
                18 => S::RunFinalizers,
                19 => S::RunCleanups,
                20 => S::Sigpanic,
                21 => S::Systemstack,
                22 => S::SystemstackSwitch,
                23 => S::Wrapper,
                _ => return None,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GoRelease {
    pub major: u32,
    pub minor: u32,
}

impl GoRelease {
    fn parse(version: &str) -> Option<Self> {
        let rest = version.strip_prefix("go")?;
        let mut parts = rest.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?;
        let digits = minor
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(minor.len());
        Some(Self {
            major,
            minor: minor[..digits].parse().ok()?,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoFunctionFacts {
    pub special: Option<SpecialFunction>,

    pub top_frame: bool,

    pub sp_write: bool,

    pub assembly: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoFunction {
    pub entry: u64,

    pub end: u64,
    pub facts: GoFunctionFacts,

    pub start_line: i32,
    name: NameOffset,
    pcsp: PcTable,
    pcfile: PcTable,
    pcln: PcTable,
    unit: CuIndex,

    pcdata: usize,
    npcdata: u32,
    nfuncdata: u8,
}

impl GoFunction {
    /// Go emits a non-zero `pcsp` table for every Go-compiled or Go-assembly
    /// function; external C/cgo stubs leave `pcsp` at zero.
    pub const fn is_go(&self) -> bool {
        self.pcsp.0 != 0
    }

    pub const fn contains(&self, address: u64) -> bool {
        self.entry <= address && address < self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlinedCall {
    pub special: Option<SpecialFunction>,
    name: NameOffset,

    pub parent_pc: u64,

    pub start_line: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NameOffset(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PcTable(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CuIndex(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcRun {
    pub start: u64,
    pub end: u64,
    pub value: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoPosition {
    pub file: Arc<str>,
    pub line: i32,
}

#[derive(Debug)]
pub struct GoTable {
    data: Arc<[u8]>,
    quantum: u32,
    funcnametab: Range<usize>,
    cutab: Range<usize>,
    filetab: Range<usize>,
    pctab: Range<usize>,
    pclntable: Range<usize>,

    gofunc: Option<usize>,
    numbering: Option<FuncIdNumbering>,
    functions: Vec<GoFunction>,

    facts: GoTableFacts,
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    let bytes = data.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

fn read_varint(data: &[u8], offset: &mut usize) -> Result<u32> {
    let mut value = 0_u32;
    for shift in (0_u32..).step_by(7).take(MAX_VARINT_BYTES) {
        let byte = *data
            .get(*offset)
            .ok_or(PclntabError::Truncated("a pc-value table"))?;
        *offset += 1;
        let bits = u32::from(byte & 0x7f);
        if shift == 28 && bits > 0xf {
            return Err(malformed("a pc-value delta exceeds 32 bits"));
        }

        value |= bits << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }

    Err(malformed("a pc-value delta exceeds 32 bits"))
}

/// Per Go's `pcvalue` format, the running value starts at `-1`, each value delta
/// is zig-zag encoded, and a zero delta terminates the table except at `entry`.
#[derive(Debug, Clone)]
pub struct PcRuns<'a> {
    data: &'a [u8],
    offset: usize,
    pc: u64,
    entry: u64,
    value: i32,
    quantum: u32,
    done: bool,
}

impl Iterator for PcRuns<'_> {
    type Item = Result<PcRun>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let step = self.step();
        if !matches!(step, Ok(Some(_))) {
            self.done = true;
        }

        step.transpose()
    }
}

impl PcRuns<'_> {
    fn step(&mut self) -> Result<Option<PcRun>> {
        let delta = read_varint(self.data, &mut self.offset)?;
        if delta == 0 && self.pc != self.entry {
            return Ok(None);
        }

        let delta = if delta & 1 == 0 {
            (delta >> 1).cast_signed()
        } else {
            (!(delta >> 1)).cast_signed()
        };

        let pc_delta = read_varint(self.data, &mut self.offset)?
            .checked_mul(self.quantum)
            .ok_or_else(|| malformed("a pc-value table's pc overflows"))?;
        let start = self.pc;
        self.pc = start
            .checked_add(u64::from(pc_delta))
            .ok_or_else(|| malformed("a pc-value table's pc overflows"))?;
        self.value = self
            .value
            .checked_add(delta)
            .ok_or_else(|| malformed("a pc-value table's value overflows"))?;

        Ok(Some(PcRun {
            start,
            end: self.pc,
            value: self.value,
        }))
    }
}

impl GoTable {
    pub fn parse(
        data: Arc<[u8]>,
        address: u64,
        text: u64,
        gofunc: Option<u64>,
        release: Option<GoRelease>,
    ) -> Result<Self> {
        check_magic(&data)?;
        let Some(&[.., pad_low, pad_high, quantum, pointer_size]) = data
            .first_chunk::<8>()
            .filter(|_| data.len() >= HEADER_SIZE)
        else {
            return Err(PclntabError::Truncated("the header"));
        };

        let quantum = u32::from(quantum);
        if pad_low != 0 || pad_high != 0 || !matches!(quantum, 1 | 2 | 4) {
            return Err(malformed(
                "the header's padding or instruction size is invalid",
            ));
        }

        if pointer_size != 8 {
            return Err(PclntabError::UnsupportedRelease(
                "a toolchain for 32-bit pointers",
            ));
        }

        let word = |index: usize| {
            read_u64(&data, 8 + 8 * index)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or(PclntabError::Truncated("the header"))
        };

        let nfunc = word(0)?;

        let offsets = [word(3)?, word(4)?, word(5)?, word(6)?, word(7)?, data.len()];
        if offsets[0] < HEADER_SIZE
            || offsets
                .windows(2)
                .any(|pair| matches!(pair, [before, after] if before > after))
        {
            return Err(malformed("the header's table offsets are out of order"));
        }

        let facts = GoTableFacts {
            address,
            text,
            go_func: gofunc,
            release: release.map(|release| (release.major, release.minor)),
        };

        let mut table = Self {
            facts,
            quantum,
            funcnametab: offsets[0]..offsets[1],
            cutab: offsets[1]..offsets[2],
            filetab: offsets[2]..offsets[3],
            pctab: offsets[3]..offsets[4],
            pclntable: offsets[4]..offsets[5],
            gofunc: gofunc
                .and_then(|gofunc| gofunc.checked_sub(address))
                .and_then(|offset| usize::try_from(offset).ok())
                .filter(|offset| *offset < data.len()),
            numbering: release.and_then(FuncIdNumbering::of),
            functions: Vec::new(),
            data,
        };

        table.functions = table.decode_functions(nfunc, text)?;

        Ok(table)
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`parse` checked that the tables lie in order within the data"
    )]
    fn table(&self, range: &Range<usize>) -> &[u8] {
        debug_assert!(
            range.start <= range.end && range.end <= self.data.len(),
            "a table at {range:?} is outside the {} bytes of data",
            self.data.len(),
        );

        &self.data[range.clone()]
    }

    fn decode_functions(&self, nfunc: usize, text: u64) -> Result<Vec<GoFunction>> {
        let pclntable = self.table(&self.pclntable);
        if nfunc
            .checked_mul(8)
            .and_then(|size| size.checked_add(4))
            .is_none_or(|size| size > pclntable.len())
        {
            return Err(PclntabError::Truncated("the function table"));
        }

        let entry = |index: usize| -> Result<u64> {
            let offset = read_u32(pclntable, 8 * index)
                .ok_or(PclntabError::Truncated("the function table"))?;
            text.checked_add(u64::from(offset))
                .ok_or_else(|| malformed("a function's address overflows"))
        };

        let mut functions = Vec::with_capacity(nfunc);
        if nfunc == 0 {
            return Ok(functions);
        }

        let mut start = entry(0)?;

        for index in 0..nfunc {
            let end = entry(index + 1)?;
            if end < start {
                return Err(malformed("the function table is not sorted by address"));
            }

            let record = read_u32(pclntable, 8 * index + 4)
                .and_then(|offset| usize::try_from(offset).ok())
                .ok_or(PclntabError::Truncated("the function table"))?;
            functions.push(self.decode_function(pclntable, record, start, end, text)?);
            start = end;
        }

        Ok(functions)
    }

    fn decode_function(
        &self,
        pclntable: &[u8],
        record: usize,
        entry: u64,
        end: u64,
        text: u64,
    ) -> Result<GoFunction> {
        let fixed = record
            .checked_add(FUNC_SIZE)
            .and_then(|end| pclntable.get(record..end))
            .and_then(|fixed| <&[u8; FUNC_SIZE]>::try_from(fixed).ok())
            .ok_or(PclntabError::Truncated("a function record"))?;
        let field = |index: usize| {
            read_u32(fixed, 4 * index).ok_or(PclntabError::Truncated("a function record"))
        };

        if text.checked_add(u64::from(field(0)?)) != Some(entry) {
            return Err(malformed(
                "a function record's entry differs from the function table's",
            ));
        }

        let npcdata = field(7)?;
        let nfuncdata = fixed[43];
        let trailing = usize::try_from(npcdata)
            .ok()
            .and_then(|npcdata| npcdata.checked_add(usize::from(nfuncdata)))
            .and_then(|count| count.checked_mul(4))
            .ok_or(PclntabError::Truncated("a function record"))?;
        let pcdata = self.pclntable.start + record + FUNC_SIZE;
        if pcdata
            .checked_add(trailing)
            .is_none_or(|end| end > self.pclntable.end)
        {
            return Err(PclntabError::Truncated("a function record"));
        }

        let name = NameOffset(field(1)?);
        if usize::try_from(name.0).map_or(true, |name| name >= self.funcnametab.len()) {
            return Err(malformed("a function's name is outside the name table"));
        }

        let flags = fixed[41];

        Ok(GoFunction {
            entry,
            end,
            facts: GoFunctionFacts {
                special: self
                    .numbering
                    .and_then(|numbering| numbering.special(fixed[40])),
                top_frame: flags & FLAG_TOP_FRAME != 0,
                sp_write: flags & FLAG_SP_WRITE != 0,
                assembly: flags & FLAG_ASM != 0,
            },
            start_line: field(9)?.cast_signed(),
            name,
            pcsp: PcTable(field(4)?),
            pcfile: PcTable(field(5)?),
            pcln: PcTable(field(6)?),
            unit: CuIndex(field(8)?),
            pcdata,
            npcdata,
            nfuncdata,
        })
    }

    pub fn reparse(data: Arc<[u8]>, facts: GoTableFacts) -> Result<Self> {
        Self::parse(
            data,
            facts.address,
            facts.text,
            facts.go_func,
            facts
                .release
                .map(|(major, minor)| GoRelease { major, minor }),
        )
    }

    pub const fn source(&self) -> (&Arc<[u8]>, GoTableFacts) {
        (&self.data, self.facts)
    }

    pub fn functions(&self) -> &[GoFunction] {
        &self.functions
    }

    pub fn function_containing(&self, address: u64) -> Option<&GoFunction> {
        let after = self
            .functions
            .partition_point(|function| function.entry <= address);
        after
            .checked_sub(1)
            .and_then(|last| self.functions.get(last))
            .filter(|function| function.contains(address))
    }

    fn string(&self, table: &Range<usize>, offset: usize) -> Result<Arc<str>> {
        let bytes = self
            .table(table)
            .get(offset..)
            .ok_or_else(|| malformed("a string is outside its table"))?;
        let string = bytes
            .split(|byte| *byte == 0)
            .next()
            .filter(|string| string.len() < bytes.len())
            .ok_or(PclntabError::Truncated("a string"))?;

        Ok(String::from_utf8_lossy(string).into())
    }

    pub fn name(&self, function: &GoFunction) -> Result<Arc<str>> {
        self.function_name(function.name)
    }

    pub fn inlined_name(&self, call: &InlinedCall) -> Result<Arc<str>> {
        self.function_name(call.name)
    }

    fn function_name(&self, offset: NameOffset) -> Result<Arc<str>> {
        self.string(&self.funcnametab, offset.0 as usize)
    }

    fn pcdata(&self, function: &GoFunction, index: usize) -> Option<PcTable> {
        (index < function.npcdata as usize)
            .then(|| read_u32(&self.data, function.pcdata + 4 * index).map(PcTable))
            .flatten()
    }

    fn runs(&self, function: &GoFunction, table: PcTable) -> Option<PcRuns<'_>> {
        if table.0 == 0 {
            return None;
        }

        Some(PcRuns {
            data: self.table(&self.pctab).get(table.0 as usize..)?,
            offset: 0,
            pc: function.entry,
            entry: function.entry,
            value: -1,
            quantum: self.quantum,
            done: false,
        })
    }

    fn value_at(&self, function: &GoFunction, table: PcTable, address: u64) -> Result<Option<i32>> {
        let Some(runs) = self.runs(function, table) else {
            return if table.0 == 0 {
                Ok(None)
            } else {
                Err(malformed("a pc-value table is outside the table"))
            };
        };

        for run in runs {
            let run = run?;
            if run.start >= function.end {
                break;
            }

            if address < run.end {
                return Ok((run.start <= address).then_some(run.value));
            }
        }

        Ok(None)
    }

    pub fn preemptible(&self, function: &GoFunction, address: u64) -> Result<bool> {
        const UNSAFE: i32 = -2;
        let Some(table) = self.pcdata(function, PCDATA_UNSAFE_POINT) else {
            return Ok(true);
        };

        Ok(self.value_at(function, table, address)? != Some(UNSAFE))
    }

    pub fn grows_stacks(&self, address: u64) -> bool {
        self.function_containing(address).is_some_and(|function| {
            function.entry == address
                && (function.facts.special == Some(SpecialFunction::Morestack)
                    || self
                        .name(function)
                        .is_ok_and(|name| name.starts_with("runtime.morestack")))
        })
    }

    pub fn restarts_at_entry(&self, function: &GoFunction) -> Result<bool> {
        const RESTART_AT_ENTRY: i32 = -5;
        let Some(runs) = self
            .pcdata(function, PCDATA_UNSAFE_POINT)
            .and_then(|table| self.runs(function, table))
        else {
            return Ok(false);
        };

        for run in clipped(runs, function)? {
            if run.value == RESTART_AT_ENTRY {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub fn sp_runs(&self, function: &GoFunction) -> Option<PcRuns<'_>> {
        self.runs(function, function.pcsp)
    }

    pub fn sp_delta(&self, function: &GoFunction, address: u64) -> Result<Option<i32>> {
        self.value_at(function, function.pcsp, address)
    }

    fn file(&self, function: &GoFunction, index: i32) -> Result<Option<Arc<str>>> {
        let Ok(index) = u32::try_from(index) else {
            return Ok(None);
        };

        let slot = function
            .unit
            .0
            .checked_add(index)
            .and_then(|slot| usize::try_from(slot).ok())
            .and_then(|slot| slot.checked_mul(4))
            .and_then(|offset| read_u32(self.table(&self.cutab), offset))
            .ok_or_else(|| malformed("a file index is outside the compile-unit table"))?;
        if slot == u32::MAX {
            return Ok(None);
        }

        self.string(&self.filetab, slot as usize).map(Some)
    }

    pub fn positions(&self, function: &GoFunction) -> Result<Vec<(Range<u64>, GoPosition)>> {
        let (Some(files), Some(lines)) = (
            self.runs(function, function.pcfile),
            self.runs(function, function.pcln),
        ) else {
            return Ok(Vec::new());
        };

        let files = clipped(files, function)?;
        let lines = clipped(lines, function)?;
        let mut positions: Vec<(Range<u64>, GoPosition)> = Vec::new();
        let mut file_names = Vec::<(i32, Option<Arc<str>>)>::new();
        let (mut file, mut line) = (files.iter().peekable(), lines.iter().peekable());
        let mut pc = function.entry;

        while let (Some(file_run), Some(line_run)) = (file.peek(), line.peek()) {
            let end = file_run.end.min(line_run.end);
            let name = if let Some((_, name)) = file_names
                .iter()
                .find(|(index, _)| *index == file_run.value)
            {
                name.clone()
            } else {
                let name = self.file(function, file_run.value)?;
                file_names.push((file_run.value, name.clone()));
                name
            };

            if let Some(name) = name
                && line_run.value > 0
                && pc < end
            {
                let position = GoPosition {
                    file: name,
                    line: line_run.value,
                };

                match positions.last_mut() {
                    Some((range, last)) if range.end == pc && *last == position => range.end = end,
                    _ => positions.push((pc..end, position)),
                }
            }

            pc = end;
            if file_run.end == end {
                file.next();
            }

            if line_run.end == end {
                line.next();
            }
        }

        Ok(positions)
    }

    pub fn position(&self, function: &GoFunction, address: u64) -> Result<Option<GoPosition>> {
        let (Some(file), Some(line)) = (
            self.value_at(function, function.pcfile, address)?,
            self.value_at(function, function.pcln, address)?,
        ) else {
            return Ok(None);
        };

        Ok(self
            .file(function, file)?
            .map(|file| GoPosition { file, line }))
    }

    pub fn inline_runs(&self, function: &GoFunction) -> Result<Vec<PcRun>> {
        let Some(table) = self.pcdata(function, PCDATA_INLINE_TREE_INDEX) else {
            return Ok(Vec::new());
        };

        match self.runs(function, table) {
            Some(runs) => clipped(runs, function),
            None if table.0 == 0 => Ok(Vec::new()),
            None => Err(malformed("a pc-value table is outside the table")),
        }
    }

    pub fn inlined_call(&self, function: &GoFunction, index: i32) -> Result<InlinedCall> {
        if function.nfuncdata as usize <= FUNCDATA_INLINE_TREE {
            return Err(malformed("an inline-tree index names no inline tree"));
        }

        let tree = read_u32(
            &self.data,
            function.pcdata + 4 * (function.npcdata as usize + FUNCDATA_INLINE_TREE),
        )
        .ok_or(PclntabError::Truncated("an inline tree"))?;
        if tree == u32::MAX {
            return Err(malformed("an inline-tree index names no inline tree"));
        }

        let gofunc = self
            .gofunc
            .ok_or_else(|| malformed("the inline trees' base address is unknown"))?;
        let record = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(INLINED_CALL_SIZE))
            .and_then(|offset| offset.checked_add(gofunc)?.checked_add(tree as usize))
            .and_then(|start| self.data.get(start..start.checked_add(INLINED_CALL_SIZE)?))
            .and_then(|record| <&[u8; INLINED_CALL_SIZE]>::try_from(record).ok())
            .ok_or(PclntabError::Truncated("an inline tree"))?;
        let field = |index: usize| {
            read_u32(record, 4 * index).ok_or(PclntabError::Truncated("an inline tree"))
        };

        let name = NameOffset(field(1)?);
        if name.0 as usize >= self.funcnametab.len() {
            return Err(malformed(
                "an inlined function's name is outside the name table",
            ));
        }

        let parent_pc = function
            .entry
            .checked_add_signed(i64::from(field(2)?.cast_signed()))
            .filter(|parent| function.contains(*parent))
            .ok_or_else(|| malformed("an inlined call's site is outside its function"))?;

        Ok(InlinedCall {
            special: self
                .numbering
                .and_then(|numbering| numbering.special(record[0])),
            name,
            parent_pc,
            start_line: field(3)?.cast_signed(),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prologue {
    pub frame_pointer_saved: Option<Range<u64>>,

    pub end: Option<u64>,
}

impl GoTable {
    /// `.gopclntab` records SP deltas but not frame-pointer saves, and stripped
    /// binaries lack DWARF prologue markers; matching `push rbp; mov rbp, rsp`
    /// (`0x55; 0x48 0x89 0xe5`) at the first non-zero SP delta recovers both.
    pub fn prologue<'code>(
        &self,
        function: &GoFunction,
        code: impl Fn(u64, usize) -> Option<&'code [u8]>,
    ) -> Result<Prologue> {
        let Some(runs) = self.sp_runs(function) else {
            return Ok(Prologue::default());
        };

        let runs = clipped(runs, function)?;
        let Some((pushed, after)) = runs
            .iter()
            .position(|run| run.value != 0)
            .and_then(|first| runs.get(first..))
            .and_then(<[PcRun]>::split_first)
        else {
            return Ok(Prologue::default());
        };

        let saved = pushed.value == 8
            && pushed.start > function.entry
            && code(pushed.start - 1, 1) == Some(&[0x55][..])
            && code(pushed.start, 3) == Some(&[0x48, 0x89, 0xe5][..]);
        if !saved {
            return Ok(Prologue::default());
        }

        let assembly = function.facts.assembly;
        let saved_until = if assembly {
            after
                .iter()
                .find(|run| run.value == 0)
                .map_or(function.end, |run| run.start)
        } else {
            function.end
        };

        let framed = pushed.start + 3;
        let allocates_locals = after.first().is_some_and(|locals| {
            locals.start == pushed.end
                && locals.value > 8
                && locals
                    .start
                    .checked_sub(framed)
                    .and_then(|length| code(framed, usize::try_from(length).ok()?))
                    .is_some_and(allocates_stack)
        });

        Ok(Prologue {
            frame_pointer_saved: Some(pushed.start..saved_until),
            end: (!assembly).then_some(if allocates_locals {
                framed
            } else {
                pushed.start
            }),
        })
    }
}

const fn allocates_stack(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        [0x48, 0x83, 0xec | 0xc4, _] | [0x48, 0x81, 0xec | 0xc4, _, _, _, _]
    )
}

fn clipped(runs: PcRuns<'_>, function: &GoFunction) -> Result<Vec<PcRun>> {
    let mut clipped = Vec::new();
    for run in runs {
        let run = run?;
        if run.start >= function.end {
            break;
        }

        if run.start < run.end {
            clipped.push(PcRun {
                end: run.end.min(function.end),
                ..run
            });
        }
    }

    Ok(clipped)
}

fn check_magic(data: &[u8]) -> Result<()> {
    let magic = read_u32(data, 0).ok_or(PclntabError::Truncated("the header"))?;
    match magic {
        GO120_MAGIC => Ok(()),
        0xffff_fffb => Err(PclntabError::UnsupportedRelease("Go 1.2 through 1.15")),
        0xffff_fffa => Err(PclntabError::UnsupportedRelease("Go 1.16 and 1.17")),
        0xffff_fff0 => Err(PclntabError::UnsupportedRelease("Go 1.18 and 1.19")),
        _ if magic.swap_bytes() == GO120_MAGIC => Err(PclntabError::UnsupportedRelease(
            "a toolchain for a big-endian target",
        )),
        _ => Err(PclntabError::UnknownMagic(magic)),
    }
}

struct FoundTable<'data> {
    address: u64,
    data: &'data [u8],

    section_end: Option<u64>,
}

pub fn load(object: &object::File<'_>) -> Result<Option<GoTable>> {
    let Some(found) = find_table(object)? else {
        return Ok(None);
    };

    check_magic(found.data)?;
    let header = |index: usize| read_u64(found.data, 8 + 8 * index);
    let module = ModuleWords::new(object).find_moduledata(&found, header);

    let text = module
        .as_ref()
        .map(|module| module.text)
        .or_else(|| symbol_address(object, "runtime.text"))
        .ok_or(PclntabError::TextStartUnknown)?;
    let table = GoTable::parse(
        found.data.into(),
        found.address,
        text,
        module.and_then(|module| module.gofunc),
        go_release(object),
    )?;
    let code = super::elf::executable_ranges(object);
    let within_code = |function: &GoFunction| {
        code.iter()
            .any(|range| range.start.get() <= function.entry && function.end <= range.end.get())
    };

    if !table.functions().iter().all(within_code) {
        return Err(malformed("functions lie outside the image's code"));
    }

    Ok(Some(table))
}

/// Finds `.gopclntab` by section name, falling back to an 8-byte-aligned header
/// scan of non-executable allocated sections when `.go.buildinfo` is present.
///
/// Stripped or custom-linked Go binaries may fold `.gopclntab` into `.rodata`
/// without preserving its section name.
fn find_table<'data>(object: &object::File<'data>) -> Result<Option<FoundTable<'data>>> {
    use object::{Object, ObjectSection};

    for name in [".gopclntab", ".data.rel.ro.gopclntab"] {
        if let Some(section) = object.section_by_name(name) {
            let data = section.data().map_err(|error| {
                malformed(format!("the table's section is unreadable: {error}"))
            })?;
            return Ok(Some(FoundTable {
                address: section.address(),
                data,
                section_end: section.address().checked_add(section.size()),
            }));
        }
    }

    if object.section_by_name(".go.buildinfo").is_none() {
        return Ok(None);
    }

    for section in object.sections() {
        let object::SectionFlags::Elf { sh_flags } = section.flags() else {
            continue;
        };

        let allocated = u64::from(object::elf::SHF_ALLOC);
        let executable = u64::from(object::elf::SHF_EXECINSTR);
        if sh_flags & (allocated | executable) != allocated {
            continue;
        }

        let Ok(data) = section.data() else {
            continue;
        };

        let start = section.address();

        for offset in (aligned_offset(start)..data.len()).step_by(8) {
            if let Some(candidate) = data.get(offset..)
                && let Some(address) = start.checked_add(offset as u64)
                && plausible_header(candidate)
            {
                return Ok(Some(FoundTable {
                    address,
                    data: candidate,
                    section_end: None,
                }));
            }
        }
    }

    Ok(None)
}

const fn aligned_offset(start: u64) -> usize {
    ((8 - start % 8) % 8) as usize
}

fn plausible_header(data: &[u8]) -> bool {
    if read_u32(data, 0) != Some(GO120_MAGIC)
        || data
            .get(4..8)
            .is_none_or(|bytes| !matches!(bytes, [0, 0, _, 8]))
    {
        return false;
    }

    let words = (3..8)
        .map(|index| read_u64(data, 8 + 8 * index))
        .collect::<Option<Vec<_>>>();
    words.is_some_and(|words| {
        matches!(words.as_slice(), [first, .., last]
            if *first >= HEADER_SIZE as u64 && *last <= data.len() as u64)
            && words
                .windows(2)
                .all(|pair| matches!(pair, [before, after] if before <= after))
    })
}

fn symbol_address(object: &object::File<'_>, name: &str) -> Option<u64> {
    use object::{Object, ObjectSymbol};

    object
        .symbols()
        .find(|symbol| symbol.name_bytes().ok() == Some(name.as_bytes()))
        .map(|symbol| symbol.address())
}

struct ModuleData {
    text: u64,
    gofunc: Option<u64>,
}

struct ModuleWords<'data> {
    sections: Vec<(u64, &'data [u8])>,
    relative: std::collections::BTreeMap<u64, u64>,
}

const MODULE_PC_HEADER: u64 = 0;
const MODULE_FUNCNAMETAB: u64 = 1;
const MODULE_FTAB: u64 = 16;
const MODULE_FTAB_LENGTH: u64 = 17;
const MODULE_MINPC: u64 = 20;
const MODULE_TEXT: u64 = 22;

const MODULE_LATER_FIELDS: Range<u64> = 24..96;

impl<'data> ModuleWords<'data> {
    fn new(object: &object::File<'data>) -> Self {
        use object::{Object, ObjectSection};

        let writable = u64::from(object::elf::SHF_ALLOC | object::elf::SHF_WRITE);
        let sections = object
            .sections()
            .filter(|section| {
                matches!(section.flags(), object::SectionFlags::Elf { sh_flags }
                    if sh_flags & writable == writable)
            })
            .filter_map(|section| Some((section.address(), section.data().ok()?)))
            .collect();
        let relative = object
            .dynamic_relocations()
            .into_iter()
            .flatten()
            .filter(|(_, relocation)| {
                matches!(relocation.flags(), object::RelocationFlags::Elf { r_type }
                    if r_type == object::elf::R_X86_64_RELATIVE)
            })
            .filter_map(|(address, relocation)| {
                Some((address, u64::try_from(relocation.addend()).ok()?))
            })
            .collect();

        Self { sections, relative }
    }

    fn word(&self, address: u64) -> Option<u64> {
        if let Some(value) = self.relative.get(&address) {
            return Some(*value);
        }

        let (start, data) = self
            .sections
            .iter()
            .find(|(start, data)| *start <= address && address - start < data.len() as u64)?;

        read_u64(data, usize::try_from(address - start).ok()?)
    }

    /// Go 1.20+ stores function entries relative to `text` and inline trees
    /// relative to `gofunc`, neither of which lives in `.gopclntab` itself in
    /// stripped PIE binaries; cross-checking five fields avoids false matches.
    fn find_moduledata(
        &self,
        found: &FoundTable<'_>,
        header: impl Fn(usize) -> Option<u64>,
    ) -> Option<ModuleData> {
        let table = found.address;
        let nfunc = header(0)?;
        let funcnametab = table.checked_add(header(3)?)?;
        let ftab = table.checked_add(header(7)?)?;
        let first_entry = u64::from(read_u32(found.data, usize::try_from(header(7)?).ok()?)?);
        let mut candidates = self
            .relative
            .iter()
            .filter(|(_, value)| **value == table)
            .map(|(address, _)| *address)
            .collect::<Vec<_>>();

        for (start, data) in &self.sections {
            candidates.extend(
                (aligned_offset(*start)..data.len().saturating_sub(7))
                    .step_by(8)
                    .filter(|offset| read_u64(data, *offset) == Some(table))
                    .filter_map(|offset| start.checked_add(offset as u64)),
            );
        }

        candidates.sort_unstable();
        candidates.dedup();
        candidates.into_iter().find_map(|module| {
            let field = |index: u64| self.word(module.checked_add(8 * index)?);
            let text = field(MODULE_TEXT)?;
            let valid = field(MODULE_PC_HEADER) == Some(table)
                && field(MODULE_FUNCNAMETAB) == Some(funcnametab)
                && field(MODULE_FTAB) == Some(ftab)
                && field(MODULE_FTAB_LENGTH) == nfunc.checked_add(1)
                && field(MODULE_MINPC) == text.checked_add(first_entry);

            valid.then(|| ModuleData {
                text,
                gofunc: found.section_end.and_then(|end| {
                    MODULE_LATER_FIELDS
                        .filter(|index| field(*index) == Some(end))
                        .find_map(|index| field(index - 1))
                        .filter(|gofunc| (table..end).contains(gofunc))
                }),
            })
        })
    }
}

fn go_release(object: &object::File<'_>) -> Option<GoRelease> {
    use object::{Object, ObjectSection};

    let section = object.section_by_name(".go.buildinfo")?;
    let data = section.data().ok()?;

    if data.get(..14)? != b"\xff Go buildinf:" || data.get(15)? & 2 == 0 {
        return None;
    }

    let mut offset = 32;
    let length = read_varint(data, &mut offset).ok()? as usize;
    let version = data.get(offset..offset.checked_add(length)?)?;

    GoRelease::parse(std::str::from_utf8(version).ok()?)
}

#[cfg(any(test, feature = "fuzzing"))]
#[allow(
    clippy::expect_used,
    reason = "a fuzz harness reports a bug by panicking"
)]
pub fn fuzz(data: &[u8]) {
    if data.starts_with(b"\x7fELF") {
        if let Ok(object) = object::File::parse(data)
            && let Ok(Some(table)) = load(&object)
        {
            fuzz_table(&table, data);
        }

        return;
    }

    let mut bytes = data.to_vec();

    if let Some(head) = bytes.first_chunk_mut::<8>()
        && head[0] & 1 == 0
    {
        *head = [0xf1, 0xff, 0xff, 0xff, 0, 0, 1, 8];
    }

    let gofunc = bytes
        .get(8..16)
        .map(|word| 0x1000 + u64::from_le_bytes(word.try_into().expect("eight bytes")) % 0x1_0000);
    let release = Some(GoRelease {
        major: 1,
        minor: 27,
    });

    if let Ok(table) = GoTable::parse(bytes.into(), 0x1000, 0x40_0000, gofunc, release) {
        fuzz_table(&table, data);
    }
}

#[cfg(any(test, feature = "fuzzing"))]
#[allow(
    clippy::expect_used,
    reason = "a fuzz harness reports a bug by panicking"
)]
fn fuzz_table(table: &GoTable, code: &[u8]) {
    let code = |address: u64, length: usize| {
        let start = usize::try_from(address % 0x1_0000).ok()?;
        code.get(start..start.checked_add(length)?)
    };

    for function in table.functions() {
        assert!(function.entry <= function.end);
        let _ = table.name(function);
        if let Some(runs) = table.sp_runs(function) {
            for run in runs.take(4096) {
                let Ok(run) = run else { break };
                assert!(run.start <= run.end);
            }
        }

        let _ = table.sp_delta(function, function.entry);
        if let Ok(prologue) = table.prologue(function, code) {
            assert!(prologue.end.is_none_or(|end| function.contains(end)));
        }

        if let Ok(positions) = table.positions(function) {
            for (range, _) in positions {
                assert!(function.entry <= range.start);
                assert!(range.end <= function.end);
            }
        }

        let _ = table.position(function, function.end.saturating_sub(1));
        if let Ok(runs) = table.inline_runs(function) {
            for run in runs.iter().filter(|run| run.value >= 0) {
                if let Ok(call) = table.inlined_call(function, run.value) {
                    assert!(function.contains(call.parent_pc));
                    let _ = table.inlined_name(&call);
                }
            }
        }

        assert!(
            function.entry == function.end
                || table
                    .function_containing(function.entry)
                    .is_some_and(|found| found.contains(function.entry))
        );
    }

    let (mut functions, mut instances) = (Vec::new(), Vec::new());
    let mut lines = crate::image::lines::LineTables::default();
    let mut files = crate::image::lines::Files::default();
    let completed = {
        let mut catalog = Catalog {
            functions: &mut functions,
            code_instances: &mut instances,
            lines: &mut lines,
            source_file: &mut |path| files.intern(path),
        };

        complete_metadata(table, code, &mut catalog).is_ok()
    };

    if completed {
        for (index, instance) in instances.iter().enumerate() {
            assert_eq!(instance.id.index(), index);
            assert!(instance.function.index() < functions.len());
            assert!(
                instance
                    .parent
                    .is_none_or(|parent| parent.index() < instances.len())
            );
            assert!(instance.ranges.iter().all(|range| range.start < range.end));
        }

        let mut image = crate::image::Builder::new(crate::TargetDescription::X86_64);
        lines.add_to(&mut image);
        let mut paths = crate::image::PathsBuilder::default();
        files
            .add_to(&mut image, &mut paths)
            .expect("paths without NULs");
        image.bytes(crate::image::TableKind::Paths, paths.into_bytes());
        image
            .seal(crate::image::Limits::default())
            .expect("the completed line tables are valid");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &[u8] = include_bytes!("../../fuzz/corpus/gopclntab/inlined-call");

    #[test]
    fn the_fuzz_seed_is_a_table_of_functions() {
        let word = SEED[8..16].try_into().expect("eight bytes");
        let gofunc = 0x1000 + u64::from_le_bytes(word) % 0x1_0000;
        let release = GoRelease {
            major: 1,
            minor: 27,
        };
        let table = GoTable::parse(SEED.into(), 0x1000, 0x40_0000, Some(gofunc), Some(release))
            .expect("the seed parses");
        assert!(!table.functions().is_empty());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// Everything a damaged table decodes keeps what its decoding
        /// promises: ranges within their functions, lookups that find the
        /// function holding an address, and line tables that seal.
        #[test]
        fn a_damaged_tables_decodings_keep_their_promises(
            writes in proptest::collection::vec(
                (proptest::prelude::any::<proptest::sample::Index>(), proptest::prelude::any::<u8>()),
                0..6,
            ),
            cut in proptest::prelude::any::<proptest::sample::Index>(),
            whole in proptest::prelude::any::<bool>(),
        ) {
            let mut bytes = SEED.to_vec();
            for (at, byte) in writes {
                let at = at.index(bytes.len());
                bytes[at] = byte;
            }
            if !whole {
                bytes.truncate(cut.index(bytes.len() + 1));
            }

            fuzz(&bytes);
        }

        #[test]
        fn any_tables_decodings_keep_their_promises(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..256),
        ) {
            fuzz(&bytes);
        }
    }

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs")
            .join(name);

        std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "missing {}: {error}; run `just build-test-programs`",
                path.display()
            )
        })
    }

    #[test]
    fn the_reader_agrees_with_go_debug_gosym() {
        for name in ["callers-go", "callers-go-stripped"] {
            let bytes = fixture(name);
            let object = object::File::parse(bytes.as_slice()).expect("parse the fixture");
            let table = load(&object)
                .expect("read the function table")
                .expect("a Go image has a function table");
            let oracle = String::from_utf8(fixture(&format!("{name}.gosym"))).expect("text");
            let mut lines = oracle.lines();
            assert_eq!(lines.next(), Some("microscope-gosym-oracle-v1"));
            let hex = |field: &str| {
                u64::from_str_radix(field.trim_start_matches("0x"), 16).expect("hex address")
            };

            let mut functions = Vec::new();
            let mut positions = 0;

            for line in lines {
                match line.split('\t').collect::<Vec<_>>().as_slice() {
                    ["func", entry, end, function] => {
                        functions.push((hex(entry), hex(end), (*function).to_owned()));
                    }
                    ["line", pc, file, line] => {
                        let pc = hex(pc);
                        let function = table.function_containing(pc).expect("a sampled function");
                        let expected = (!file.is_empty()).then(|| GoPosition {
                            file: (*file).into(),
                            line: line.parse().expect("line number"),
                        });
                        assert_eq!(
                            table.position(function, pc).expect("read the position"),
                            expected,
                            "{name}: {pc:#x}"
                        );
                        positions += 1;
                    }
                    other => panic!("unknown oracle line {other:?}"),
                }
            }

            let read = table
                .functions()
                .iter()
                .map(|function| {
                    let name = table.name(function).expect("a name");
                    (function.entry, function.end, name.to_string())
                })
                .collect::<Vec<_>>();
            assert_eq!(read, functions, "{name}");
            assert!(functions.len() > 1000 && positions > 3000, "{name}");
        }
    }

    #[test]
    fn prologue_ends_agree_with_go_dwarf() {
        use object::{Object, ObjectSection};

        for name in ["callers-go", "crash-go-o0"] {
            let bytes = fixture(name);
            let object = object::File::parse(bytes.as_slice()).expect("parse the fixture");
            let table = load(&object).expect("read the table").expect("a table");
            let text = object.section_by_name(".text").expect("a text section");
            let code_bytes = text.data().expect("text bytes");
            let code = |address: u64, length: usize| {
                let start = usize::try_from(address.checked_sub(text.address())?).ok()?;
                code_bytes.get(start..start.checked_add(length)?)
            };

            let image = crate::debug_info::load_program(
                std::path::Path::new(name),
                &bytes,
                &crate::debug_info::DebugFileSearch::default(),
            )
            .expect("load the fixture's DWARF")
            .image;
            let mut compared = 0;

            for instance in image
                .code_instances()
                .filter(|instance| matches!(instance.kind(), crate::CodeInstanceKind::OutOfLine))
            {
                let entry = instance.ranges().next().expect("a range").start.get();
                let Some(function) = table
                    .function_containing(entry)
                    .filter(|function| function.entry == entry && !function.facts.assembly)
                else {
                    continue;
                };

                let marked = image
                    .recommended_entries_for_instance(instance.id())
                    .filter(|entry| entry.provenance == crate::EntryProvenance::Statement)
                    .map(|entry| entry.address.get())
                    .collect::<Vec<_>>();
                let derived = table.prologue(function, code).expect("a prologue").end;
                assert_eq!(
                    marked,
                    derived.into_iter().collect::<Vec<_>>(),
                    "{name}: {}",
                    table.name(function).expect("a name")
                );
                compared += 1;
            }

            assert!(compared > 1000, "{name}: {compared}");
        }
    }
}
