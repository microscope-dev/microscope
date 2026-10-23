//! Post-mortem Linux x86-64 ELF core dumps.
//!
//! Core files are untrusted input. Notes and program headers are bounds-checked
//! and malformed metadata is rejected instead of being guessed around. Memory
//! the producer did not save stays unavailable unless a verified module file
//! supplies those exact bytes.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::libc;
use object::elf;
use object::read::elf::{ElfFile64, FileHeader as _, Note, NoteIterator, ProgramHeader as _};
use object::{LittleEndian, ReadCache, ReadRef};

use crate::{ImageAddress, LoadBias, VirtualAddress};

const PRSTATUS_SIZE: usize = 336;
const PRSTATUS_SIGNAL_OFFSET: usize = 12;
const PRSTATUS_PID_OFFSET: usize = 32;
const PRSTATUS_REGISTERS_OFFSET: usize = 112;
const PRPSINFO_SIZE: usize = 136;
const PRPSINFO_PID_OFFSET: usize = 24;
const PRPSINFO_NAME: std::ops::Range<usize> = 40..56;
const PRPSINFO_ARGUMENTS: std::ops::Range<usize> = 56..136;
const SIGINFO_SIZE: usize = 128;

pub(super) const FXSAVE_SIZE: usize = 512;

pub(super) const GENERAL_REGISTER_COUNT: usize = 27;
const AUXV_ENTRY_SIZE: usize = 16;
const FILE_ENTRY_SIZE: usize = 24;

/// The most note bytes read from all `PT_NOTE` segments together. Notes
/// describe threads and mappings, not memory, so more is rejected rather
/// than buffered. The cap is cumulative: zeroed bytes parse as a stream of
/// ignored notes, so many headers naming one region would each be read.
const MAX_NOTE_BYTES: u64 = 256 * 1024 * 1024;
const PAGE_SIZE: u64 = 4096;
const ELF_MAGIC: [u8; 4] = *b"\x7fELF";
const CONTENT_COMPARE_CHUNK: u64 = 64 * 1024;

/// Build-id notes sit in an image's first page; larger note segments are
/// skipped rather than read.
const MAX_BUILD_ID_NOTES: u64 = 64 * 1024;

/// The most saved header bytes read to find an image's recorded build-id.
const MAX_RECORDED_HEADER: u64 = 64 * 1024;

pub(super) const AT_PHDR: u64 = 3;

pub(super) const AT_ENTRY: u64 = 9;

/// A core file that could not be used.
#[derive(Debug, thiserror::Error)]
pub(super) enum CoreError {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("{0}")]
    Invalid(String),
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::Invalid(message.into())
}

/// Why a memory read from a core dump did not complete.
#[derive(Debug)]
pub(super) enum CoreMemoryError {
    /// The dump neither saved the bytes nor maps them to a verified file.
    Unavailable,
    Io(io::Error),
}

/// The bytes of a core file, read positionally so multi-gigabyte dumps are
/// never buffered whole.
enum CoreBytes {
    File(File),

    #[cfg(any(test, feature = "fuzzing"))]
    Memory(Arc<[u8]>),
}

impl CoreBytes {
    fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<()> {
        match self {
            Self::File(file) => file.read_exact_at(buffer, offset),

            #[cfg(any(test, feature = "fuzzing"))]
            Self::Memory(bytes) => {
                let start = usize::try_from(offset)
                    .map_err(|_| io::Error::from(io::ErrorKind::UnexpectedEof))?;
                let source = start
                    .checked_add(buffer.len())
                    .and_then(|end| bytes.get(start..end))
                    .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
                buffer.copy_from_slice(source);
                Ok(())
            }
        }
    }
}

/// The holes of `file`, `len` bytes long, by offset: none where the file
/// system cannot say.
fn holes(file: &File, len: u64) -> Vec<Range<u64>> {
    use std::os::fd::AsFd as _;

    use nix::unistd::{Whence, lseek};

    let seek = |offset: u64, whence| {
        let offset = i64::try_from(offset).ok()?;
        lseek(file.as_fd(), offset, whence)
            .ok()
            .and_then(|found| u64::try_from(found).ok())
    };
    let mut holes = Vec::new();
    let mut offset = 0;
    while offset < len {
        let Some(hole) = seek(offset, Whence::SeekHole).filter(|hole| *hole < len) else {
            break;
        };
        // No data past a hole means it runs to the end.
        let data = seek(hole, Whence::SeekData).unwrap_or(len).min(len);
        if data <= hole {
            break;
        }
        holes.push(hole..data);
        offset = data;
    }
    holes
}

/// Process-wide identity recorded by `NT_PRPSINFO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CoreProcess {
    pub(super) pid: u32,
    pub(super) name: Arc<str>,
    pub(super) arguments: Arc<str>,
}

/// The signal details recorded by `NT_SIGINFO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CoreSignal {
    pub(super) number: i32,
    pub(super) code: i32,

    /// The faulting address for synchronous fault signals.
    pub(super) fault_address: Option<u64>,

    /// The sending process for user-generated signals.
    pub(super) sender: Option<i32>,
}

/// One thread's saved register state.
#[derive(Clone)]
pub(super) struct CoreThread {
    pub(super) tid: u32,

    /// `pr_cursig`: the signal being delivered when the dump was written.
    pub(super) current_signal: u16,
    pub(super) registers: libc::user_regs_struct,
    pub(super) fxsave: Option<Arc<[u8; FXSAVE_SIZE]>>,
    pub(super) signal: Option<CoreSignal>,
}

/// One `NT_FILE` entry: a file-backed mapping at dump time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CoreFileMapping {
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) file_offset: u64,
    pub(super) path: PathBuf,
}

/// One `PT_LOAD` segment. The producer saved the first `recorded` bytes and
/// omitted the rest; truncation can leave only the first `saved` of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CoreSegment {
    start: u64,
    end: u64,
    file_offset: u64,
    recorded: u64,
    saved: u64,

    /// The mapping's `p_flags`: whether it could be read, written, and
    /// executed.
    flags: u32,
}

/// A parsed core dump and its saved memory.
pub(super) struct CoreDump {
    source: CoreBytes,
    pub(super) process: CoreProcess,

    /// Threads in note order. Producers place the thread that triggered the
    /// dump first.
    pub(super) threads: Vec<CoreThread>,
    pub(super) auxv: Vec<(u64, u64)>,
    pub(super) files: Vec<CoreFileMapping>,
    segments: Vec<CoreSegment>,

    /// The ranges of the core file, by offset and in order, that its
    /// producer never wrote, where the file system says so.
    holes: Vec<Range<u64>>,
}

struct Metadata {
    process: CoreProcess,
    threads: Vec<CoreThread>,
    auxv: Vec<(u64, u64)>,
    files: Vec<CoreFileMapping>,
    segments: Vec<CoreSegment>,
}

impl CoreDump {
    pub(super) fn open(path: &Path) -> Result<Self, CoreError> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let cache = ReadCache::new(file);
        let metadata = parse(&cache, len)?;
        let file = cache.into_inner();
        let holes = holes(&file, len);
        Ok(Self {
            holes,
            ..Self::new(CoreBytes::File(file), metadata)
        })
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub(super) fn from_bytes(bytes: Arc<[u8]>) -> Result<Self, CoreError> {
        let metadata = parse(bytes.as_ref(), bytes.len() as u64)?;
        Ok(Self::new(CoreBytes::Memory(bytes), metadata))
    }

    fn new(source: CoreBytes, metadata: Metadata) -> Self {
        Self {
            source,
            process: metadata.process,
            threads: metadata.threads,
            auxv: metadata.auxv,
            files: metadata.files,
            segments: metadata.segments,
            holes: Vec::new(),
        }
    }

    /// Whether the dump holds the byte at `address` as its producer read
    /// it: one the producer saved and wrote. gcore leaves a hole in the
    /// file for the memory it failed to read, which reads back as zeros,
    /// as glibc's guard regions inside a thread's stack make it fail for
    /// the rest of the stack (F9 in plans/node.html).
    ///
    /// A byte the dump did not save at all is unreadable instead, which
    /// already says the memory is missing.
    pub(super) fn written(&self, address: u64) -> bool {
        self.saved_run(address).is_none_or(|(offset, _)| {
            let hole = self.holes.partition_point(|hole| hole.end <= offset);
            self.holes.get(hole).is_none_or(|hole| offset < hole.start)
        })
    }

    pub(super) fn auxv_value(&self, key: u64) -> Option<u64> {
        self.auxv
            .iter()
            .find_map(|&(entry, value)| (entry == key).then_some(value))
    }

    /// The saved mappings no file backed that the process could execute,
    /// with whether it could write them too.
    pub(super) fn anonymous_code(&self) -> Vec<(std::ops::Range<u64>, bool)> {
        self.segments
            .iter()
            .filter(|segment| {
                segment.flags & elf::PF_X != 0
                    && !self
                        .files
                        .iter()
                        .any(|file| file.start < segment.end && segment.start < file.end)
            })
            .map(|segment| (segment.start..segment.end, segment.flags & elf::PF_W != 0))
            .collect()
    }

    pub(super) fn thread(&self, tid: u32) -> Option<&CoreThread> {
        self.threads.iter().find(|thread| thread.tid == tid)
    }

    fn segment_index(&self, address: u64) -> Option<usize> {
        let index = self
            .segments
            .partition_point(|segment| segment.start <= address)
            .checked_sub(1)?;
        (address < self.segments[index].end).then_some(index)
    }

