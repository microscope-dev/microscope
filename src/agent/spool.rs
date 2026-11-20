use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Write as _},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
};

use microscope_data::{
    object::{DEFAULT_ZSTD_LEVEL, ObjectReader, TimeUnit, decode_tables},
    sanitize::{EncodedPost, SanitizedChunk, Sanitizer, encode_post, sanitize_chunk},
};

const NEXT_CHUNK_FILE: &str = "next_chunk";
const CHUNK_PREFIX: &str = "chunk-";
const CHUNK_SUFFIX: &str = ".usob";

pub const MAX_POST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpoolStats {
    pub queued_chunks: usize,
    pub queued_bytes: u64,
    pub dropped_chunks: u64,
    pub next_chunk: u64,
}

pub struct Batch {
    pub first_chunk: u64,
    pub last_chunk: u64,
    pub chunk_count: usize,
    pub post: EncodedPost,
}

#[derive(Debug)]
pub struct Spool {
    dir: PathBuf,
    max_bytes: u64,
    entries: BTreeMap<u64, u64>,
    total_bytes: u64,
    next_chunk: u64,
    dropped_chunks: u64,
}

impl Spool {
    pub fn open(dir: &Path, max_bytes: u64) -> io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;

        let mut entries = BTreeMap::new();
        let mut total_bytes = 0_u64;
        let mut highest = 0_u64;

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(text) = name.to_str() else {
                continue;
            };
            if text.starts_with('.') {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            if let Some(number) = parse_chunk_name(text) {
                let size = entry.metadata()?.len();
                entries.insert(number, size);
                total_bytes = total_bytes.saturating_add(size);
                highest = highest.max(number);
            }
        }

        let persisted = read_next_chunk(dir)?.unwrap_or(1);
        let next_chunk = persisted.max(highest.saturating_add(1)).max(1);

