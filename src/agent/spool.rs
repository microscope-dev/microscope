//! Crash-safe on-disk chunk spool with atomic writes, strict owner binding
//! (`server` + `agent_id`), bounded FIFO eviction, and lock-free batch assembly
//! via `SpoolSnapshot`.

use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Write as _},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
};

use microscope_data::{
    object::{
        ChunkData, DEFAULT_ZSTD_LEVEL, ObjectReader, TimeUnit, decode_tables, shift_capture_times,
    },
    sanitize::{EncodedPost, SanitizedChunk, Sanitizer, encode_post, sanitize_chunk},
};

const NEXT_CHUNK_FILE: &str = "next_chunk";
const OWNER_FILE: &str = "owner";
const CHUNK_PREFIX: &str = "chunk-";
const CHUNK_SUFFIX: &str = ".usob";
const WALL_CLOCK_MIN_NS: i64 = 1_000_000_000_000_000_000;

pub const MAX_POST_BYTES: usize = 8 * 1024 * 1024;

const SCANNED_PER_POSTED: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpoolStats {
    pub queued_chunks: usize,
    pub queued_bytes: u64,
    pub dropped_chunks: u64,
    pub next_chunk: u64,
}

pub struct Batch {
    pub owner_gen: u64,
    pub first_chunk: u64,
    pub last_chunk: u64,
    pub chunk_count: usize,
    pub numbers: Vec<u64>,
    pub project: Option<String>,
    pub post: EncodedPost,
}

impl Batch {
    #[must_use]
    pub fn acked_by(&self, last_chunk: u64) -> Vec<u64> {
        self.numbers
            .iter()
            .copied()
            .filter(|number| *number <= last_chunk)
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct SpoolSnapshot {
    dir: PathBuf,
    pub owner_gen: u64,
    pub queued_total: usize,
    candidates: Vec<(u64, u64)>,
}

pub struct BatchBuildResult {
    pub batch: Option<Batch>,
    pub missing_chunks: Vec<u64>,
    pub drop_chunks: Vec<u64>,
}

#[derive(Debug)]
pub struct Spool {
    dir: PathBuf,
    max_bytes: u64,
    entries: BTreeMap<u64, u64>,
    total_bytes: u64,
    next_chunk: u64,
    dropped_chunks: u64,
    highest_lost_chunk: u64,
    owner_gen: u64,
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
            highest_lost_chunk: 0,
            owner_gen: 0,
        };
        spool.evict()?;
        Ok(spool)
    }