    fn next_segment_start(&self, address: u64) -> u64 {
        let index = self
            .segments
            .partition_point(|segment| segment.start <= address);
        self.segments
            .get(index)
            .map_or(u64::MAX, |segment| segment.start)
    }

    /// Returns the core-file offset and contiguous length of saved bytes at
    /// `address`, if the dump saved that address.
    fn saved_run(&self, address: u64) -> Option<(u64, u64)> {
        let segment = self.segments[self.segment_index(address)?];
        let offset = address - segment.start;
        (offset < segment.saved).then(|| (segment.file_offset + offset, segment.saved - offset))
    }

    /// Whether the producer saved `address` but truncation lost it. Only the
    /// dumped process held those bytes; no file may stand in for them.
    fn truncated(&self, address: u64) -> bool {
        self.segment_index(address).is_some_and(|index| {
            let segment = &self.segments[index];
            let offset = address - segment.start;
            segment.saved <= offset && offset < segment.recorded
        })
    }

    /// Reads bytes the dump itself saved, never substituting file contents.
    pub(super) fn read_saved(
        &self,
        address: u64,
        buffer: &mut [u8],
    ) -> Result<(), CoreMemoryError> {
        let mut done = 0;

        while done < buffer.len() {
            let current = address
                .checked_add(done as u64)
                .ok_or(CoreMemoryError::Unavailable)?;
            let (offset, run) = self
                .saved_run(current)
                .ok_or(CoreMemoryError::Unavailable)?;
            let count = usize::try_from(run)
                .unwrap_or(usize::MAX)
                .min(buffer.len() - done);
            self.source
                .read_exact_at(&mut buffer[done..done + count], offset)
                .map_err(CoreMemoryError::Io)?;
            done += count;
        }

        Ok(())
    }

    /// Saved address ranges in ascending order.
    fn saved_ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.segments
            .iter()
            .filter(|segment| segment.saved != 0)
            .map(|segment| (segment.start, segment.start + segment.saved))
    }
}

fn parse<'data, R: ReadRef<'data>>(data: R, len: u64) -> Result<Metadata, CoreError> {
    let header = elf::FileHeader64::<LittleEndian>::parse(data)
        .map_err(|error| invalid(format!("not a 64-bit ELF file: {error}")))?;
    let endian = header
        .endian()
        .map_err(|_| invalid("only little-endian core files are supported"))?;
    if header.e_type.get(endian) != elf::ET_CORE {
        return Err(invalid("the ELF file is not a core dump"));
    }

    if header.e_machine.get(endian) != elf::EM_X86_64 {
        return Err(invalid("only x86-64 core dumps are supported"));
    }

    let program_headers = header
        .program_headers(endian, data)
        .map_err(|error| invalid(format!("program headers are malformed: {error}")))?;

    let mut segments = Vec::new();
    let mut notes = NoteState::default();
    let mut note_bytes = 0_u64;

    for program_header in program_headers {
        match program_header.p_type(endian) {
            elf::PT_LOAD => {
                let start = program_header.p_vaddr(endian);
                let memory_size = program_header.p_memsz(endian);
                let file_size = program_header.p_filesz(endian);
                let file_offset = program_header.p_offset(endian);
                if file_size > memory_size {
                    return Err(invalid(format!(
                        "load segment at {start:#x} saves more bytes than it maps"
                    )));
                }

                if memory_size == 0 {
                    continue;
                }

                let end = start
                    .checked_add(memory_size)
                    .ok_or_else(|| invalid(format!("load segment at {start:#x} overflows")))?;
                file_offset
                    .checked_add(file_size)
                    .ok_or_else(|| invalid(format!("load segment at {start:#x} overflows")))?;

                // A truncated dump keeps only the bytes still inside the file.
                let saved = file_size.min(len.saturating_sub(file_offset));
                segments.push(CoreSegment {
                    start,
                    end,
                    file_offset,
                    recorded: file_size,
                    saved,
                    flags: program_header.p_flags(endian),
                });
            }
            elf::PT_NOTE => {
                note_bytes = note_bytes.saturating_add(program_header.p_filesz(endian));
                if note_bytes > MAX_NOTE_BYTES {
                    return Err(invalid("note segments exceed the supported size"));
                }

                let mut iterator = program_header
                    .notes(endian, data)
                    .map_err(|error| invalid(format!("note segment is malformed: {error}")))?
                    .ok_or_else(|| invalid("note segment is malformed"))?;
                parse_notes(&mut iterator, &mut notes)?;
            }
            _ => {}
        }
    }

    segments.sort_by_key(|segment| segment.start);
    if let Some(pair) = segments.windows(2).find(|pair| pair[0].end > pair[1].start) {
        return Err(invalid(format!(
            "load segments at {:#x} and {:#x} overlap",
            pair[0].start, pair[1].start
        )));
    }

    let process = notes
        .process
        .ok_or_else(|| invalid("core has no NT_PRPSINFO process note"))?;
    if notes.threads.is_empty() {
        return Err(invalid("core has no NT_PRSTATUS thread notes"));
    }

    Ok(Metadata {
        process,
        threads: notes.threads,
        auxv: notes.auxv.unwrap_or_default(),
        files: notes.files.unwrap_or_default(),
        segments,
    })
}

#[derive(Default)]
struct NoteState {
    process: Option<CoreProcess>,
    threads: Vec<CoreThread>,
    auxv: Option<Vec<(u64, u64)>>,
    files: Option<Vec<CoreFileMapping>>,
}

fn parse_notes(
    iterator: &mut NoteIterator<'_, elf::FileHeader64<LittleEndian>>,
    state: &mut NoteState,
) -> Result<(), CoreError> {
    while let Some(note) = iterator
        .next()
        .map_err(|error| invalid(format!("note is malformed: {error}")))?
    {
        // Thread state follows each thread's NT_PRSTATUS. Other owners (such
        // as LINUX extended register state) are not interpreted.
        if note.name() != b"CORE" {
            continue;
        }

        let descriptor = note.desc();
        match note.n_type(LittleEndian) {
            elf::NT_PRSTATUS => {
                let thread = parse_prstatus(descriptor)?;
                if state.threads.iter().any(|known| known.tid == thread.tid) {
                    return Err(invalid(format!("thread {} is described twice", thread.tid)));
                }

                state.threads.push(thread);
            }
            elf::NT_PRFPREG => {
                let thread = current_thread(state, "NT_FPREGSET")?;
                let fxsave: [u8; FXSAVE_SIZE] = descriptor.try_into().map_err(|_| {
                    invalid(format!(
                        "NT_FPREGSET has {} bytes; expected {FXSAVE_SIZE}",
                        descriptor.len()
                    ))
                })?;
                if thread.fxsave.replace(Arc::new(fxsave)).is_some() {
                    return Err(invalid(format!(
                        "thread {} has two NT_FPREGSET notes",
                        thread.tid
                    )));
                }
            }
            elf::NT_SIGINFO => {
                let signal = parse_siginfo(descriptor)?;
                let thread = current_thread(state, "NT_SIGINFO")?;
                if thread.signal.replace(signal).is_some() {
                    return Err(invalid(format!(
                        "thread {} has two NT_SIGINFO notes",
                        thread.tid
                    )));
                }
            }
            elf::NT_PRPSINFO if state.process.replace(parse_prpsinfo(descriptor)?).is_some() => {
                return Err(invalid("core has two NT_PRPSINFO notes"));
            }
            elf::NT_AUXV if state.auxv.replace(parse_auxv(descriptor)?).is_some() => {
                return Err(invalid("core has two NT_AUXV notes"));
            }
            elf::NT_FILE if state.files.replace(parse_files(descriptor)?).is_some() => {
                return Err(invalid("core has two NT_FILE notes"));
            }
            _ => {}
        }
    }

    Ok(())
}

fn current_thread<'a>(
    state: &'a mut NoteState,
    note: &str,
) -> Result<&'a mut CoreThread, CoreError> {
    state
        .threads
        .last_mut()
        .ok_or_else(|| invalid(format!("{note} precedes every NT_PRSTATUS note")))
}

/// Reads `N` bytes at `offset`, or reports that the note ends before them.
fn bytes_at<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], CoreError> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk::<N>)
        .copied()
        .ok_or_else(|| invalid("a core note field lies past the end of its note"))
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, CoreError> {
    Ok(u16::from_le_bytes(bytes_at(bytes, offset)?))
}

