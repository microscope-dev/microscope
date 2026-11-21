use super::{Capture, CaptureBuilder};
use crate::object::{
    ChunkData, DEFAULT_ZSTD_LEVEL, OBJECT_HEADER_LEN, OBJECT_MAGIC, OBJECT_VERSION, ObjectError,
    ObjectKind, ObjectLevel, ObjectReader, ObjectWriter, PartKind, PartSpec, Signal, TimeUnit,
    decode_tables_budgeted, encode_tables,
};

#[cfg(feature = "fuzzing")]
const CAPTURE_DECODE_BUDGET_BYTES: usize = crate::object::MAX_DECOMPRESSED_OBJECT_BYTES;
#[cfg(not(feature = "fuzzing"))]
const CAPTURE_DECODE_BUDGET_BYTES: usize = 2 << 30;

pub const FILE_MAGIC: [u8; 8] = OBJECT_MAGIC;
pub const FILE_VERSION: u32 = OBJECT_VERSION;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    #[error("the file is not a microscope capture")]
    NotACapture,

    #[error("the capture file's version is {0}, and this microscope reads version {FILE_VERSION}")]
    Version(u32),

    #[error("the capture holds no complete chunk")]
    Empty,

    #[error("the capture is damaged: {0}")]
    Object(#[from] ObjectError),
}

pub struct Writer<W: std::io::Write> {
    out: W,
    writer: ObjectWriter,
}

impl<W: std::io::Write + std::fmt::Debug> std::fmt::Debug for Writer<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("out", &self.out)
            .field("parts", &self.writer.part_count())
            .finish()
    }
}

impl<W: std::io::Write> Writer<W> {
    pub fn new(mut out: W) -> std::io::Result<Self> {
        let mut writer = ObjectWriter::new(ObjectKind::Capture, ObjectLevel::None, 0, 0);
        let header = writer
            .take_bytes()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        out.write_all(&header)?;
        Ok(Self { out, writer })
    }

    #[must_use]
    pub const fn written(&self) -> u64 {
        self.writer.written_bytes()
    }

    pub fn chunk(&mut self, capture: &Capture) -> std::io::Result<()> {
        let (starts_at, ends_at) = capture.span();
        let data = ChunkData {
            capture: capture.clone(),
            chunks: Vec::new(),
            dimensions: Vec::new(),
            sanitizer_counts: Vec::new(),
        };
        let sections = encode_tables(&data, TimeUnit::Ns, DEFAULT_ZSTD_LEVEL, 0)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        verify_capture_decode_budget(capture, &sections)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.writer
            .add_part(&PartSpec {
                org_id: 0,
                project_id: 0,
                starts_at,
                ends_at,
                signal: Signal::Profile,
                part: PartKind::Chunks,
                sections,
            })
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.writer
            .write_checkpoint()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let pending = self
            .writer
            .take_bytes()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.out.write_all(&pending)?;
        self.out.flush()
    }

