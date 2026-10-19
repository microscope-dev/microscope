//! A capture file: a header, sealed chunks, and an index of them.
//!
//! A writer appends each chunk as it is sealed and never
//! seeks, so a recording cut short by a crash still holds every chunk it
//! finished. A reader uses the index when it is whole, and otherwise finds
//! the chunks by scanning, reading every complete one.
//!
//! ```text
//! "USCOPEPF" | version u32 | flags u32
//! ( length u64 | chunk: one sealed image of CaptureFormat )*
//! "USCOPEIX" | count u32 | ( offset u64, length u64 )* | index length u64 | "USCOPEND"
//! ```

use super::{Capture, CaptureBuilder};
use crate::image::{AlignedBytes, ImageError};

pub const FILE_MAGIC: [u8; 8] = *b"USCOPEPF";

pub const FILE_VERSION: u32 = 1;
const INDEX_MAGIC: [u8; 8] = *b"USCOPEIX";
const END_MAGIC: [u8; 8] = *b"USCOPEND";
const HEADER: usize = 16;

/// Why bytes are not a capture file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    #[error("the file is not a microscope capture")]
    NotACapture,

    #[error("the capture file's version is {0}, and this microscope reads version {FILE_VERSION}")]
    Version(u32),

    #[error("the capture holds no complete chunk")]
    Empty,

    #[error("a chunk of the capture is damaged: {0}")]
    Chunk(ImageError),
}

/// Writes a capture file chunk by chunk.
#[derive(Debug)]
pub struct Writer<W: std::io::Write> {
    out: W,
    written: u64,
    chunks: Vec<(u64, u64)>,
}

impl<W: std::io::Write> Writer<W> {
    pub fn new(mut out: W) -> std::io::Result<Self> {
        out.write_all(&FILE_MAGIC)?;
        out.write_all(&FILE_VERSION.to_le_bytes())?;
        out.write_all(&0_u32.to_le_bytes())?;
        Ok(Self {
            out,
            written: HEADER as u64,
            chunks: Vec::new(),
        })
    }

    /// Appends one sealed chunk.
    pub fn chunk(&mut self, chunk: &AlignedBytes) -> std::io::Result<()> {
        let bytes = chunk.as_bytes();
        let length = bytes.len() as u64;
        self.out.write_all(&length.to_le_bytes())?;
        self.out.write_all(bytes)?;
        self.chunks.push((self.written + 8, length));
        self.written += 8 + length;
        self.out.flush()
    }

    /// Writes the index and returns the output.
    pub fn finish(mut self) -> std::io::Result<W> {
        let mut index = Vec::with_capacity(12 + 16 * self.chunks.len());
        index.extend_from_slice(&INDEX_MAGIC);
        index.extend_from_slice(
            &u32::try_from(self.chunks.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );

        for (offset, length) in &self.chunks {
            index.extend_from_slice(&offset.to_le_bytes());
            index.extend_from_slice(&length.to_le_bytes());
        }

        let length = index.len() as u64;
        index.extend_from_slice(&length.to_le_bytes());
        index.extend_from_slice(&END_MAGIC);
        self.out.write_all(&index)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Writes one capture as a whole file.
pub fn write(capture: &Capture, out: impl std::io::Write) -> std::io::Result<()> {
    let chunk = capture
        .seal()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut writer = Writer::new(out)?;
    writer.chunk(&chunk)?;
    writer.finish()?;
    Ok(())
}

/// What reading a file found.
#[derive(Debug)]
pub struct Read {
    /// Every chunk merged into one capture.
    pub capture: Capture,

    /// Chunks read.
    pub chunks: usize,

    /// Whether the file ended without its index, as one cut short does.
    pub truncated: bool,
}

/// Reads a capture file, merging its chunks.
pub fn read(bytes: &[u8]) -> Result<Read, FileError> {
    if bytes.len() < HEADER || bytes[..8] != FILE_MAGIC {
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

    let (spans, truncated) =
        index(bytes).map_or_else(|| (scan(bytes), true), |spans| (spans, false));
    let mut captures = Vec::with_capacity(spans.len());

    for (offset, length) in &spans {
        let (Ok(offset), Ok(length)) = (usize::try_from(*offset), usize::try_from(*length)) else {
            return Err(FileError::Chunk(ImageError::TooLarge));
        };

        let chunk = bytes
            .get(offset..offset.saturating_add(length))
            .ok_or(FileError::Chunk(ImageError::Truncated))?;

        // Chunks are read from aligned copies: a table's records are read in
        // place, and the file's bytes need not be aligned.
        let mut aligned =
            AlignedBytes::zeroed(chunk.len()).ok_or(FileError::Chunk(ImageError::TooLarge))?;
        aligned.as_mut_bytes().copy_from_slice(chunk);
        captures.push(Capture::from_chunk(aligned.as_bytes()).map_err(FileError::Chunk)?);
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

            builder.finish()
        }
    };

    Ok(Read {
        capture,
        chunks,
        truncated,
    })
}

/// The chunks the index lists, if the file ends with a whole one.
fn index(bytes: &[u8]) -> Option<Vec<(u64, u64)>> {
    let end = bytes.len().checked_sub(16)?;
    if bytes[end + 8..] != END_MAGIC {
        return None;
    }

    let length = usize::try_from(u64::from_le_bytes(bytes[end..end + 8].try_into().ok()?)).ok()?;
    let start = end.checked_sub(length)?;
    let index = bytes.get(start..end)?;
    if index.len() < 12 || index[..8] != INDEX_MAGIC {
        return None;
    }

    let count = u32::from_le_bytes(index[8..12].try_into().ok()?) as usize;
    if index.len() != 12 + 16 * count {
        return None;
    }

    let spans: Vec<(u64, u64)> = index[12..]
        .as_chunks::<16>()
        .0
        .iter()
        .map(|entry| {
            let (offset, length) = entry.split_at(8);
            Some((
                u64::from_le_bytes(*offset.first_chunk::<8>()?),
                u64::from_le_bytes(*length.first_chunk::<8>()?),
            ))
        })
        .collect::<Option<_>>()?;

    spans
        .iter()
        .all(|(offset, length)| {
            offset
                .checked_add(*length)
                .is_some_and(|end| end <= start as u64)
        })
        .then_some(spans)
}

/// The complete chunks from the header on, for a file without its index.
fn scan(bytes: &[u8]) -> Vec<(u64, u64)> {
    let mut spans = Vec::new();
    let mut position = HEADER;

    while let Some(length) = bytes
        .get(position..)
        .and_then(<[u8]>::first_chunk::<8>)
        .map(|length| u64::from_le_bytes(*length))
    {
        let start = position + 8;
        let Some(end) = usize::try_from(length)
            .ok()
            .and_then(|length| start.checked_add(length))
            .filter(|end| *end <= bytes.len())
        else {
            break;
        };

        if bytes.get(start..start + 8) != Some(&b"USCOPECP"[..]) {
            break;
        }

        spans.push((start as u64, length));
        position = end;
    }

    spans
}