fn i32_at(bytes: &[u8], offset: usize) -> Result<i32, CoreError> {
    Ok(i32::from_le_bytes(bytes_at(bytes, offset)?))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, CoreError> {
    Ok(u64::from_le_bytes(bytes_at(bytes, offset)?))
}

fn c_string(bytes: &[u8]) -> Arc<str> {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into()
}

fn parse_prstatus(bytes: &[u8]) -> Result<CoreThread, CoreError> {
    if bytes.len() != PRSTATUS_SIZE {
        return Err(invalid(format!(
            "NT_PRSTATUS has {} bytes; expected {PRSTATUS_SIZE}",
            bytes.len()
        )));
    }

    let tid = u32::try_from(i32_at(bytes, PRSTATUS_PID_OFFSET)?)
        .ok()
        .filter(|tid| *tid != 0)
        .ok_or_else(|| invalid("NT_PRSTATUS names an invalid thread identifier"))?;
    let mut values = [0_u64; GENERAL_REGISTER_COUNT];
    for (index, value) in values.iter_mut().enumerate() {
        *value = u64_at(bytes, PRSTATUS_REGISTERS_OFFSET + index * 8)?;
    }

    Ok(CoreThread {
        tid,
        current_signal: u16_at(bytes, PRSTATUS_SIGNAL_OFFSET)?,
        registers: user_registers(values),
        fxsave: None,
        signal: None,
    })
}

/// Builds the kernel's `user_regs_struct`, whose field order is the
/// `elf_gregset_t` order saved in `NT_PRSTATUS`.
pub(super) const fn user_registers(
    values: [u64; GENERAL_REGISTER_COUNT],
) -> libc::user_regs_struct {
    let [
        r15,
        r14,
        r13,
        r12,
        rbp,
        rbx,
        r11,
        r10,
        r9,
        r8,
        rax,
        rcx,
        rdx,
        rsi,
        rdi,
        orig_rax,
        rip,
        cs,
        eflags,
        rsp,
        ss,
        fs_base,
        gs_base,
        ds,
        es,
        fs,
        gs,
    ] = values;
    libc::user_regs_struct {
        r15,
        r14,
        r13,
        r12,
        rbp,
        rbx,
        r11,
        r10,
        r9,
        r8,
        rax,
        rcx,
        rdx,
        rsi,
        rdi,
        orig_rax,
        rip,
        cs,
        eflags,
        rsp,
        ss,
        fs_base,
        gs_base,
        ds,
        es,
        fs,
        gs,
    }
}

fn parse_siginfo(bytes: &[u8]) -> Result<CoreSignal, CoreError> {
    let bytes = <&[u8; SIGINFO_SIZE]>::try_from(bytes).map_err(|_| {
        invalid(format!(
            "NT_SIGINFO has {} bytes; expected {SIGINFO_SIZE}",
            bytes.len()
        ))
    })?;
    let (number, metadata) = super::native::decode_siginfo(bytes);
    Ok(CoreSignal {
        number,
        code: metadata.code,
        fault_address: metadata.fault_address,
        sender: metadata.sender,
    })
}

fn parse_prpsinfo(bytes: &[u8]) -> Result<CoreProcess, CoreError> {
    if bytes.len() != PRPSINFO_SIZE {
        return Err(invalid(format!(
            "NT_PRPSINFO has {} bytes; expected {PRPSINFO_SIZE}",
            bytes.len()
        )));
    }

    let pid = u32::try_from(i32_at(bytes, PRPSINFO_PID_OFFSET)?)
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| invalid("NT_PRPSINFO names an invalid process identifier"))?;
    Ok(CoreProcess {
        pid,
        name: c_string(&bytes[PRPSINFO_NAME]),
        arguments: c_string(&bytes[PRPSINFO_ARGUMENTS]).trim_end().into(),
    })
}

fn parse_auxv(bytes: &[u8]) -> Result<Vec<(u64, u64)>, CoreError> {
    let (entries, remainder) = bytes.as_chunks::<AUXV_ENTRY_SIZE>();
    if !remainder.is_empty() {
        return Err(invalid("NT_AUXV is not a whole number of entries"));
    }

    let mut pairs = Vec::with_capacity(entries.len());
    for entry in entries {
        let key = u64_at(entry, 0)?;
        if key == 0 {
            break;
        }

        pairs.push((key, u64_at(entry, 8)?));
    }

    Ok(pairs)
}

fn parse_files(bytes: &[u8]) -> Result<Vec<CoreFileMapping>, CoreError> {
    let malformed = || invalid("NT_FILE is malformed");
    if bytes.len() < 16 {
        return Err(malformed());
    }

    let count = usize::try_from(u64_at(bytes, 0)?).map_err(|_| malformed())?;
    let page_size = u64_at(bytes, 8)?;
    if page_size == 0 {
        return Err(invalid("NT_FILE declares a zero page size"));
    }

    let names_start = count
        .checked_mul(FILE_ENTRY_SIZE)
        .and_then(|size| size.checked_add(16))
        .filter(|end| *end <= bytes.len())
        .ok_or_else(malformed)?;

    // Every path is NUL-terminated; a table cut short is malformed.
    let mut names = bytes[names_start..]
        .split_inclusive(|&byte| byte == 0)
        .map(|name| name.strip_suffix(&[0]));
    let mut files = Vec::with_capacity(count);

    for index in 0..count {
        let entry = 16 + index * FILE_ENTRY_SIZE;
        let start = u64_at(bytes, entry)?;
        let end = u64_at(bytes, entry + 8)?;
        let file_offset = u64_at(bytes, entry + 16)?
            .checked_mul(page_size)
            .ok_or_else(malformed)?;
        if start >= end {
            return Err(invalid(format!("NT_FILE mapping at {start:#x} is empty")));
        }

        let name = names.next().flatten().ok_or_else(malformed)?;
        if name.is_empty() {
            return Err(invalid(format!(
                "NT_FILE mapping at {start:#x} has no path"
            )));
        }

        files.push(CoreFileMapping {
            start,
            end,
            file_offset,
            path: PathBuf::from(OsStr::from_bytes(name)),
        });
    }

    Ok(files)
}

/// One loaded instance of a file-backed image: the mapping at file offset zero
/// and the later mappings of the same file that follow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ImageMappings {
    pub(super) path: PathBuf,
    pub(super) mappings: Vec<CoreFileMapping>,
}

impl ImageMappings {
    pub(super) fn start(&self) -> u64 {
        self.mappings[0].start
    }

    pub(super) fn contains(&self, address: u64) -> bool {
        self.mappings
            .iter()
            .any(|mapping| (mapping.start..mapping.end).contains(&address))
    }
}

/// Groups `NT_FILE` mappings into image instances. A mapping at file offset
/// zero starts an instance; later mappings of the same path join the nearest
/// instance below them. Mappings with no such instance are not images.
/// Verification against a file can join instances that its extent covers.
pub(super) fn image_mappings(files: &[CoreFileMapping]) -> Vec<ImageMappings> {
    let mut sorted = files.to_vec();
    sorted.sort_by_key(|mapping| mapping.start);
    let mut images: Vec<ImageMappings> = Vec::new();
    let mut latest = BTreeMap::<PathBuf, usize>::new();

    for mapping in sorted {
        let path = strip_deleted(&mapping.path);
        if mapping.file_offset == 0 {
            latest.insert(path.clone(), images.len());
            images.push(ImageMappings {
                path,
                mappings: vec![mapping],
            });
        } else if let Some(&index) = latest.get(&path) {
            images[index].mappings.push(mapping);
        }
    }

    images
}

/// The kernel marks unlinked mapped files with this suffix. The replacement
/// at the original path is then verified like any other candidate.
fn strip_deleted(path: &Path) -> PathBuf {
    let bytes = path.as_os_str().as_bytes();
    let trimmed = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
    PathBuf::from(OsStr::from_bytes(trimmed))
}

/// What the dump saved at the start of an image's first mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SavedHeader {
    /// An ELF header: the image is a loaded module.
    Elf,

    /// Other content: the image is a mapped data file.
    Data,

    /// Nothing; only the file on disk can say what was mapped.
    Unsaved,
}

pub(super) fn saved_header(
    core: &CoreDump,
    image: &ImageMappings,
) -> Result<SavedHeader, CoreError> {
    let mut magic = [0; 4];
    match core.read_saved(image.start(), &mut magic) {
        Ok(()) if magic == ELF_MAGIC => Ok(SavedHeader::Elf),
        Ok(()) => Ok(SavedHeader::Data),
        Err(CoreMemoryError::Unavailable) => Ok(SavedHeader::Unsaved),
        Err(CoreMemoryError::Io(error)) => Err(error.into()),
    }
}

/// Whether a file begins with an ELF header.
pub(super) fn is_elf(data: &[u8]) -> bool {
    data.starts_with(&ELF_MAGIC)
}

/// The GNU build-id of a 64-bit little-endian ELF image. Only the header,
/// program headers, and note segments are read, so a large file costs a few
/// small reads.
///
/// A segment's notes are read at its alignment. One that does not read
/// whole at that alignment is read again at 4 bytes, the alignment of
/// ordinary note sections, and used only if it reads whole: the kernel
/// links its vDSO with 4- and 8-byte aligned notes in one segment aligned
/// to 8.
pub(super) fn elf_build_id<'data, R: ReadRef<'data>>(data: R) -> Option<&'data [u8]> {
    let header = elf::FileHeader64::<LittleEndian>::parse(data).ok()?;
    let endian = header.endian().ok()?;
    for segment in header.program_headers(endian, data).ok()? {
        if segment.p_type(endian) != elf::PT_NOTE || segment.p_filesz(endian) > MAX_BUILD_ID_NOTES {
            continue;
        }

        let Ok(notes) = segment.data(endian, data) else {
            continue;
        };

        let Some(notes) = whole_notes(endian, segment.p_align(endian), notes)
            .or_else(|| whole_notes(endian, 4, notes))
        else {
            continue;
        };

        let build_id = notes.into_iter().find(|note| {
            note.name() == elf::ELF_NOTE_GNU
                && note.n_type(endian) == elf::NT_GNU_BUILD_ID
                && !note.desc().is_empty()
        });
        if let Some(build_id) = build_id {
            return Some(build_id.desc());
        }
    }

    None
}

/// Every note in `data` read at `align`, or `None` unless all of it reads.
fn whole_notes(
    endian: LittleEndian,
    align: u64,
    data: &[u8],
) -> Option<Vec<Note<'_, elf::FileHeader64<LittleEndian>>>> {
    let mut notes = NoteIterator::new(endian, align, data).ok()?;
    let mut whole = Vec::new();
    while let Some(note) = notes.next().ok()? {
        whole.push(note);
    }

    Some(whole)
}