    pub fn finish(mut self) -> std::io::Result<W> {
        self.writer
            .write_final_footer()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let pending = self
            .writer
            .take_bytes()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.out.write_all(&pending)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

pub fn write(capture: &Capture, out: impl std::io::Write) -> std::io::Result<()> {
    let _span = crate::span!("capture write");
    let mut writer = Writer::new(out)?;
    writer.chunk(capture)?;
    writer.finish()?;
    Ok(())
}

fn verify_capture_decode_budget(
    capture: &Capture,
    sections: &[crate::object::EncodedSection],
) -> Result<(), ObjectError> {
    use crate::capture::records::{
        Account, Frame, Function, Gap, Line, Mapping, Marker, Module, OffCpu, Plot, Process, Range,
        Sample, Setting, Site, Stack, Syscall, SyscallErrno, SyscallNote, SyscallStat, Task,
        TaskWait, Thread, ThreadRange, Zone, ZoneRange,
    };

    let mut upper = 0_usize;
    for sec in sections {
        let reader = crate::object::SectionReader::open(&sec.bytes, sec.kind)?;
        for page in reader.pages() {
            upper = upper.saturating_add(page.uncompressed_len.get() as usize);
        }
    }

    let add_table = |acc: usize, rows: usize, row_size: usize, u64_cols: usize| -> usize {
        acc.saturating_add(rows.saturating_mul(row_size.saturating_add(u64_cols * 8)))
    };

    upper = upper
        .saturating_add(capture.strings.len().saturating_mul(3))
        .saturating_add(capture.syscall_blobs.len().saturating_mul(3));
    upper = add_table(upper, capture.settings.len(), size_of::<Setting>(), 10);
    upper = add_table(upper, capture.modules.len(), size_of::<Module>(), 10);
    upper = add_table(upper, capture.processes.len(), size_of::<Process>(), 10);
    upper = add_table(upper, capture.threads.len(), size_of::<Thread>(), 10);
    upper = add_table(upper, capture.mappings.len(), size_of::<Mapping>(), 12);
    upper = add_table(upper, capture.frames.len(), size_of::<Frame>(), 10);
    upper = add_table(upper, capture.stacks.len(), size_of::<Stack>(), 10);
    upper = add_table(
        upper,
        capture.sample_ranges.len(),
        size_of::<ThreadRange>(),
        10,
    );
    upper = add_table(upper, capture.samples.len(), size_of::<Sample>(), 10);
    upper = add_table(upper, capture.frame_symbols.len(), size_of::<Range>(), 10);
    upper = add_table(upper, capture.lines.len(), size_of::<Line>(), 10);
    upper = add_table(upper, capture.functions.len(), size_of::<Function>(), 10);
    upper = add_table(upper, capture.accounting.len(), size_of::<Account>(), 10);
    upper = add_table(
        upper,
        capture.off_cpu_ranges.len(),
        size_of::<ThreadRange>(),
        10,
    );
    upper = add_table(upper, capture.off_cpu.len(), size_of::<OffCpu>(), 10);
    upper = add_table(upper, capture.sites.len(), size_of::<Site>(), 10);
    upper = add_table(upper, capture.zones.len(), size_of::<Zone>(), 10);
    upper = add_table(upper, capture.zone_ranges.len(), size_of::<ZoneRange>(), 10);
    upper = add_table(upper, capture.plots.len(), size_of::<Plot>(), 10);
    upper = add_table(upper, capture.markers.len(), size_of::<Marker>(), 10);
    upper = add_table(upper, capture.tasks.len(), size_of::<Task>(), 12);
    upper = add_table(upper, capture.task_waits.len(), size_of::<TaskWait>(), 10);
    upper = add_table(upper, capture.gaps.len(), size_of::<Gap>(), 10);
    upper = add_table(upper, capture.syscalls.len(), size_of::<Syscall>(), 28);
    upper = add_table(
        upper,
        capture.syscall_notes.len(),
        size_of::<SyscallNote>(),
        16,
    );
    upper = add_table(
        upper,
        capture.syscall_stats.len(),
        size_of::<SyscallStat>() + crate::syscalls::stats::BUCKETS * 8,
        16,
    );
    upper = add_table(
        upper,
        capture.syscall_errnos.len(),
        size_of::<SyscallErrno>(),
        10,
    );

    if upper > CAPTURE_DECODE_BUDGET_BYTES {
        let raw: Vec<crate::object::RawSection<'_>> = sections
            .iter()
            .map(|s| crate::object::RawSection {
                kind: s.kind,
                rows: s.rows,
                key: s.key,
                checksum: 0,
                bytes: &s.bytes,
            })
            .collect();
        let _ = decode_tables_budgeted(&raw, CAPTURE_DECODE_BUDGET_BYTES)?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct Read {
    pub capture: Capture,
    pub chunks: usize,
    pub truncated: bool,
}

pub fn read(bytes: &[u8]) -> Result<Read, FileError> {
    let _span = crate::span!("capture read");
    if bytes.len() < OBJECT_HEADER_LEN || bytes.first_chunk::<8>() != Some(&FILE_MAGIC) {
        return Err(FileError::NotACapture);
    }

    let version = u32::from_le_bytes(
        *bytes
            .get(8..)
            .and_then(<[u8]>::first_chunk::<4>)
            .ok_or(FileError::NotACapture)?,
    );
    if version != FILE_VERSION {
        return Err(FileError::Version(version));
    }

    if bytes.get(12).copied() != Some(ObjectKind::Capture as u8) {
        return Err(FileError::NotACapture);
    }

    let reader = match ObjectReader::open_recoverable(bytes) {
        Ok(reader) => reader,
        Err(ObjectError::Truncated) => return Err(FileError::Empty),
        Err(ObjectError::BadMagic) => return Err(FileError::NotACapture),
        Err(ObjectError::UnsupportedVersion(v)) => return Err(FileError::Version(v)),
        Err(error) => return Err(FileError::Object(error)),
    };

    let part_count = reader.parts().len();
    if part_count == 0 {
        return Err(FileError::Empty);
    }

    let mut captures = Vec::with_capacity(part_count);
    for idx in 0..part_count {
        let sections = reader.part_sections(idx, None)?;
        let chunk = decode_tables_budgeted(&sections, CAPTURE_DECODE_BUDGET_BYTES)?;
        captures.push(chunk.capture);
    }

    let chunks = captures.len();
    let capture = match captures.pop() {
        None => return Err(FileError::Empty),
        Some(only) if captures.is_empty() => only,
        Some(last) => {
            captures.push(last);
            let mut builder = CaptureBuilder::new();
            for capture in &captures {
                builder.absorb(capture);
            }

            builder
                .finish_read()
                .map_err(|error| FileError::Object(ObjectError::Capture(error)))?
        }
    };

    Ok(Read {
        capture,
        chunks,
        truncated: reader.truncated,
    })
}

#[cfg(any(test, feature = "fuzzing"))]
#[allow(clippy::expect_used, reason = "a fuzz oracle panics on what it finds")]
pub fn fuzz(data: &[u8]) {
    let Ok(first) = read(data) else {
        return;
    };

    let keys = crate::analysis::Keys::new(&first.capture, crate::analysis::Granularity::Function);
    let everything = crate::analysis::Query::default();
    let _ = crate::analysis::aggregate(&first.capture, &keys, &everything);
    let _ = crate::analysis::functions(&first.capture, &keys, &everything);

    let mut written = Vec::new();
    write(&first.capture, &mut written).expect("a capture read is written");
    let again = read(&written).expect("a capture written reads back");
    assert!(
        again.capture == first.capture,
        "writing changed the capture"
    );
}
