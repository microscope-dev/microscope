mod cut;
mod policy;

#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use microscope_data::{object::TimeUnit, sanitize::Sanitizer};

pub use self::cut::build_chunk_data;
use self::{
    cut::{
        complete_failed_cut, enqueue_multi_project_cut, finalize_session_locked, flush_cuts_done,
        unix_now_ns,
    },
    policy::{
        build_record_options, build_sanitizers, effective_policy_for_process,
        max_concurrent_groups, preset_code, remote_selection_rules, syscalls_fit_memory_limit,
        time_unit_rank,
    },
};
use super::{
    client::{Client, IngestReply},
    config::{Config, Limits, RecordConfig, SanitizeConfig},
    host::Root,
    machine::Instant,
    poster::{PosterAction, PosterMachine, PosterOutcome},
    select::{BuildIdCache, ScanOptions, SelectedProcess, scan_selected_processes},
    spool::{MAX_POST_BYTES, Spool},
    symbols::{
        PrepareOutcome, SymbolUploadQueue, UploadOutcome, prepare_debug_payload,
        upload_compressed_symbol,
    },
    wire::{BurstDirective, PolicyDirective, WatchDirective},
};
use crate::profiler::{
    Preset, Profile, Save, SharedBpfEngine, Stopping, Watch, record_attached_many_exact,
};

const RESCAN_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(super) struct PendingFlush {
    pub id: u64,
    pub deadline: std::time::Instant,
    pub target_group_cuts: Vec<u64>,
    pub min_fresh_chunk: u64,
    pub required_chunk: Option<u64>,
    pub any_failed: bool,
}

pub(super) struct SharedState {
    pub server: String,
    pub token: Option<String>,
    pub agent_id: Option<u64>,
    pub need_token: bool,
    pub spool_seq: u64,
    pub spool: Spool,
    pub attached: bool,
    pub active_groups: usize,
    pub group_cut_seqs: Vec<u64>,
    pub session_stop: Option<Arc<AtomicBool>>,
    pub group_triggers: Vec<Arc<AtomicBool>>,
    pub watch: Option<WatchDirective>,
    pub watch_seq: u64,
    pub flush_requested: Option<PendingFlush>,
    pub flush_completed: u64,
    pub flush_empty_acked: u64,
    pub flush_failed_id: u64,
    pub policy: Option<PolicyDirective>,
    pub policy_seq: u64,
    pub bursts: Vec<BurstDirective>,
    pub burst_seq: u64,
    pub applied_policy_version: u64,
    pub applied_sanitizer_version: u64,
    pub active_projects: Vec<String>,
    pub active_services: Vec<String>,
    pub sanitizer: Arc<Sanitizer>,
    pub project_sanitizers: BTreeMap<String, Arc<Sanitizer>>,
    pub symbol_queue: SymbolUploadQueue,
}

#[derive(Clone)]
pub(super) struct PlannedTarget {
    pub selected: SelectedProcess,
    pub preset: Preset,
    pub time_unit: TimeUnit,
}

#[derive(Clone)]
pub struct PipelineControl {
    shared: Arc<(Mutex<SharedState>, Condvar)>,
}

impl PipelineControl {
    pub fn apply_watch(&self, watch: Option<WatchDirective>) {
        let trigger_cut = watch.is_some();
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.watch = watch;
        guard.watch_seq = guard.watch_seq.saturating_add(1);
        let triggers = if trigger_cut {
            guard.group_triggers.clone()
        } else {
            Vec::new()
        };
        drop(guard);
        for trigger in triggers {
            trigger.store(true, Ordering::SeqCst);
        }
        cvar.notify_all();
    }

