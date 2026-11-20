use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use microscope_data::{
    capture::records::NONE,
    object::{ChunkData, ChunkRow, DimensionRow},
    sanitize::{Builtin, HashKey, Sanitizer, sanitize_chunk},
};

use super::{
    client::{Client, IngestReply},
    config::{Config, RecordConfig},
    machine::Instant,
    poster::{PosterAction, PosterMachine, PosterOutcome},
    spool::{MAX_POST_BYTES, Spool},
};
use crate::profiler::{
    Cuts, Preset, Profile, RecordOptions, Save, SharedBpfEngine, SyscallLevel, Watch,
    record_attached,
};

const AGENT_DEV_MAX_SYSCALL_CAP: u32 = 20_000;

struct SharedState {
    token: Option<String>,
    agent_id: Option<u64>,
    need_token: bool,
    spool_seq: u64,
    spool: Spool,
}

pub struct Pipeline {
    stop: Arc<AtomicBool>,
    shared: Arc<(Mutex<SharedState>, Condvar)>,
    recorder: Option<JoinHandle<()>>,
    poster: Option<JoinHandle<()>>,
}

impl Pipeline {
    pub fn start(
        config: &Config,
        record: &RecordConfig,
        host_name: String,
        initial_agent_id: Option<i64>,
    ) -> anyhow::Result<Self> {
        let max_bytes = u64::from(record.spool_mib) * 1024 * 1024;
        let spool = Spool::open(&config.state_dir.join("spool"), max_bytes)?;
        let client = Client::new(config.server.clone(), config.ca_file.as_deref())?;
        let sanitizer = Arc::new(Sanitizer::new(
            &Builtin::DEFAULT,
            Vec::new(),
            HashKey::new(b"microscope-agent-local"),
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new((
            Mutex::new(SharedState {
                token: None,
                agent_id: initial_agent_id.and_then(|id| u64::try_from(id).ok()),
                need_token: false,
                spool_seq: 0,
                spool,
            }),
            Condvar::new(),
        ));

        let rec_stop = Arc::clone(&stop);
        let rec_shared = Arc::clone(&shared);
        let rec_sanitizer = Arc::clone(&sanitizer);
        let rec_config = record.clone();
        let tags = config.tags.clone();
        let recorder = thread::Builder::new()
            .name("microscope-agent-recorder".into())
            .spawn(move || {
                recorder_loop(
                    &rec_stop,
                    &rec_shared,
                    &rec_config,
                    &host_name,
                    &tags,
                    &rec_sanitizer,
                );
            })?;

        let post_stop = Arc::clone(&stop);
        let post_shared = Arc::clone(&shared);
        let post_interval = Duration::from_millis(record.post_ms);
        let poster = thread::Builder::new()
            .name("microscope-agent-poster".into())
            .spawn(move || {
                poster_loop(&post_stop, &post_shared, &client, post_interval, &sanitizer);
            })?;

        Ok(Self {
            stop,
            shared,
            recorder: Some(recorder),
            poster: Some(poster),
        })
    }

    pub fn set_auth(&self, agent_id: i64, token: Option<String>) {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.agent_id = u64::try_from(agent_id).ok();
        if let Some(tok) = token {
            guard.token = Some(tok);
            guard.need_token = false;
        } else {
            guard.token = None;
        }
        drop(guard);
        cvar.notify_all();
    }

    #[must_use]
    pub fn take_need_token(&self) -> bool {
        let (lock, _) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let needed = guard.need_token;
        guard.need_token = false;
        needed
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let (_, cvar) = &*self.shared;
        cvar.notify_all();
        if let Some(handle) = self.recorder.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.poster.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

fn unix_now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

const fn preset_code(preset: Preset) -> u8 {
    match preset {
        Preset::Production => 0,
        Preset::Development => 1,
        Preset::Maximum => 2,
    }
}

fn find_target_pid(name: &str) -> Option<u32> {
    let self_pid = std::process::id();
    let entries = fs::read_dir("/proc").ok()?;
    let mut best = None;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let proc_dir = entry.path();
        let comm_match =
            fs::read_to_string(proc_dir.join("comm")).is_ok_and(|comm| comm.trim() == name);
        let exe_match = !comm_match
            && fs::read_link(proc_dir.join("exe")).is_ok_and(|exe| {
                exe.file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|f| f == name || f.strip_suffix(" (deleted)") == Some(name))
            });
        if comm_match || exe_match {
            best = Some(best.map_or(pid, |current: u32| current.min(pid)));
        }
    }
    best
}

fn build_record_options(
    record: &RecordConfig,
    shared_bpf: Option<&SharedBpfEngine>,
) -> RecordOptions {
    let mut options = RecordOptions::preset(record.preset);
    options.cuts = Cuts::Segments(Duration::from_millis(record.chunk_ms));
    options.symbols_later = false;
    if let Some(bpf) = shared_bpf {
        if record.preset != Preset::Production {
            options.syscalls.kept_per_second = Some(AGENT_DEV_MAX_SYSCALL_CAP);
        }
        options.syscalls.shared_bpf = Some(bpf.clone());
    } else {
        options.syscalls.level = SyscallLevel::Off;
        options.syscalls.kept_per_second = None;
    }
    options
}

const fn profile_has_data(profile: &Profile) -> bool {
    let c = profile.capture();
    !c.samples.is_empty()
        || !c.off_cpu.is_empty()
        || !c.syscalls.is_empty()
        || !c.syscall_stats.is_empty()
        || !c.zones.is_empty()
        || !c.markers.is_empty()
}

#[must_use]
pub fn build_chunk_data(
    profile: Profile,
    agent_id: u64,
    chunk_number: u64,
    record: &RecordConfig,
    host_name: &str,
    tags: &BTreeMap<String, String>,
    now_ns: i64,
) -> ChunkData {
    let (mono_start, mono_end) = profile.capture().span();
    let span_ns = mono_end.saturating_sub(mono_start).max(0);
    let end = now_ns;
    let start = end.saturating_sub(span_ns);
    let sampled_ns = u64::try_from(span_ns).unwrap_or(0);

    let mut dimensions = vec![
        DimensionRow {
            process: NONE,
            name: "service.name".into(),
            value: record.service.clone(),
        },
        DimensionRow {
            process: NONE,
            name: "microscope.project".into(),
            value: record.project.clone(),
        },
        DimensionRow {
            process: NONE,
            name: "host.name".into(),
            value: host_name.to_owned(),
        },
    ];
    for (key, value) in tags {
        dimensions.push(DimensionRow {
            process: NONE,
            name: key.clone(),
            value: value.clone(),
        });
    }

    ChunkData {
        capture: profile.into_capture(),
        chunks: vec![ChunkRow {
            agent: agent_id,
            number: chunk_number,
            start,
            end,
            sampled_ns,
            preset: preset_code(record.preset),
            time_unit: record.time_unit,
            policy: 0,
            rules: String::new(),
        }],
        dimensions,
        sanitizer_counts: Vec::new(),
    }
}

fn enqueue_profile(
    profile: Profile,
    shared: &(Mutex<SharedState>, Condvar),
    record: &RecordConfig,
    host_name: &str,
    tags: &BTreeMap<String, String>,
    sanitizer: &Sanitizer,
) {
    if !profile_has_data(&profile) {
        return;
    }

    let (lock, cvar) = shared;
    let (agent_id, number) = {
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let agent_id = guard.agent_id.unwrap_or(0);
        let allocated = guard.spool.allocate_chunk();
        drop(guard);
        let Ok(number) = allocated else {
            return;
        };
        (agent_id, number)
    };

    let now_ns = unix_now_ns();
    let raw = build_chunk_data(profile, agent_id, number, record, host_name, tags, now_ns);
    let clean = sanitize_chunk(raw, record.time_unit, sanitizer);

    let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
    if let Err(error) = guard.spool.push(number, &clean, agent_id, now_ns) {
        eprintln!("microscope agent: cannot spool chunk {number}: {error}");
        return;
    }
    guard.spool_seq = guard.spool_seq.saturating_add(1);
    drop(guard);
    cvar.notify_all();
}

fn sleep_interruptible(stop: &AtomicBool, total: Duration) {
    let start = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) && start.elapsed() < total {
        let remain = total.saturating_sub(start.elapsed());
        thread::sleep(remain.min(Duration::from_millis(50)));
    }
}

fn recorder_loop(
    stop: &AtomicBool,
    shared: &(Mutex<SharedState>, Condvar),
    record: &RecordConfig,
    host_name: &str,
    tags: &BTreeMap<String, String>,
    sanitizer: &Sanitizer,
) {
    let stack_copy = record.preset != Preset::Production;
    let shared_bpf = match SharedBpfEngine::load(stack_copy) {
        Ok(engine) => Some(engine),
        Err(error) => {
            eprintln!(
                "microscope agent: syscall BPF unavailable ({error}); recording samples only"
            );
            None
        }
    };

    let options = build_record_options(record, shared_bpf.as_ref());
    let trigger = AtomicBool::new(false);

    while !stop.load(Ordering::Relaxed) {
        let Some(pid) = find_target_pid(&record.process) else {
            sleep_interruptible(stop, Duration::from_millis(200));
            continue;
        };

        let mut publish = |_profile: Profile| {};
        let mut on_segment = |segment: Profile| {
            enqueue_profile(segment, shared, record, host_name, tags, sanitizer);
        };
        let mut watch = Watch {
            every: Duration::MAX,
            publish: &mut publish,
            stop,
            save: Some(Save {
                trigger: &trigger,
                to: &mut on_segment,
            }),
        };

        match record_attached(pid, &options, &mut watch) {
            Ok(attached) => {
                enqueue_profile(attached.profile, shared, record, host_name, tags, sanitizer);
            }
            Err(error) => {
                if !stop.load(Ordering::Relaxed) {
                    eprintln!(
                        "microscope agent: recording {} (pid {pid}) failed: {error}",
                        record.process
                    );
                }
            }
        }

        if !stop.load(Ordering::Relaxed) {
            sleep_interruptible(stop, Duration::from_millis(250));
        }
    }
}

fn jitter() -> f64 {
    let mut bytes = [0; 4];
    let _ = getrandom::fill(&mut bytes);
    f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX)
}

