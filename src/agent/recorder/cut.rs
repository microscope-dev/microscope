use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Condvar, Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use microscope_data::{
    capture::{Capture, records::NONE},
    object::{ChunkData, ChunkRow, DimensionRow, TimeUnit, shift_capture_times},
    sanitize::sanitize_chunk,
};

use super::{
    PendingFlush, PlannedTarget, SharedState,
    policy::{preset_code, time_unit_rank},
};
use crate::{
    agent::{config::RecordConfig, select::SelectedProcess, split::filter_capture_by_pids},
    profiler::{Preset, Profile},
};

pub fn unix_now_ns() -> i64 {
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

pub fn wall_clock_reference() -> (i64, i64) {
    let mono_before = monotonic_ns();
    let wall = unix_now_ns();
    let mono_after = monotonic_ns();
    let mono_mid = mono_before.saturating_add(mono_after.saturating_sub(mono_before) / 2);
    (wall, wall.saturating_sub(mono_mid))
}

pub const fn capture_has_data(c: &Capture) -> bool {
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
pub fn build_selected_chunk_data(
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
    if sel.service_explicit {
        push("service.name", &sel.service);
    }
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
pub enum CutOutcome {
    Pushed(u64),
    Empty,
    Failed,
}

pub fn flush_cuts_done(pf: &PendingFlush, group_cut_seqs: &[u64]) -> bool {
    pf.target_group_cuts
        .iter()
        .enumerate()
        .all(|(i, &target)| group_cut_seqs.get(i).copied().unwrap_or(target) >= target)
}

pub fn finish_cut_locked(guard: &mut SharedState, group_idx: usize, outcome: CutOutcome) {
    if let Some(slot) = guard.group_cut_seqs.get_mut(group_idx) {
        *slot = slot.saturating_add(1);
    }
    if let Some(mut pf) = guard.flush_requested.take() {
        match outcome {
            CutOutcome::Pushed(chunk_no) if chunk_no >= pf.min_fresh_chunk => {
                pf.required_chunk = Some(pf.required_chunk.map_or(chunk_no, |c| c.max(chunk_no)));
            }
            CutOutcome::Failed => {
                pf.any_failed = true;
            }
            CutOutcome::Pushed(_) | CutOutcome::Empty => {}
        }
        if guard.spool.highest_lost_chunk() >= pf.min_fresh_chunk {
            pf.any_failed = true;
        }
        let cut_done = flush_cuts_done(&pf, &guard.group_cut_seqs);
        if cut_done && pf.any_failed {
            guard.flush_failed_id = guard.flush_failed_id.max(pf.id);
        } else if cut_done && pf.required_chunk.is_none() {
            if guard.spool.is_empty() {
                guard.flush_completed = guard.flush_completed.max(pf.id);
                guard.flush_empty_acked = guard.flush_empty_acked.max(pf.id);
            } else {
                guard.flush_requested = Some(pf);
            }
        } else {
            guard.flush_requested = Some(pf);
        }
    }
}

pub fn finalize_session_locked(guard: &mut SharedState) {
    guard.attached = false;
    guard.session_stop = None;
    guard.active_groups = 0;
    guard.group_triggers.clear();
    if let Some(mut pf) = guard.flush_requested.take() {
        pf.target_group_cuts.clear();
        if guard.spool.highest_lost_chunk() >= pf.min_fresh_chunk {
            pf.any_failed = true;
        }
        if pf.any_failed {
            guard.flush_failed_id = guard.flush_failed_id.max(pf.id);
        } else if pf.required_chunk.is_none() {
            if guard.spool.is_empty() {
                guard.flush_completed = guard.flush_completed.max(pf.id);
                guard.flush_empty_acked = guard.flush_empty_acked.max(pf.id);
            } else {
                guard.flush_requested = Some(pf);
            }
        } else {
            guard.flush_requested = Some(pf);
        }
    }
    guard.group_cut_seqs.clear();
}

type UnhintedKey = (String, Option<String>, Option<String>);
type BucketKey = (Option<String>, u8, Option<UnhintedKey>);
type CutBucket<'a> = BTreeMap<BucketKey, (TimeUnit, Vec<&'a SelectedProcess>)>;

pub fn bucket_allowed_pids(members: &[&SelectedProcess]) -> BTreeSet<u32> {
    members.iter().map(|m| m.pid).collect()
}

pub fn bucket_targets(targets: &[PlannedTarget]) -> CutBucket<'_> {
    let mut by_bucket: CutBucket<'_> = BTreeMap::new();
    for t in targets {
        let unhinted_key = t.selected.project.is_none().then(|| {
            (
                t.selected.program.clone(),
                t.selected.meta.build_id.clone(),
                t.selected
                    .service_explicit
                    .then(|| t.selected.service.clone()),
            )
        });
        let key = (
            t.selected.project.clone(),
            time_unit_rank(t.time_unit),
            unhinted_key,
        );
        by_bucket
            .entry(key)
            .or_insert_with(|| (t.time_unit, Vec::new()))
            .1
            .push(&t.selected);
    }
    by_bucket
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "splits a multi-process segment by project and unhinted program, exports exact selected PIDs, records binary paths for symbols, sanitizes, and spools in strict chunk order"
)]
pub fn enqueue_multi_project_cut(
    profile: &Profile,
    shared: &(Mutex<SharedState>, Condvar),
    publish_lock: &Mutex<()>,
    group_idx: usize,
    targets: &[PlannedTarget],
    preset: Preset,
    host_name: &str,
    tags: &BTreeMap<String, String>,
) {
    let (lock, cvar) = shared;
    let capture = profile.capture();
    if !capture_has_data(capture) || targets.is_empty() {
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        finish_cut_locked(&mut guard, group_idx, CutOutcome::Empty);
        drop(guard);
        cvar.notify_all();
        return;
    }

    let by_bucket = bucket_targets(targets);
    let (now_ns, wall_offset_ns) = wall_clock_reference();
    let mut last_pushed: Option<u64> = None;
    let mut any_failed = false;

    for ((proj_opt, _, _), (time_unit, members)) in by_bucket {
        let allowed_pids = bucket_allowed_pids(&members);
        let filtered = filter_capture_by_pids(capture, &allowed_pids);
        if !capture_has_data(&filtered) {
            continue;
        }

        let publish_guard = publish_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (alloc, sanitizer) = {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            let mut module_owner_pid: BTreeMap<u32, u32> = BTreeMap::new();
            for mapping in &filtered.mappings {
                let mod_idx = mapping.module.get();
                if let Some(proc) = filtered.processes.get(mapping.process.get() as usize) {
                    module_owner_pid
                        .entry(mod_idx)
                        .or_insert_with(|| proc.pid.get());
                }
            }
            let member_service_by_pid: BTreeMap<u32, String> = members
                .iter()
                .filter(|m| m.service_explicit)
                .map(|m| (m.pid, m.service.clone()))
                .collect();
            for (idx, m) in filtered.modules.iter().enumerate() {
                if m.build_id.get() != NONE && m.path.get() != NONE {
                    let bid = filtered.string(m.build_id.get());
                    let path = filtered.string(m.path.get());
                    if !bid.is_empty() && path.starts_with('/') {
                        let owner_pid = u32::try_from(idx)
                            .ok()
                            .and_then(|mod_idx| module_owner_pid.get(&mod_idx).copied());
                        let svc_hint =
                            owner_pid.and_then(|pid| member_service_by_pid.get(&pid).cloned());
                        guard.symbol_queue.record_binary(
                            bid,
                            std::path::PathBuf::from(path),
                            owner_pid,
                            proj_opt.clone(),
                            svc_hint,
                        );
                    }
                }
            }
            for member in &members {
                if let (Some(bid), Some(exe)) = (&member.meta.build_id, &member.meta.exe_path) {
                    guard.symbol_queue.record_binary(
                        bid,
                        std::path::PathBuf::from(exe),
                        Some(member.pid),
                        proj_opt.clone(),
                        member.service_explicit.then(|| member.service.clone()),
                    );
                }
            }
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
        drop(publish_guard);

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

    let outcome = if any_failed {
        CutOutcome::Failed
    } else if let Some(n) = last_pushed {
        CutOutcome::Pushed(n)
    } else {
        CutOutcome::Empty
    };
    let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
    finish_cut_locked(&mut guard, group_idx, outcome);
    drop(guard);
    cvar.notify_all();
}

pub fn complete_failed_cut(shared: &(Mutex<SharedState>, Condvar), group_idx: usize) {
    let (lock, cvar) = shared;
    let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
    finish_cut_locked(&mut guard, group_idx, CutOutcome::Failed);
    drop(guard);
    cvar.notify_all();
}