    #[must_use]
    pub const fn highest_lost_chunk(&self) -> u64 {
        self.highest_lost_chunk
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

    /// Binds the spool to a `(server, agent_id)` owner, purging queued chunks
    /// on mismatch so data from one identity is never uploaded under another.
    pub fn bind_owner(
        &mut self,
        server: &str,
        agent_id: Option<u64>,
        adopt_legacy: bool,
    ) -> io::Result<bool> {
        let existing = read_owner(&self.dir)?;
        let mut purged = false;
        let target_agent = match &existing {
            OwnerFile::Valid(owner) if owner.server != server => {
                self.clear()?;
                purged = true;
                agent_id
            }
            OwnerFile::Valid(owner) => match (owner.agent_id, agent_id) {
                (Some(old), Some(new)) if old != new => {
                    self.clear()?;
                    purged = true;
                    Some(new)
                }
                (Some(old), None) => Some(old),
                (None, new) => new,
                (Some(same), Some(_)) => Some(same),
            },
            OwnerFile::Corrupt => {
                purged = !self.entries.is_empty();
                self.clear()?;
                agent_id
            }
            OwnerFile::Missing => {
                if !(adopt_legacy && agent_id.is_some())
                    && (!self.entries.is_empty() || self.next_chunk > 1)
                {
                    purged = !self.entries.is_empty();
                    self.clear()?;
                }
                agent_id
            }
        };

        if purged {
            crate::agent::log(format_args!(
                "discarded spooled chunks another server or agent recorded"
            ));
        }

        let next_owner = SpoolOwner {
            server: server.to_owned(),
            agent_id: target_agent,
        };
        if !matches!(&existing, OwnerFile::Valid(current) if current == &next_owner) {
            write_owner(&self.dir, &next_owner)?;
        }
        Ok(purged)
    }

    pub fn clear(&mut self) -> io::Result<usize> {
        let numbers: Vec<u64> = self.entries.keys().copied().collect();
        for &number in &numbers {
            let path = self.dir.join(chunk_file_name(number));
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        self.entries.clear();
        self.total_bytes = 0;
        self.next_chunk = 1;
        self.highest_lost_chunk = 0;
        self.owner_gen = self.owner_gen.saturating_add(1);
        write_atomic(&self.dir, NEXT_CHUNK_FILE, b"1\n")?;
        Ok(numbers.len())
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
        if bytes.len() > MAX_POST_BYTES {
            self.dropped_chunks = self.dropped_chunks.saturating_add(1);
            self.highest_lost_chunk = self.highest_lost_chunk.max(number);
            return Ok(0);
        }
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

    #[must_use]
    pub fn snapshot_candidates(&self) -> SpoolSnapshot {
        SpoolSnapshot {
            dir: self.dir.clone(),
            owner_gen: self.owner_gen,
            queued_total: self.entries.len(),
            candidates: self
                .entries
                .iter()
                .map(|(&number, &size)| (number, size))
                .collect(),
        }
    }

    pub fn apply_batch_drops(
        &mut self,
        owner_gen: u64,
        missing_chunks: &[u64],
        drop_chunks: &[u64],
    ) -> io::Result<()> {
        if owner_gen != self.owner_gen {
            return Ok(());
        }
        for &number in missing_chunks {
            if let Some(removed) = self.entries.remove(&number) {
                self.total_bytes = self.total_bytes.saturating_sub(removed);
            }
            self.highest_lost_chunk = self.highest_lost_chunk.max(number);
        }
        for &number in drop_chunks {
            if self.entries.contains_key(&number) {
                self.drop_entry(number)?;
            }
        }
        Ok(())
    }

    pub fn peek_batch(
        &mut self,
        max_bytes: usize,
        agent_id: u64,
        created_at: i64,
        sanitizer: &Sanitizer,
    ) -> io::Result<Option<Batch>> {
        self.peek_batch_capped(max_bytes, usize::MAX, agent_id, created_at, sanitizer)
    }

    pub fn peek_batch_capped(
        &mut self,
        max_bytes: usize,
        max_chunks: usize,
        agent_id: u64,
        created_at: i64,
        sanitizer: &Sanitizer,
    ) -> io::Result<Option<Batch>> {
        self.peek_batch_with_resolver(max_bytes, max_chunks, agent_id, created_at, |_| sanitizer)
    }

    pub fn peek_batch_with_resolver<'a>(
        &mut self,
        max_bytes: usize,
        max_chunks: usize,
        agent_id: u64,
        created_at: i64,
        sanitizer_for: impl Fn(Option<&str>) -> &'a Sanitizer,
    ) -> io::Result<Option<Batch>> {
        while !self.entries.is_empty() {
            let snapshot = self.snapshot_candidates();
            let built = build_batch_from_snapshot(
                &snapshot,
                max_bytes,
                max_chunks,
                agent_id,
                created_at,
                &sanitizer_for,
            )?;
            self.apply_batch_drops(
                snapshot.owner_gen,
                &built.missing_chunks,
                &built.drop_chunks,
            )?;
            if let Some(batch) = built.batch {
                return Ok(Some(batch));
            }
            if built.missing_chunks.is_empty() && built.drop_chunks.is_empty() {
                break;
            }
        }
        Ok(None)
    }

    pub fn drop_chunk(&mut self, owner_gen: u64, number: u64) -> io::Result<bool> {
        if owner_gen != self.owner_gen || !self.entries.contains_key(&number) {
            return Ok(false);
        }
        self.drop_entry(number)?;
        Ok(true)
    }

    fn drop_entry(&mut self, number: u64) -> io::Result<()> {
        if let Some(size) = self.entries.remove(&number) {
            self.total_bytes = self.total_bytes.saturating_sub(size);
        }
        self.dropped_chunks = self.dropped_chunks.saturating_add(1);
        self.highest_lost_chunk = self.highest_lost_chunk.max(number);
        let path = self.dir.join(chunk_file_name(number));
        match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        File::open(&self.dir)?.sync_all()
    }

    pub fn ack(&mut self, owner_gen: u64, numbers: &[u64]) -> io::Result<usize> {
        if owner_gen != self.owner_gen {
            return Ok(0);
        }

        let mut removed = 0;
        for number in numbers {
            let Some(size) = self.entries.remove(number) else {
                continue;
            };
            self.total_bytes = self.total_bytes.saturating_sub(size);
            removed += 1;
            let path = self.dir.join(chunk_file_name(*number));
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        if removed > 0 {
            File::open(&self.dir)?.sync_all()?;
        }
        Ok(removed)
    }

    #[must_use]
    pub fn holds_through(&self, number: u64) -> bool {
        self.entries.range(..=number).next().is_some()
    }

    /// Evicts oldest chunks until `total_bytes <= max_bytes`, always keeping
    /// at least one chunk so a single large chunk is not immediately dropped.
    fn evict(&mut self) -> io::Result<()> {
        let mut evicted = false;
        while self.total_bytes > self.max_bytes && self.entries.len() > 1 {
            let Some((&oldest, &size)) = self.entries.first_key_value() else {
                break;
            };
            self.entries.remove(&oldest);
            self.total_bytes = self.total_bytes.saturating_sub(size);
            self.dropped_chunks = self.dropped_chunks.saturating_add(1);
            self.highest_lost_chunk = self.highest_lost_chunk.max(oldest);
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

#[expect(
    clippy::too_many_lines,
    reason = "builds project-homogeneous sanitized batch from snapshot outside lock"
)]
pub fn build_batch_from_snapshot<'a>(
    snapshot: &SpoolSnapshot,
    max_bytes: usize,
    max_chunks: usize,
    agent_id: u64,
    created_at: i64,
    sanitizer_for: impl Fn(Option<&str>) -> &'a Sanitizer,
) -> io::Result<BatchBuildResult> {
    let limit = max_bytes.min(MAX_POST_BYTES);
    let chunk_cap = max_chunks.max(1);
    let mut chunks = Vec::new();
    let mut numbers = Vec::new();
    let mut missing_chunks = Vec::new();
    let mut drop_chunks = Vec::new();
    let mut estimated = 0_usize;
    let mut batch_project: Option<String> = None;
    let mut has_project = false;
    let mut read = 0_usize;

    for &(number, size) in &snapshot.candidates {
        let size_usize = usize::try_from(size).unwrap_or(usize::MAX);
        if chunks.len() >= chunk_cap
            || (!chunks.is_empty() && estimated.saturating_add(size_usize) > limit)
            || read > limit.saturating_mul(SCANNED_PER_POSTED)
        {
            break;
        }
        read = read.saturating_add(size_usize);
        let path = snapshot.dir.join(chunk_file_name(number));
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if chunks.is_empty() {
                    missing_chunks.push(number);
                    continue;
                }
                break;
            }
            Err(error) => {
                if chunks.is_empty() {
                    return Err(error);
                }
                break;
            }
        };

        let decoded = ObjectReader::open(&raw)
            .and_then(|reader| reader.part_sections(0, None))
            .and_then(|sections| decode_tables(&sections));
        let mut data = match decoded {
            Ok(data) if data.chunks.iter().all(|row| row.agent == agent_id) => data,
            Ok(_) | Err(_) => {
                if chunks.is_empty() {
                    drop_chunks.push(number);
                    break;
                }
                break;
            }
        };

        let project = normalize_project_dimension(&mut data);
        normalize_legacy_clock(&mut data);
        if has_project {
            if batch_project != project {
                continue;
            }
        } else {
            batch_project = project;
            has_project = true;
        }

        let unit = data
            .chunks
            .first()
            .map_or(TimeUnit::Us, |row| row.time_unit);
        let sanitizer = sanitizer_for(batch_project.as_deref());
        chunks.push(sanitize_chunk(data, unit, sanitizer));
        numbers.push(number);
        estimated = estimated.saturating_add(size_usize);
    }

    if chunks.is_empty() {
        return Ok(BatchBuildResult {
            batch: None,
            missing_chunks,
            drop_chunks,
        });
    }

    while chunks.len() > 1 {
        if let Ok(post) = encode_post(&chunks, agent_id, created_at, DEFAULT_ZSTD_LEVEL)
            && post.as_bytes().len() <= limit
        {
            let first_chunk = numbers.first().copied().unwrap_or(0);
            let last_chunk = numbers.last().copied().unwrap_or(0);
            return Ok(BatchBuildResult {
                batch: Some(Batch {
                    owner_gen: snapshot.owner_gen,
                    first_chunk,
                    last_chunk,
                    chunk_count: chunks.len(),
                    numbers,
                    project: batch_project,
                    post,
                }),
                missing_chunks,
                drop_chunks,
            });
        }
        chunks.pop();
        numbers.pop();
    }

    if let Ok(post) = encode_post(&chunks, agent_id, created_at, DEFAULT_ZSTD_LEVEL)
        && post.as_bytes().len() <= limit
    {
        let first_chunk = numbers.first().copied().unwrap_or(0);
        let last_chunk = numbers.last().copied().unwrap_or(0);
        return Ok(BatchBuildResult {
            batch: Some(Batch {
                owner_gen: snapshot.owner_gen,
                first_chunk,
                last_chunk,
                chunk_count: chunks.len(),
                numbers,
                project: batch_project,
                post,
            }),
            missing_chunks,
            drop_chunks,
        });
    }

    if let Some(oversized) = numbers.first().copied() {
        drop_chunks.push(oversized);
    }

    Ok(BatchBuildResult {
        batch: None,
        missing_chunks,
        drop_chunks,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct SpoolOwner {
    server: String,
    agent_id: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
enum OwnerFile {
    Missing,
    Corrupt,
    Valid(SpoolOwner),
}

fn read_owner(dir: &Path) -> io::Result<OwnerFile> {
    let text = match fs::read_to_string(dir.join(OWNER_FILE)) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(OwnerFile::Missing),
        Err(error) => return Err(error),
    };
    let mut lines = text.lines();
    let Some(server) = lines.next().filter(|line| !line.is_empty()) else {
        return Ok(OwnerFile::Corrupt);
    };
    let agent_id = match lines.next().filter(|line| !line.is_empty()) {
        Some(raw) => match raw.parse::<u64>() {
            Ok(id) => Some(id),
            Err(_) => return Ok(OwnerFile::Corrupt),
        },
        None => None,
    };
    Ok(OwnerFile::Valid(SpoolOwner {
        server: server.to_owned(),
        agent_id,
    }))
}

fn write_owner(dir: &Path, owner: &SpoolOwner) -> io::Result<()> {
    let content = owner.agent_id.map_or_else(
        || format!("{}\n", owner.server),
        |id| format!("{}\n{id}\n", owner.server),
    );
    write_atomic(dir, OWNER_FILE, content.as_bytes())
}

fn normalize_project_dimension(data: &mut ChunkData) -> Option<String> {
    let mut kept: Option<String> = None;
    data.dimensions.retain(|dim| {
        if dim.name != "microscope.project" {
            return true;
        }
        if kept.is_none() {
            kept = Some(dim.value.clone());
            true
        } else {
            false
        }
    });
    kept
}

fn normalize_legacy_clock(data: &mut ChunkData) {
    let Some(block_end) = data.chunks.iter().map(|row| row.end).max() else {
        return;
    };
    if block_end < WALL_CLOCK_MIN_NS {
        return;
    }
    let span_end = data.capture.span().1;
    let sample_max = data
        .capture
        .samples
        .iter()
        .map(|sample| sample.time.get())
        .max()
        .unwrap_or(0);
    let event_max = span_end.max(sample_max);
    if event_max <= 0 || event_max >= WALL_CLOCK_MIN_NS {
        return;
    }
    let delta = block_end.saturating_sub(event_max);
    shift_capture_times(&mut data.capture, delta);
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

        assert_eq!(reopened.ack(batch.owner_gen, &[1]).expect("ack 1"), 1);
        assert_eq!(reopened.stats().queued_chunks, 1);
        assert_eq!(reopened.ack(batch.owner_gen, &[2]).expect("ack 2"), 1);
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

    fn project_chunk(agent_id: u64, number: u64, project: &str) -> SanitizedChunk {
        let data = ChunkData {
            capture: Capture::default(),
            chunks: vec![ChunkRow {
                agent: agent_id,
                number,
                start: i64::try_from(number * 5_000_000_000).expect("ts"),
                end: i64::try_from((number + 1) * 5_000_000_000).expect("ts"),
                sampled_ns: 5_000_000_000,
                preset: 0,
                time_unit: TimeUnit::Us,
                policy: 0,
                rules: "default".into(),
            }],
            dimensions: vec![DimensionRow {
                process: microscope_data::capture::records::NONE,
                name: "microscope.project".into(),
                value: project.into(),
            }],
            sanitizer_counts: Vec::new(),
        };
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"spool-test"));
        sanitize_chunk(data, TimeUnit::Us, &policy)
    }

    #[test]
    fn spool_purges_queued_chunks_when_owner_changes() {
        let scratch = Scratch::new();
        let dir = scratch.path().join("spool");
        let mut spool = Spool::open(&dir, 1024 * 1024).expect("open");
        assert!(
            !spool
                .bind_owner("https://a.example", Some(7), false)
                .expect("bind")
        );
        let n1 = spool.allocate_chunk().expect("alloc");
        spool
            .push(n1, &project_chunk(7, n1, "alpha"), 7, 100)
            .expect("push");
        assert_eq!(spool.stats().queued_chunks, 1);

        assert!(
            !spool
                .bind_owner("https://a.example", Some(7), false)
                .expect("same")
        );
        assert_eq!(spool.stats().queued_chunks, 1);

        assert!(
            spool
                .bind_owner("https://a.example", Some(8), false)
                .expect("new agent")
        );
        assert!(spool.is_empty());
        assert_eq!(spool.allocate_chunk().expect("reset alloc"), 1);

        spool
            .push(1, &project_chunk(8, 1, "alpha"), 8, 200)
            .expect("push 2");
        assert!(
            spool
                .bind_owner("https://b.example", Some(8), false)
                .expect("new server")
        );
        assert!(spool.is_empty());
    }

    #[test]
    fn spool_splits_batches_on_project_boundary() {
        let scratch = Scratch::new();
        let dir = scratch.path().join("spool");
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"spool-test"));
        let mut spool = Spool::open(&dir, 1024 * 1024).expect("open");
        spool
            .push(1, &project_chunk(7, 1, "alpha"), 7, 100)
            .expect("push 1");
        spool
            .push(2, &project_chunk(7, 2, "alpha"), 7, 200)
            .expect("push 2");
        spool
            .push(3, &project_chunk(7, 3, "beta"), 7, 300)
            .expect("push 3");

        let first = spool
            .peek_batch(MAX_POST_BYTES, 7, 400, &policy)
            .expect("peek 1")
            .expect("batch 1");
        assert_eq!(
            (first.first_chunk, first.last_chunk, first.chunk_count),
            (1, 2, 2)
        );
        spool.ack(first.owner_gen, &first.numbers).expect("ack 1");

        let second = spool
            .peek_batch(MAX_POST_BYTES, 7, 500, &policy)
            .expect("peek 2")
            .expect("batch 2");
        assert_eq!(
            (second.first_chunk, second.last_chunk, second.chunk_count),
            (3, 3, 1)
        );
    }