        let mut spool = Self {
            dir: dir.to_path_buf(),
            max_bytes,
            entries,
            total_bytes,
            next_chunk,
            dropped_chunks: 0,
        };
        spool.evict()?;
        Ok(spool)
    }

    #[must_use]
    pub fn stats(&self) -> SpoolStats {
        SpoolStats {
            queued_chunks: self.entries.len(),
            queued_bytes: self.total_bytes,
            dropped_chunks: self.dropped_chunks,
            next_chunk: self.next_chunk,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn allocate_chunk(&mut self) -> io::Result<u64> {
        let number = self.next_chunk;
        self.next_chunk = number.saturating_add(1);
        write_atomic(
            &self.dir,
            NEXT_CHUNK_FILE,
            format!("{}\n", self.next_chunk).as_bytes(),
        )?;
        Ok(number)
    }

    pub fn push(
        &mut self,
        number: u64,
        chunk: &SanitizedChunk,
        agent_id: u64,
        created_at: i64,
    ) -> io::Result<u64> {
        if number >= self.next_chunk {
            self.next_chunk = number.saturating_add(1);
            write_atomic(
                &self.dir,
                NEXT_CHUNK_FILE,
                format!("{}\n", self.next_chunk).as_bytes(),
            )?;
        }

        let encoded = encode_post(
            std::slice::from_ref(chunk),
            agent_id,
            created_at,
            DEFAULT_ZSTD_LEVEL,
        )
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        let bytes = encoded.as_bytes();
        let name = chunk_file_name(number);
        write_atomic(&self.dir, &name, bytes)?;

        let size = bytes.len() as u64;
        if let Some(old) = self.entries.insert(number, size) {
            self.total_bytes = self.total_bytes.saturating_sub(old);
        }
        self.total_bytes = self.total_bytes.saturating_add(size);
        self.evict()?;
        Ok(size)
    }

    pub fn peek_batch(
        &self,
        max_bytes: usize,
        agent_id: u64,
        created_at: i64,
        sanitizer: &Sanitizer,
    ) -> io::Result<Option<Batch>> {
        if self.entries.is_empty() {
            return Ok(None);
        }

        let limit = max_bytes.min(MAX_POST_BYTES);
        let mut selected = Vec::new();
        let mut estimated = 0_usize;

        for (&number, &size) in &self.entries {
            let size_usize = usize::try_from(size).unwrap_or(usize::MAX);
            if !selected.is_empty() && estimated.saturating_add(size_usize) > limit {
                break;
            }
            selected.push(number);
            estimated = estimated.saturating_add(size_usize);
        }

        let mut chunks = Vec::with_capacity(selected.len());
        let mut numbers = Vec::with_capacity(selected.len());
        for number in selected {
            let path = self.dir.join(chunk_file_name(number));
            let raw = match fs::read(&path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let reader = ObjectReader::open(&raw)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            let sections = reader
                .part_sections(0, None)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            let data = decode_tables(&sections)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            let unit = data
                .chunks
                .first()
                .map_or(TimeUnit::Us, |row| row.time_unit);
            chunks.push(sanitize_chunk(data, unit, sanitizer));
            numbers.push(number);
        }

        if chunks.is_empty() {
            return Ok(None);
        }

        while chunks.len() > 1 {
            let post = encode_post(&chunks, agent_id, created_at, DEFAULT_ZSTD_LEVEL)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            if post.as_bytes().len() <= limit {
                let first_chunk = numbers.first().copied().unwrap_or(0);
                let last_chunk = numbers.last().copied().unwrap_or(0);
                return Ok(Some(Batch {
                    first_chunk,
                    last_chunk,
                    chunk_count: chunks.len(),
                    post,
                }));
            }
            chunks.pop();
            numbers.pop();
        }

        let post = encode_post(&chunks, agent_id, created_at, DEFAULT_ZSTD_LEVEL)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        let first_chunk = numbers.first().copied().unwrap_or(0);
        let last_chunk = numbers.last().copied().unwrap_or(0);
        Ok(Some(Batch {
            first_chunk,
            last_chunk,
            chunk_count: chunks.len(),
            post,
        }))
    }

    pub fn ack(&mut self, last_chunk: u64) -> io::Result<usize> {
        let remove: Vec<u64> = self
            .entries
            .range(..=last_chunk)
            .map(|(&number, _)| number)
            .collect();
        if remove.is_empty() {
            return Ok(0);
        }

        for &number in &remove {
            if let Some(size) = self.entries.remove(&number) {
                self.total_bytes = self.total_bytes.saturating_sub(size);
            }
            let path = self.dir.join(chunk_file_name(number));
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        File::open(&self.dir)?.sync_all()?;
        Ok(remove.len())
    }

    fn evict(&mut self) -> io::Result<()> {
        let mut evicted = false;
        while self.total_bytes > self.max_bytes && self.entries.len() > 1 {
            let Some((&oldest, &size)) = self.entries.first_key_value() else {
                break;
            };
            self.entries.remove(&oldest);
            self.total_bytes = self.total_bytes.saturating_sub(size);
            self.dropped_chunks = self.dropped_chunks.saturating_add(1);
            let path = self.dir.join(chunk_file_name(oldest));
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            evicted = true;
        }
        if evicted {
            File::open(&self.dir)?.sync_all()?;
        }
        Ok(())
    }
}

fn chunk_file_name(number: u64) -> String {
    format!("{CHUNK_PREFIX}{number:020}{CHUNK_SUFFIX}")
}

fn parse_chunk_name(name: &str) -> Option<u64> {
    name.strip_prefix(CHUNK_PREFIX)?
        .strip_suffix(CHUNK_SUFFIX)?
        .parse::<u64>()
        .ok()
}

fn read_next_chunk(dir: &Path) -> io::Result<Option<u64>> {
    match fs::read_to_string(dir.join(NEXT_CHUNK_FILE)) {
        Ok(text) => Ok(text.trim().parse::<u64>().ok()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temp = dir.join(format!(".{name}.new"));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    fs::rename(&temp, dir.join(name))?;
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use microscope_data::{
        capture::Capture,
        object::{ChunkData, ChunkRow, DimensionRow},
        sanitize::{Builtin, HashKey},
    };

    use super::*;
    use crate::agent::scratch::Scratch;

    fn sample_chunk(number: u64, secret: Option<&str>) -> SanitizedChunk {
        let mut dims = vec![
            DimensionRow {
                process: microscope_data::capture::records::NONE,
                name: "service.name".into(),
                value: "demo".into(),
            },
            DimensionRow {
                process: microscope_data::capture::records::NONE,
                name: "microscope.project".into(),
                value: "services".into(),
            },
        ];
        if let Some(secret) = secret {
            dims.push(DimensionRow {
                process: microscope_data::capture::records::NONE,
                name: "note".into(),
                value: format!("token={secret}"),
            });
        }
        let data = ChunkData {
            capture: Capture::default(),
            chunks: vec![ChunkRow {
                agent: 7,
                number,
                start: i64::try_from(number * 5_000_000_000).expect("ts"),
                end: i64::try_from((number + 1) * 5_000_000_000).expect("ts"),
                sampled_ns: 5_000_000_000,
                preset: 0,
                time_unit: TimeUnit::Us,
                policy: 0,
                rules: "default".into(),
            }],
            dimensions: dims,
            sanitizer_counts: Vec::new(),
        };
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"spool-test"));
        sanitize_chunk(data, TimeUnit::Us, &policy)
    }

    #[test]
    fn spool_persists_monotonic_numbers_batches_and_acks() {
        let scratch = Scratch::new();
        let dir = scratch.path().join("spool");
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"spool-test"));

        let mut spool = Spool::open(&dir, 1024 * 1024).expect("open");
        let n1 = spool.allocate_chunk().expect("alloc 1");
        let n2 = spool.allocate_chunk().expect("alloc 2");
        assert_eq!((n1, n2), (1, 2));

        spool
            .push(n1, &sample_chunk(n1, None), 7, 100)
            .expect("push 1");
        spool
            .push(n2, &sample_chunk(n2, None), 7, 200)
            .expect("push 2");
        assert_eq!(spool.stats().queued_chunks, 2);

        drop(spool);
        let mut reopened = Spool::open(&dir, 1024 * 1024).expect("reopen");
        assert_eq!(reopened.stats().queued_chunks, 2);
        assert_eq!(reopened.allocate_chunk().expect("alloc 3"), 3);

        let batch = reopened
            .peek_batch(MAX_POST_BYTES, 7, 300, &policy)
            .expect("peek")
            .expect("batch");
        assert_eq!(
            (batch.first_chunk, batch.last_chunk, batch.chunk_count),
            (1, 2, 2)
        );

        assert_eq!(reopened.ack(1).expect("ack 1"), 1);
        assert_eq!(reopened.stats().queued_chunks, 1);
        assert_eq!(reopened.ack(2).expect("ack 2"), 1);
        assert!(reopened.is_empty());
    }

    #[test]
    fn spool_evicts_oldest_when_over_byte_cap() {
        let scratch = Scratch::new();
        let dir = scratch.path().join("spool");
        let mut spool = Spool::open(&dir, 1).expect("open");
        for i in 1..=5 {
            let n = spool.allocate_chunk().expect("alloc");
            spool
                .push(n, &sample_chunk(n, None), 7, i * 10)
                .expect("push");
        }
        let stats = spool.stats();
        assert_eq!(stats.queued_chunks, 1);
        assert_eq!(stats.dropped_chunks, 4);
    }
}