/// The build-id the dump saved in an image's header pages, which names the
/// file to look for before any candidate is read.
///
/// The image's first mapping begins at file offset zero, so its saved bytes
/// are the file's leading bytes and file offsets address them directly.
pub(super) fn recorded_build_id(
    core: &CoreDump,
    image: &ImageMappings,
) -> Result<Option<Vec<u8>>, CoreError> {
    let first = &image.mappings[0];
    let Some((offset, run)) = core.saved_run(first.start) else {
        return Ok(None);
    };

    let length = run.min(first.end - first.start).min(MAX_RECORDED_HEADER);
    let mut header =
        vec![0; usize::try_from(length).map_err(|_| invalid("the header bound is too large"))?];

    core.source.read_exact_at(&mut header, offset)?;
    Ok(elf_build_id(header.as_slice()).map(<[u8]>::to_vec))
}

/// How strongly a module file is known to match a dumped image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ImageEvidence {
    /// The dumped GNU build-id note equals the file's.
    BuildId,

    /// Every saved byte of the file's read-only segments equals the file.
    SavedContent { compared: u64 },

    /// The file provably differs from the dumped image.
    Mismatch(String),

    /// The dump saved nothing that could confirm or refute the file.
    Unverifiable,
}

/// A candidate file's placement and identity evidence for one dumped image.
#[derive(Debug)]
pub(super) enum ImageVerification {
    /// The file's segments are placed at the dumped image by `load_bias`.
    Placed {
        /// The recorded image with every mapping the file's extent covers.
        image: ImageMappings,
        load_bias: LoadBias,
        evidence: ImageEvidence,

        /// Dump-time addresses of the file's read-only, file-backed bytes.
        read_only: Vec<Range<u64>>,
    },

    /// No load bias places the file at the dumped image, which proves it is
    /// a different file.
    Unplaced(String),
}

/// The file's loadable segments, normalized from its program headers.
struct FileSegment {
    address: u64,
    file_offset: u64,
    file_size: u64,
    memory_size: u64,
    writable: bool,
}

/// Places `file` at the dumped image and checks that it is the same image.
///
/// The image grows to every later mapping of its file within the file's
/// extent: lld packs a small file's segments into its first page, so several
/// of them map from file offset zero. Placement must agree with every
/// recorded mapping. Identity is then proven
/// by the build-id note when the dump saved it, and otherwise by comparing
/// every saved byte of the file's read-only segments. Writable segments are
/// never compared because the process legitimately modified them.
pub(super) fn verify_image(
    core: &CoreDump,
    image: &ImageMappings,
    file: &[u8],
) -> Result<ImageVerification, CoreError> {
    // The dump recorded an ELF image here; a file that no longer parses as
    // one, or whose segments lie outside it, is a different file.
    let object = match ElfFile64::<LittleEndian>::parse(file) {
        Ok(object) => object,
        Err(error) => {
            return Ok(ImageVerification::Unplaced(format!(
                "the file is not a 64-bit ELF image: {error}"
            )));
        }
    };

    let endian = LittleEndian;
    let mut segments = Vec::new();
    let mut notes = Vec::new();
    let mut outside = false;

    for header in object.elf_program_headers() {
        let file_offset = header.p_offset(endian);
        let file_size = header.p_filesz(endian);
        if matches!(header.p_type(endian), elf::PT_LOAD | elf::PT_NOTE)
            && file_range(file, file_offset, file_size).is_err()
        {
            outside = true;
            continue;
        }

        match header.p_type(endian) {
            elf::PT_LOAD => segments.push(FileSegment {
                address: header.p_vaddr(endian),
                file_offset,
                file_size,
                memory_size: header.p_memsz(endian),
                writable: header.p_flags(endian) & elf::PF_W != 0,
            }),
            elf::PT_NOTE => notes.push((header.p_vaddr(endian), file_offset, file_size)),
            _ => {}
        }
    }

    let Some(load_bias) = segments
        .iter()
        .find(|segment| segment.file_offset & !(PAGE_SIZE - 1) == 0)
        .and_then(|segment| {
            image
                .start()
                .checked_sub(segment.address & !(PAGE_SIZE - 1))
                .map(LoadBias::new)
        })
    else {
        return Ok(ImageVerification::Unplaced(
            "no loadable segment of the file is placed at the dumped image".to_owned(),
        ));
    };

    let image = &whole_image(core, image, &segments, load_bias);
    let verified = |evidence| {
        Ok(ImageVerification::Placed {
            image: image.clone(),
            load_bias,
            evidence,
            read_only: read_only_ranges(&segments, load_bias),
        })
    };

    // Mismatched files may still supply metadata, so they keep the placement
    // the recorded mapping implies.
    if outside {
        return verified(ImageEvidence::Mismatch(
            "a segment of the file lies outside it".to_owned(),
        ));
    }

    if let Some(detail) = placement_mismatch(image, &segments, load_bias) {
        return verified(ImageEvidence::Mismatch(detail));
    }

    if let Some(evidence) = build_id_evidence(core, file, &notes, load_bias)? {
        return verified(evidence);
    }

    let compared = compare_read_only_content(core, image, &segments, file)?;
    let evidence = match compared {
        Err(address) => ImageEvidence::Mismatch(format!(
            "saved read-only bytes differ from the file at {address:#x}"
        )),
        Ok(0) => ImageEvidence::Unverifiable,
        Ok(compared) => ImageEvidence::SavedContent { compared },
    };

    verified(evidence)
}

/// `image` and the later mappings of its path that lie within the extent of
/// the file's segments at `load_bias`.
fn whole_image(
    core: &CoreDump,
    image: &ImageMappings,
    segments: &[FileSegment],
    load_bias: LoadBias,
) -> ImageMappings {
    let extent_end = segments
        .iter()
        .filter_map(|segment| {
            load_bias
                .load(ImageAddress::new(segment.address))
                .map(VirtualAddress::get)?
                .checked_add(segment.memory_size)?
                .checked_next_multiple_of(PAGE_SIZE)
        })
        .max()
        .unwrap_or(0);
    let mut mappings = image.mappings.clone();
    mappings.extend(
        core.files
            .iter()
            .filter(|mapping| {
                image.start() < mapping.start
                    && mapping.end <= extent_end
                    && strip_deleted(&mapping.path) == image.path
                    && !image.mappings.contains(mapping)
            })
            .cloned(),
    );
    mappings.sort_by_key(|mapping| mapping.start);
    ImageMappings {
        path: image.path.clone(),
        mappings,
    }
}

/// Writable segments are excluded: the process may have modified them, and a
/// dump can omit modified pages when its filter or truncation drops them.
fn read_only_ranges(segments: &[FileSegment], load_bias: LoadBias) -> Vec<Range<u64>> {
    segments
        .iter()
        .filter(|segment| !segment.writable)
        .filter_map(|segment| {
            let start = load_bias
                .load(ImageAddress::new(segment.address))
                .map(VirtualAddress::get)?;
            Some(start..start.checked_add(segment.file_size)?)
        })
        .collect()
}

/// Protection changes such as RELRO split one segment into several mappings,
/// so each mapping must fall inside a segment's file-backed pages at the
/// address that segment implies.
///
/// glibc's loader first maps the file's whole extent from its first mapping
/// and then removes access to the gaps between segments, which stay mapped.
/// A gap is therefore the first mapping continued: its file offset is its
/// distance from the image's start, and it lies between two segments.
fn placement_mismatch(
    image: &ImageMappings,
    segments: &[FileSegment],
    load_bias: LoadBias,
) -> Option<String> {
    let page_start = |segment: &FileSegment| {
        load_bias
            .load(ImageAddress::new(segment.address & !(PAGE_SIZE - 1)))
            .map(VirtualAddress::get)
    };

    let page_end = |segment: &FileSegment| {
        load_bias
            .load(ImageAddress::new(segment.address))
            .map(VirtualAddress::get)?
            .checked_add(segment.memory_size)?
            .checked_next_multiple_of(PAGE_SIZE)
    };

    let in_segment = |mapping: &CoreFileMapping| {
        segments.iter().any(|segment| {
            let aligned_offset = segment.file_offset & !(PAGE_SIZE - 1);
            let file_end = segment.file_offset.saturating_add(segment.file_size);
            let delta = mapping.file_offset.wrapping_sub(aligned_offset);
            mapping.file_offset >= aligned_offset
                && mapping.file_offset < file_end
                && page_start(segment).and_then(|start| start.checked_add(delta))
                    == Some(mapping.start)
        })
    };

    let in_gap = |mapping: &CoreFileMapping| {
        let below =
            |segment: &FileSegment| page_end(segment).is_some_and(|end| end <= mapping.start);
        let above =
            |segment: &FileSegment| page_start(segment).is_some_and(|start| mapping.end <= start);
        image.start().checked_add(mapping.file_offset) == Some(mapping.start)
            && segments.iter().any(below)
            && segments.iter().any(above)
            && segments
                .iter()
                .all(|segment| below(segment) || above(segment))
    };

    image
        .mappings
        .iter()
        .find(|mapping| !in_segment(mapping) && !in_gap(mapping))
        .map(|mapping| {
            format!(
                "the file's segment layout does not match the mapping at {:#x}",
                mapping.start
            )
        })
}

