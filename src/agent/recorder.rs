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
    object::{ChunkData, ChunkRow, DimensionRow, TimeUnit, shift_capture_times},
    sanitize::{Builtin, HashKey, Sanitizer, sanitize_chunk},
};

use super::{
    client::{Client, IngestReply},
    config::{Config, RecordConfig},
    machine::Instant,
    poster::{PosterAction, PosterMachine, PosterOutcome},
    spool::{MAX_POST_BYTES, Spool},
    wire::{PolicyDirective, WatchDirective},
};
use crate::profiler::{
    Cuts, Preset, Profile, RecordOptions, Save, SharedBpfEngine, SyscallLevel, Watch,
    record_attached,
};

const AGENT_DEV_MAX_SYSCALL_CAP: u32 = 20_000;

#[derive(Clone, Copy)]
struct PendingFlush {
    id: u64,
    deadline: std::time::Instant,
    target_cut_seq: Option<u64>,
    required_chunk: Option<u64>,
}

struct SharedState {
    server: String,
    token: Option<String>,
    agent_id: Option<u64>,
    need_token: bool,
    spool_seq: u64,
    spool: Spool,
    attached: bool,
    cut_seq: u64,
    session_stop: Option<Arc<AtomicBool>>,
    watch: Option<WatchDirective>,
    watch_seq: u64,
    flush_requested: Option<PendingFlush>,
    flush_completed: u64,
    flush_empty_acked: u64,
    flush_failed_id: u64,
    policy: Option<PolicyDirective>,
    policy_seq: u64,
    applied_policy_version: u64,
    applied_sanitizer_version: u64,
}

#[derive(Clone)]
pub struct PipelineControl {
    flush_trigger: Arc<AtomicBool>,
    shared: Arc<(Mutex<SharedState>, Condvar)>,
}

impl PipelineControl {
    pub fn apply_watch(&self, watch: Option<WatchDirective>) {
        let trigger_cut = watch.is_some();
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.watch = watch;
        guard.watch_seq = guard.watch_seq.saturating_add(1);
        drop(guard);
        if trigger_cut {
            self.flush_trigger.store(true, Ordering::SeqCst);
        }
        cvar.notify_all();
    }

    pub fn request_flush(&self, flush_id: u64, deadline_ms: u64) {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut trigger_cut = false;
        if flush_id > guard.flush_completed
            && flush_id > guard.flush_failed_id
            && guard
                .flush_requested
                .is_none_or(|existing| flush_id > existing.id)
        {
            if !guard.attached && guard.spool.is_empty() {
                guard.flush_completed = guard.flush_completed.max(flush_id);
                guard.flush_empty_acked = guard.flush_empty_acked.max(flush_id);
                guard.flush_requested = None;
            } else {
                let target_cut_seq = guard.attached.then(|| guard.cut_seq.saturating_add(1));
                let deadline = std::time::Instant::now()
                    + Duration::from_millis(deadline_ms.clamp(100, 30_000));
                guard.flush_requested = Some(PendingFlush {
                    id: flush_id,
                    deadline,
                    target_cut_seq,
                    required_chunk: None,
                });
                trigger_cut = guard.attached;
            }
        }
        drop(guard);
        if trigger_cut {
            self.flush_trigger.store(true, Ordering::SeqCst);
        }
        cvar.notify_all();
    }

    #[must_use]
    pub fn completed_flush_id(&self) -> u64 {
        let (lock, _) = &*self.shared;
        let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.flush_completed
    }

    #[must_use]
    pub fn empty_flush_id(&self) -> u64 {
        let (lock, _) = &*self.shared;
        let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.flush_empty_acked
    }

    #[must_use]
    pub fn applied_policy_versions(&self) -> (u64, u64) {
        let (lock, _) = &*self.shared;
        let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        (
            guard.applied_policy_version,
            guard.applied_sanitizer_version,
        )
    }

    pub fn apply_policy(&self, policy: PolicyDirective) {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.policy = Some(policy);
        guard.policy_seq = guard.policy_seq.saturating_add(1);
        if let Some(session_stop) = &guard.session_stop {
            session_stop.store(true, Ordering::SeqCst);
        }
        drop(guard);
        cvar.notify_all();
    }
}