#[expect(
    clippy::too_many_lines,
    reason = "drives the poster state machine against the spool and ingest endpoint"
)]
fn poster_loop(
    stop: &AtomicBool,
    shared: &(Mutex<SharedState>, Condvar),
    client: &Client,
    post_interval: Duration,
    sanitizer: &Sanitizer,
) {
    let started = std::time::Instant::now();
    let now = || Instant(started.elapsed());
    let mut machine = PosterMachine::new(now(), post_interval);
    let (lock, cvar) = shared;
    let mut last_seq = 0_u64;

    while !stop.load(Ordering::Relaxed) {
        let (has_chunks, token, agent_id, seq) = {
            let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            (
                !guard.spool.is_empty(),
                guard.token.clone(),
                guard.agent_id,
                guard.spool_seq,
            )
        };
        if seq != last_seq {
            let was_empty = last_seq == 0;
            last_seq = seq;
            machine.chunk_queued(now(), was_empty);
        }
        if token.is_some() && machine.needs_token() {
            machine.token_refreshed(now());
        }

        match machine.next(now(), has_chunks, token.is_some()) {
            PosterAction::NeedToken => {
                let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                guard.need_token = true;
                let _ = cvar.wait_timeout(guard, Duration::from_millis(200));
            }
            PosterAction::Wait { until } => {
                let wait = now().until(until).min(Duration::from_millis(250));
                let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = cvar.wait_timeout(guard, wait);
            }
            PosterAction::Post => {
                let Some(token) = token else {
                    continue;
                };
                let agent_id = agent_id.unwrap_or(0);
                let batch = {
                    let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    guard
                        .spool
                        .peek_batch(MAX_POST_BYTES, agent_id, unix_now_ns(), sanitizer)
                };
                let batch = match batch {
                    Ok(Some(batch)) => batch,
                    Ok(None) => continue,
                    Err(error) => {
                        eprintln!("microscope agent: cannot read spool batch: {error}");
                        machine.on(
                            PosterOutcome::Retry { retry_after: None },
                            now(),
                            has_chunks,
                            jitter(),
                        );
                        continue;
                    }
                };

                let reply = client.ingest(&token, &batch.post);
                let outcome = match reply {
                    IngestReply::Accepted { last_chunk } => {
                        let _ = lock
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .spool
                            .ack(last_chunk);
                        PosterOutcome::Accepted { last_chunk }
                    }
                    IngestReply::Overlap { last_chunk } => {
                        let _ = lock
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .spool
                            .ack(last_chunk);
                        PosterOutcome::Overlap { last_chunk }
                    }
                    IngestReply::Unauthorized => {
                        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        guard.token = None;
                        guard.need_token = true;
                        PosterOutcome::Unauthorized
                    }
                    IngestReply::Refused {
                        status,
                        retry_after,
                        message,
                    } => {
                        eprintln!("microscope agent: ingest refused ({status}): {message}");
                        PosterOutcome::Retry { retry_after }
                    }
                    IngestReply::Unreachable(error) => {
                        eprintln!("microscope agent: ingest failed: {error}");
                        PosterOutcome::Retry { retry_after: None }
                    }
                };

                let still_has = {
                    let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    !guard.spool.is_empty()
                };
                machine.on(outcome, now(), still_has, jitter());
            }
        }
    }
}