/// Compares the file's build-id note with the dump's copy, when both exist.
fn build_id_evidence(
    core: &CoreDump,
    file: &[u8],
    notes: &[(u64, u64, u64)],
    load_bias: LoadBias,
) -> Result<Option<ImageEvidence>, CoreError> {
    for &(address, file_offset, size) in notes {
        let expected = file_range(file, file_offset, size)?;
        if !note_has_build_id(expected) {
            continue;
        }

        let Some(dumped) = load_bias
            .load(ImageAddress::new(address))
            .map(VirtualAddress::get)
        else {
            continue;
        };

        let mut saved = vec![0; expected.len()];
        match core.read_saved(dumped, &mut saved) {
            Ok(()) if saved == expected => return Ok(Some(ImageEvidence::BuildId)),
            Ok(()) => {
                return Ok(Some(ImageEvidence::Mismatch(
                    "the build-id note differs".to_owned(),
                )));
            }
            Err(CoreMemoryError::Unavailable) => {}
            Err(CoreMemoryError::Io(error)) => return Err(error.into()),
        }
    }

    Ok(None)
}

fn file_range(file: &[u8], offset: u64, size: u64) -> Result<&[u8], CoreError> {
    usize::try_from(offset)
        .ok()
        .zip(usize::try_from(size).ok())
        .and_then(|(start, size)| file.get(start..start.checked_add(size)?))
        .ok_or_else(|| invalid("a module segment lies outside its file"))
}

fn note_has_build_id(notes: &[u8]) -> bool {
    let Ok(mut iterator) =
        NoteIterator::<elf::FileHeader64<LittleEndian>>::new(LittleEndian, 4, notes)
    else {
        return false;
    };

    while let Ok(Some(note)) = iterator.next() {
        if note.name() == elf::ELF_NOTE_GNU && note.n_type(LittleEndian) == elf::NT_GNU_BUILD_ID {
            return true;
        }
    }

    false
}

/// Compares saved bytes against the file's read-only segments. Returns the
/// number of bytes compared, or the first differing address.
fn compare_read_only_content(
    core: &CoreDump,
    image: &ImageMappings,
    segments: &[FileSegment],
    file: &[u8],
) -> Result<Result<u64, u64>, CoreError> {
    let mut compared = 0_u64;

    for mapping in &image.mappings {
        for (saved_start, saved_end) in core.saved_ranges() {
            let start = saved_start.max(mapping.start);
            let end = saved_end.min(mapping.end);
            if start >= end {
                continue;
            }

            for segment in segments.iter().filter(|segment| !segment.writable) {
                // Intersect in file-offset space with this read-only segment.
                // Both sides are untrusted, so offsets saturate and an
                // unrepresentable range compares nothing.
                let mapped_offset = mapping.file_offset.saturating_add(start - mapping.start);
                let mapped_end = mapped_offset.saturating_add(end - start);
                let offset = mapped_offset.max(segment.file_offset);
                let offset_end =
                    mapped_end.min(segment.file_offset.saturating_add(segment.file_size));
                let mut current = offset;
                while current < offset_end {
                    let count = (offset_end - current).min(CONTENT_COMPARE_CHUNK);
                    let expected = file_range(file, current, count)?;
                    let address = mapping.start + (current - mapping.file_offset);
                    let mut saved = vec![0; expected.len()];
                    match core.read_saved(address, &mut saved) {
                        Ok(()) => {}
                        Err(CoreMemoryError::Unavailable) => {
                            return Err(invalid("saved core memory disappeared during comparison"));
                        }
                        Err(CoreMemoryError::Io(error)) => return Err(error.into()),
                    }

                    if let Some(index) = saved
                        .iter()
                        .zip(expected)
                        .position(|(left, right)| left != right)
                    {
                        return Ok(Err(address + index as u64));
                    }

                    compared += count;
                    current += count;
                }
            }
        }
    }

    Ok(Ok(compared))
}

/// Bytes of a verified image file mapped at dump time, used for memory the
/// producer did not save. Only read-only segments qualify: a dump may omit
/// modified writable pages, whose file contents are then stale.
#[derive(Clone)]
pub(super) struct FileBacking {
    start: u64,
    end: u64,
    file_offset: u64,
    data: Arc<[u8]>,
}

impl FileBacking {
    /// Backs each part of the image's mappings that lies in `read_only`.
    pub(super) fn for_image(
        image: &ImageMappings,
        data: &Arc<[u8]>,
        read_only: &[Range<u64>],
    ) -> Vec<Self> {
        image
            .mappings
            .iter()
            .flat_map(|mapping| {
                read_only.iter().filter_map(move |range| {
                    let start = mapping.start.max(range.start);
                    let end = mapping.end.min(range.end);
                    (start < end).then(|| Self {
                        start,
                        end,
                        file_offset: mapping.file_offset.saturating_add(start - mapping.start),
                        data: Arc::clone(data),
                    })
                })
            })
            .collect()
    }
}

/// A core dump's memory: saved bytes first, then verified file backings.
pub(super) struct CoreMemory {
    core: Arc<CoreDump>,
    backings: Vec<FileBacking>,
}

impl CoreMemory {
    pub(super) fn new(core: Arc<CoreDump>, mut backings: Vec<FileBacking>) -> Self {
        backings.sort_by_key(|backing| backing.start);
        Self { core, backings }
    }

    pub(super) const fn core(&self) -> &Arc<CoreDump> {
        &self.core
    }

    pub(super) fn read(&self, address: u64, buffer: &mut [u8]) -> Result<(), CoreMemoryError> {
        let mut done = 0;

        while done < buffer.len() {
            let current = address
                .checked_add(done as u64)
                .ok_or(CoreMemoryError::Unavailable)?;
            let remaining = buffer.len() - done;
            let count = if let Some((offset, run)) = self.core.saved_run(current) {
                let count = usize::try_from(run).unwrap_or(usize::MAX).min(remaining);
                self.core
                    .source
                    .read_exact_at(&mut buffer[done..done + count], offset)
                    .map_err(CoreMemoryError::Io)?;
                count
            } else if self.core.truncated(current) {
                return Err(CoreMemoryError::Unavailable);
            } else {
                // Unsaved bytes end where the next saved segment begins.
                let limit = self.core.next_segment_start(current) - current;
                self.read_backing(current, &mut buffer[done..], limit)?
            };

            done += count;
        }

        Ok(())
    }

    fn read_backing(
        &self,
        address: u64,
        buffer: &mut [u8],
        limit: u64,
    ) -> Result<usize, CoreMemoryError> {
        let index = self
            .backings
            .partition_point(|backing| backing.start <= address)
            .checked_sub(1)
            .ok_or(CoreMemoryError::Unavailable)?;
        let backing = &self.backings[index];
        if address >= backing.end {
            return Err(CoreMemoryError::Unavailable);
        }

        let run = (backing.end - address).min(limit);
        let count = usize::try_from(run).unwrap_or(usize::MAX).min(buffer.len());
        let offset = backing
            .file_offset
            .checked_add(address - backing.start)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or(CoreMemoryError::Unavailable)?;

        // Pages past the end of the file were never backed by it.
        let source = offset
            .checked_add(count)
            .and_then(|end| backing.data.get(offset..end))
            .ok_or(CoreMemoryError::Unavailable)?;
        buffer[..count].copy_from_slice(source);
        Ok(count)
    }
}

