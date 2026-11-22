use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::analysis::{
    ChosenZones, DiffCost, Granularity, KeyKind, Keys, Lanes, Look, Query, Side, Source, Transform,
    ZoneBlock, ZoneScope, diff_costs, diff_trees, kernel_lanes, off_cpu, plot_series, sites_named,
    zone_blocks, zone_stats as analyze_zone_stats,
};
use crate::capture::Capture;
use crate::capture::records::{AccountKind, FrameKind, GapReason, MarkerKind, NONE, Truncation};

use super::protocol::{
    ProfileBaseWeight, ProfileCost, ProfileCount, ProfileFunctionQuery, ProfileFunctions,
    ProfileGranularity, ProfileKey, ProfileKeyKind, ProfileLabelValue, ProfileLane, ProfileLines,
    ProfileLook, ProfileMark, ProfileMarkKind, ProfileMarker, ProfilePlot, ProfileProbes,
    ProfileQuery, ProfileSetting, ProfileSource, ProfileSourceFile, ProfileSummary, ProfileTask,
    ProfileThread, ProfileTimeline, ProfileTimelineQuery, ProfileTree, ProfileWait, ProfileZoneAt,
    ProfileZoneScope, ProfileZoneSite, ProfileZoneStats, ProfileZoneStatsQuery, ProfileZones,
    ProfileZonesQuery, TransformOp,
};
use super::{BaseLookup, CaptureView, SourceLines, ViewError, inspect, syscalls};

const MOST_BUCKETS: u32 = 4096;
const MOST_MARKS: usize = 4096;
const SOURCE_MARGIN: u64 = 5;

pub(super) const fn granularity(granularity: ProfileGranularity) -> Granularity {
    match granularity {
        ProfileGranularity::Function => Granularity::Function,
        ProfileGranularity::Physical => Granularity::PhysicalFunction,
        ProfileGranularity::Line => Granularity::Line,
        ProfileGranularity::Address => Granularity::Address,
    }
}

pub(super) fn resolve(
    capture: &Capture,
    keys: &Keys<'_>,
    query: &ProfileQuery,
) -> Result<Query, ViewError> {
    resolve_in(capture, keys, query, false)
}

fn resolve_base(
    capture: &Capture,
    keys: &Keys<'_>,
    query: &ProfileQuery,
) -> Result<Query, ViewError> {
    resolve_in(capture, keys, query, true)
}

pub(super) fn resolve_view(
    view: &CaptureView,
    keys: &Keys<'_>,
    query: &ProfileQuery,
) -> Result<Query, ViewError> {
    let mut resolved = resolve(&view.capture, keys, query)?;
    if let Some(during) = &query.during {
        resolved.during = Some(syscalls::rows(&view.capture, view.trace(), during)?);
    }
    Ok(resolved)
}