    pub fn request_flush(&self, flush_id: u64, deadline_ms: u64) {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut triggers = Vec::new();
        if flush_id > guard.flush_completed
            && flush_id > guard.flush_failed_id
            && guard
                .flush_requested
                .as_ref()
                .is_none_or(|existing| flush_id > existing.id)
        {
            if !guard.attached && guard.spool.is_empty() {
                guard.flush_completed = guard.flush_completed.max(flush_id);
                guard.flush_empty_acked = guard.flush_empty_acked.max(flush_id);
                guard.flush_requested = None;
            } else {
                let target_group_cuts = if guard.attached {
                    guard
                        .group_cut_seqs
                        .iter()
                        .map(|&seq| seq.saturating_add(1))
                        .collect()
                } else {
                    Vec::new()
                };
                let min_fresh_chunk = guard.spool.stats().next_chunk;
                let deadline = std::time::Instant::now()
                    + Duration::from_millis(deadline_ms.clamp(100, 30_000));
                guard.flush_requested = Some(PendingFlush {
                    id: flush_id,
                    deadline,
                    target_group_cuts,
                    min_fresh_chunk,
                    required_chunk: None,
                    any_failed: false,
                });
                if guard.attached {
                    triggers.clone_from(&guard.group_triggers);
                }
            }
        }
        drop(guard);
        for trigger in triggers {
            trigger.store(true, Ordering::SeqCst);
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

    pub fn apply_bursts(&self, bursts: Vec<BurstDirective>) {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.bursts = bursts;
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
    shared: Arc<(Mutex<SharedState>, Condvar)>,
    recorder: Option<JoinHandle<()>>,
    poster: Option<JoinHandle<()>>,
    symbol_uploader: Option<JoinHandle<()>>,
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
        let shared = Arc::new((
            Mutex::new(SharedState {
                server,
                token: None,
                agent_id,
                need_token: false,
                spool_seq: 0,
                spool,
                attached: false,
                active_groups: 0,
                group_cut_seqs: Vec::new(),
                session_stop: None,
                group_triggers: Vec::new(),
                watch: None,
                watch_seq: 0,
                flush_requested: None,
                flush_completed: 0,
                flush_empty_acked: 0,
                flush_failed_id: 0,
                policy: None,
                policy_seq: 0,
                bursts: Vec::new(),
                burst_seq: 0,
                applied_policy_version: 0,
                applied_sanitizer_version: 0,
                active_projects: Vec::new(),
                active_services: Vec::new(),
                sanitizer: initial_sanitizer,
                project_sanitizers: initial_proj_sanitizers,
                symbol_queue: SymbolUploadQueue::default(),
            }),
            Condvar::new(),
        ));

        let rec_stop = Arc::clone(&stop);
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
        let symbols_upload_enabled = config.symbols.upload;
        let poster = thread::Builder::new()
            .name("microscope-agent-poster".into())
            .spawn(move || {
                poster_loop(
                    &post_stop,
                    &post_shared,
                    &client,
                    post_interval,
                    symbols_upload_enabled,
                );
            })?;

        let symbol_uploader = if config.symbols.upload {
            let sym_client = Client::new(config.server.clone(), config.ca_file.as_deref())?;
            let sym_stop = Arc::clone(&stop);
            let sym_shared = Arc::clone(&shared);
            let max_bps = config.symbols.max_bytes_per_sec;
            Some(
                thread::Builder::new()
                    .name("microscope-agent-symbols".into())
                    .spawn(move || {
                        symbol_uploader_loop(&sym_stop, &sym_shared, &sym_client, max_bps);
                    })?,
            )
        } else {
            None
        };

        Ok(Self {
            stop,
            shared,
            recorder: Some(recorder),
            poster: Some(poster),
            symbol_uploader,
        })
    }

    #[must_use]
    pub fn control(&self) -> PipelineControl {
        PipelineControl {
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
        if let Some(handle) = self.symbol_uploader.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

fn sleep_interruptible(stop: &AtomicBool, total: Duration) {
    let start = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) && start.elapsed() < total {
        let remain = total.saturating_sub(start.elapsed());
        thread::sleep(remain.min(Duration::from_millis(50)));
    }
}

type PlanSignature = Vec<(u32, Option<String>, u8, u8)>;

fn plan_signature(targets: &[PlannedTarget]) -> PlanSignature {
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
    clippy::too_many_lines,
    reason = "runs multi-process discovery, policy/burst/limit enforcement, shared-budget recording, and dynamic rescanning"
)]
fn recorder_loop(
    stop: &AtomicBool,
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
    let build_ids = Mutex::new(BuildIdCache::default());
    let mut active_policy: Option<PolicyDirective> = None;
    let mut active_bursts: Vec<BurstDirective> = Vec::new();
    let mut last_policy_seq = 0_u64;
    let mut last_burst_seq = 0_u64;
    let publish_lock = Mutex::new(());

    while !stop.load(Ordering::Relaxed) {
        let (lock, cvar) = shared;
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
                Some(guard.bursts.clone())
            };
            (p_upd, b_upd)
        };

        if let Some(pol_opt) = policy_update {
            if let Some(pol) = &pol_opt {
                let (cur_policy_ver, cur_san_ver) = {
                    let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    (
                        guard.applied_policy_version,
                        guard.applied_sanitizer_version,
                    )
                };
                let accept_sanitizer = pol.sanitizer_version > cur_san_ver;
                let next_sanitizers = if accept_sanitizer {
                    match policy::try_build_sanitizers(local_sanitize, pol.sanitizer.as_ref()) {
                        Ok(pair) => Some(pair),
                        Err(error) => {
                            eprintln!(
                                "microscope agent: rejecting remote sanitizer policy v{} ({error}); keeping previous sanitizer",
                                pol.sanitizer_version
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                let accept_policy = pol.version > cur_policy_ver;
                {
                    let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some((org_san, proj_sans)) = next_sanitizers {
                        guard.sanitizer = org_san;
                        guard.project_sanitizers = proj_sans;
                        guard.applied_sanitizer_version = pol.sanitizer_version;
                    }
                    if accept_policy && pol.version > 0 {
                        guard.applied_policy_version = pol.version;
                    }
                }
                if accept_policy {
                    active_policy = pol_opt;
                }
            } else {
                active_policy = None;
            }
        }
        if let Some(b_list) = burst_update {
            active_bursts = b_list;
        }

        let remote_rules = remote_selection_rules(active_policy.as_ref());
        let scan = {
            let mut cache = build_ids.lock().unwrap_or_else(PoisonError::into_inner);
            scan_selected_processes(
                &root,
                &scan_opts,
                &record.rules,
                &remote_rules,
                limits.allow_remote_rules,
                &mut cache,
            )
        };

        let now_ns = unix_now_ns();
        let planned: Vec<PlannedTarget> = scan
            .selected
            .into_iter()
            .map(|sel| {
                let (preset, time_unit) = effective_policy_for_process(
                    &sel,
                    record.preset,
                    record.time_unit,
                    active_policy.as_ref(),
                    &active_bursts,
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
                if t.selected.service_explicit {
                    svcs.insert(t.selected.service.clone());
                }
            }
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            guard.active_projects = projs.into_iter().collect();
            guard.active_services = svcs.into_iter().collect();
        }

        if planned.is_empty() {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(pf) = guard.flush_requested.as_ref()
                && guard.spool.is_empty()
            {
                let fid = pf.id;
                guard.flush_completed = guard.flush_completed.max(fid);
                guard.flush_empty_acked = guard.flush_empty_acked.max(fid);
                guard.flush_requested = None;
            }
            drop(guard);
            sleep_interruptible(stop, Duration::from_millis(250));
            continue;
        }

        let current_sig = plan_signature(&planned);
        let mut by_preset: BTreeMap<u8, (Preset, Vec<PlannedTarget>)> = BTreeMap::new();
        for t in planned {
            by_preset
                .entry(preset_code(t.preset))
                .or_insert_with(|| (t.preset, Vec::new()))
                .1
                .push(t);
        }
        let max_groups = max_concurrent_groups(limits);
        if max_groups == 0 {
            eprintln!(
                "microscope agent: limits.memory_mib ({:?}) is too small for minimum perf rings on this host; skipping recording",
                limits.memory_mib
            );
            sleep_interruptible(stop, Duration::from_secs(5));
            continue;
        }
        let groups: Vec<(Preset, Vec<PlannedTarget>)> = if by_preset.len() > max_groups {
            let mut combined_targets = Vec::new();
            let mut chosen_preset = Preset::Production;
            for (preset, targets) in by_preset.into_values() {
                if combined_targets.is_empty() {
                    chosen_preset = preset;
                }
                combined_targets.extend(targets);
            }
            vec![(chosen_preset, combined_targets)]
        } else {
            by_preset.into_values().collect()
        };
        let group_count = groups.len();
        let group_triggers: Vec<Arc<AtomicBool>> = (0..group_count)
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
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
            guard.active_groups = group_count;
            guard.group_cut_seqs = vec![0; group_count];
            guard.session_stop = Some(Arc::clone(&session_stop));
            guard.group_triggers.clone_from(&group_triggers);
        }

        let watcher_stop = Arc::new(AtomicBool::new(false));
        let mut bpf_failed = false;
        let publish_ref = &publish_lock;

        thread::scope(|scope| {
            let watcher_done = Arc::clone(&watcher_stop);
            let sess_stop_watch = Arc::clone(&session_stop);
            let watch_root = Root::system();
            let watch_scan_opts = scan_opts.clone();
            let watch_local_rules = record.rules.clone();
            let watch_remote_rules = remote_rules.clone();
            let watch_policy = active_policy.clone();
            let watch_bursts = active_bursts.clone();
            let watch_limits = limits.clone();
            let watch_base_preset = record.preset;
            let watch_base_unit = record.time_unit;
            let watch_sig = current_sig.clone();

            let build_ids_ref = &build_ids;
            scope.spawn(move || {
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
                    let rescanned = {
                        let mut cache =
                            build_ids_ref.lock().unwrap_or_else(PoisonError::into_inner);
                        scan_selected_processes(
                            &watch_root,
                            &watch_scan_opts,
                            &watch_local_rules,
                            &watch_remote_rules,
                            watch_limits.allow_remote_rules,
                            &mut cache,
                        )
                    };
                    let now_check = unix_now_ns();
                    let next_plan: Vec<PlannedTarget> = rescanned
                        .selected
                        .into_iter()
                        .map(|sel| {
                            let (preset, time_unit) = effective_policy_for_process(
                                &sel,
                                watch_base_preset,
                                watch_base_unit,
                                watch_policy.as_ref(),
                                &watch_bursts,
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

            let mut handles = Vec::with_capacity(group_count);
            for (idx, ((preset, targets), trigger)) in
                groups.into_iter().zip(group_triggers).enumerate()
            {
                let owns_bpf = idx + 1 == group_count && syscalls_fit_memory_limit(limits);
                let use_shared = if owns_bpf { shared_bpf.clone() } else { None };
                let allow_syscalls = owns_bpf && shared_bpf.is_some();
                let options = build_record_options(
                    preset,
                    record.chunk_ms,
                    limits,
                    group_count,
                    use_shared.as_ref(),
                    allow_syscalls,
                );
                assert_eq!(options.stopping, Stopping::Never);
                let sess_stop = Arc::clone(&session_stop);
                let pids: Vec<u32> = targets.iter().map(|t| t.selected.pid).collect();
                handles.push(scope.spawn(move || {
                    let mut publish = |_profile: Profile| {};
                    let mut on_segment = |segment: Profile| {
                        enqueue_multi_project_cut(
                            &segment,
                            shared,
                            publish_ref,
                            idx,
                            &targets,
                            preset,
                            host_name,
                            tags,
                        );
                    };
                    let mut watch = Watch {
                        every: Duration::MAX,
                        publish: &mut publish,
                        stop: &sess_stop,
                        save: Some(Save {
                            trigger: &trigger,
                            to: &mut on_segment,
                        }),
                    };
                    match record_attached_many_exact(&pids, &options, &mut watch) {
                        Ok(attached) => {
                            enqueue_multi_project_cut(
                                &attached.profile,
                                shared,
                                publish_ref,
                                idx,
                                &targets,
                                preset,
                                host_name,
                                tags,
                            );
                            Ok(())
                        }
                        Err(error) => {
                            complete_failed_cut(shared, idx);
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

        {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            finalize_session_locked(&mut guard);
        }
        cvar.notify_all();

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
    reason = "drives the poster state machine with dynamic watch intervals, hot sanitizer updates, and multi-project flush coordination"
)]
fn poster_loop(
    stop: &AtomicBool,
    shared: &(Mutex<SharedState>, Condvar),
    client: &Client,
    post_interval: Duration,
    symbols_upload_enabled: bool,
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
            if let Some(pf) = guard.flush_requested.as_ref() {
                let wall_now = std::time::Instant::now();
                let fid = pf.id;
                let deadline = pf.deadline;
                let req_chunk = pf.required_chunk;
                let cut_done = flush_cuts_done(pf, &guard.group_cut_seqs);
                if wall_now >= deadline {
                    guard.flush_requested = None;
                    machine.clear_flush(fid);
                } else if cut_done {
                    if guard.spool.is_empty() && req_chunk.is_none() {
                        guard.flush_completed = guard.flush_completed.max(fid);
                        guard.flush_empty_acked = guard.flush_empty_acked.max(fid);
                        guard.flush_requested = None;
                        machine.clear_flush(fid);
                    } else {
                        let rem_ms =
                            u64::try_from(deadline.saturating_duration_since(wall_now).as_millis())
                                .unwrap_or(5_000)
                                .max(100);
                        ready_flush = Some((fid, rem_ms));
                    }
                } else {
                    waiting_for_cut = true;
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
                let (token, agent_id, org_sanitizer, project_sanitizers, snapshot) = {
                    let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    let (Some(token), Some(agent_id)) = (guard.token.clone(), guard.agent_id)
                    else {
                        continue;
                    };
                    (
                        token,
                        agent_id,
                        Arc::clone(&guard.sanitizer),
                        guard.project_sanitizers.clone(),
                        guard.spool.snapshot_candidates(),
                    )
                };
                let built = super::spool::build_batch_from_snapshot(
                    &snapshot,
                    MAX_POST_BYTES,
                    max_batch_chunks,
                    agent_id,
                    unix_now_ns(),
                    |project| {
                        project
                            .and_then(|p| project_sanitizers.get(&p.to_ascii_lowercase()))
                            .map_or_else(|| org_sanitizer.as_ref(), Arc::as_ref)
                    },
                );
                let (batch, post_flush_id) = {
                    let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    let batch = match built {
                        Ok(result) => {
                            if let Err(err) = guard.spool.apply_batch_drops(
                                snapshot.owner_gen,
                                &result.missing_chunks,
                                &result.drop_chunks,
                            ) {
                                Err(err)
                            } else {
                                Ok(result.batch)
                            }
                        }
                        Err(err) => Err(err),
                    };
                    let queued_total = snapshot.queued_total;
                    let wall_now = std::time::Instant::now();
                    let flush_check = guard.flush_requested.as_ref().map(|pf| {
                        (
                            pf.id,
                            pf.deadline,
                            pf.required_chunk,
                            pf.any_failed || guard.spool.highest_lost_chunk() >= pf.min_fresh_chunk,
                            flush_cuts_done(pf, &guard.group_cut_seqs),
                        )
                    });
                    let post_flush_id = match (&batch, flush_check) {
                        (Ok(Some(_)), Some((fid, _, _, true, _))) => {
                            guard.flush_failed_id = guard.flush_failed_id.max(fid);
                            guard.flush_requested = None;
                            machine.clear_flush(fid);
                            None
                        }
                        (Ok(Some(b)), Some((fid, deadline, req_chunk, false, true)))
                            if wall_now < deadline =>
                        {
                            req_chunk.map_or_else(
                                || (b.chunk_count >= queued_total).then_some(fid),
                                |req| {
                                    if b.first_chunk > req {
                                        guard.flush_failed_id = guard.flush_failed_id.max(fid);
                                        guard.flush_requested = None;
                                        machine.clear_flush(fid);
                                        None
                                    } else {
                                        (b.first_chunk <= req && req <= b.last_chunk).then_some(fid)
                                    }
                                },
                            )
                        }
                        _ => None,
                    };
                    drop(guard);
                    (batch, post_flush_id)
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
                    IngestReply::Accepted {
                        last_chunk,
                        project_id,
                        missing_symbols,
                    } => {
                        max_batch_chunks = usize::MAX;
                        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        let _ = guard.spool.ack(batch.owner_gen, last_chunk);
                        if symbols_upload_enabled
                            && !missing_symbols.is_empty()
                            && guard.symbol_queue.enqueue_missing(
                                project_id,
                                batch.project.as_deref(),
                                &missing_symbols,
                            )
                        {
                            cvar.notify_all();
                        }
                        if let Some(fid) = post_flush_id
                            && guard.flush_requested.as_ref().is_some_and(|pf| {
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
                        PosterOutcome::Accepted { last_chunk }
                    }
                    IngestReply::Overlap { last_chunk } => {
                        max_batch_chunks = usize::MAX;
                        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        let _ = guard.spool.ack(batch.owner_gen, last_chunk);
                        if let Some(fid) = post_flush_id
                            && guard.flush_requested.as_ref().is_some_and(|pf| {
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
                            if let Some(pf) = guard.flush_requested.as_ref()
                                && pf.required_chunk.is_some_and(|req| {
                                    batch.first_chunk >= pf.min_fresh_chunk
                                        && batch.first_chunk <= req
                                })
                            {
                                let fid = pf.id;
                                guard.flush_failed_id = guard.flush_failed_id.max(fid);
                                guard.flush_requested = None;
                                machine.clear_flush(fid);
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

fn symbol_uploader_loop(
    stop: &AtomicBool,
    shared: &(Mutex<SharedState>, Condvar),
    client: &Client,
    max_bytes_per_sec: u64,
) {
    let (lock, cvar) = shared;
    while !stop.load(Ordering::Relaxed) {
        let next = {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(token) = guard.token.clone()
                && let Some((bid, bin)) = guard.symbol_queue.pop_next()
            {
                Some((token, bid, bin))
            } else {
                let wait = if guard.token.is_none() {
                    Duration::from_millis(500)
                } else {
                    guard
                        .symbol_queue
                        .next_wait(std::time::Instant::now())
                        .map_or(Duration::from_millis(500), |w| {
                            w.min(Duration::from_millis(500))
                        })
                };
                let _ = cvar.wait_timeout(guard, wait);
                None
            }
        };

        let Some((token, task, binary)) = next else {
            continue;
        };

        let compressed = match prepare_debug_payload(&binary, &task.build_id) {
            PrepareOutcome::Ready(bytes) => bytes,
            PrepareOutcome::PermanentSkip => {
                lock.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .symbol_queue
                    .mark_done(&task);
                continue;
            }
            PrepareOutcome::RetryLater => {
                lock.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .symbol_queue
                    .mark_retry_later(&task);
                continue;
            }
        };

        let outcome = upload_compressed_symbol(
            client,
            &token,
            &task,
            &binary,
            &compressed,
            max_bytes_per_sec,
            stop,
        );
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        match outcome {
            UploadOutcome::Done => guard.symbol_queue.mark_done(&task),
            UploadOutcome::Unauthorized => {
                if guard.token.as_deref() == Some(token.as_str()) {
                    guard.token = None;
                    guard.need_token = true;
                }
                guard.symbol_queue.mark_retry_later(&task);
            }
            UploadOutcome::RetryLater => guard.symbol_queue.mark_retry_later(&task),
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