#[must_use]
pub fn host_name_from(root: &Path) -> String {
    fs::read_to_string(root.join("etc/hostname"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use microscope_data::object::{ObjectReader, TimeUnit, decode_tables};

    use super::*;
    use crate::agent::scratch::Scratch;

    #[test]
    fn planted_secrets_in_real_recording_never_leave_the_agent() {
        let arg_secret = "supersecret_cli_pw_998877";
        let gh_secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        let bearer_secret = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";

        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            "true",
            "sh",
            &format!("--password={arg_secret}"),
            "--token",
            gh_secret,
            &format!("Authorization: Bearer {bearer_secret}"),
        ]);

        let mut options = RecordOptions::preset(Preset::Production);
        options.syscalls.level = SyscallLevel::Off;
        let recorded = crate::profiler::record(cmd, &options).expect("record /bin/sh");

        let record_cfg = RecordConfig {
            process: "sh".into(),
            service: "planted-svc".into(),
            project: "planted-proj".into(),
            preset: Preset::Production,
            time_unit: TimeUnit::Us,
            chunk_ms: 5_000,
            post_ms: 10_000,
            spool_mib: 64,
        };
        let tags = BTreeMap::from([(
            "custom_tag".to_owned(),
            format!("Authorization: Bearer {bearer_secret}"),
        )]);

        let raw = build_chunk_data(
            recorded.profile,
            9,
            1,
            &record_cfg,
            "test-host",
            &tags,
            1_700_000_000_000_000_000,
        );
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"planted-key"));
        let sanitized = sanitize_chunk(raw, TimeUnit::Us, &policy);

        let scratch = Scratch::new();
        let mut spool = Spool::open(&scratch.path().join("spool"), 1024 * 1024).expect("spool");
        spool
            .push(1, &sanitized, 9, 1_700_000_000_000_000_000)
            .expect("push");
        let batch = spool
            .peek_batch(MAX_POST_BYTES, 9, 1_700_000_000_000_000_000, &policy)
            .expect("peek")
            .expect("batch");

        let post_bytes = batch.post.as_bytes();
        for secret in [arg_secret, gh_secret, bearer_secret] {
            assert!(
                !post_bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "secret {secret} leaked in raw USCOPEOB post bytes"
            );
        }

        let reader = ObjectReader::open(post_bytes).expect("open post");
        let decoded =
            decode_tables(&reader.part_sections(0, None).expect("sections")).expect("dec");
        let strings = String::from_utf8_lossy(&decoded.capture.strings);
        let blobs = String::from_utf8_lossy(&decoded.capture.syscall_blobs);
        for secret in [arg_secret, gh_secret, bearer_secret] {
            assert!(!strings.contains(secret), "secret leaked in strings");
            assert!(!blobs.contains(secret), "secret leaked in blobs");
            for dim in &decoded.dimensions {
                assert!(!dim.value.contains(secret), "secret leaked in dimension");
            }
        }
        assert!(
            !decoded.sanitizer_counts.is_empty(),
            "expected sanitizer counts to record scrubbed secrets"
        );
    }
}