pub struct Pipeline {
    stop: Arc<AtomicBool>,
    flush_trigger: Arc<AtomicBool>,
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
        adopt_legacy: bool,
    ) -> anyhow::Result<Self> {
        let max_bytes = u64::from(record.spool_mib) * 1024 * 1024;
        let server = config.server.origin().to_owned();
        let agent_id = initial_agent_id
            .and_then(|id| u64::try_from(id).ok())
            .filter(|&id| id > 0);
        let mut spool = Spool::open(&config.state_dir.join("spool"), max_bytes)?;
        spool.bind_owner(&server, agent_id, adopt_legacy)?;
        let client = Client::new(config.server.clone(), config.ca_file.as_deref())?;
        let sanitizer = Arc::new(Sanitizer::new(
            &Builtin::DEFAULT,
            Vec::new(),
            HashKey::new(b"microscope-agent-local"),
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let flush_trigger = Arc::new(AtomicBool::new(false));
        let shared = Arc::new((
            Mutex::new(SharedState {
                server,
                token: None,
                agent_id,
                need_token: false,
                spool_seq: 0,
                spool,
                attached: false,
                cut_seq: 0,
                session_stop: None,
                watch: None,
                watch_seq: 0,
                flush_requested: None,
                flush_completed: 0,
                flush_empty_acked: 0,
                flush_failed_id: 0,
                policy: None,
                policy_seq: 0,
                applied_policy_version: 1,
                applied_sanitizer_version: 1,
            }),
            Condvar::new(),
        ));

        let rec_stop = Arc::clone(&stop);
        let rec_trigger = Arc::clone(&flush_trigger);
        let rec_shared = Arc::clone(&shared);
        let rec_sanitizer = Arc::clone(&sanitizer);
        let rec_config = record.clone();
        let tags = config.tags.clone();
        let recorder = thread::Builder::new()
            .name("microscope-agent-recorder".into())
            .spawn(move || {
                recorder_loop(
                    &rec_stop,
                    &rec_trigger,
                    &rec_shared,
                    rec_config,
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
            flush_trigger,
            shared,
            recorder: Some(recorder),
            poster: Some(poster),
        })
    }

    #[must_use]
    pub fn control(&self) -> PipelineControl {
        PipelineControl {
            flush_trigger: Arc::clone(&self.flush_trigger),
            shared: Arc::clone(&self.shared),
        }
    }

    pub fn set_auth(&self, agent_id: Option<i64>, token: Option<String>) {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let parsed = agent_id
            .and_then(|id| u64::try_from(id).ok())
            .filter(|&id| id > 0);
        let server = guard.server.clone();
        if let Err(error) = guard.spool.bind_owner(&server, parsed, false) {
            eprintln!("microscope agent: cannot update spool owner: {error}");
            guard.agent_id = None;
            guard.token = None;
            drop(guard);
            cvar.notify_all();
            return;
        }
        guard.agent_id = parsed;
        if parsed.is_some()
            && let Some(tok) = token
        {
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
        let (lock, cvar) = &*self.shared;
        {
            let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(session_stop) = &guard.session_stop {
                session_stop.store(true, Ordering::SeqCst);
            }
        }
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

fn monotonic_ns() -> i64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |ts| {
        ts.tv_sec()
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec())
    })
}

fn wall_clock_reference() -> (i64, i64) {
    let mono_before = monotonic_ns();
    let wall = unix_now_ns();
    let mono_after = monotonic_ns();
    let mono_mid = mono_before.saturating_add(mono_after.saturating_sub(mono_before) / 2);
    (wall, wall.saturating_sub(mono_mid))
}

const fn preset_code(preset: Preset) -> u8 {
    match preset {
        Preset::Production => 0,
        Preset::Development => 1,
        Preset::Maximum => 2,
    }
}

fn parse_policy_preset(name: &str) -> Option<Preset> {
    match name {
        "prod" | "production" => Some(Preset::Production),
        "dev" | "development" => Some(Preset::Development),
        "max" | "maximum" => Some(Preset::Maximum),
        _ => None,
    }
}

fn parse_policy_time_unit(name: &str) -> Option<TimeUnit> {
    match name {
        "1ns" | "ns" => Some(TimeUnit::Ns),
        "1us" | "us" => Some(TimeUnit::Us),
        "1ms" | "ms" => Some(TimeUnit::Ms),
        _ => None,
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
    build_chunk_data_with_offset(
        profile,
        agent_id,
        chunk_number,
        record,
        host_name,
        tags,
        now_ns,
        None,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "packs a recorded profile with chunk metadata and clock alignment"
)]
fn build_chunk_data_with_offset(
    profile: Profile,
    agent_id: u64,
    chunk_number: u64,
    record: &RecordConfig,
    host_name: &str,
    tags: &BTreeMap<String, String>,
    now_ns: i64,
    wall_offset_ns: Option<i64>,
) -> ChunkData {
    let mut capture = profile.into_capture();
    let (mut mono_start, mut mono_end) = capture.span();
    if mono_end == 0 && !capture.samples.is_empty() {
        mono_start = capture
            .samples
            .iter()
            .map(|s| s.time.get())
            .min()
            .unwrap_or(0);
        mono_end = capture
            .samples
            .iter()
            .map(|s| s.time.get())
            .max()
            .unwrap_or(0);
    }
    let (start, end, sampled_ns) = if mono_end > 0 {
        let delta = wall_offset_ns.unwrap_or_else(|| now_ns.saturating_sub(mono_end));
        shift_capture_times(&mut capture, delta);
        let shifted_start = mono_start.saturating_add(delta);
        let shifted_end = mono_end.saturating_add(delta).max(shifted_start);
        let span_ns = shifted_end.saturating_sub(shifted_start).max(0);
        (
            shifted_start,
            shifted_end,
            u64::try_from(span_ns).unwrap_or(0),
        )
    } else {
        (now_ns, now_ns, 0)
    };

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
        if key == "microscope.project" {
            continue;
        }
        dimensions.push(DimensionRow {
            process: NONE,
            name: key.clone(),
            value: value.clone(),
        });
    }

    ChunkData {
        capture,
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

#[derive(Clone, Copy)]
enum CutOutcome {
    Pushed(u64),
    Empty,
    Failed,
}

fn finish_cut_locked(guard: &mut SharedState, outcome: CutOutcome, mark_detached: bool) {
    if mark_detached {
        guard.attached = false;
        guard.session_stop = None;
    }
    guard.cut_seq = guard.cut_seq.saturating_add(1);
    let current_cut = guard.cut_seq;
    if let Some(mut pf) = guard.flush_requested
        && pf
            .target_cut_seq
            .is_some_and(|target| current_cut >= target)
        && pf.required_chunk.is_none()
    {
        match outcome {
            CutOutcome::Pushed(chunk_no) => {
                pf.required_chunk = Some(chunk_no);
                guard.flush_requested = Some(pf);
            }
            CutOutcome::Empty => {
                if guard.spool.is_empty() {
                    guard.flush_completed = guard.flush_completed.max(pf.id);
                    guard.flush_empty_acked = guard.flush_empty_acked.max(pf.id);
                    guard.flush_requested = None;
                }
            }
            CutOutcome::Failed => {
                guard.flush_failed_id = guard.flush_failed_id.max(pf.id);
                guard.flush_requested = None;
            }
        }
    }
}

fn enqueue_and_complete_cut(
    profile: Profile,
    shared: &(Mutex<SharedState>, Condvar),
    record: &RecordConfig,
    host_name: &str,
    tags: &BTreeMap<String, String>,
    sanitizer: &Sanitizer,
    mark_detached: bool,
) {
    let (lock, cvar) = shared;
    if !profile_has_data(&profile) {
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        finish_cut_locked(&mut guard, CutOutcome::Empty, mark_detached);
        drop(guard);
        cvar.notify_all();
        return;
    }

    let (now_ns, wall_offset_ns) = wall_clock_reference();

    let alloc = {
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard
            .agent_id
            .and_then(|agent_id| guard.spool.allocate_chunk().ok().map(|n| (agent_id, n)))
    };
    let Some((agent_id, number)) = alloc else {
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        finish_cut_locked(&mut guard, CutOutcome::Failed, mark_detached);
        drop(guard);
        cvar.notify_all();
        return;
    };

    let raw = build_chunk_data_with_offset(
        profile,
        agent_id,
        number,
        record,
        host_name,
        tags,
        now_ns,
        Some(wall_offset_ns),
    );
    let clean = sanitize_chunk(raw, record.time_unit, sanitizer);

    let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
    let outcome = if guard.agent_id == Some(agent_id) {
        match guard.spool.push(number, &clean, agent_id, now_ns) {
            Ok(0) => {
                eprintln!(
                    "microscope agent: dropped oversized chunk {number} exceeding post limit"
                );
                CutOutcome::Failed
            }
            Ok(_) => {
                guard.spool_seq = guard.spool_seq.saturating_add(1);
                CutOutcome::Pushed(number)
            }
            Err(error) => {
                eprintln!("microscope agent: cannot spool chunk {number}: {error}");
                CutOutcome::Failed
            }
        }
    } else {
        CutOutcome::Failed
    };
    finish_cut_locked(&mut guard, outcome, mark_detached);
    drop(guard);
    cvar.notify_all();
}

fn complete_failed_cut(shared: &(Mutex<SharedState>, Condvar), mark_detached: bool) {
    let (lock, cvar) = shared;
    let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
    finish_cut_locked(&mut guard, CutOutcome::Failed, mark_detached);
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

#[expect(
    clippy::too_many_lines,
    reason = "runs the live recording attach loop with hot policy reload and cut correlation"
)]
fn recorder_loop(
    stop: &AtomicBool,
    flush_trigger: &AtomicBool,
    shared: &(Mutex<SharedState>, Condvar),
    mut record: RecordConfig,
    host_name: &str,
    tags: &BTreeMap<String, String>,
    sanitizer: &Sanitizer,
) {
    let mut shared_bpf = match SharedBpfEngine::available() {
        Ok(engine) => Some(engine),
        Err(error) => {
            eprintln!(
                "microscope agent: syscall BPF unavailable ({error}); recording samples only"
            );
            None
        }
    };

    let mut options = build_record_options(&record, shared_bpf.as_ref());
    let base_preset = record.preset;
    let base_time_unit = record.time_unit;
    let mut last_policy_seq = 0_u64;

    while !stop.load(Ordering::Relaxed) {
        let (lock, _) = shared;
        let policy_update = {
            let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if guard.policy_seq == last_policy_seq {
                None
            } else {
                last_policy_seq = guard.policy_seq;
                guard.policy.clone()
            }
        };
        if let Some(policy) = policy_update {
            let next_preset = policy
                .preset
                .as_deref()
                .and_then(parse_policy_preset)
                .unwrap_or(base_preset);
            let next_unit = policy
                .time_unit
                .as_deref()
                .and_then(parse_policy_time_unit)
                .unwrap_or(base_time_unit);
            if next_preset != record.preset || next_unit != record.time_unit {
                record.preset = next_preset;
                record.time_unit = next_unit;
                options = build_record_options(&record, shared_bpf.as_ref());
            }
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            guard.applied_policy_version = guard.applied_policy_version.max(policy.version);
            guard.applied_sanitizer_version = guard
                .applied_sanitizer_version
                .max(policy.sanitizer_version);
            drop(guard);
        }

        let Some(pid) = find_target_pid(&record.process) else {
            sleep_interruptible(stop, Duration::from_millis(200));
            continue;
        };

        let session_stop = Arc::new(AtomicBool::new(false));
        {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if stop.load(Ordering::Relaxed) || guard.policy_seq != last_policy_seq {
                continue;
            }
            guard.attached = true;
            guard.session_stop = Some(Arc::clone(&session_stop));
        }

        let mut publish = |_profile: Profile| {};
        let mut on_segment = |segment: Profile| {
            enqueue_and_complete_cut(segment, shared, &record, host_name, tags, sanitizer, false);
        };
        let mut watch = Watch {
            every: Duration::MAX,
            publish: &mut publish,
            stop: &session_stop,
            save: Some(Save {
                trigger: flush_trigger,
                to: &mut on_segment,
            }),
        };

        match record_attached(pid, &options, &mut watch) {
            Ok(attached) => {
                enqueue_and_complete_cut(
                    attached.profile,
                    shared,
                    &record,
                    host_name,
                    tags,
                    sanitizer,
                    true,
                );
            }
            Err(error) => {
                complete_failed_cut(shared, true);
                if shared_bpf
                    .as_ref()
                    .is_some_and(SharedBpfEngine::load_failed)
                {
                    eprintln!(
                        "microscope agent: syscall BPF failed to load ({error}); falling back to samples only"
                    );
                    shared_bpf = None;
                    options = build_record_options(&record, None);
                    continue;
                }
                if !stop.load(Ordering::Relaxed) {
                    eprintln!(
                        "microscope agent: recording {} (pid {pid}) failed: {error}",
                        record.process
                    );
                }
            }
        }

        let policy_pending = {
            let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            guard.policy_seq != last_policy_seq
        };
        if !stop.load(Ordering::Relaxed) && !policy_pending {
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
    reason = "drives the poster state machine with dynamic watch intervals and flush coordination"
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
    let mut last_watch_seq = 0_u64;
    let mut last_flush_seen = 0_u64;
    let mut max_batch_chunks = usize::MAX;

    while !stop.load(Ordering::Relaxed) {
        machine.expire_flush(now());
        let (has_chunks, has_token, seq, watch_update, ready_flush, waiting_for_cut) = {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            let watch_update = if guard.watch_seq == last_watch_seq {
                None
            } else {
                last_watch_seq = guard.watch_seq;
                Some(guard.watch.clone())
            };
            let mut ready_flush: Option<(u64, u64)> = None;
            let mut waiting_for_cut = false;
            if let Some(pf) = guard.flush_requested {
                let wall_now = std::time::Instant::now();
                if wall_now >= pf.deadline {
                    guard.flush_requested = None;
                    machine.clear_flush(pf.id);
                } else {
                    let cut_done = pf.target_cut_seq.is_none_or(|t| guard.cut_seq >= t);
                    if cut_done {
                        if guard.spool.is_empty() && pf.required_chunk.is_none() {
                            guard.flush_completed = guard.flush_completed.max(pf.id);
                            guard.flush_empty_acked = guard.flush_empty_acked.max(pf.id);
                            guard.flush_requested = None;
                            machine.clear_flush(pf.id);
                        } else {
                            let rem_ms = u64::try_from(
                                pf.deadline.saturating_duration_since(wall_now).as_millis(),
                            )
                            .unwrap_or(5_000)
                            .max(100);
                            ready_flush = Some((pf.id, rem_ms));
                        }
                    } else {
                        waiting_for_cut = true;
                    }
                }
            }
            (
                !guard.spool.is_empty(),
                guard.token.is_some() && guard.agent_id.is_some(),
                guard.spool_seq,
                watch_update,
                ready_flush,
                waiting_for_cut,
            )
        };
        if let Some(watch_opt) = watch_update {
            machine.apply_watch(watch_opt.as_ref(), now());
        }
        if let Some((fid, rem_ms)) = ready_flush
            && fid > last_flush_seen
        {
            last_flush_seen = fid;
            machine.request_flush_with_deadline(fid, rem_ms, now());
        }
        if seq != last_seq {
            let was_empty = last_seq == 0;
            last_seq = seq;
            machine.chunk_queued(now(), was_empty);
        }
        if has_token && machine.needs_token() {
            machine.token_refreshed(now());
        }

        match machine.next(now(), has_chunks, has_token) {
            PosterAction::NeedToken => {
                let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                guard.need_token = true;
                let _ = cvar.wait_timeout(guard, Duration::from_millis(100));
            }
            PosterAction::Wait { until } => {
                let cap = if waiting_for_cut || machine.active_flush(now()).is_some() {
                    Duration::from_millis(50)
                } else {
                    Duration::from_millis(200)
                };
                let wait = now().until(until).min(cap);
                let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = cvar.wait_timeout(guard, wait);
            }
            PosterAction::Post => {
                let (token, batch, post_flush_id) = {
                    let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    let (Some(token), Some(agent_id)) = (guard.token.clone(), guard.agent_id)
                    else {
                        continue;
                    };
                    let queued_total = guard.spool.stats().queued_chunks;
                    let batch = guard.spool.peek_batch_capped(
                        MAX_POST_BYTES,
                        max_batch_chunks,
                        agent_id,
                        unix_now_ns(),
                        sanitizer,
                    );
                    let wall_now = std::time::Instant::now();
                    let post_flush_id = match (&batch, guard.flush_requested) {
                        (Ok(Some(b)), Some(pf))
                            if wall_now < pf.deadline
                                && pf.target_cut_seq.is_none_or(|t| guard.cut_seq >= t) =>
                        {
                            let satisfies_cut = pf
                                .required_chunk
                                .map_or(b.chunk_count >= queued_total, |req| {
                                    b.first_chunk <= req && req <= b.last_chunk
                                });
                            satisfies_cut.then_some(pf.id)
                        }
                        _ => None,
                    };
                    drop(guard);
                    (token, batch, post_flush_id)
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

                let reply = client.ingest_with_flush(&token, &batch.post, post_flush_id);
                let outcome = match reply {
                    IngestReply::Accepted { last_chunk } => {
                        max_batch_chunks = usize::MAX;
                        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        let _ = guard.spool.ack(batch.owner_gen, last_chunk);
                        if let Some(fid) = post_flush_id
                            && guard.flush_requested.is_some_and(|pf| {
                                pf.id == fid
                                    && pf.required_chunk.is_none_or(|req| last_chunk >= req)
                            })
                        {
                            guard.flush_completed = guard.flush_completed.max(fid);
                            guard.flush_requested = None;
                            machine.clear_flush(fid);
                        }
                        drop(guard);
                        PosterOutcome::Accepted { last_chunk }
                    }
                    IngestReply::Overlap { last_chunk } => {
                        max_batch_chunks = usize::MAX;
                        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        let _ = guard.spool.ack(batch.owner_gen, last_chunk);
                        if let Some(fid) = post_flush_id
                            && guard.flush_requested.is_some_and(|pf| {
                                pf.id == fid
                                    && pf
                                        .required_chunk
                                        .map_or(guard.spool.is_empty(), |req| last_chunk >= req)
                            })
                        {
                            guard.flush_completed = guard.flush_completed.max(fid);
                            guard.flush_requested = None;
                            machine.clear_flush(fid);
                        }
                        drop(guard);
                        PosterOutcome::Overlap { last_chunk }
                    }
                    IngestReply::Unauthorized => {
                        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        if guard.token.as_deref() == Some(token.as_str()) {
                            guard.token = None;
                            guard.need_token = true;
                        }
                        drop(guard);
                        PosterOutcome::Unauthorized
                    }
                    IngestReply::Refused {
                        status,
                        error,
                        retry_after,
                        message,
                    } => {
                        eprintln!("microscope agent: ingest refused ({status}): {message}");
                        if status == 413
                            || (status == 400 && error.as_deref() == Some("invalid_payload"))
                        {
                            if batch.chunk_count > 1 {
                                max_batch_chunks = (batch.chunk_count / 2).max(1);
                                continue;
                            }
                            max_batch_chunks = usize::MAX;
                            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                            let _ = guard.spool.drop_chunk(batch.owner_gen, batch.first_chunk);
                            if let Some(pf) = guard.flush_requested
                                && pf.required_chunk == Some(batch.first_chunk)
                            {
                                guard.flush_failed_id = guard.flush_failed_id.max(pf.id);
                                guard.flush_requested = None;
                                machine.clear_flush(pf.id);
                            }
                            drop(guard);
                            continue;
                        }
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
        let phrase_secret = "correct horse battery staple";
        let gh_secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        let bearer_secret = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";

        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            "true",
            "sh",
            &format!("--password={arg_secret}"),
            "--secret",
            phrase_secret,
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
        let forbidden = [
            arg_secret,
            phrase_secret,
            "horse",
            "battery",
            "staple",
            gh_secret,
            bearer_secret,
        ];
        for secret in forbidden {
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
        for secret in forbidden {
            assert!(
                !strings.contains(secret),
                "secret {secret} leaked in strings"
            );
            assert!(!blobs.contains(secret), "secret {secret} leaked in blobs");
            for dim in &decoded.dimensions {
                assert!(
                    !dim.value.contains(secret),
                    "secret {secret} leaked in dimension"
                );
            }
        }
        assert!(
            !decoded.sanitizer_counts.is_empty(),
            "expected sanitizer counts to record scrubbed secrets"
        );
    }
}