fn resolve_in(
    capture: &Capture,
    keys: &Keys<'_>,
    query: &ProfileQuery,
    base: bool,
) -> Result<Query, ViewError> {
    let (start, _) = capture.span();
    if query.syscall_leaves
        && capture
            .setting("weight")
            .is_some_and(|unit| unit != "nanoseconds")
    {
        return Err(ViewError::Invalid(
            "calls are weighed in time, and these samples are not".to_owned(),
        ));
    }

    let mut transforms = Vec::new();
    for transform in &query.transforms {
        if transform.op == TransformOp::CollapseCycles {
            transforms.push(Transform::CollapseCycles);
            continue;
        }

        let named = keys.named(&transform.name);
        if named.is_empty() && base {
            if transform.op == TransformOp::Focus {
                transforms.push(Transform::Focus(NONE));
            }
            continue;
        }

        if named.is_empty() {
            return Err(ViewError::Invalid(
                match keys.similar(&transform.name, 5).as_slice() {
                    [] => format!("nothing in the profile is named {}", transform.name),
                    similar => format!(
                        "nothing in the profile is named {}; names that contain it: {}",
                        transform.name,
                        similar
                            .iter()
                            .map(|&key| keys.exact_name(key))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                },
            ));
        }

        if transform.op == TransformOp::Focus && named.len() > 1 {
            return Err(ViewError::Invalid(format!(
                "{} things are named {}",
                named.len(),
                transform.name
            )));
        }

        transforms.extend(named.into_iter().map(|key| match transform.op {
            TransformOp::Focus => Transform::Focus(key),
            TransformOp::Merge => Transform::Merge(key),
            TransformOp::Drop => Transform::Drop(key),
            TransformOp::CollapseCycles => Transform::CollapseCycles,
        }));
    }

    Ok(Query {
        source: match query.source {
            ProfileSource::Samples => Source::Samples,
            ProfileSource::Waits => Source::Waits,
        },
        range: query
            .range
            .map(|[from, to]| (start.saturating_add(from), start.saturating_add(to))),
        threads: query.threads.clone(),
        tasks: query
            .tasks
            .as_ref()
            .map(|tasks| tasks.iter().map(|task| task.unwrap_or(NONE)).collect()),
        labels: query
            .labels
            .iter()
            .map(|label| {
                label
                    .split_once('=')
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .ok_or_else(|| ViewError::Invalid(format!("{label} is not key=value")))
            })
            .collect::<Result<_, _>>()?,
        by_label: query.by_label.clone(),
        zones: query
            .zones
            .as_ref()
            .map(|scope| zone_scope(capture, scope, base))
            .transpose()?,
        transforms,
        invert: query.invert,
        look: match query.look {
            ProfileLook::Logical => Look::Logical,
            ProfileLook::Native => Look::Native,
            ProfileLook::Python => Look::Python,
        },
        by_task: query.by_task,
        during: None,
        calls: query.syscall_leaves,
    })
}

fn zone_scope(
    capture: &Capture,
    scope: &ProfileZoneScope,
    base: bool,
) -> Result<ZoneScope, ViewError> {
    let sites = sites_named(capture, &scope.name);
    if sites.is_empty() && !base {
        return Err(ViewError::Invalid(format!(
            "no zones are named {}",
            scope.name
        )));
    }

    if let Some([shortest, longest]) = scope.band
        && shortest > longest
    {
        return Err(ViewError::Invalid(format!(
            "the band {shortest}..{longest} is empty"
        )));
    }

    Ok(ZoneScope {
        sites,
        band: scope.band.map(Into::into),
        outside: scope.outside,
    })
}

fn resolve_base_view(
    view: &CaptureView,
    query: &ProfileQuery,
    base_lookup: Option<BaseLookup<'_>>,
) -> Result<Option<(Arc<CaptureView>, ProfileQuery)>, ViewError> {
    let Some(base) = &query.base else {
        return Ok(None);
    };

    let same_profile = [view.id.as_str(), query.profile.as_str()].contains(&base.profile.as_str());
    if !same_profile && (query.threads.is_some() || query.tasks.is_some()) {
        return Err(ViewError::Invalid(
            "a diff with another profile cannot keep a thread or task filter, \
             whose rows are this profile's"
                .to_owned(),
        ));
    }

    let mut of_base = query.clone();
    of_base.profile.clone_from(&base.profile);
    of_base.range = base.range;
    of_base.base = None;
    if base.zones.is_some() {
        of_base.zones.clone_from(&base.zones);
    }

    if !same_profile {
        of_base.threads = None;
        of_base.tasks = None;
    }

    let target = if same_profile {
        Arc::new(CaptureView::new(
            Arc::clone(&view.capture),
            view.id.clone(),
            view.name.clone(),
            view.file.clone(),
        ))
    } else {
        base_lookup
            .and_then(|lookup| lookup(&base.profile))
            .ok_or_else(|| ViewError::Invalid(format!("no profile {}", base.profile)))?
    };

    Ok(Some((target, of_base)))
}

pub(super) fn summary(view: &CaptureView) -> ProfileSummary {
    let capture = &view.capture;
    let (start, end) = capture.span();

    let total_weight = capture
        .samples
        .iter()
        .map(|sample| sample.weight.get())
        .sum();
    let waited = capture
        .task_waits
        .iter()
        .map(|wait| wait.weight.get())
        .sum();

    let keys = Keys::new(capture, Granularity::Function);
    let mut label_keys: Vec<String> = keys
        .labels()
        .iter()
        .filter(|(_, value)| value.is_none())
        .map(|(key, _)| (*key).to_owned())
        .collect();
    label_keys.sort();

    ProfileSummary {
        id: view.id.clone(),
        name: view.name.clone(),
        program: capture.setting("program").map(str::to_owned),
        command: inspect::recorded_command(capture),
        unit: capture
            .setting("weight")
            .unwrap_or("nanoseconds")
            .to_owned(),
        source: capture.setting("source").map(str::to_owned),
        samples: capture.samples.len() as u64,
        weight: total_weight,
        syscalls: syscalls::syscall_count(capture),
        syscalls_lost: syscalls::syscalls_lost(capture).into(),
        duration: end - start,
        threads: threads(capture),
        lost: capture.account(AccountKind::LostSamples as u32),
        throttles: capture.account(AccountKind::Throttles as u32),
        recorder_cpu: capture.account(AccountKind::RecorderCpu as u32),
        recorder_loading: capture.account(AccountKind::RecorderLoading as u32),
        behind: paused(capture, GapReason::Behind, start),
        budget_changes: markers(capture, MarkerKind::Budget, start),
        tasks: tasks(capture, &keys),
        interpreted: capture
            .frames
            .iter()
            .any(|frame| frame.kind == FrameKind::Interpreted as u8),
        snapshots: capture.account(AccountKind::Snapshots as u32),
        snapshot_pause: capture.account(AccountKind::SnapshotPause as u32),
        task_samples: capture.account(AccountKind::TaskSamples as u32),
        snapshot_problems: markers(capture, MarkerKind::TaskSnapshot, start),
        waited,
        probes: ProfileProbes {
            events: capture.account(AccountKind::ProbeEvents as u32),
            overwritten: capture.account(AccountKind::ProbeOverwritten as u32),
            no_ring: capture.account(AccountKind::ProbeNoRing as u32),
            interrupted: capture.account(AccountKind::ProbeInterrupted as u32),
            malformed: capture.account(AccountKind::ProbeMalformed as u32),
            plotted: capture.plots.len() as u64,
        },
        zone_sites: zone_sites(capture),
        label_keys,
        probe_problems: markers(capture, MarkerKind::Probes, start),
        truncations: Truncation::ALL
            .into_iter()
            .map(|reason| {
                (
                    reason.describe().to_owned(),
                    capture.account(AccountKind::Truncated as u32 + reason as u32),
                )
            })
            .filter(|(_, count)| *count > 0)
            .map(|(what, count)| ProfileCount { what, count })
            .collect(),
        settings: capture
            .settings
            .iter()
            .map(|setting| ProfileSetting {
                name: capture.string(setting.key.get()).to_owned(),
                value: capture.string(setting.value.get()).to_owned(),
            })
            .collect(),
    }
}

fn threads(capture: &Capture) -> Vec<ProfileThread> {
    let mut out = BTreeMap::<u32, ProfileThread>::new();
    let new = |row: u32| {
        let thread = capture.thread(row);
        ProfileThread {
            row,
            pid: capture.process(thread.process.get()).pid.get(),
            tid: thread.tid.get(),
            name: Some(capture.string(thread.name.get()))
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            samples: 0,
            weight: 0,
            zones: 0,
            blocked: 0,
            preempted: 0,
        }
    };

    for range in &capture.sample_ranges {
        let samples = capture.thread_samples(range);
        let row = range.thread.get();
        let thread = out.entry(row).or_insert_with(|| new(row));
        thread.samples = samples.len() as u64;
        thread.weight = samples.iter().map(|sample| sample.weight.get()).sum();
    }

    for range in &capture.zone_ranges {
        let row = range.thread.get();
        out.entry(row).or_insert_with(|| new(row)).zones += u64::from(range.count.get());
    }

    for range in &capture.off_cpu_ranges {
        let row = range.thread.get();
        let thread = out.entry(row).or_insert_with(|| new(row));
        for interval in capture.thread_off_cpu(range) {
            let total = if interval.preempted == 0 {
                &mut thread.blocked
            } else {
                &mut thread.preempted
            };
            *total += interval.duration.get();
        }
    }

    out.into_values().collect()
}

fn paused(capture: &Capture, reason: GapReason, start: i64) -> Vec<[i64; 2]> {
    capture
        .gaps
        .iter()
        .filter(|gap| gap.reason == reason as u8)
        .map(|gap| [gap.start.get() - start, gap.end.get() - start])
        .collect()
}

fn markers(capture: &Capture, kind: MarkerKind, start: i64) -> Vec<ProfileMarker> {
    capture
        .markers
        .iter()
        .filter(|marker| marker.kind == kind as u8)
        .map(|marker| ProfileMarker {
            time: marker.time.get() - start,
            text: capture.string(marker.text.get()).to_owned(),
        })
        .collect()
}

fn tasks(capture: &Capture, keys: &Keys<'_>) -> Vec<ProfileTask> {
    let mut weights = vec![(0_u64, 0_u64); capture.tasks.len()];
    for sample in &capture.samples {
        if let Some(weight) = weights.get_mut(sample.task.get() as usize) {
            weight.0 += sample.weight.get();
        }
    }
    for wait in &capture.task_waits {
        if let Some(weight) = weights.get_mut(wait.task.get() as usize) {
            weight.1 += wait.weight.get();
        }
    }

    let mut out: Vec<ProfileTask> = weights
        .into_iter()
        .enumerate()
        .map(|(row, (weight, waited))| {
            let row = u32::try_from(row).unwrap_or(u32::MAX);
            ProfileTask {
                row,
                name: keys.name(keys.group(row)),
                weight,
                waited,
            }
        })
        .collect();
    out.sort_by(|a, b| (b.weight, b.waited, a.row).cmp(&(a.weight, a.waited, b.row)));
    out
}

fn zone_sites(capture: &Capture) -> Vec<ProfileZoneSite> {
    let mut sites: Vec<ProfileZoneSite> = Vec::new();
    for zone in &capture.zones {
        let site = capture.site(zone.site.get());
        let name = capture.string(site.name.get());
        let label = site.kind == microscope_probes::abi::SITE_LABEL;
        let position = sites
            .iter()
            .position(|known| known.name == name && known.label == label)
            .unwrap_or_else(|| {
                sites.push(ProfileZoneSite {
                    name: name.to_owned(),
                    label,
                    count: 0,
                    total: 0,
                    longest: 0,
                    flagged: 0,
                });
                sites.len() - 1
            });

        let length = u64::try_from(zone.end.get() - zone.start.get()).unwrap_or(0);
        let Some(entry) = sites.get_mut(position) else {
            continue;
        };
        entry.count += 1;
        entry.total += length;
        entry.longest = entry.longest.max(length);
        entry.flagged += u64::from(zone.flags.get() != 0);
    }

    sites.sort_by(|a, b| (b.total, &a.name).cmp(&(a.total, &b.name)));
    sites
}

pub(super) fn tree(
    view: &CaptureView,
    query: &ProfileQuery,
    base_lookup: Option<BaseLookup<'_>>,
) -> Result<(Value, Vec<u8>), ViewError> {
    let gran = granularity(query.granularity);
    let keys = Keys::new(&view.capture, gran);
    let tree = keys.aggregate(&resolve_view(view, &keys, query)?);
    let base = resolve_base_view(view, query, base_lookup)?;
    let base_keys = base
        .as_ref()
        .map(|(base, _)| Keys::new(&base.capture, gran));
    let mut bytes = Vec::new();
    let (nodes, base_weight, keys) =
        if let (Some((base, of_base)), Some(base_keys)) = (&base, &base_keys) {
            let base_tree = base_keys.aggregate(&resolve_base(&base.capture, base_keys, of_base)?);
            let diff = {
                let _span = crate::span!("web diff tree");
                diff_trees((&keys, &tree), (base_keys, &base_tree))
            };
            let mut named = Named::with_base(&keys, base_keys);
            let columns = [&diff.own, &diff.total, &diff.base_own, &diff.base_total];

            for weights in &columns[..2] {
                put_weights(&mut bytes, weights);
            }

            for (key, side) in diff.key.iter().zip(&diff.side) {
                bytes.extend_from_slice(&named.index_on(*side, *key).to_le_bytes());
            }

            for depth in &diff.depth {
                bytes.extend_from_slice(&depth.to_le_bytes());
            }

            for weights in &columns[2..] {
                put_weights(&mut bytes, weights);
            }

            let weight = ProfileBaseWeight {
                weight: base_tree.weight,
                samples: base_tree.samples,
            };

            (diff.len(), Some(weight), named.keys)
        } else {
            let mut named = Named::new(&keys);
            put_weights(&mut bytes, &tree.own);
            put_weights(&mut bytes, &tree.total);
            for key in &tree.key {
                bytes.extend_from_slice(&named.index(*key).to_le_bytes());
            }

            for depth in &tree.depth {
                bytes.extend_from_slice(&depth.to_le_bytes());
            }

            (tree.len(), None, named.keys)
        };

    let answer = ProfileTree {
        nodes: u32::try_from(nodes).unwrap_or(u32::MAX),
        weight: tree.weight,
        samples: tree.samples,
        keys,
        base: base_weight,
    };

    Ok((to_value(&answer), bytes))
}

pub(super) fn functions(
    view: &CaptureView,
    query: &ProfileQuery,
    base_lookup: Option<BaseLookup<'_>>,
) -> Result<ProfileFunctions, ViewError> {
    let gran = granularity(query.granularity);
    let keys = Keys::new(&view.capture, gran);
    let costs = keys.functions(&resolve_view(view, &keys, query)?);
    let Some((base, of_base)) = resolve_base_view(view, query, base_lookup)? else {
        let mut named = Named::new(&keys);
        let rows = costs
            .rows
            .iter()
            .map(|cost| ProfileCost {
                key: named.index(cost.key),
                own: cost.own,
                total: cost.total,
                base_own: 0,
                base_total: 0,
            })
            .collect();
        return Ok(ProfileFunctions {
            weight: costs.weight,
            rows,
            keys: named.keys,
            base: None,
        });
    };

    let base_keys = Keys::new(&base.capture, gran);
    let base_costs = base_keys.functions(&resolve_base(&base.capture, &base_keys, &of_base)?);
    let diff_span = crate::span!("web diff functions");
    let mut diff = diff_costs((&keys, &costs), (&base_keys, &base_costs));

    let share = |weight: u64, of: u64| {
        if of == 0 {
            0.0
        } else {
            exact(weight) / exact(of)
        }
    };

    let change = |cost: &DiffCost| {
        (share(cost.own, costs.weight) - share(cost.base_own, base_costs.weight)).abs()
    };

    diff.sort_by(|a, b| change(b).total_cmp(&change(a)));
    drop(diff_span);
    let mut named = Named::with_base(&keys, &base_keys);
    let rows = diff
        .iter()
        .map(|cost| ProfileCost {
            key: named.index_on(cost.side, cost.key),
            own: cost.own,
            total: cost.total,
            base_own: cost.base_own,
            base_total: cost.base_total,
        })
        .collect();

    Ok(ProfileFunctions {
        weight: costs.weight,
        rows,
        keys: named.keys,
        base: Some(ProfileBaseWeight {
            weight: base_costs.weight,
            samples: 0,
        }),
    })
}

pub(super) fn timeline(
    view: &CaptureView,
    timeline: &ProfileTimelineQuery,
) -> Result<ProfileTimeline, ViewError> {
    let capture = &view.capture;
    let keys = Keys::new(capture, granularity(timeline.query.granularity));
    let mut resolved = resolve_view(view, &keys, &timeline.query)?;
    let (start, end) = capture.span();
    let range = resolved
        .range
        .unwrap_or_else(|| (start, end.saturating_add(1)));
    resolved.range = Some(range);
    let buckets = timeline.buckets.clamp(1, MOST_BUCKETS) as usize;
    let by_task = timeline.task_lanes || timeline.query.source == ProfileSource::Waits;
    let of = if by_task {
        Lanes::Tasks
    } else {
        Lanes::Threads
    };

    let lane = |id: u32, weights| ProfileLane {
        thread: (!by_task).then_some(id),
        task: (by_task && id != NONE).then_some(id),
        weights,
        blocked: Vec::new(),
        preempted: Vec::new(),
        syscalls: Vec::new(),
        failed: Vec::new(),
        waits: Vec::new(),
    };

    let mut lanes: BTreeMap<u32, ProfileLane> = keys
        .density(&resolved, buckets, of)
        .into_iter()
        .map(|(id, weights)| (id, lane(id, weights)))
        .collect();

    if !by_task {
        for off in off_cpu(capture, &resolved, buckets) {
            let lane = lanes
                .entry(off.thread)
                .or_insert_with(|| lane(off.thread, Vec::new()));
            lane.blocked = off.blocked;
            lane.preempted = off.preempted;
        }

        for kernel in kernel_lanes(capture, &keys, &resolved, buckets) {
            let lane = lanes
                .entry(kernel.thread)
                .or_insert_with(|| lane(kernel.thread, Vec::new()));
            lane.syscalls = kernel.calls;
            lane.failed = kernel.failed;
            lane.waits = kernel
                .long
                .into_iter()
                .map(|(row, from, to)| ProfileWait {
                    call: syscalls::syscall_name(capture, row),
                    start: from - start,
                    end: to - start,
                })
                .collect();
        }
    }

    Ok(ProfileTimeline {
        range: [range.0 - start, range.1 - start],
        lanes: lanes.into_values().collect(),
    })
}

pub(super) fn lines(
    view: &CaptureView,
    query: &ProfileFunctionQuery,
    source_lines: Option<SourceLines<'_>>,
) -> Result<ProfileLines, ViewError> {
    let mut plain = query.query.clone();
    plain.transforms.clear();
    plain.base = None;
    plain.syscall_leaves = false;
    let keys = Keys::new(&view.capture, granularity(plain.granularity));
    let selected = resolve_view(view, &keys, &plain)?;

    let capture = &view.capture;
    let line_keys = Keys::new(capture, Granularity::Line);
    let costs = line_keys.functions(&Query {
        transforms: Vec::new(),
        ..selected
    });

    let mut by_file: BTreeMap<Option<String>, Vec<[u64; 3]>> = BTreeMap::new();
    for cost in costs.rows {
        if let KeyKind::Line {
            function,
            file,
            line,
        } = line_keys.kind(cost.key)
            && capture
                .functions
                .get(function as usize)
                .is_some_and(|row| capture.string(row.name.get()) == query.function)
        {
            let path = (file != NONE).then(|| capture.string(file).to_owned());
            by_file
                .entry(path)
                .or_default()
                .push([u64::from(line), cost.own, cost.total]);
        }
    }

    let files = by_file
        .into_iter()
        .map(|(path, mut lines)| {
            lines.sort_by_key(|line| line[0]);
            let lowest = lines.iter().map(|line| line[0]).min().unwrap_or(1);
            let highest = lines.iter().map(|line| line[0]).max().unwrap_or(1);
            let first = lowest.saturating_sub(SOURCE_MARGIN).max(1);
            let read = match (path.as_deref(), source_lines) {
                (None, _) => {
                    Err("the debug information has no source file for these lines".to_owned())
                }
                (Some(path), Some(reader)) => reader(path, first, highest + SOURCE_MARGIN),
                (Some(_), None) => Err("source files are not available on this server".to_owned()),
            };
            let (text, unread) = match read {
                Ok(text) => (Some(text), None),
                Err(why) => (None, Some(why)),
            };
            ProfileSourceFile {
                path,
                lines,
                first: u32::try_from(first).unwrap_or(u32::MAX),
                text,
                unread,
            }
        })
        .collect();

    Ok(ProfileLines {
        weight: costs.weight,
        files,
    })
}

fn absolute(capture: &Capture, range: Option<[i64; 2]>) -> (i64, i64) {
    let (start, end) = capture.span();
    range.map_or_else(
        || (start, end.saturating_add(1)),
        |[from, to]| (start.saturating_add(from), start.saturating_add(to)),
    )
}

fn zone_extent(capture: &Capture) -> Option<(i64, i64)> {
    let marks = capture
        .markers
        .iter()
        .filter(|marker| marker.site.get() != NONE)
        .map(|marker| (marker.time.get(), marker.time.get()));
    capture
        .zones
        .iter()
        .map(|zone| (zone.start.get(), zone.end.get()))
        .chain(marks)
        .reduce(|(start, end), (from, to)| (start.min(from), end.max(to)))
}

fn site_name(capture: &Capture, site: u32) -> String {
    capture
        .sites
        .get(site as usize)
        .map(|site| capture.string(site.name.get()).to_owned())
        .unwrap_or_default()
}

fn zone_name(capture: &Capture, site: u32, value: u32) -> String {
    match (site, value) {
        (NONE, _) => "zones".to_owned(),
        (site, NONE) => site_name(capture, site),
        (site, value) => format!("{}={}", site_name(capture, site), capture.string(value)),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "packs zone columns, markers, and plot series into one response"
)]
pub(super) fn zones(capture: &Capture, query: &ProfileZonesQuery) -> (Value, Vec<u8>) {
    let (start, _) = capture.span();
    let range = match (query.range, zone_extent(capture)) {
        (None, Some((first, last))) => {
            let margin = ((last - first) / 50).max(1);
            (first - margin, last + margin)
        }
        (range, _) => absolute(capture, range),
    };

    let fold = (range.1 - range.0) * 3 / i64::from(query.width.max(1));
    let band = query.band.map(Into::into);
    let sites = query
        .site
        .as_deref()
        .map(|site| sites_named(capture, site))
        .unwrap_or_default();
    let chosen = query.site.as_deref().map(|_| ChosenZones {
        sites: &sites,
        band,
    });
    let blocks = zone_blocks(
        capture,
        range,
        query.threads.as_deref(),
        fold.max(1),
        chosen,
    );
    let mut names: Vec<String> = Vec::new();
    let mut indices: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    let mut name = |site: u32, value: u32| {
        *indices.entry((site, value)).or_insert_with(|| {
            names.push(zone_name(capture, site, value));
            u32::try_from(names.len() - 1).unwrap_or(u32::MAX)
        })
    };

    let mut bytes = Vec::with_capacity(blocks.len() * 64);
    let times: [&dyn Fn(&ZoneBlock) -> i64; 5] = [
        &|block| block.start - start,
        &|block| block.end - start,
        &|block| block.covered,
        &|block| block.blocked,
        &|block| block.preempted,
    ];
    for column in times {
        for block in &blocks {
            bytes.extend_from_slice(&exact_time(column(block)).to_le_bytes());
        }
    }

    let columns: [&dyn Fn(&ZoneBlock) -> u32; 2] = [&|block| block.thread, &|block| block.depth];
    for column in columns {
        for block in &blocks {
            bytes.extend_from_slice(&column(block).to_le_bytes());
        }
    }

    for block in &blocks {
        bytes.extend_from_slice(&name(block.site, block.value).to_le_bytes());
    }

    let columns: [&dyn Fn(&ZoneBlock) -> u32; 3] =
        [&|block| block.count, &|block| block.flags, &|block| {
            u32::from(block.chosen)
        }];
    for column in columns {
        for block in &blocks {
            bytes.extend_from_slice(&column(block).to_le_bytes());
        }
    }

    let marks = capture
        .markers
        .iter()
        .filter_map(|marker| {
            let kind = MarkerKind::from_number(marker.kind)?;
            if !matches!(
                kind,
                MarkerKind::Frame | MarkerKind::Message | MarkerKind::Trigger
            ) {
                return None;
            }
            let time = marker.time.get();
            if !(range.0 <= time && time < range.1) {
                return None;
            }
            let thread = capture
                .threads
                .get(marker.thread.get() as usize)
                .map(|_| marker.thread.get())?;
            Some(ProfileMark {
                thread,
                time: time - start,
                kind: match kind {
                    MarkerKind::Frame => ProfileMarkKind::Frame,
                    MarkerKind::Trigger => ProfileMarkKind::Trigger,
                    _ => ProfileMarkKind::Message,
                },
                name: site_name(capture, marker.site.get()),
                text: (marker.text.get() != NONE)
                    .then(|| capture.string(marker.text.get()).to_owned()),
            })
        })
        .take(MOST_MARKS)
        .collect();

    let plots = plot_series(
        capture,
        range,
        query.threads.as_deref(),
        query.width.max(1) as usize,
    )
    .into_iter()
    .map(|series| ProfilePlot {
        name: capture.string(series.name).to_owned(),
        values: series.values,
        unshown: series.unshown,
        bins: series
            .bins
            .iter()
            .map(|bin| {
                let time = exact_time(bin.time - start);
                [time, bin.first, bin.last, bin.low, bin.high]
            })
            .collect(),
        held: series.held,
        continues: series.continues,
    })
    .collect();

    let answer = ProfileZones {
        range: [range.0 - start, range.1 - start],
        blocks: u32::try_from(blocks.len()).unwrap_or(u32::MAX),
        off_cpu: capture.setting("off-CPU") == Some("on"),
        names,
        marks,
        plots,
    };

    (to_value(&answer), bytes)
}

pub(super) fn zone_stats(capture: &Capture, query: &ProfileZoneStatsQuery) -> ProfileZoneStats {
    let (start, _) = capture.span();
    let range = absolute(capture, query.range);
    let sites = sites_named(capture, &query.name);
    let stats = analyze_zone_stats(
        capture,
        &sites,
        (range, query.threads.as_deref()),
        query.band.map(Into::into),
        query.rank_of,
    );
    let value = |value: u32| (value != NONE).then(|| capture.string(value).to_owned());

    ProfileZoneStats {
        count: stats.count,
        total: stats.total,
        flagged: stats.flagged,
        min: stats.min,
        median: stats.median,
        p90: stats.p90,
        p99: stats.p99,
        max: stats.max,
        histogram: stats
            .histogram
            .iter()
            .map(|&bucket| bucket.into())
            .collect(),
        longest: stats
            .longest
            .iter()
            .map(|zone| ProfileZoneAt {
                thread: zone.thread,
                start: zone.start - start,
                end: zone.end - start,
                value: value(zone.value),
            })
            .collect(),
        banded: stats.banded.into(),
        shorter: stats.shorter,
        values: stats
            .values
            .iter()
            .map(|&(text, total, count)| ProfileLabelValue {
                value: value(text).unwrap_or_default(),
                total,
                count,
            })
            .collect(),
    }
}

struct Named<'a, 'k> {
    keys_of: &'a Keys<'k>,
    base_of: Option<&'a Keys<'k>>,
    indices: BTreeMap<(bool, u32), u32>,
    keys: Vec<ProfileKey>,
}

impl<'a, 'k> Named<'a, 'k> {
    const fn new(keys_of: &'a Keys<'k>) -> Self {
        Self {
            keys_of,
            base_of: None,
            indices: BTreeMap::new(),
            keys: Vec::new(),
        }
    }

    const fn with_base(keys_of: &'a Keys<'k>, base_of: &'a Keys<'k>) -> Self {
        Self {
            keys_of,
            base_of: Some(base_of),
            indices: BTreeMap::new(),
            keys: Vec::new(),
        }
    }

    fn index(&mut self, key: u32) -> u32 {
        self.index_on(Side::Current, key)
    }

    fn index_on(&mut self, side: Side, key: u32) -> u32 {
        let from_base = side == Side::Base;
        if let Some(index) = self.indices.get(&(from_base, key)) {
            return *index;
        }

        let keys = match (side, self.base_of) {
            (Side::Base, Some(base)) => base,
            _ => self.keys_of,
        };

        let index = u32::try_from(self.keys.len()).unwrap_or(u32::MAX);
        let location = keys.location(key);
        self.keys.push(ProfileKey {
            name: keys.exact_name(key),
            kind: match keys.kind(key) {
                KeyKind::Function { .. } => ProfileKeyKind::Function,
                KeyKind::Line { .. } => ProfileKeyKind::Line,
                KeyKind::Address { .. } => ProfileKeyKind::Address,
                KeyKind::Truncation { .. } => ProfileKeyKind::Truncation,
                KeyKind::Task { .. } => ProfileKeyKind::Task,
                KeyKind::Group { .. } => ProfileKeyKind::Group,
                KeyKind::Label { .. } => ProfileKeyKind::Label,
                KeyKind::Syscall { .. } => ProfileKeyKind::Syscall,
            },
            module: keys.module(key).map(str::to_owned),
            file: location.map(|(file, _)| file.to_owned()),
            line: location.map(|(_, line)| line),
        });
        self.indices.insert((from_base, key), index);
        index
    }
}

fn put_weights(bytes: &mut Vec<u8>, weights: &[u64]) {
    for weight in weights {
        bytes.extend_from_slice(&exact(*weight).to_le_bytes());
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "no view can show a part in 2^53"
)]
const fn exact(weight: u64) -> f64 {
    weight as f64
}

#[expect(
    clippy::cast_precision_loss,
    reason = "no view can show a part in 2^53"
)]
const fn exact_time(time: i64) -> f64 {
    time as f64
}

pub(super) fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|error| json!({ "error": error.to_string() }))
}