    #[test]
    fn stale_generation_ack_does_not_delete_new_owner_chunks() {
        let scratch = Scratch::new();
        let dir = scratch.path().join("spool");
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"spool-test"));
        let mut spool = Spool::open(&dir, 1024 * 1024).expect("open");
        spool
            .bind_owner("https://a.example", Some(7), false)
            .expect("bind 7");
        spool
            .push(1, &project_chunk(7, 1, "alpha"), 7, 100)
            .expect("push 7");
        let old_batch = spool
            .peek_batch(MAX_POST_BYTES, 7, 150, &policy)
            .expect("peek old")
            .expect("old batch");

        assert!(
            spool
                .bind_owner("https://a.example", Some(8), false)
                .expect("bind 8")
        );
        spool
            .push(1, &project_chunk(8, 1, "alpha"), 8, 200)
            .expect("push 8");
        assert_eq!(
            spool
                .ack(old_batch.owner_gen, &old_batch.numbers)
                .expect("stale ack"),
            0
        );
        assert_eq!(spool.stats().queued_chunks, 1);
    }

    #[test]
    fn spool_drops_corrupt_and_oversized_head_chunks_without_stalling() {
        let scratch = Scratch::new();
        let dir = scratch.path().join("spool");
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"spool-test"));
        let mut spool = Spool::open(&dir, 1024 * 1024).expect("open");
        spool
            .push(1, &project_chunk(7, 1, "alpha"), 7, 100)
            .expect("push 1");
        spool
            .push(2, &project_chunk(7, 2, "alpha"), 7, 200)
            .expect("push 2");
        fs::write(dir.join(chunk_file_name(1)), b"not a valid usob").expect("corrupt 1");

        let batch = spool
            .peek_batch(MAX_POST_BYTES, 7, 300, &policy)
            .expect("peek after corrupt")
            .expect("chunk 2 survives");
        assert_eq!(
            (batch.first_chunk, batch.last_chunk, batch.chunk_count),
            (2, 2, 1)
        );
        assert_eq!(spool.stats().dropped_chunks, 1);

        let oversized = spool
            .peek_batch(10, 7, 400, &policy)
            .expect("peek with tiny limit");
        assert!(oversized.is_none());
        assert!(spool.is_empty());
        assert_eq!(spool.stats().dropped_chunks, 2);
    }

    #[test]
    fn legacy_spool_without_owner_preserves_chunks_and_sequence_on_upgrade() {
        let scratch = Scratch::new();
        let dir = scratch.path().join("spool");
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"spool-test"));
        let mut spool = Spool::open(&dir, 1024 * 1024).expect("open");
        let n1 = spool.allocate_chunk().expect("alloc 1");
        let n2 = spool.allocate_chunk().expect("alloc 2");
        spool
            .push(n1, &project_chunk(7, n1, "alpha"), 7, 100)
            .expect("push 1");
        spool
            .push(n2, &project_chunk(7, n2, "alpha"), 7, 200)
            .expect("push 2");
        drop(spool);

        let mut upgraded = Spool::open(&dir, 1024 * 1024).expect("reopen");
        assert!(
            !upgraded
                .bind_owner("https://a.example", Some(7), true)
                .expect("bind")
        );
        assert_eq!(upgraded.stats().queued_chunks, 2);
        assert_eq!(upgraded.allocate_chunk().expect("alloc 3"), 3);
        let batch = upgraded
            .peek_batch(MAX_POST_BYTES, 7, 300, &policy)
            .expect("peek")
            .expect("batch");
        assert_eq!((batch.first_chunk, batch.last_chunk), (1, 2));
    }

    #[test]
    fn legacy_spool_without_owner_is_purged_when_unverified() {
        let scratch = Scratch::new();
        let dir = scratch.path().join("spool");
        let mut spool = Spool::open(&dir, 1024 * 1024).expect("open");
        let n1 = spool.allocate_chunk().expect("alloc 1");
        spool
            .push(n1, &project_chunk(7, n1, "alpha"), 7, 100)
            .expect("push 1");
        drop(spool);

        let mut switched = Spool::open(&dir, 1024 * 1024).expect("reopen");
        assert!(
            switched
                .bind_owner("https://new.example", None, false)
                .expect("bind new server")
        );
        assert!(switched.is_empty());
        assert_eq!(switched.allocate_chunk().expect("alloc reset"), 1);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "builds legacy monotonic and wall-clock chunks and verifies merged batch"
    )]
    fn mixed_legacy_monotonic_and_wall_clock_batch_normalizes_all_samples() {
        use microscope_data::{
            capture::{builder::CaptureBuilder, records::FrameKind},
            model::{Pid, Tid},
        };

        let scratch = Scratch::new();
        let dir = scratch.path().join("spool");
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"spool-test"));
        let wall_base = 1_700_000_000_000_000_000_i64;

        let mut legacy = CaptureBuilder::new();
        legacy.setting("start", "1000000000");
        legacy.setting("end", "7000000000");
        let _p1 = legacy.process(Pid::new(100), 1_000_000_000);
        let t1 = legacy.thread(Pid::new(100), Tid::new(100), 1_000_000_000);
        let m1 = legacy.module("/usr/bin/a", None, [1; 16], 4096);
        let f1 = legacy.frame(FrameKind::Native, m1, 0x1000, 0);
        let s1 = legacy.stack(&[f1]);
        legacy.sample(
            t1,
            2_000_000_000,
            10_000_000,
            s1,
            microscope_data::capture::records::NONE,
        );
        let legacy_chunk = sanitize_chunk(
            ChunkData {
                capture: legacy.finish(),
                chunks: vec![ChunkRow {
                    agent: 7,
                    number: 1,
                    start: wall_base + 1_000_000_000,
                    end: wall_base + 7_000_000_000,
                    sampled_ns: 6_000_000_000,
                    preset: 0,
                    time_unit: TimeUnit::Us,
                    policy: 0,
                    rules: "default".into(),
                }],
                dimensions: vec![DimensionRow {
                    process: microscope_data::capture::records::NONE,
                    name: "microscope.project".into(),
                    value: "alpha".into(),
                }],
                sanitizer_counts: Vec::new(),
            },
            TimeUnit::Us,
            &policy,
        );

        let mut modern = CaptureBuilder::new();
        modern.setting("start", &(wall_base + 8_000_000_000).to_string());
        modern.setting("end", &(wall_base + 12_000_000_000).to_string());
        let _p2 = modern.process(Pid::new(100), wall_base + 8_000_000_000);
        let t2 = modern.thread(Pid::new(100), Tid::new(100), wall_base + 8_000_000_000);
        let m2 = modern.module("/usr/bin/a", None, [1; 16], 4096);
        let f2 = modern.frame(FrameKind::Native, m2, 0x2000, 0);
        let s2 = modern.stack(&[f2]);
        modern.sample(
            t2,
            wall_base + 9_000_000_000,
            10_000_000,
            s2,
            microscope_data::capture::records::NONE,
        );
        let modern_chunk = sanitize_chunk(
            ChunkData {
                capture: modern.finish(),
                chunks: vec![ChunkRow {
                    agent: 7,
                    number: 2,
                    start: wall_base + 8_000_000_000,
                    end: wall_base + 12_000_000_000,
                    sampled_ns: 4_000_000_000,
                    preset: 0,
                    time_unit: TimeUnit::Us,
                    policy: 0,
                    rules: "default".into(),
                }],
                dimensions: vec![DimensionRow {
                    process: microscope_data::capture::records::NONE,
                    name: "microscope.project".into(),
                    value: "alpha".into(),
                }],
                sanitizer_counts: Vec::new(),
            },
            TimeUnit::Us,
            &policy,
        );

        let mut spool = Spool::open(&dir, 1024 * 1024).expect("open");
        spool.push(1, &legacy_chunk, 7, wall_base).expect("push 1");
        spool.push(2, &modern_chunk, 7, wall_base).expect("push 2");

        let batch = spool
            .peek_batch(MAX_POST_BYTES, 7, wall_base + 15_000_000_000, &policy)
            .expect("peek")
            .expect("batch");
        assert_eq!(batch.chunk_count, 2);

        let reader = ObjectReader::open(batch.post.as_bytes()).expect("open post");
        let sections = reader.part_sections(0, None).expect("sections");
        let decoded = decode_tables(&sections).expect("decode");
        assert_eq!(decoded.capture.samples.len(), 2);
        let times: Vec<i64> = decoded
            .capture
            .samples
            .iter()
            .map(|s| s.time.get())
            .collect();
        assert!(
            times.iter().all(|&t| t >= WALL_CLOCK_MIN_NS),
            "all samples must be wall-clock: {times:?}"
        );
        assert!(times.contains(&(wall_base + 2_000_000_000)), "{times:?}");
        assert!(times.contains(&(wall_base + 9_000_000_000)), "{times:?}");
    }

    #[test]
    fn spool_shrinks_batch_when_merged_strings_exceed_page_raw_limit() {
        use crate::capture::CaptureBuilder;
        use crate::model::Pid;

        let scratch = Scratch::new();
        let dir = scratch.path().join("spool");
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"test-key"));
        let wall_base = 1_770_000_000_000_000_000_i64;

        let make_chunk = |num: u64, fill: char| {
            let mut b = CaptureBuilder::new();
            let big = std::iter::repeat_n(fill, 18 * 1024 * 1024).collect::<String>();
            b.setting(&format!("note_{num}"), &big);
            let _p = b.process(Pid::new(100), wall_base);
            sanitize_chunk(
                ChunkData {
                    capture: b.finish(),
                    chunks: vec![ChunkRow {
                        agent: 7,
                        number: num,
                        start: wall_base + i64::try_from(num).expect("i64") * 1_000_000_000,
                        end: wall_base + (i64::try_from(num).expect("i64") + 1) * 1_000_000_000,
                        sampled_ns: 1_000_000_000,
                        preset: 0,
                        time_unit: TimeUnit::Us,
                        policy: 0,
                        rules: "default".into(),
                    }],
                    dimensions: vec![DimensionRow {
                        process: microscope_data::capture::records::NONE,
                        name: "microscope.project".into(),
                        value: "alpha".into(),
                    }],
                    sanitizer_counts: Vec::new(),
                },
                TimeUnit::Us,
                &policy,
            )
        };

        let c1 = make_chunk(1, 'a');
        let c2 = make_chunk(2, 'b');

        let mut spool = Spool::open(&dir, 64 * 1024 * 1024).expect("open");
        spool.push(1, &c1, 7, wall_base).expect("push 1");
        spool.push(2, &c2, 7, wall_base).expect("push 2");

        let batch = spool
            .peek_batch(MAX_POST_BYTES, 7, wall_base + 5_000_000_000, &policy)
            .expect("peek")
            .expect("batch");
        assert_eq!(batch.chunk_count, 1);
        assert_eq!(batch.first_chunk, 1);
        assert_eq!(batch.last_chunk, 1);

        let reader = ObjectReader::open(batch.post.as_bytes()).expect("open post");
        let sections = reader.part_sections(0, None).expect("sections");
        let decoded = decode_tables(&sections).expect("decode shrunken batch");
        assert_eq!(decoded.chunks.len(), 1);
    }
}
