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