/// Parses arbitrary bytes as a core dump and reads its memory, exercising
/// every bounds check on untrusted input.
#[cfg(feature = "fuzzing")]
pub(super) fn fuzz(data: &[u8]) {
    let Ok(core) = CoreDump::from_bytes(data.into()) else {
        return;
    };

    let images = image_mappings(&core.files);
    let data: Arc<[u8]> = Arc::from(data);
    let mut backings = Vec::new();
    let _ = elf_build_id(data.as_ref());

    for image in &images {
        let _ = saved_header(&core, image);
        let _ = recorded_build_id(&core, image);
        if let Ok(ImageVerification::Placed {
            image, read_only, ..
        }) = verify_image(&core, image, &data)
        {
            backings.extend(FileBacking::for_image(&image, &data, &read_only));
        }
    }

    let addresses = core
        .segments
        .iter()
        .flat_map(|segment| {
            [
                segment.start,
                segment.end.saturating_sub(1),
                segment.start + segment.saved,
                segment.start.saturating_add(segment.recorded),
            ]
        })
        .chain(core.files.iter().map(|file| file.start))
        .collect::<Vec<_>>();

    // An image no file backs, as the vDSO, is read from saved memory alone.
    for start in addresses
        .iter()
        .copied()
        .chain(core.auxv_value(super::vdso::AT_SYSINFO_EHDR))
    {
        let _ = super::vdso::read_memory_image(start, u64::MAX, |address, buffer| {
            core.read_saved(address, buffer).is_ok()
        });
    }

    let memory = CoreMemory::new(Arc::new(core), backings);
    for address in addresses {
        let mut buffer = [0; 64];
        let _ = memory.read(address.saturating_sub(8), &mut buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORE: &[u8] = b"CORE";
    const ELF_HEADER_SIZE: usize = 64;
    const PROGRAM_HEADER_SIZE: usize = 56;

    struct Note {
        owner: &'static [u8],
        kind: u32,
        descriptor: Vec<u8>,
    }

    fn note(kind: u32, descriptor: Vec<u8>) -> Note {
        Note {
            owner: CORE,
            kind,
            descriptor,
        }
    }

    struct Load {
        address: u64,
        memory: u64,
        saved: Vec<u8>,
    }

    fn put(bytes: &mut [u8], offset: usize, value: &[u8]) {
        bytes[offset..offset + value.len()].copy_from_slice(value);
    }

    fn align4(bytes: &mut Vec<u8>) {
        bytes.resize(bytes.len().next_multiple_of(4), 0);
    }

    /// Writes a minimal little-endian x86-64 ELF core.
    fn core_bytes(notes: &[Note], loads: &[Load]) -> Vec<u8> {
        let mut note_bytes = Vec::new();
        for note in notes {
            let name_size = u32::try_from(note.owner.len() + 1).unwrap();
            note_bytes.extend_from_slice(&name_size.to_le_bytes());
            note_bytes
                .extend_from_slice(&u32::try_from(note.descriptor.len()).unwrap().to_le_bytes());
            note_bytes.extend_from_slice(&note.kind.to_le_bytes());
            note_bytes.extend_from_slice(note.owner);
            note_bytes.push(0);
            align4(&mut note_bytes);
            note_bytes.extend_from_slice(&note.descriptor);
            align4(&mut note_bytes);
        }

        let headers = 1 + loads.len();
        let notes_offset = ELF_HEADER_SIZE + headers * PROGRAM_HEADER_SIZE;
        let mut bytes = vec![0; notes_offset];
        put(&mut bytes, 0, b"\x7fELF\x02\x01\x01");
        put(&mut bytes, 16, &elf::ET_CORE.to_le_bytes());
        put(&mut bytes, 18, &elf::EM_X86_64.to_le_bytes());
        put(&mut bytes, 20, &1_u32.to_le_bytes());
        put(&mut bytes, 32, &(ELF_HEADER_SIZE as u64).to_le_bytes());
        put(
            &mut bytes,
            52,
            &u16::try_from(ELF_HEADER_SIZE).unwrap().to_le_bytes(),
        );
        put(
            &mut bytes,
            54,
            &u16::try_from(PROGRAM_HEADER_SIZE).unwrap().to_le_bytes(),
        );
        put(
            &mut bytes,
            56,
            &u16::try_from(headers).unwrap().to_le_bytes(),
        );
        let mut program_header =
            |index: usize, kind: u32, offset: u64, address: u64, saved: u64, memory: u64| {
                let base = ELF_HEADER_SIZE + index * PROGRAM_HEADER_SIZE;
                put(&mut bytes, base, &kind.to_le_bytes());
                put(&mut bytes, base + 8, &offset.to_le_bytes());
                put(&mut bytes, base + 16, &address.to_le_bytes());
                put(&mut bytes, base + 32, &saved.to_le_bytes());
                put(&mut bytes, base + 40, &memory.to_le_bytes());
                put(&mut bytes, base + 48, &4_u64.to_le_bytes());
            };
        program_header(
            0,
            elf::PT_NOTE,
            notes_offset as u64,
            0,
            note_bytes.len() as u64,
            0,
        );
        let mut data_offset = (notes_offset + note_bytes.len()) as u64;
        for (index, load) in loads.iter().enumerate() {
            let saved = load.saved.len() as u64;
            program_header(
                index + 1,
                elf::PT_LOAD,
                data_offset,
                load.address,
                saved,
                load.memory,
            );
            data_offset += saved;
        }

        bytes.extend_from_slice(&note_bytes);
        for load in loads {
            bytes.extend_from_slice(&load.saved);
        }

        bytes
    }

    fn prstatus(tid: i32, signal: u16, rip: u64) -> Vec<u8> {
        let mut bytes = vec![0; PRSTATUS_SIZE];
        put(&mut bytes, PRSTATUS_SIGNAL_OFFSET, &signal.to_le_bytes());
        put(&mut bytes, PRSTATUS_PID_OFFSET, &tid.to_le_bytes());
        put(
            &mut bytes,
            PRSTATUS_REGISTERS_OFFSET + 16 * 8,
            &rip.to_le_bytes(),
        );
        bytes
    }

    fn prpsinfo(pid: i32, name: &[u8], arguments: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; PRPSINFO_SIZE];
        put(&mut bytes, PRPSINFO_PID_OFFSET, &pid.to_le_bytes());
        put(&mut bytes, PRPSINFO_NAME.start, name);
        put(&mut bytes, PRPSINFO_ARGUMENTS.start, arguments);
        bytes
    }

    fn siginfo(number: i32, code: i32, address: u64) -> Vec<u8> {
        let mut bytes = vec![0; SIGINFO_SIZE];
        put(&mut bytes, 0, &number.to_le_bytes());
        put(&mut bytes, 8, &code.to_le_bytes());
        put(&mut bytes, 16, &address.to_le_bytes());
        bytes
    }

    #[test]
    fn siginfo_fields_follow_the_layout_their_code_selects() {
        let parse = |number, code| {
            let signal = parse_siginfo(&siginfo(number, code, 0x1234)).expect("valid siginfo");
            (signal.fault_address, signal.sender)
        };

        let segv_maperr = 1;

        // A page fault records its address; a general-protection fault
        // (SI_KERNEL) records none, so 0 must not appear as an address.
        assert_eq!(parse(libc::SIGSEGV, segv_maperr), (Some(0x1234), None));
        assert_eq!(parse(libc::SIGSEGV, libc::SI_KERNEL), (None, None));
        assert_eq!(parse(libc::SIGABRT, libc::SI_TKILL), (None, Some(0x1234)));
        assert_eq!(parse(libc::SIGTERM, libc::SI_USER), (None, Some(0x1234)));

        // These codes reuse the sender field for a timer ID and a poll band.
        assert_eq!(parse(libc::SIGALRM, libc::SI_TIMER), (None, None));
        assert_eq!(parse(libc::SIGIO, libc::SI_SIGIO), (None, None));
    }

    fn fxsave(marker: u8) -> Vec<u8> {
        vec![marker; FXSAVE_SIZE]
    }

    fn auxv(entries: &[(u64, u64)]) -> Vec<u8> {
        entries
            .iter()
            .chain([&(0, 0)])
            .flat_map(|(key, value)| key.to_le_bytes().into_iter().chain(value.to_le_bytes()))
            .collect()
    }

    fn file_note(page_size: u64, mappings: &[(u64, u64, u64, &str)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(mappings.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&page_size.to_le_bytes());
        for &(start, end, offset, _) in mappings {
            for value in [start, end, offset / page_size] {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }

        for &(.., path) in mappings {
            bytes.extend_from_slice(path.as_bytes());
            bytes.push(0);
        }

        bytes
    }

    fn parse_bytes(bytes: Vec<u8>) -> Result<CoreDump, CoreError> {
        CoreDump::from_bytes(bytes.into())
    }

    fn rejection(bytes: Vec<u8>) -> String {
        match parse_bytes(bytes) {
            Err(CoreError::Invalid(message)) => message,
            Err(CoreError::Io(error)) => panic!("unexpected I/O error: {error}"),
            Ok(_) => panic!("malformed core was accepted"),
        }
    }

    const MAPPINGS: &[(u64, u64, u64, &str)] = &[
        (0x40_0000, 0x40_1000, 0, "/bin/app"),
        (0x40_1000, 0x40_3000, 0x1000, "/bin/app"),
        (0x7f00_0000, 0x7f00_1000, 0, "/lib/libc.so.6 (deleted)"),
    ];

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "both producer orders are written side by side and checked identically"
    )]
    fn thread_state_follows_its_own_prstatus_in_kernel_and_gdb_note_orders() {
        let kernel = core_bytes(
            &[
                note(elf::NT_PRSTATUS, prstatus(10, 11, 0x40_1234)),
                note(elf::NT_PRPSINFO, prpsinfo(10, b"app", b"/bin/app --flag ")),
                note(elf::NT_SIGINFO, siginfo(11, 1, 0x1)),
                note(
                    elf::NT_AUXV,
                    auxv(&[(AT_PHDR, 0x40_0040), (AT_ENTRY, 0x40_1000)]),
                ),
                note(elf::NT_FILE, file_note(PAGE_SIZE, MAPPINGS)),
                note(elf::NT_PRFPREG, fxsave(1)),
                Note {
                    owner: b"LINUX",
                    kind: 0x202,
                    descriptor: vec![0xff; 64],
                },
                note(elf::NT_PRSTATUS, prstatus(11, 11, 0x40_2000)),
                note(elf::NT_PRFPREG, fxsave(2)),
            ],
            &[],
        );
        let gdb = core_bytes(
            &[
                note(elf::NT_PRPSINFO, prpsinfo(10, b"app", b"/bin/app --flag ")),
                note(elf::NT_PRSTATUS, prstatus(10, 11, 0x40_1234)),
                note(elf::NT_PRFPREG, fxsave(1)),
                note(elf::NT_SIGINFO, siginfo(11, 1, 0x1)),
                note(elf::NT_PRSTATUS, prstatus(11, 11, 0x40_2000)),
                note(elf::NT_PRFPREG, fxsave(2)),
                note(
                    elf::NT_AUXV,
                    auxv(&[(AT_PHDR, 0x40_0040), (AT_ENTRY, 0x40_1000)]),
                ),
                // gdb records byte offsets with a unit page size.
                note(elf::NT_FILE, file_note(1, MAPPINGS)),
            ],
            &[],
        );

        for (producer, bytes) in [("kernel", kernel), ("gdb", gdb)] {
            let core = parse_bytes(bytes).unwrap_or_else(|error| panic!("{producer}: {error}"));
            assert_eq!(
                core.process,
                CoreProcess {
                    pid: 10,
                    name: "app".into(),
                    arguments: "/bin/app --flag".into(),
                },
                "{producer}"
            );
            assert_eq!(
                core.threads
                    .iter()
                    .map(|thread| thread.tid)
                    .collect::<Vec<_>>(),
                [10, 11],
                "{producer}: the dumping thread stays first"
            );
            let [first, second] = core.threads.as_slice() else {
                unreachable!()
            };

            assert_eq!(
                (first.registers.rip, second.registers.rip),
                (0x40_1234, 0x40_2000)
            );
            assert_eq!(
                first.fxsave.as_deref().map(|bytes| bytes[0]),
                Some(1),
                "{producer}"
            );
            assert_eq!(
                second.fxsave.as_deref().map(|bytes| bytes[0]),
                Some(2),
                "{producer}"
            );
            assert_eq!(
                first.signal,
                Some(CoreSignal {
                    number: 11,
                    code: 1,
                    fault_address: Some(0x1),
                    sender: None,
                }),
                "{producer}"
            );
            assert_eq!(second.signal, None, "{producer}");
            assert_eq!(core.auxv_value(AT_ENTRY), Some(0x40_1000));
            assert_eq!(core.auxv_value(AT_PHDR), Some(0x40_0040));
            assert_eq!(core.files[1].file_offset, 0x1000, "{producer}");
            assert_eq!(
                image_mappings(&core.files),
                [
                    ImageMappings {
                        path: "/bin/app".into(),
                        mappings: core.files[..2].to_vec(),
                    },
                    ImageMappings {
                        path: "/lib/libc.so.6".into(),
                        mappings: core.files[2..].to_vec(),
                    },
                ],
                "{producer}"
            );
        }
    }

    fn minimal_notes() -> Vec<Note> {
        vec![
            note(elf::NT_PRPSINFO, prpsinfo(10, b"app", b"app")),
            note(elf::NT_PRSTATUS, prstatus(10, 0, 0)),
        ]
    }

    fn with_notes(extra: Vec<Note>) -> Vec<u8> {
        let mut notes = minimal_notes();
        notes.extend(extra);
        core_bytes(&notes, &[])
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one table names every rejected malformation and its reason"
    )]
    fn malformed_metadata_is_rejected_with_its_reason() {
        let header_edit = |offset: usize, value: &[u8]| {
            let mut bytes = core_bytes(&minimal_notes(), &[]);
            put(&mut bytes, offset, value);
            bytes
        };

        let load = |address, memory, saved: usize| Load {
            address,
            memory,
            saved: vec![0; saved],
        };

        let cases: Vec<(&str, Vec<u8>, &str)> = vec![
            (
                "executable",
                header_edit(16, &elf::ET_EXEC.to_le_bytes()),
                "not a core dump",
            ),
            (
                "machine",
                header_edit(18, &elf::EM_AARCH64.to_le_bytes()),
                "only x86-64",
            ),
            ("big endian", header_edit(5, &[2]), "little-endian"),
            ("32-bit", header_edit(4, &[1]), "not a 64-bit ELF file"),
            (
                "no process",
                core_bytes(&minimal_notes()[1..], &[]),
                "no NT_PRPSINFO",
            ),
            (
                "no threads",
                core_bytes(&minimal_notes()[..1], &[]),
                "no NT_PRSTATUS",
            ),
            (
                "short prstatus",
                with_notes(vec![note(elf::NT_PRSTATUS, vec![0; 335])]),
                "335 bytes",
            ),
            (
                "zero thread",
                with_notes(vec![note(elf::NT_PRSTATUS, prstatus(0, 0, 0))]),
                "invalid thread",
            ),
            (
                "duplicate thread",
                with_notes(vec![note(elf::NT_PRSTATUS, prstatus(10, 0, 0))]),
                "described twice",
            ),
            (
                "duplicate process",
                with_notes(vec![note(elf::NT_PRPSINFO, prpsinfo(10, b"", b""))]),
                "two NT_PRPSINFO",
            ),
            (
                "orphan registers",
                core_bytes(
                    &[
                        note(elf::NT_PRFPREG, fxsave(0)),
                        note(elf::NT_PRPSINFO, prpsinfo(1, b"", b"")),
                        note(elf::NT_PRSTATUS, prstatus(1, 0, 0)),
                    ],
                    &[],
                ),
                "precedes every NT_PRSTATUS",
            ),
            (
                "double registers",
                with_notes(vec![
                    note(elf::NT_PRFPREG, fxsave(0)),
                    note(elf::NT_PRFPREG, fxsave(0)),
                ]),
                "two NT_FPREGSET",
            ),
            (
                "short registers",
                with_notes(vec![note(elf::NT_PRFPREG, vec![0; 100])]),
                "expected 512",
            ),
            (
                "double signal",
                with_notes(vec![
                    note(elf::NT_SIGINFO, siginfo(6, -6, 0)),
                    note(elf::NT_SIGINFO, siginfo(6, -6, 0)),
                ]),
                "two NT_SIGINFO",
            ),
            (
                "partial auxv",
                with_notes(vec![note(elf::NT_AUXV, vec![0; 20])]),
                "whole number",
            ),
            (
                "zero page",
                with_notes(vec![note(
                    elf::NT_FILE,
                    file_note(1, &[])
                        .into_iter()
                        .enumerate()
                        .map(|(index, byte)| if index == 8 { 0 } else { byte })
                        .collect(),
                )]),
                "zero page size",
            ),
            (
                "empty mapping",
                with_notes(vec![note(
                    elf::NT_FILE,
                    file_note(1, &[(0x2000, 0x2000, 0, "/a")]),
                )]),
                "is empty",
            ),
            (
                "unnamed mapping",
                with_notes(vec![note(
                    elf::NT_FILE,
                    file_note(1, &[(0x1000, 0x2000, 0, "")]),
                )]),
                "has no path",
            ),
            (
                "missing names",
                with_notes(vec![note(
                    elf::NT_FILE,
                    file_note(1, &[(0x1000, 0x2000, 0, "/a")])[..40].to_vec(),
                )]),
                "NT_FILE is malformed",
            ),
            (
                "huge count",
                with_notes(vec![note(
                    elf::NT_FILE,
                    [u64::MAX.to_le_bytes(), 1_u64.to_le_bytes()].concat(),
                )]),
                "NT_FILE is malformed",
            ),
            (
                "overflowing offset",
                with_notes(vec![note(
                    elf::NT_FILE,
                    file_note(1 << 40, &[(0x1000, 0x2000, 1 << 40, "/a")])
                        .into_iter()
                        .enumerate()
                        .map(|(index, byte)| {
                            if (32..40).contains(&index) {
                                0xff
                            } else {
                                byte
                            }
                        })
                        .collect(),
                )]),
                "NT_FILE is malformed",
            ),
            (
                "overlap",
                core_bytes(
                    &minimal_notes(),
                    &[load(0x1000, 0x2000, 0), load(0x2000, 0x1000, 0)],
                ),
                "overlap",
            ),
            (
                "oversaved",
                core_bytes(&minimal_notes(), &[load(0x1000, 0x10, 0x20)]),
                "saves more bytes",
            ),
            (
                "wrapping",
                core_bytes(&minimal_notes(), &[load(u64::MAX - 0xfff, 0x2000, 0)]),
                "overflows",
            ),
        ];

        for (name, bytes, expected) in cases {
            let message = rejection(bytes);
            assert!(
                message.contains(expected),
                "{name}: {message:?} lacks {expected:?}"
            );
        }

        // A truncated note segment cannot be partially trusted.
        let mut truncated = core_bytes(&minimal_notes(), &[]);
        truncated.truncate(truncated.len() - 8);
        assert!(rejection(truncated).contains("note segment is malformed"));
    }

    fn page(marker: u8) -> Vec<u8> {
        vec![marker; usize::try_from(PAGE_SIZE).unwrap()]
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one address space exercises every precedence and boundary rule"
    )]
    fn memory_prefers_saved_bytes_then_verified_files_and_never_crosses_gaps() {
        let mut bytes = core_bytes(
            &minimal_notes(),
            &[
                // Kernel form: the first page saved, the second omitted.
                Load {
                    address: 0x10000,
                    memory: 0x2000,
                    saved: page(0xa1),
                },
                Load {
                    address: 0x13000,
                    memory: 0x1000,
                    saved: page(0xb2),
                },
                // Truncated half way through by the end of the file.
                Load {
                    address: 0x20000,
                    memory: 0x1000,
                    saved: page(0xc3),
                },
            ],
        );
        bytes.truncate(bytes.len() - 0x800);
        let core = Arc::new(parse_bytes(bytes).unwrap());
        let file: Arc<[u8]> = (0..0x4800_u32)
            .map(|offset| u8::try_from(offset / 0x1000).unwrap() + 1)
            .collect();
        let image = ImageMappings {
            path: "/bin/app".into(),
            mappings: vec![
                CoreFileMapping {
                    start: 0x10000,
                    end: 0x15000,
                    file_offset: 0,
                    path: "/bin/app".into(),
                },
                // The file could supply the truncated segment's lost tail.
                CoreFileMapping {
                    start: 0x20000,
                    end: 0x21000,
                    file_offset: 0x3000,
                    path: "/bin/app".into(),
                },
            ],
        };

        let memory = CoreMemory::new(
            Arc::clone(&core),
            FileBacking::for_image(&image, &file, &[0x10000..0x15000, 0x20000..0x21000]),
        );
        let unbacked = CoreMemory::new(Arc::clone(&core), Vec::new());
        let read = |memory: &CoreMemory, address: u64, size: usize| {
            let mut buffer = vec![0; size];
            memory.read(address, &mut buffer).map(|()| buffer)
        };

        assert_eq!(read(&memory, 0x10ff8, 8).unwrap(), [0xa1; 8], "saved bytes");
        assert_eq!(
            read(&memory, 0x11000, 4).unwrap(),
            [2; 4],
            "unsaved tail uses the file"
        );
        assert_eq!(
            read(&memory, 0x10ffc, 8).unwrap(),
            [0xa1, 0xa1, 0xa1, 0xa1, 2, 2, 2, 2],
            "one read crosses from saved bytes into the file"
        );
        assert_eq!(
            read(&memory, 0x12000, 4).unwrap(),
            [3; 4],
            "unrecorded gap uses the file"
        );
        assert_eq!(
            read(&memory, 0x13000, 4).unwrap(),
            [0xb2; 4],
            "saved bytes win over the file"
        );
        assert_eq!(
            read(&memory, 0x12ffe, 4).unwrap(),
            [3, 3, 0xb2, 0xb2],
            "file bytes stop where the next saved segment begins"
        );
        assert_eq!(read(&memory, 0x14000, 4).unwrap(), [5; 4]);
        assert!(
            matches!(read(&memory, 0x147fe, 4), Err(CoreMemoryError::Unavailable)),
            "past the file's end"
        );
        assert!(
            matches!(read(&memory, 0x15000, 1), Err(CoreMemoryError::Unavailable)),
            "past the mapping"
        );
        assert_eq!(
            read(&memory, 0x207f8, 8).unwrap(),
            [0xc3; 8],
            "truncation keeps the prefix"
        );
        assert!(
            matches!(read(&memory, 0x207fc, 8), Err(CoreMemoryError::Unavailable)),
            "and loses the rest, which the producer saved and the file cannot replace"
        );
        assert!(matches!(
            read(&memory, 0x20ffc, 4),
            Err(CoreMemoryError::Unavailable)
        ));
        assert!(
            matches!(
                read(&memory, u64::MAX - 3, 8),
                Err(CoreMemoryError::Unavailable)
            ),
            "no wrap-around"
        );
        assert_eq!(read(&memory, 0x10000, 0).unwrap(), Vec::<u8>::new());

        assert!(
            matches!(
                read(&unbacked, 0x11000, 1),
                Err(CoreMemoryError::Unavailable)
            ),
            "no file, no bytes"
        );

        // Only read-only segment bytes are backed, at their own file offsets.
        let clipped = CoreMemory::new(
            Arc::clone(&core),
            FileBacking::for_image(&image, &file, std::slice::from_ref(&(0x12000..0x12800))),
        );
        assert_eq!(read(&clipped, 0x127fc, 4).unwrap(), [3; 4]);
        assert!(matches!(
            read(&clipped, 0x127fe, 4),
            Err(CoreMemoryError::Unavailable)
        ));
        assert!(matches!(
            read(&clipped, 0x11000, 1),
            Err(CoreMemoryError::Unavailable)
        ));

        // An NT_FILE offset near the top of the address space cannot wrap
        // around into the start of the file.
        let wrapping = CoreMemory::new(
            Arc::clone(&core),
            vec![FileBacking {
                start: 0x11000,
                end: 0x12000,
                file_offset: u64::MAX - 0x10,
                data: Arc::clone(&file),
            }],
        );
        assert!(matches!(
            read(&wrapping, 0x11800, 1),
            Err(CoreMemoryError::Unavailable)
        ));
        let mut saved = [0; 4];
        assert!(matches!(
            core.read_saved(0x11000, &mut saved),
            Err(CoreMemoryError::Unavailable)
        ));
        core.read_saved(0x13000, &mut saved).unwrap();
        assert_eq!(saved, [0xb2; 4]);
    }

    /// An image's first page: an ELF header whose one note segment, at
    /// `note_offset`, holds a GNU build-id.
    fn image_header(note_offset: usize, build_id: &[u8]) -> Vec<u8> {
        let note_size = 16 + build_id.len();
        let mut bytes = page(0);
        bytes.resize(bytes.len().max(note_offset + note_size), 0);
        put(&mut bytes, 0, b"\x7fELF\x02\x01\x01");
        put(&mut bytes, 16, &elf::ET_DYN.to_le_bytes());
        put(&mut bytes, 18, &elf::EM_X86_64.to_le_bytes());
        put(&mut bytes, 20, &1_u32.to_le_bytes());
        put(&mut bytes, 32, &(ELF_HEADER_SIZE as u64).to_le_bytes());
        put(
            &mut bytes,
            52,
            &u16::try_from(ELF_HEADER_SIZE).unwrap().to_le_bytes(),
        );
        put(
            &mut bytes,
            54,
            &u16::try_from(PROGRAM_HEADER_SIZE).unwrap().to_le_bytes(),
        );
        put(&mut bytes, 56, &1_u16.to_le_bytes());
        let header = ELF_HEADER_SIZE;
        put(&mut bytes, header, &elf::PT_NOTE.to_le_bytes());
        put(&mut bytes, header + 8, &(note_offset as u64).to_le_bytes());
        put(&mut bytes, header + 32, &(note_size as u64).to_le_bytes());
        put(&mut bytes, header + 48, &4_u64.to_le_bytes());
        put(&mut bytes, note_offset, &4_u32.to_le_bytes());
        put(
            &mut bytes,
            note_offset + 4,
            &u32::try_from(build_id.len()).unwrap().to_le_bytes(),
        );
        put(
            &mut bytes,
            note_offset + 8,
            &elf::NT_GNU_BUILD_ID.to_le_bytes(),
        );
        put(&mut bytes, note_offset + 12, b"GNU\0");
        put(&mut bytes, note_offset + 16, build_id);
        bytes
    }

    #[test]
    fn recorded_build_ids_come_only_from_saved_bytes_of_the_first_mapping() {
        const BUILD_ID: &[u8] = &[0xab; 20];
        let recorded = |saved: Vec<u8>, mapping_end: u64| {
            let mut notes = minimal_notes();
            notes.push(note(
                elf::NT_FILE,
                file_note(PAGE_SIZE, &[(0x10000, mapping_end, 0, "/lib/a.so")]),
            ));
            let load = Load {
                address: 0x10000,
                memory: 0x2000,
                saved,
            };

            let core = parse_bytes(core_bytes(&notes, &[load])).unwrap();
            recorded_build_id(&core, &image_mappings(&core.files)[0]).unwrap()
        };

        assert_eq!(
            recorded(image_header(0x200, BUILD_ID), 0x12000).as_deref(),
            Some(BUILD_ID)
        );

        // File offsets address saved bytes only within the first mapping,
        // and only bytes the dump kept.
        let late = image_header(0x1800, BUILD_ID);
        assert_eq!(recorded(late.clone(), 0x12000).as_deref(), Some(BUILD_ID));
        assert_eq!(recorded(late.clone(), 0x11000), None);
        assert_eq!(recorded(late[..0x1000].to_vec(), 0x12000), None);
        assert_eq!(recorded(Vec::new(), 0x12000), None);
        assert_eq!(recorded(page(0x7f), 0x12000), None);
    }

    /// Ubuntu 18.04's libc: text, then a 2 MiB gap, then data whose first
    /// page RELRO leaves read-only.
    #[test]
    fn mappings_must_follow_the_segments_or_the_loaders_gaps_between_them() {
        const BASE: u64 = 0x7f00_0000_0000;
        let segments = [
            FileSegment {
                address: 0,
                file_offset: 0,
                file_size: 0x1e_6a7c,
                memory_size: 0x1e_6a7c,
                writable: false,
            },
            FileSegment {
                address: 0x3e_7618,
                file_offset: 0x1e_7618,
                file_size: 0x5248,
                memory_size: 0x94c8,
                writable: true,
            },
        ];
        let mismatch = |extra: &[(u64, u64, u64)]| {
            let mut mappings = vec![
                (0, 0x1e_7000, 0),
                (0x3e_7000, 0x3e_b000, 0x1e_7000),
                (0x3e_b000, 0x3e_d000, 0x1e_b000),
            ];
            mappings.extend_from_slice(extra);
            let image = ImageMappings {
                path: PathBuf::from("/lib/libc.so.6"),
                mappings: mappings
                    .into_iter()
                    .map(|(start, end, file_offset)| CoreFileMapping {
                        start: BASE + start,
                        end: BASE + end,
                        file_offset,
                        path: PathBuf::from("/lib/libc.so.6"),
                    })
                    .collect(),
            };

            placement_mismatch(&image, &segments, LoadBias::new(BASE))
                .map(|detail| detail.contains(&format!("{:#x}", BASE + extra[0].0)))
        };

        assert_eq!(mismatch(&[(0x1e_7000, 0x3e_7000, 0x1e_7000)]), None);
        assert_eq!(mismatch(&[(0x2e_7000, 0x2e_8000, 0x2e_7000)]), None);

        // A gap mapped from any other offset, or one that reaches into a
        // segment's pages or past the last segment, is another file's layout.
        for wrong in [
            (0x1e_7000, 0x3e_7000, 0x1e_8000),
            (0x1e_7000, 0x3e_8000, 0x1e_7000),
            (0x3f_0000, 0x3f_1000, 0x3f_0000),
            (0x3f_1000, 0x3f_2000, 0x3f_1000),
        ] {
            assert_eq!(mismatch(&[wrong]), Some(true), "{wrong:x?}");
        }
    }
}
