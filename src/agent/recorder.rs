use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use microscope_data::{
    capture::{Capture, records::NONE},
    object::{ChunkData, ChunkRow, DimensionRow, TimeUnit, shift_capture_times},
    sanitize::{Action, Builtin, HashKey, Rule, RuleSpec, Sanitizer, sanitize_chunk},
};

use super::{
    client::{Client, IngestReply},
    config::{Config, Limits, RecordConfig, SanitizeConfig},
    host::Root,
    machine::Instant,
    poster::{PosterAction, PosterMachine, PosterOutcome},
    select::{
        BuildIdCache, RuleAction, RuleKind, ScanOptions, SelectedProcess, SelectionRule,
        scan_selected_processes,
    },
    split::filter_capture_by_pids,
    spool::{MAX_POST_BYTES, Spool},
    wire::{BurstDirective, PolicyDirective, SanitizerPolicyDirective, WatchDirective},
};
use crate::{
    backend::perf,
    profiler::{
        Cuts, Preset, Profile, RecordOptions, Save, SharedBpfEngine, SyscallLevel, Watch,
        budget::Budget, record_attached_many,
    },
};

const AGENT_DEV_MAX_SYSCALL_CAP: u32 = 20_000;
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);

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
    burst: Option<BurstDirective>,
    burst_seq: u64,
    applied_policy_version: u64,
    applied_sanitizer_version: u64,
    active_projects: Vec<String>,
    active_services: Vec<String>,
    sanitizer: Arc<Sanitizer>,
    project_sanitizers: BTreeMap<String, Arc<Sanitizer>>,
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

    #[must_use]
    pub fn active_targets(&self) -> (Vec<String>, Vec<String>) {
        let (lock, _) = &*self.shared;
        let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        (guard.active_projects.clone(), guard.active_services.clone())
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

    pub fn apply_burst(&self, burst: Option<BurstDirective>) {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.burst = burst;
        guard.burst_seq = guard.burst_seq.saturating_add(1);
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
        let (initial_sanitizer, initial_proj_sanitizers) = build_sanitizers(&config.sanitize, None);

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
                burst: None,
                burst_seq: 0,
                applied_policy_version: 0,
                applied_sanitizer_version: 0,
                active_projects: Vec::new(),
                active_services: Vec::new(),
                sanitizer: initial_sanitizer,
                project_sanitizers: initial_proj_sanitizers,
            }),
            Condvar::new(),
        ));

        let rec_stop = Arc::clone(&stop);
        let rec_trigger = Arc::clone(&flush_trigger);
        let rec_shared = Arc::clone(&shared);
        let rec_config = record.clone();
        let limits = config.limits.clone();
        let local_sanitize = config.sanitize.clone();
        let tags = config.tags.clone();
        let recorder = thread::Builder::new()
            .name("microscope-agent-recorder".into())
            .spawn(move || {
                recorder_loop(
                    &rec_stop,
                    &rec_trigger,
                    &rec_shared,
                    &rec_config,
                    &limits,
                    &local_sanitize,
                    &host_name,
                    &tags,
                );
            })?;

        let post_stop = Arc::clone(&stop);
        let post_shared = Arc::clone(&shared);
        let post_interval = Duration::from_millis(record.post_ms);
        let poster = thread::Builder::new()
            .name("microscope-agent-poster".into())
            .spawn(move || {
                poster_loop(&post_stop, &post_shared, &client, post_interval);
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

const fn preset_rank(preset: Preset) -> u8 {
    match preset {
        Preset::Production => 0,
        Preset::Development => 1,
        Preset::Maximum => 2,
    }
}

const fn time_unit_rank(unit: TimeUnit) -> u8 {
    match unit {
        TimeUnit::Ms => 0,
        TimeUnit::Us => 1,
        TimeUnit::Ns => 2,
    }
}

const fn clamp_preset(preset: Preset, max_preset: Option<Preset>) -> Preset {
    match max_preset {
        Some(cap) if preset_rank(preset) > preset_rank(cap) => cap,
        _ => preset,
    }
}

const fn clamp_time_unit(unit: TimeUnit, min_unit: Option<TimeUnit>) -> TimeUnit {
    match min_unit {
        Some(cap) if time_unit_rank(unit) > time_unit_rank(cap) => cap,
        _ => unit,
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
        "1us" | "1µs" | "us" | "µs" => Some(TimeUnit::Us),
        "1ms" | "ms" => Some(TimeUnit::Ms),
        _ => None,
    }
}

fn parse_action(raw: &str) -> Option<Action> {
    match raw {
        "redact" => Some(Action::Redact),
        "hash" => Some(Action::Hash),
        "mask" => Some(Action::Mask),
        "drop-field" => Some(Action::DropField),
        "drop-record" => Some(Action::DropRecord),
        _ => None,
    }
}

fn build_sanitizers(
    local: &SanitizeConfig,
    remote: Option<&SanitizerPolicyDirective>,
) -> (Arc<Sanitizer>, BTreeMap<String, Arc<Sanitizer>>) {
    let mut builtins: Vec<Builtin> = Builtin::DEFAULT
        .iter()
        .copied()
        .filter(|b| !local.disable_builtins.contains(b))
        .collect();
    if let Some(rem) = remote {
        for name in &rem.enabled_builtins {
            if let Ok(b) = name.parse::<Builtin>()
                && !local.disable_builtins.contains(&b)
                && !builtins.contains(&b)
            {
                builtins.push(b);
            }
        }
    }

    let key_bytes: Vec<u8> = remote
        .and_then(|r| r.hash_key.as_deref())
        .and_then(|b64| URL_SAFE_NO_PAD.decode(b64).ok())
        .filter(|bytes| !bytes.is_empty())
        .unwrap_or_else(|| b"microscope-agent-local".to_vec());

    let mut org_specs: Vec<RuleSpec> = local.rules.clone();
    let mut per_project_specs: BTreeMap<String, Vec<RuleSpec>> = BTreeMap::new();
    if let Some(rem) = remote {
        for r in &rem.rules {
            let Some(action) = parse_action(&r.action) else {
                continue;
            };
            let spec = RuleSpec {
                name: r.name.clone(),
                fields: r.fields.clone(),
                pattern: r.pattern.clone(),
                action,
            };
            if Rule::new(&spec).is_err() {
                continue;
            }
            if let Some(proj) = &r.project {
                per_project_specs
                    .entry(proj.to_ascii_lowercase())
                    .or_default()
                    .push(spec);
            } else {
                org_specs.push(spec);
            }
        }
    }

    let compile = |specs: &[RuleSpec]| -> Vec<Rule> {
        specs
            .iter()
            .filter_map(|spec| Rule::new(spec).ok())
            .collect()
    };

    let org_sanitizer = Arc::new(Sanitizer::new(
        &builtins,
        compile(&org_specs),
        HashKey::new(&key_bytes),
    ));

    let mut project_sanitizers = BTreeMap::new();
    for (proj_key, extra_specs) in per_project_specs {
        let mut combined = org_specs.clone();
        combined.extend(extra_specs);
        project_sanitizers.insert(
            proj_key,
            Arc::new(Sanitizer::new(
                &builtins,
                compile(&combined),
                HashKey::new(&key_bytes),
            )),
        );
    }

    (org_sanitizer, project_sanitizers)
}

fn remote_selection_rules(policy: Option<&PolicyDirective>) -> Vec<SelectionRule> {
    let Some(pol) = policy else {
        return Vec::new();
    };
    pol.rules
        .iter()
        .filter_map(|r| {
            Some(SelectionRule {
                kind: RuleKind::parse(&r.kind)?,
                pattern: r.pattern.clone(),
                action: RuleAction::parse(&r.action)?,
                project: r.project.clone(),
                service: r.service.clone(),
            })
        })
        .collect()
}

fn effective_policy_for_process(
    proc_sel: &SelectedProcess,
    base_preset: Preset,
    base_unit: TimeUnit,
    policy: Option<&PolicyDirective>,
    burst: Option<&BurstDirective>,
    limits: &Limits,
    now_ns: i64,
) -> (Preset, TimeUnit) {
    let mut preset = base_preset;
    let mut unit = base_unit;

    if let Some(pol) = policy {
        if let Some(p) = pol.preset.as_deref().and_then(parse_policy_preset) {
            preset = p;
        }
        if let Some(u) = pol.time_unit.as_deref().and_then(parse_policy_time_unit) {
            unit = u;
        }
        if let Some(proj) = &proc_sel.project
            && let Some(pp) = pol
                .project_policies
                .iter()
                .find(|p| p.project.eq_ignore_ascii_case(proj))
        {
            if let Some(p) = parse_policy_preset(&pp.preset) {
                preset = p;
            }
            if let Some(u) = parse_policy_time_unit(&pp.time_unit) {
                unit = u;
            }
        }
    }

    if let Some(b) = burst
        && b.ends_at_ns.is_none_or(|ends| ends > now_ns)
    {
        let project_matches = match (&b.project, &proc_sel.project) {
            (None, _) => true,
            (Some(bp), Some(pp)) => bp.eq_ignore_ascii_case(pp),
            (Some(_), None) => false,
        };
        if project_matches {
            if let Some(p) = parse_policy_preset(&b.preset)
                && preset_rank(p) >= preset_rank(preset)
            {
                preset = p;
            }
            if let Some(u) = b.time_unit.as_deref().and_then(parse_policy_time_unit)
                && time_unit_rank(u) >= time_unit_rank(unit)
            {
                unit = u;
            }
        }
    }

    (
        clamp_preset(preset, limits.max_preset),
        clamp_time_unit(unit, limits.min_time_unit),
    )
}

fn build_record_options(
    preset: Preset,
    chunk_ms: u64,
    limits: &Limits,
    shared_bpf: Option<&SharedBpfEngine>,
    allow_syscalls: bool,
) -> RecordOptions {
    let mut options = RecordOptions::preset(preset);
    options.cuts = Cuts::Segments(Duration::from_millis(chunk_ms));
    options.symbols_later = false;
    if let Some(cpu_pct) = limits.cpu_percent {
        let mut budget = options.budget.unwrap_or(Budget::PRODUCTION);
        budget.cpu = (f64::from(cpu_pct) / 100.0).clamp(0.01, 1.0);
        options.budget = Some(budget);
    }
    if allow_syscalls {
        if let Some(bpf) = shared_bpf {
            if preset != Preset::Production {
                options.syscalls.kept_per_second = Some(AGENT_DEV_MAX_SYSCALL_CAP);
            }
            options.syscalls.shared_bpf = Some(bpf.clone());
        } else if preset != Preset::Production {
            options.syscalls.kept_per_second = Some(AGENT_DEV_MAX_SYSCALL_CAP);
        }
    } else {
        options.syscalls.level = SyscallLevel::Off;
        options.syscalls.kept_per_second = None;
    }
    options
}

const fn capture_has_data(c: &Capture) -> bool {
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
    let mut capture = profile.into_capture();
    let (start, end, sampled_ns) = align_capture_times(&mut capture, now_ns, None);
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

fn align_capture_times(
    capture: &mut Capture,
    now_ns: i64,
    wall_offset_ns: Option<i64>,
) -> (i64, i64, u64) {
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
    if mono_end > 0 {
        let delta = wall_offset_ns.unwrap_or_else(|| now_ns.saturating_sub(mono_end));
        shift_capture_times(capture, delta);
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
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "builds one project-scoped ChunkData with per-process metadata dimensions from a filtered capture"
)]
fn build_selected_chunk_data(
    mut capture: Capture,
    agent_id: u64,
    chunk_number: u64,
    project: Option<&str>,
    selected: &[&SelectedProcess],
    preset: Preset,
    time_unit: TimeUnit,
    host_name: &str,
    tags: &BTreeMap<String, String>,
    now_ns: i64,
    wall_offset_ns: i64,
) -> ChunkData {
    let (start, end, sampled_ns) = align_capture_times(&mut capture, now_ns, Some(wall_offset_ns));

    let mut dimensions = vec![DimensionRow {
        process: NONE,
        name: "host.name".into(),
        value: host_name.to_owned(),
    }];
    if let Some(proj) = project
        && !proj.trim().is_empty()
    {
        dimensions.push(DimensionRow {
            process: NONE,
            name: "microscope.project".into(),
            value: proj.to_owned(),
        });
    }
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

    let by_pid: BTreeMap<u32, &SelectedProcess> = selected.iter().map(|s| (s.pid, *s)).collect();
    let mut attached_any = false;
    for (idx, proc_row) in capture.processes.iter().enumerate() {
        let proc_idx = u32::try_from(idx).unwrap_or(NONE);
        let sel = by_pid
            .get(&proc_row.pid.get())
            .copied()
            .or_else(|| selected.first().copied());
        if let Some(s) = sel {
            attached_any = true;
            push_selected_dimensions(&mut dimensions, proc_idx, s);
        }
    }
    if !attached_any && let Some(s) = selected.first() {
        push_selected_dimensions(&mut dimensions, NONE, s);
    }

    ChunkData {
        capture,
        chunks: vec![ChunkRow {
            agent: agent_id,
            number: chunk_number,
            start,
            end,
            sampled_ns,
            preset: preset_code(preset),
            time_unit,
            policy: 0,
            rules: String::new(),
        }],
        dimensions,
        sanitizer_counts: Vec::new(),
    }
}

fn push_selected_dimensions(
    dimensions: &mut Vec<DimensionRow>,
    process: u32,
    sel: &SelectedProcess,
) {
    let mut push = |name: &str, value: &str| {
        if !value.is_empty() {
            dimensions.push(DimensionRow {
                process,
                name: name.to_owned(),
                value: value.to_owned(),
            });
        }
    };
    push("service.name", &sel.service);
    push("process.executable.name", &sel.program);
    push("microscope.selection", &sel.matched_by);
    if let Some(path) = &sel.meta.exe_path {
        push("process.executable.path", path);
    }
    if let Some(bid) = &sel.meta.build_id {
        push("process.executable.build_id", bid);
    }
    if let Some(unit) = &sel.meta.systemd_unit {
        push("systemd.unit", unit);
    }
    if let Some(pod) = &sel.meta.k8s_pod_name {
        push("k8s.pod.name", pod);
    }
    if let Some(ns) = &sel.meta.k8s_namespace {
        push("k8s.namespace.name", ns);
    }
    if let Some(cname) = &sel.meta.container_name {
        push("container.name", cname);
    }
    if let Some(img) = &sel.meta.container_image {
        push("container.image.name", img);
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

#[derive(Clone)]
struct PlannedTarget {
    selected: SelectedProcess,
    preset: Preset,
    time_unit: TimeUnit,
}

type CutBucket<'a> = BTreeMap<(Option<String>, u8), (TimeUnit, Vec<&'a SelectedProcess>)>;

#[expect(
    clippy::too_many_lines,
    reason = "splits a multi-process segment by project, builds metadata, sanitizes, and spools each project cleanly"
)]
fn enqueue_multi_project_cut(
    profile: &Profile,
    shared: &(Mutex<SharedState>, Condvar),
    targets: &[PlannedTarget],
    preset: Preset,
    host_name: &str,
    tags: &BTreeMap<String, String>,
    mark_detached: bool,
) {
    let (lock, cvar) = shared;
    let capture = profile.capture();
    if !capture_has_data(capture) || targets.is_empty() {
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        finish_cut_locked(&mut guard, CutOutcome::Empty, mark_detached);
        drop(guard);
        cvar.notify_all();
        return;
    }

    let mut by_bucket: CutBucket<'_> = BTreeMap::new();
    for t in targets {
        let key = (t.selected.project.clone(), time_unit_rank(t.time_unit));
        by_bucket
            .entry(key)
            .or_insert_with(|| (t.time_unit, Vec::new()))
            .1
            .push(&t.selected);
    }

    let (now_ns, wall_offset_ns) = wall_clock_reference();
    let mut last_pushed: Option<u64> = None;
    let mut any_failed = false;

    for ((proj_opt, _), (time_unit, members)) in by_bucket {
        let mut allowed_pids: BTreeSet<u32> = BTreeSet::new();
        for m in &members {
            allowed_pids.insert(m.pid);
            for child in perf::descendants(m.pid) {
                allowed_pids.insert(child);
            }
        }
        let filtered = filter_capture_by_pids(capture, &allowed_pids);
        if !capture_has_data(&filtered) {
            continue;
        }

        let (alloc, sanitizer) = {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            let san = proj_opt
                .as_deref()
                .and_then(|p| guard.project_sanitizers.get(&p.to_ascii_lowercase()))
                .cloned()
                .unwrap_or_else(|| Arc::clone(&guard.sanitizer));
            let alloc = guard
                .agent_id
                .and_then(|agent_id| guard.spool.allocate_chunk().ok().map(|n| (agent_id, n)));
            drop(guard);
            (alloc, san)
        };
        let Some((agent_id, number)) = alloc else {
            any_failed = true;
            continue;
        };

        let raw = build_selected_chunk_data(
            filtered,
            agent_id,
            number,
            proj_opt.as_deref(),
            &members,
            preset,
            time_unit,
            host_name,
            tags,
            now_ns,
            wall_offset_ns,
        );
        let clean = sanitize_chunk(raw, time_unit, &sanitizer);

        let push_result = {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if guard.agent_id == Some(agent_id) {
                let res = guard.spool.push(number, &clean, agent_id, now_ns);
                if matches!(res, Ok(n) if n > 0) {
                    guard.spool_seq = guard.spool_seq.saturating_add(1);
                }
                drop(guard);
                Some(res)
            } else {
                drop(guard);
                None
            }
        };
        match push_result {
            Some(Ok(0)) => {
                eprintln!(
                    "microscope agent: dropped oversized chunk {number} exceeding post limit"
                );
                any_failed = true;
            }
            Some(Ok(_)) => {
                last_pushed = Some(number);
            }
            Some(Err(error)) => {
                eprintln!("microscope agent: cannot spool chunk {number}: {error}");
                any_failed = true;
            }
            None => {
                any_failed = true;
            }
        }
    }

    let outcome = match (last_pushed, any_failed) {
        (Some(n), _) => CutOutcome::Pushed(n),
        (None, true) => CutOutcome::Failed,
        (None, false) => CutOutcome::Empty,
    };
    let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
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

fn plan_signature(targets: &[PlannedTarget]) -> Vec<(u32, Option<String>, u8, u8)> {
    targets
        .iter()
        .map(|t| {
            (
                t.selected.pid,
                t.selected.project.clone(),
                preset_code(t.preset),
                time_unit_rank(t.time_unit),
            )
        })
        .collect()
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "runs multi-process discovery, policy/burst/limit enforcement, shared-budget recording, and dynamic rescanning"
)]
fn recorder_loop(
    stop: &AtomicBool,
    flush_trigger: &AtomicBool,
    shared: &(Mutex<SharedState>, Condvar),
    record: &RecordConfig,
    limits: &Limits,
    local_sanitize: &SanitizeConfig,
    host_name: &str,
    tags: &BTreeMap<String, String>,
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

    let root = Root::system();
    let scan_opts = ScanOptions {
        runtime_dirs: record.runtime_dirs.clone(),
        pod_annotations_dir: record.pod_annotations_dir.clone(),
        self_pid: std::process::id(),
    };
    let mut build_ids = BuildIdCache::default();
    let mut active_policy: Option<PolicyDirective> = None;
    let mut active_burst: Option<BurstDirective> = None;
    let mut last_policy_seq = 0_u64;
    let mut last_burst_seq = 0_u64;

    while !stop.load(Ordering::Relaxed) {
        let (lock, _) = shared;
        let (policy_update, burst_update) = {
            let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            let p_upd = if guard.policy_seq == last_policy_seq {
                None
            } else {
                last_policy_seq = guard.policy_seq;
                Some(guard.policy.clone())
            };
            let b_upd = if guard.burst_seq == last_burst_seq {
                None
            } else {
                last_burst_seq = guard.burst_seq;
                Some(guard.burst.clone())
            };
            (p_upd, b_upd)
        };

        if let Some(pol_opt) = policy_update {
            if let Some(pol) = &pol_opt {
                let (org_san, proj_sans) = build_sanitizers(local_sanitize, pol.sanitizer.as_ref());
                let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                guard.sanitizer = org_san;
                guard.project_sanitizers = proj_sans;
                guard.applied_policy_version = guard.applied_policy_version.max(pol.version);
                guard.applied_sanitizer_version =
                    guard.applied_sanitizer_version.max(pol.sanitizer_version);
            }
            active_policy = pol_opt;
        }
        if let Some(b_opt) = burst_update {
            active_burst = b_opt;
        }

        let remote_rules = remote_selection_rules(active_policy.as_ref());
        let selected = scan_selected_processes(
            &root,
            &scan_opts,
            &record.rules,
            &remote_rules,
            limits.allow_remote_rules,
            &mut build_ids,
        );

        let now_ns = unix_now_ns();
        let planned: Vec<PlannedTarget> = selected
            .into_iter()
            .map(|sel| {
                let (preset, time_unit) = effective_policy_for_process(
                    &sel,
                    record.preset,
                    record.time_unit,
                    active_policy.as_ref(),
                    active_burst.as_ref(),
                    limits,
                    now_ns,
                );
                PlannedTarget {
                    selected: sel,
                    preset,
                    time_unit,
                }
            })
            .collect();

        {
            let mut projs: BTreeSet<String> = BTreeSet::new();
            let mut svcs: BTreeSet<String> = BTreeSet::new();
            for t in &planned {
                if let Some(p) = &t.selected.project {
                    projs.insert(p.clone());
                }
                svcs.insert(t.selected.service.clone());
            }
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            guard.active_projects = projs.into_iter().collect();
            guard.active_services = svcs.into_iter().collect();
        }

        if planned.is_empty() {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(pf) = guard.flush_requested
                && guard.spool.is_empty()
            {
                guard.flush_completed = guard.flush_completed.max(pf.id);
                guard.flush_empty_acked = guard.flush_empty_acked.max(pf.id);
                guard.flush_requested = None;
            }
            drop(guard);
            sleep_interruptible(stop, Duration::from_millis(250));
            continue;
        }

        let current_sig = plan_signature(&planned);
        let session_stop = Arc::new(AtomicBool::new(false));
        {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if stop.load(Ordering::Relaxed)
                || guard.policy_seq != last_policy_seq
                || guard.burst_seq != last_burst_seq
            {
                continue;
            }
            guard.attached = true;
            guard.session_stop = Some(Arc::clone(&session_stop));
        }

        let mut by_preset: BTreeMap<u8, (Preset, Vec<PlannedTarget>)> = BTreeMap::new();
        for t in planned {
            by_preset
                .entry(preset_code(t.preset))
                .or_insert_with(|| (t.preset, Vec::new()))
                .1
                .push(t);
        }
        let groups: Vec<(Preset, Vec<PlannedTarget>)> = by_preset.into_values().collect();
        let watcher_stop = Arc::new(AtomicBool::new(false));

        let mut bpf_failed = false;
        thread::scope(|scope| {
            let watcher_done = Arc::clone(&watcher_stop);
            let sess_stop_watch = Arc::clone(&session_stop);
            let watch_root = Root::system();
            let watch_scan_opts = scan_opts.clone();
            let watch_local_rules = record.rules.clone();
            let watch_remote_rules = remote_rules.clone();
            let watch_policy = active_policy.clone();
            let watch_burst = active_burst.clone();
            let watch_limits = limits.clone();
            let watch_base_preset = record.preset;
            let watch_base_unit = record.time_unit;
            let watch_sig = current_sig.clone();

            scope.spawn(move || {
                let mut local_cache = BuildIdCache::default();
                while !stop.load(Ordering::Relaxed)
                    && !watcher_done.load(Ordering::Relaxed)
                    && !sess_stop_watch.load(Ordering::Relaxed)
                {
                    sleep_interruptible(&watcher_done, RESCAN_INTERVAL);
                    if stop.load(Ordering::Relaxed)
                        || watcher_done.load(Ordering::Relaxed)
                        || sess_stop_watch.load(Ordering::Relaxed)
                    {
                        break;
                    }
                    let rescanned = scan_selected_processes(
                        &watch_root,
                        &watch_scan_opts,
                        &watch_local_rules,
                        &watch_remote_rules,
                        watch_limits.allow_remote_rules,
                        &mut local_cache,
                    );
                    let now_check = unix_now_ns();
                    let next_plan: Vec<PlannedTarget> = rescanned
                        .into_iter()
                        .map(|sel| {
                            let (preset, time_unit) = effective_policy_for_process(
                                &sel,
                                watch_base_preset,
                                watch_base_unit,
                                watch_policy.as_ref(),
                                watch_burst.as_ref(),
                                &watch_limits,
                                now_check,
                            );
                            PlannedTarget {
                                selected: sel,
                                preset,
                                time_unit,
                            }
                        })
                        .collect();
                    if plan_signature(&next_plan) != watch_sig {
                        sess_stop_watch.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            });

            let mut handles = Vec::with_capacity(groups.len());
            for (idx, (preset, targets)) in groups.into_iter().enumerate() {
                let use_shared = if idx == 0 { shared_bpf.clone() } else { None };
                let allow_syscalls = shared_bpf.is_some();
                let options = build_record_options(
                    preset,
                    record.chunk_ms,
                    limits,
                    use_shared.as_ref(),
                    allow_syscalls,
                );
                let sess_stop = Arc::clone(&session_stop);
                let is_last_group = idx == 0;
                handles.push(scope.spawn(move || {
                    let pids: Vec<u32> = targets.iter().map(|t| t.selected.pid).collect();
                    let mut publish = |_profile: Profile| {};
                    let mut on_segment = |segment: Profile| {
                        enqueue_multi_project_cut(
                            &segment, shared, &targets, preset, host_name, tags, false,
                        );
                    };
                    let mut watch = Watch {
                        every: Duration::MAX,
                        publish: &mut publish,
                        stop: &sess_stop,
                        save: Some(Save {
                            trigger: flush_trigger,
                            to: &mut on_segment,
                        }),
                    };
                    match record_attached_many(&pids, &options, &mut watch) {
                        Ok(attached) => {
                            enqueue_multi_project_cut(
                                &attached.profile,
                                shared,
                                &targets,
                                preset,
                                host_name,
                                tags,
                                is_last_group,
                            );
                            Ok(())
                        }
                        Err(error) => {
                            if is_last_group {
                                complete_failed_cut(shared, true);
                            }
                            if !stop.load(Ordering::Relaxed) {
                                eprintln!(
                                    "microscope agent: recording pids {pids:?} failed: {error}"
                                );
                            }
                            Err(error)
                        }
                    }
                }));
            }

            for handle in handles {
                if let Ok(Err(_)) = handle.join()
                    && shared_bpf
                        .as_ref()
                        .is_some_and(SharedBpfEngine::load_failed)
                {
                    bpf_failed = true;
                }
            }
            watcher_stop.store(true, Ordering::SeqCst);
        });

        if bpf_failed {
            eprintln!("microscope agent: syscall BPF failed to load; falling back to samples only");
            shared_bpf = None;
            continue;
        }

        let update_pending = {
            let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            guard.policy_seq != last_policy_seq || guard.burst_seq != last_burst_seq
        };
        if !stop.load(Ordering::Relaxed) && !update_pending {
            sleep_interruptible(stop, Duration::from_millis(150));
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
    reason = "drives the poster state machine with dynamic watch intervals, hot sanitizer updates, and flush coordination"
)]
fn poster_loop(
    stop: &AtomicBool,
    shared: &(Mutex<SharedState>, Condvar),
    client: &Client,
    post_interval: Duration,
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
                    let sanitizer = Arc::clone(&guard.sanitizer);
                    let queued_total = guard.spool.stats().queued_chunks;
                    let batch = guard.spool.peek_batch_capped(
                        MAX_POST_BYTES,
                        max_batch_chunks,
                        agent_id,
                        unix_now_ns(),
                        &sanitizer,
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
    use crate::agent::{scratch::Scratch, select::ProcessMetadata};

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
            rules: Vec::new(),
            runtime_dirs: Vec::new(),
            pod_annotations_dir: None,
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

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end integration test covering three programs, two projects, and spool post verification"
    )]
    fn three_programs_records_two_into_two_projects_and_never_mentions_third() {
        struct KillOnDrop(std::process::Child);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let a = KillOnDrop(
            Command::new("/bin/sh")
                .env("MICROSCOPE_PROJECT", "alpha")
                .args(["-c", "while :; do :; done"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn alpha"),
        );
        let b = KillOnDrop(
            Command::new("/bin/sh")
                .env("MICROSCOPE_PROJECT", "beta")
                .args(["-c", "while :; do :; done"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn beta"),
        );
        let c = KillOnDrop(
            Command::new("/bin/sh")
                .args(["-c", "while :; do :; done"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn third"),
        );

        let pid_a = a.0.id();
        let pid_b = b.0.id();
        let pid_c = c.0.id();

        let mut options = RecordOptions::preset(Preset::Production);
        options.syscalls.level = SyscallLevel::Off;
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_timer = Arc::clone(&stop);
        let stopper = thread::spawn(move || {
            let _ = started_rx.recv();
            thread::sleep(Duration::from_millis(400));
            stop_timer.store(true, Ordering::SeqCst);
        });
        let mut started_tx = Some(started_tx);
        let mut publish = |_: Profile| {
            if let Some(tx) = started_tx.take() {
                let _ = tx.send(());
            }
        };
        let mut watch = Watch {
            every: Duration::MAX,
            publish: &mut publish,
            stop: &stop,
            save: None,
        };
        let attached = record_attached_many(&[pid_a, pid_b], &options, &mut watch)
            .expect("record two processes");
        let _ = stopper.join();
        drop(a);
        drop(b);
        drop(c);

        let sel_a = SelectedProcess {
            pid: pid_a,
            program: "prog-alpha".into(),
            project: Some("alpha".into()),
            service: "svc-alpha".into(),
            matched_by: "env: MICROSCOPE_PROJECT".into(),
            meta: ProcessMetadata {
                pid: pid_a,
                comm: "prog-alpha".into(),
                ..ProcessMetadata::default()
            },
        };
        let sel_b = SelectedProcess {
            pid: pid_b,
            program: "prog-beta".into(),
            project: Some("beta".into()),
            service: "svc-beta".into(),
            matched_by: "env: MICROSCOPE_PROJECT".into(),
            meta: ProcessMetadata {
                pid: pid_b,
                comm: "prog-beta".into(),
                ..ProcessMetadata::default()
            },
        };

        let scratch = Scratch::new();
        let mut spool = Spool::open(&scratch.path().join("spool"), 4 * 1024 * 1024).expect("spool");
        let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"test-key"));
        let (now_ns, wall_offset) = wall_clock_reference();

        let cap_a = filter_capture_by_pids(attached.profile.capture(), &BTreeSet::from([pid_a]));
        let cap_b = filter_capture_by_pids(attached.profile.capture(), &BTreeSet::from([pid_b]));
        assert!(cap_a.validate().is_ok());
        assert!(cap_b.validate().is_ok());
        assert!(
            !cap_a.processes.iter().any(|p| p.pid.get() == pid_c)
                && !cap_b.processes.iter().any(|p| p.pid.get() == pid_c)
        );
        assert!(!cap_a.processes.iter().any(|p| p.pid.get() == pid_b));
        assert!(!cap_b.processes.iter().any(|p| p.pid.get() == pid_a));

        let raw_a = build_selected_chunk_data(
            cap_a,
            7,
            1,
            Some("alpha"),
            &[&sel_a],
            Preset::Production,
            TimeUnit::Ms,
            "host-1",
            &BTreeMap::new(),
            now_ns,
            wall_offset,
        );
        let raw_b = build_selected_chunk_data(
            cap_b,
            7,
            2,
            Some("beta"),
            &[&sel_b],
            Preset::Production,
            TimeUnit::Ms,
            "host-1",
            &BTreeMap::new(),
            now_ns,
            wall_offset,
        );
        spool
            .push(1, &sanitize_chunk(raw_a, TimeUnit::Ms, &policy), 7, now_ns)
            .expect("push a");
        spool
            .push(2, &sanitize_chunk(raw_b, TimeUnit::Ms, &policy), 7, now_ns)
            .expect("push b");

        let batch1 = spool
            .peek_batch(MAX_POST_BYTES, 7, now_ns, &policy)
            .expect("peek 1")
            .expect("batch 1");
        assert_eq!(batch1.first_chunk, 1);
        assert_eq!(batch1.last_chunk, 1);
        spool
            .ack(batch1.owner_gen, batch1.last_chunk)
            .expect("ack 1");

        let batch2 = spool
            .peek_batch(MAX_POST_BYTES, 7, now_ns, &policy)
            .expect("peek 2")
            .expect("batch 2");
        assert_eq!(batch2.first_chunk, 2);
        assert_eq!(batch2.last_chunk, 2);
        spool
            .ack(batch2.owner_gen, batch2.last_chunk)
            .expect("ack 2");

        for (post, wanted_proj, forbidden_proj) in [
            (batch1.post.as_bytes(), "alpha", "beta"),
            (batch2.post.as_bytes(), "beta", "alpha"),
        ] {
            let reader = ObjectReader::open(post).expect("open");
            let dec = decode_tables(&reader.part_sections(0, None).expect("sec")).expect("dec");
            assert!(
                dec.dimensions
                    .iter()
                    .any(|d| d.name == "microscope.project" && d.value == wanted_proj)
            );
            assert!(
                !dec.dimensions
                    .iter()
                    .any(|d| d.value.contains(forbidden_proj))
            );
            assert!(!dec.capture.processes.iter().any(|p| p.pid.get() == pid_c));
        }
    }

    #[test]
    #[ignore = "multi-process CPU budget benchmark; run in --release under microscope-bpf with -- --ignored --nocapture"]
    fn measure_many_processes_under_one_cpu_budget() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/hot-calls");
        assert!(
            fixture.exists(),
            "hot-calls fixture missing at {}; run `just build-test-programs` first",
            fixture.display()
        );
        for count in [1usize, 4, 8, 16] {
            let mut children: Vec<std::process::Child> = (0..count)
                .map(|_| {
                    std::process::Command::new(&fixture)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .expect("spawn hot-calls")
                })
                .collect();
            let pids: Vec<u32> = children.iter().map(std::process::Child::id).collect();
            thread::sleep(Duration::from_millis(200));

            let mut options = RecordOptions::preset(Preset::Production);
            options.cuts = Cuts::Segments(Duration::from_secs(10));
            let stop = Arc::new(AtomicBool::new(false));
            let stop_timer = Arc::clone(&stop);
            let stopper = thread::spawn(move || {
                thread::sleep(Duration::from_secs(22));
                stop_timer.store(true, Ordering::SeqCst);
            });

            let trigger = AtomicBool::new(false);
            let (mut seg_idx, mut total_samples) = (0usize, 0usize);
            let (mut steady_net_ns, mut steady_wall_ns) = (0u64, 0u64);
            let (mut total_raised, mut total_kept) = (0u64, 0u64);
            let mut observed_pids: BTreeSet<u32> = BTreeSet::new();
            let mut collect = |profile: Profile| {
                let cap = profile.capture();
                total_samples += cap.samples.len();
                observed_pids.extend(cap.processes.iter().map(|p| p.pid.get()));
                let (cpu_ns, load_ns) = profile.recorder_cpu();
                let net_ns = cpu_ns
                    .saturating_sub(load_ns)
                    .saturating_sub(profile.recorder_setup());
                let (s, e) = profile.span();
                let span_ns = u64::try_from(e.saturating_sub(s)).unwrap_or(0);
                if let Some((raised, kept)) = profile.kept_share() {
                    total_raised += raised;
                    total_kept += kept;
                }
                if seg_idx == 1 && span_ns > 5_000_000_000 {
                    steady_net_ns += net_ns;
                    steady_wall_ns += span_ns;
                }
                seg_idx += 1;
            };
            let mut publish = |_: Profile| {};
            let mut watch = Watch {
                every: Duration::MAX,
                publish: &mut publish,
                stop: &stop,
                save: Some(Save {
                    trigger: &trigger,
                    to: &mut collect,
                }),
            };

            let attached =
                record_attached_many(&pids, &options, &mut watch).expect("record many processes");
            collect(attached.profile);
            let _ = stopper.join();
            for ch in &mut children {
                let _ = ch.kill();
                let _ = ch.wait();
            }

            let steady_pct = if steady_wall_ns > 0 {
                (Duration::from_nanos(steady_net_ns).as_secs_f64()
                    / Duration::from_nanos(steady_wall_ns).as_secs_f64())
                    * 100.0
            } else {
                0.0
            };
            let matched = pids.iter().filter(|p| observed_pids.contains(p)).count();
            eprintln!(
                "MULTI_PROC_BUDGET procs={count:>2} steady_cpu_pct={steady_pct:.3}% kept={total_kept}/{total_raised} samples={total_samples} observed_pids={matched}/{count}"
            );
            assert_eq!(matched, count, "expected all {count} PIDs to be observed");
            assert!(total_samples > 0, "expected samples for {count} PIDs");
        }
    }
}
