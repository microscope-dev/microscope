//! Deduplicated background queue and multipart uploader for debug symbols
//! requested by the ingest server, with bounded tracking maps, retry backoff,
//! and cooldown on persistent failure.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use microscope_data::symbols::{
    BuildId, extract_build_id, has_debug_or_symtab, read_elf_build_id_from_reader,
    strip_to_debug_elf,
};
use nix::libc;
use ring::digest::{SHA256, digest};

use super::client::{Client, SymbolPartHeader, SymbolPartReply};
use crate::DebugFileOptions;
use crate::debug_info::DebugFileSearch;

pub const SYMBOL_PIECE_BYTES: usize = 2 * 1024 * 1024;
const MAX_UPLOAD_COMPRESSED_BYTES: usize = 128 * 1024 * 1024;
const MAX_ELF_READ_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TRACKED_BINARIES: usize = 2048;
const MAX_COMPLETED_TASKS: usize = 4096;
const FAILED_COOLDOWN: Duration = Duration::from_secs(3600);
const MAX_RETRIES: u32 = 6;
const RETRY_BASE_DELAY: Duration = Duration::from_secs(15);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedBinary {
    pub path: PathBuf,
    pub pid: Option<u32>,
    pub project: Option<String>,
    pub service: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UploadTaskKey {
    pub project_id: Option<i64>,
    pub build_id: String,
}

#[derive(Clone, Debug)]
struct PendingUpload {
    project_key: Option<String>,
    project_name: Option<String>,
    binary: RecordedBinary,
    next_attempt: Instant,
    attempts: u32,
}

#[derive(Default)]
pub struct SymbolUploadQueue {
    known_by_project: BTreeMap<(Option<String>, String), RecordedBinary>,
    known_any: BTreeMap<String, RecordedBinary>,
    pending: BTreeMap<UploadTaskKey, PendingUpload>,
    in_flight: BTreeMap<UploadTaskKey, PendingUpload>,
    completed: BTreeSet<UploadTaskKey>,
    failed_until: BTreeMap<UploadTaskKey, Instant>,
}

fn normalize_project_key(project: Option<&str>) -> Option<String> {
    project
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
}

fn retry_backoff(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(5);
    RETRY_BASE_DELAY
        .saturating_mul(1_u32 << shift)
        .min(RETRY_MAX_DELAY)
}

impl SymbolUploadQueue {
    pub fn record_binary(
        &mut self,
        build_id: &str,
        path: PathBuf,
        pid: Option<u32>,
        project: Option<String>,
        service: Option<String>,
    ) {
        let Ok(bid) = BuildId::parse(build_id) else {
            return;
        };
        let bid_str = bid.as_str().to_owned();
        let proj_key = normalize_project_key(project.as_deref());
        let scoped_key = (proj_key, bid_str.clone());
        if self.known_by_project.len() >= MAX_TRACKED_BINARIES
            && !self.known_by_project.contains_key(&scoped_key)
            && let Some(first) = self.known_by_project.keys().next().cloned()
        {
            self.known_by_project.remove(&first);
        }
        let binary = RecordedBinary {
            path,
            pid,
            project,
            service,
        };
        self.known_by_project.insert(scoped_key, binary.clone());
        if self.known_any.len() >= MAX_TRACKED_BINARIES
            && !self.known_any.contains_key(&bid_str)
            && let Some(first) = self.known_any.keys().next().cloned()
        {
            self.known_any.remove(&first);
        }
        self.known_any.insert(bid_str, binary);
    }

    pub fn enqueue_missing(
        &mut self,
        project_id: Option<i64>,
        project_name: Option<&str>,
        build_ids: &[String],
    ) -> bool {
        let now = Instant::now();
        let proj_key = normalize_project_key(project_name);
        let mut added = false;
        for raw in build_ids {
            let Ok(bid) = BuildId::parse(raw) else {
                continue;
            };
            let bid_str = bid.as_str().to_owned();
            let binary = self
                .known_by_project
                .get(&(proj_key.clone(), bid_str.clone()))
                .or_else(|| {
                    proj_key
                        .as_ref()
                        .and_then(|_| self.known_by_project.get(&(None, bid_str.clone())))
                })
                .or_else(|| self.known_any.get(&bid_str))
                .cloned();
            let Some(mut binary) = binary else {
                continue;
            };
            if proj_key.is_none() || normalize_project_key(binary.project.as_deref()) != proj_key {
                binary.project = project_name.map(str::to_owned);
                binary.service = None;
            }
            let task = UploadTaskKey {
                project_id,
                build_id: bid_str,
            };
            if self.completed.contains(&task)
                || self.pending.contains_key(&task)
                || self.in_flight.contains_key(&task)
                || self
                    .failed_until
                    .get(&task)
                    .is_some_and(|&until| until > now)
            {
                continue;
            }
            self.failed_until.remove(&task);
            self.pending.insert(
                task,
                PendingUpload {
                    project_key: proj_key.clone(),
                    project_name: project_name.map(str::to_owned),
                    binary,
                    next_attempt: now,
                    attempts: 0,
                },
            );
            added = true;
        }
        added
    }

    pub fn pop_next(&mut self) -> Option<(UploadTaskKey, RecordedBinary)> {
        self.pop_next_at(Instant::now())
    }

    pub fn pop_next_at(&mut self, now: Instant) -> Option<(UploadTaskKey, RecordedBinary)> {
        let ready_key = self
            .pending
            .iter()
            .filter(|(task, item)| !self.completed.contains(*task) && item.next_attempt <= now)
            .min_by_key(|(_, item)| item.next_attempt)
            .map(|(task, _)| task.clone())?;
        let mut item = self.pending.remove(&ready_key)?;
        if item.project_key.is_some()
            && let Some(latest) = self
                .known_by_project
                .get(&(item.project_key.clone(), ready_key.build_id.clone()))
        {
            item.binary = latest.clone();
        } else if item.project_key.is_none()
            && let Some(latest) = self
                .known_by_project
                .get(&(None, ready_key.build_id.clone()))
                .or_else(|| self.known_any.get(&ready_key.build_id))
            && latest.path == item.binary.path
        {
            item.binary.pid = latest.pid;
            item.binary.project.clone_from(&item.project_name);
            item.binary.service = None;
        }
        let binary = item.binary.clone();
        self.in_flight.insert(ready_key.clone(), item);
        Some((ready_key, binary))
    }

    #[must_use]
    pub fn next_wait(&self, now: Instant) -> Option<Duration> {
        self.pending
            .values()
            .map(|item| item.next_attempt.saturating_duration_since(now))
            .min()
    }

    pub fn mark_done(&mut self, task: &UploadTaskKey) {
        if self.completed.len() >= MAX_COMPLETED_TASKS
            && !self.completed.contains(task)
            && let Some(first) = self.completed.first().cloned()
        {
            self.completed.remove(&first);
        }
        self.completed.insert(task.clone());
        self.failed_until.remove(task);
        self.pending.remove(task);
        self.in_flight.remove(task);
    }

    pub fn mark_retry_later(&mut self, task: &UploadTaskKey) {
        self.mark_retry_later_at(task, Instant::now());
    }

    pub fn mark_retry_later_at(&mut self, task: &UploadTaskKey, now: Instant) {
        let Some(mut item) = self.in_flight.remove(task) else {
            return;
        };
        if self.completed.contains(task) {
            return;
        }
        item.attempts = item.attempts.saturating_add(1);
        if item.attempts > MAX_RETRIES {
            if self.failed_until.len() >= MAX_COMPLETED_TASKS
                && !self.failed_until.contains_key(task)
                && let Some(first) = self.failed_until.keys().next().cloned()
            {
                self.failed_until.remove(&first);
            }
            self.failed_until
                .insert(task.clone(), now + FAILED_COOLDOWN);
            return;
        }
        item.next_attempt = now + retry_backoff(item.attempts);
        self.pending.insert(task.clone(), item);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PrepareOutcome {
    Ready(Vec<u8>),
    PermanentSkip,
    RetryLater,
}

fn read_regular_elf_bounded(path: &Path, follow_symlinks: bool) -> Option<Vec<u8>> {
    let mut flags = libc::O_NONBLOCK | libc::O_CLOEXEC;
    if !follow_symlinks {
        flags |= libc::O_NOFOLLOW;
    }
    let file: File = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.file_type().is_file() || meta.len() == 0 || meta.len() > MAX_ELF_READ_BYTES {
        return None;
    }
    let len = usize::try_from(meta.len()).ok()?;
    let mut out = Vec::with_capacity(len.min(1024 * 1024));
    file.take(MAX_ELF_READ_BYTES.saturating_add(1))
        .read_to_end(&mut out)
        .ok()?;
    if (out.len() as u64) > MAX_ELF_READ_BYTES {
        return None;
    }
    Some(out)
}

fn check_regular_elf_build_id(path: &Path, follow_symlinks: bool, expected: &str) -> bool {
    let mut flags = libc::O_NONBLOCK | libc::O_CLOEXEC;
    if !follow_symlinks {
        flags |= libc::O_NOFOLLOW;
    }
    let Ok(mut file) = OpenOptions::new().read(true).custom_flags(flags).open(path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    if !meta.file_type().is_file() || meta.len() == 0 || meta.len() > MAX_ELF_READ_BYTES {
        return false;
    }
    matches!(
        read_elf_build_id_from_reader(&mut file),
        Ok(Some(bid)) if bid.as_str() == expected
    )
}

fn read_verified_elf(
    binary: &RecordedBinary,
    expected_build_id: &str,
) -> Result<(PathBuf, Vec<u8>), PrepareOutcome> {
    let mut candidates: Vec<(PathBuf, bool)> = vec![(binary.path.clone(), true)];
    if let Some(pid) = binary.pid {
        candidates.push((PathBuf::from(format!("/proc/{pid}/exe")), true));
        if let Ok(rel) = binary.path.strip_prefix("/") {
            candidates.push((PathBuf::from(format!("/proc/{pid}/root")).join(rel), false));
        }
    }

    for (candidate, follow_symlinks) in candidates {
        if !check_regular_elf_build_id(&candidate, follow_symlinks, expected_build_id) {
            continue;
        }
        if let Some(raw) = read_regular_elf_bounded(&candidate, follow_symlinks)
            && let Ok(bid) = extract_build_id(&raw)
            && bid.as_str() == expected_build_id
        {
            return Ok((candidate, raw));
        }
    }

    Err(PrepareOutcome::RetryLater)
}

#[must_use]
pub fn prepare_debug_payload(binary: &RecordedBinary, expected_build_id: &str) -> PrepareOutcome {
    let (resolved_path, raw) = match read_verified_elf(binary, expected_build_id) {
        Ok(pair) => pair,
        Err(outcome) => return outcome,
    };

    let debug_elf = if has_debug_or_symtab(&raw) {
        match strip_to_debug_elf(&raw) {
            Ok(stripped) => stripped,
            Err(error) => {
                crate::agent::log(format_args!(
                    "refusing to upload unstripped binary {} ({expected_build_id}): {error}",
                    resolved_path.display()
                ));
                return PrepareOutcome::PermanentSkip;
            }
        }
    } else {
        let Ok(parsed) = object::File::parse(raw.as_slice()) else {
            return PrepareOutcome::PermanentSkip;
        };
        let search = DebugFileSearch::new(&DebugFileOptions {
            debuginfod: false,
            ..DebugFileOptions::default()
        });
        let Some(found) = search
            .find(&resolved_path, &parsed)
            .found
            .or_else(|| search.find(&binary.path, &parsed).found)
        else {
            return PrepareOutcome::PermanentSkip;
        };
        if !has_debug_or_symtab(&found.data) {
            return PrepareOutcome::PermanentSkip;
        }
        match strip_to_debug_elf(&found.data) {
            Ok(stripped) => stripped,
            Err(error) => {
                crate::agent::log(format_args!(
                    "refusing to upload unstripped debug file for {expected_build_id}: {error}"
                ));
                return PrepareOutcome::PermanentSkip;
            }
        }
    };

    let Ok(compressed) = zstd::bulk::compress(&debug_elf, 3) else {
        return PrepareOutcome::RetryLater;
    };
    if compressed.is_empty() || compressed.len() > MAX_UPLOAD_COMPRESSED_BYTES {
        return PrepareOutcome::PermanentSkip;
    }
    PrepareOutcome::Ready(compressed)
}

#[derive(Debug, PartialEq, Eq)]
pub enum UploadOutcome {
    Done,
    Unauthorized,
    RetryLater,
}

pub fn upload_compressed_symbol(
    client: &Client,
    token: &str,
    task: &UploadTaskKey,
    binary: &RecordedBinary,
    compressed: &[u8],
    max_bytes_per_sec: u64,
    stop: &AtomicBool,
) -> UploadOutcome {
    let build_id = task.build_id.as_str();
    if compressed.is_empty() {
        return UploadOutcome::Done;
    }
    let sha_hex = sha256_hex(compressed);
    let total_bytes = compressed.len() as u64;
    let pieces: Vec<&[u8]> = compressed.chunks(SYMBOL_PIECE_BYTES).collect();
    let Ok(total_parts) = u32::try_from(pieces.len()) else {
        return UploadOutcome::RetryLater;
    };

    let mut upload_id: Option<i64> = None;
    for (idx, piece) in pieces.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return UploadOutcome::RetryLater;
        }
        let Ok(part) = u32::try_from(idx) else {
            return UploadOutcome::RetryLater;
        };
        let started = Instant::now();
        let header = SymbolPartHeader {
            build_id,
            part,
            total_parts,
            total_bytes,
            sha256: &sha_hex,
            encoding: "zstd",
            upload_id,
            project_id: task.project_id,
            project: binary.project.as_deref(),
            service: binary.service.as_deref(),
        };
        match client.upload_symbol_part(token, &header, piece) {
            SymbolPartReply::Continue {
                upload_id: server_id,
                ..
            } => {
                upload_id = Some(server_id);
            }
            SymbolPartReply::Complete | SymbolPartReply::AlreadyPresent => {
                return UploadOutcome::Done;
            }
            SymbolPartReply::Unauthorized => return UploadOutcome::Unauthorized,
            SymbolPartReply::Refused { status, message } => {
                if status == 400 || status == 413 || status == 422 {
                    crate::agent::log(format_args!(
                        "server rejected symbol {build_id} ({status}): {message}"
                    ));
                    return UploadOutcome::Done;
                }
                return UploadOutcome::RetryLater;
            }
            SymbolPartReply::Unreachable(_) => return UploadOutcome::RetryLater,
        }

        if idx + 1 < pieces.len() && max_bytes_per_sec > 0 {
            let min_nanos =
                (piece.len() as u128).saturating_mul(1_000_000_000) / u128::from(max_bytes_per_sec);
            let min_dur = Duration::from_nanos(u64::try_from(min_nanos).unwrap_or(u64::MAX));
            let mut rem = min_dur.saturating_sub(started.elapsed());
            while !rem.is_zero() && !stop.load(Ordering::Relaxed) {
                let slice = rem.min(Duration::from_millis(50));
                std::thread::sleep(slice);
                rem = rem.saturating_sub(slice);
            }
        }
    }

    UploadOutcome::Done
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let d = digest(&SHA256, bytes);
    let mut out = String::with_capacity(64);
    for b in d.as_ref() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_tracks_per_project_isolates_metadata_and_retries_automatically() {
        let mut q = SymbolUploadQueue::default();
        assert!(!q.enqueue_missing(Some(10), Some("proj-a"), &["deadbeef".to_owned()]));

        q.record_binary(
            "DEADBEEF",
            PathBuf::from("/opt/a/app"),
            Some(42),
            Some("proj-a".to_owned()),
            Some("svc-a".to_owned()),
        );
        q.record_binary(
            "DEADBEEF",
            PathBuf::from("/opt/b/app"),
            Some(99),
            Some("proj-b".to_owned()),
            Some("svc-b".to_owned()),
        );
        assert!(q.enqueue_missing(
            Some(10),
            Some("proj-a"),
            &["deadbeef".to_owned(), "deadbeef".to_owned()]
        ));
        assert!(q.enqueue_missing(Some(20), Some("proj-b"), &["deadbeef".to_owned()]));

        let (task_a, bin_a) = q.pop_next().unwrap();
        assert_eq!(task_a.project_id, Some(10));
        assert_eq!(bin_a.path, PathBuf::from("/opt/a/app"));
        assert_eq!(bin_a.pid, Some(42));
        assert_eq!(bin_a.service.as_deref(), Some("svc-a"));
        q.mark_done(&task_a);

        let (task_b, bin_b) = q.pop_next().unwrap();
        assert_eq!(task_b.project_id, Some(20));
        assert_eq!(bin_b.path, PathBuf::from("/opt/b/app"));
        assert_eq!(bin_b.pid, Some(99));
        assert_eq!(bin_b.service.as_deref(), Some("svc-b"));

        let now = Instant::now();
        q.mark_retry_later_at(&task_b, now);
        q.record_binary(
            "DEADBEEF",
            PathBuf::from("/opt/b/app"),
            Some(1234),
            Some("proj-b".to_owned()),
            Some("svc-b".to_owned()),
        );
        assert!(q.pop_next_at(now).is_none());
        let (retry_task, retry_bin) = q
            .pop_next_at(now + Duration::from_secs(20))
            .expect("retryable task must become due again without another ingest ACK");
        assert_eq!(retry_task, task_b);
        assert_eq!(
            retry_bin.pid,
            Some(1234),
            "retry must refresh PID from the latest observation in the same project"
        );
        q.mark_done(&retry_task);

        assert!(q.enqueue_missing(Some(30), None, &["deadbeef".to_owned()]));
        let (task_unhinted, bin_unhinted) = q.pop_next().unwrap();
        assert_eq!(task_unhinted.project_id, Some(30));
        assert_eq!(
            bin_unhinted.service, None,
            "unhinted fallback must strip foreign project's service hint"
        );
        assert_eq!(bin_unhinted.project, None);
        q.mark_done(&task_unhinted);

        assert!(!q.enqueue_missing(Some(10), Some("proj-a"), &["deadbeef".to_owned()]));
        assert!(!q.enqueue_missing(Some(20), Some("proj-b"), &["deadbeef".to_owned()]));
    }

    #[test]
    fn retry_exhaustion_enters_cooldown_and_recovers_after_expiry() {
        let mut q = SymbolUploadQueue::default();
        q.record_binary(
            "cafe0001",
            PathBuf::from("/opt/app"),
            Some(7),
            Some("proj".to_owned()),
            Some("svc".to_owned()),
        );
        assert!(q.enqueue_missing(Some(1), Some("proj"), &["cafe0001".to_owned()]));
        let mut now = Instant::now();
        let (task, _) = q.pop_next_at(now).expect("initial pop");
        for i in 0..=MAX_RETRIES {
            q.mark_retry_later_at(&task, now);
            now += RETRY_MAX_DELAY + Duration::from_secs(1);
            if i < MAX_RETRIES {
                assert!(q.pop_next_at(now).is_some());
            }
        }
        assert!(q.in_flight.is_empty());
        assert!(q.pending.is_empty());
        assert!(q.failed_until.contains_key(&task));
        assert!(
            !q.enqueue_missing(Some(1), Some("proj"), &["cafe0001".to_owned()]),
            "exhausted build id must remain suppressed during cooldown"
        );
        q.failed_until.insert(
            task.clone(),
            Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
        );
        assert!(
            q.enqueue_missing(Some(1), Some("proj"), &["cafe0001".to_owned()]),
            "build id must become retryable once cooldown expires"
        );
    }
}
