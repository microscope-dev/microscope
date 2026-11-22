use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use microscope_data::symbols::{
    BuildId, extract_build_id, has_debug_or_symtab, strip_to_debug_elf,
};
use ring::digest::{SHA256, digest};

use super::client::{Client, SymbolPartHeader, SymbolPartReply};
use crate::DebugFileOptions;
use crate::debug_info::DebugFileSearch;

pub const SYMBOL_PIECE_BYTES: usize = 2 * 1024 * 1024;
const MAX_UPLOAD_COMPRESSED_BYTES: usize = 128 * 1024 * 1024;
const MAX_TRACKED_BINARIES: usize = 2048;
const RETRY_COOLDOWN: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedBinary {
    pub path: PathBuf,
    pub project: Option<String>,
    pub service: Option<String>,
}

#[derive(Default)]
pub struct SymbolUploadQueue {
    known_binaries: BTreeMap<String, RecordedBinary>,
    pending: VecDeque<String>,
    queued_set: BTreeSet<String>,
    completed: BTreeSet<String>,
    failed_until: BTreeMap<String, Instant>,
}

impl SymbolUploadQueue {
    pub fn record_binary(
        &mut self,
        build_id: &str,
        path: PathBuf,
        project: Option<String>,
        service: Option<String>,
    ) {
        let Ok(bid) = BuildId::parse(build_id) else {
            return;
        };
        let key = bid.as_str().to_owned();
        if self.known_binaries.len() >= MAX_TRACKED_BINARIES
            && !self.known_binaries.contains_key(&key)
            && let Some(first) = self.known_binaries.keys().next().cloned()
        {
            self.known_binaries.remove(&first);
        }
        self.known_binaries.insert(
            key,
            RecordedBinary {
                path,
                project,
                service,
            },
        );
    }

    pub fn enqueue_missing(&mut self, build_ids: &[String]) -> bool {
        let now = Instant::now();
        let mut added = false;
        for raw in build_ids {
            let Ok(bid) = BuildId::parse(raw) else {
                continue;
            };
            let key = bid.as_str().to_owned();
            if self.completed.contains(&key) || self.queued_set.contains(&key) {
                continue;
            }
            if self
                .failed_until
                .get(&key)
                .is_some_and(|&until| now < until)
            {
                continue;
            }
            if !self.known_binaries.contains_key(&key) {
                continue;
            }
            self.queued_set.insert(key.clone());
            self.pending.push_back(key);
            added = true;
        }
        added
    }

    pub fn pop_next(&mut self) -> Option<(String, RecordedBinary)> {
        while let Some(key) = self.pending.pop_front() {
            self.queued_set.remove(&key);
            if self.completed.contains(&key) {
                continue;
            }
            if let Some(bin) = self.known_binaries.get(&key).cloned() {
                return Some((key, bin));
            }
        }
        None
    }

    pub fn mark_done(&mut self, build_id: &str) {
        self.completed.insert(build_id.to_owned());
        self.failed_until.remove(build_id);
    }

    pub fn mark_retry_later(&mut self, build_id: &str) {
        self.failed_until
            .insert(build_id.to_owned(), Instant::now() + RETRY_COOLDOWN);
    }
}

#[must_use]
pub fn prepare_debug_payload(path: &Path, expected_build_id: &str) -> Option<Vec<u8>> {
    let raw = fs::read(path).ok()?;
    let Ok(bid) = extract_build_id(&raw) else {
        return None;
    };
    if bid.as_str() != expected_build_id {
        return None;
    }

    let debug_elf = if has_debug_or_symtab(&raw) {
        strip_to_debug_elf(&raw).unwrap_or(raw)
    } else {
        let parsed = object::File::parse(raw.as_slice()).ok()?;
        let search = DebugFileSearch::new(&DebugFileOptions {
            debuginfod: false,
            ..DebugFileOptions::default()
        });
        let found = search.find(path, &parsed).found?;
        if !has_debug_or_symtab(&found.data) {
            return None;
        }
        strip_to_debug_elf(&found.data).unwrap_or(found.data)
    };

    let compressed = zstd::bulk::compress(&debug_elf, 3).ok()?;
    if compressed.is_empty() || compressed.len() > MAX_UPLOAD_COMPRESSED_BYTES {
        return None;
    }
    Some(compressed)
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
    build_id: &str,
    binary: &RecordedBinary,
    compressed: &[u8],
    max_bytes_per_sec: u64,
    stop: &AtomicBool,
) -> UploadOutcome {
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
                    eprintln!(
                        "microscope agent: server rejected symbol {build_id} ({status}): {message}"
                    );
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
    fn queue_tracks_only_recorded_binaries_and_deduplicates() {
        let mut q = SymbolUploadQueue::default();
        assert!(!q.enqueue_missing(&["deadbeef".to_owned()]));

        q.record_binary(
            "DEADBEEF",
            PathBuf::from("/usr/bin/app"),
            Some("proj".to_owned()),
            Some("svc".to_owned()),
        );
        assert!(q.enqueue_missing(&["deadbeef".to_owned(), "deadbeef".to_owned()]));
        let (bid, bin) = q.pop_next().unwrap();
        assert_eq!(bid, "deadbeef");
        assert_eq!(bin.project.as_deref(), Some("proj"));
        assert!(q.pop_next().is_none());

        q.mark_done("deadbeef");
        assert!(!q.enqueue_missing(&["deadbeef".to_owned()]));
    }
}
