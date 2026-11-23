use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::capture::Capture;
use crate::syscalls::fds::{FdTable, Kind as FdKind};
use crate::syscalls::filter;
use crate::syscalls::present::{self, Context, Tokens};
use crate::syscalls::specs::{self, Class};
use crate::syscalls::stats::{self, SyscallStats, bytes_moved};
use crate::syscalls::{Outcome, Pid, SyscallEvent, TraceEvent};

use super::super::protocol::{
    SyscallArgument, SyscallCall, SyscallCallQuery, SyscallCaller, SyscallDetail,
    SyscallDetailQuery, SyscallEndpoint, SyscallFile, SyscallFiles, SyscallGroup, SyscallHeatmap,
    SyscallHeatmapQuery, SyscallLine, SyscallLog, SyscallLogQuery, SyscallNetwork, SyscallProcess,
    SyscallProcesses, SyscallQuery, SyscallRow, SyscallSite, SyscallSummary, SyscallSummaryQuery,
};
use super::super::{SourceLines, ViewError};
use super::{
    CLASSES, Grouping, MOST_BUCKETS, MOST_LINES, SITE_MARGIN, Seen, Selection, Trace, calls,
    counts, end_text, is_connect, known, named_path, row, site, sites, tokens, walk,
};

pub(in crate::view) fn summary(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallSummaryQuery,
) -> Result<SyscallSummary, ViewError> {
    let selection = Selection::new(capture, trace, &query.query)?;
    let mut grouping = Grouping::new(capture, trace, query.group);
    let mut groups: BTreeMap<String, (String, SyscallStats)> = BTreeMap::new();
    let mut processes = BTreeSet::new();
    let mut add = |key: String, name: String, stats: &SyscallStats| {
        groups
            .entry(key)
            .or_insert_with(|| (name, SyscallStats::default()))
            .1
            .merge(stats);
    };

    known(trace, &selection, |index, call, fds| {
        if call.flags.kept() {
            return;
        }

        processes.insert(call.process);
        let (key, name) = grouping.row(index, call, fds);
        let mut one = SyscallStats::default();
        one.add(call.outcome, call.duration(), bytes_moved(call));
        add(key, name, &one);
    });

    let counted = !trace.counted.by_call.is_empty();
    for (&(pid, tid, id), stats) in &trace.counted.by_call {
        if !selection.filter.calls.contains(id) {
            continue;
        }

        processes.insert(pid);
        let name = id.display_name().into_owned();
        let (key, name) = match query.group {
            SyscallGroup::Thread if tid.get() != 0 => (tid.to_string(), tid.to_string()),
            SyscallGroup::Syscall
            | SyscallGroup::Thread
            | SyscallGroup::Path
            | SyscallGroup::Site => (name.clone(), name),
            SyscallGroup::Class => {
                let class = specs::spec(id)
                    .and_then(|spec| {
                        CLASSES
                            .into_iter()
                            .find(|&class| spec.classes.contains(class))
                    })
                    .map_or("other", Class::name);
                (format!("%{class}"), class.to_owned())
            }
            SyscallGroup::Process => (pid.to_string(), pid.to_string()),
        };

        add(key, name, stats);
    }

    let mut rows: Vec<SyscallRow> = groups
        .into_iter()
        .map(|(key, (name, stats))| row(key, name, &stats))
        .collect();
    rows.sort_by(|a, b| {
        (b.total_ns, b.calls)
            .cmp(&(a.total_ns, a.calls))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(SyscallSummary {
        total: trace.calls
            + trace
                .counted
                .by_call
                .values()
                .map(|stats| stats.calls)
                .sum::<u64>(),
        calls: rows.iter().map(|row| row.calls).sum(),
        errors: rows.iter().map(|row| row.errors).sum(),
        lost: trace.lost.into(),
        counted,
        engine: trace.engine.clone(),
        processes: u32::try_from(processes.len()).unwrap_or(u32::MAX),
        rows,
    })
}

struct Slices {
    start: i64,
    width: i64,
    count: usize,
}

impl Slices {
    fn new(span: (i64, i64), buckets: u32) -> Self {
        let count = buckets.clamp(1, MOST_BUCKETS);
        let length = (i128::from(span.1) - i128::from(span.0)).max(1);
        let width_128 = (length + i128::from(count) - 1) / i128::from(count);
        let width = i64::try_from(width_128).unwrap_or(i64::MAX).max(1);

        Self {
            start: span.0,
            width,
            count: count as usize,
        }
    }

    fn of(&self, time: u64) -> Option<usize> {
        let offset = i128::from(time.cast_signed()) - i128::from(self.start);
        if offset < 0 {
            return None;
        }
        usize::try_from(offset / i128::from(self.width))
            .ok()
            .filter(|&slice| slice < self.count)
    }
}

pub(in crate::view) fn heatmap(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallHeatmapQuery,
) -> Result<SyscallHeatmap, ViewError> {
    let selection = Selection::new(capture, trace, &query.query)?;
    let slices = Slices::new(selection.span(trace), query.buckets);
    let rows = stats::BUCKETS;
    let mut counts = vec![0_u32; slices.count * rows];
    let mut failed = vec![0_u32; slices.count * rows];
    calls(trace, &selection, |_, call, _| {
        if let (Some(duration), Some(slice)) = (call.duration(), slices.of(call.time())) {
            let cell = slice * rows + stats::bucket(duration);
            if let Some(slot) = counts.get_mut(cell) {
                *slot += 1;
            }
            if let Some(slot) = failed.get_mut(cell) {
                *slot += u32::from(matches!(call.outcome, Outcome::Failed(_)));
            }
        }
    });

    Ok(SyscallHeatmap {
        start: slices.start.saturating_sub(trace.start),
        width: slices.width,
        rows: u32::try_from(rows).unwrap_or(u32::MAX),
        most: counts.iter().copied().max().unwrap_or(0),
        counts,
        failed,
    })
}

pub(in crate::view) fn detail(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallDetailQuery,
) -> Result<SyscallDetail, ViewError> {
    let named = |call: &SyscallEvent| call.call.display_name() == query.call;
    let mut whole = vec![0; stats::BUCKETS];
    for event in &trace.events {
        if let TraceEvent::Call(call) = &event.event
            && named(call)
            && let Some(duration) = call.duration()
            && let Some(slot) = whole.get_mut(stats::bucket(duration))
        {
            *slot += 1;
        }
    }

    let selection = Selection::new(capture, trace, &query.query)?;
    let mut selected = SyscallStats::default();
    let mut chosen = Vec::new();
    calls(trace, &selection, |index, call, _| {
        if named(call) {
            selected.add(call.outcome, call.duration(), bytes_moved(call));
            chosen.push(index);
        }
    });

    let slow_ns = selected.quantile_ns(0.9);
    let mut site_cache: HashMap<u32, String> = HashMap::new();
    let mut callers: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut unplaced = 0;

    for index in chosen {
        let Some(event) = trace.events.get(index) else {
            continue;
        };
        let TraceEvent::Call(call) = &event.event else {
            continue;
        };

        let slow = slow_ns.is_some_and(|floor| call.duration().is_some_and(|took| took >= floor));
        let failed = matches!(call.outcome, Outcome::Failed(_));
        if !slow && !failed {
            continue;
        }

        let Some(stack) = event.stack else {
            unplaced += 1;
            continue;
        };

        let name = site(capture, &mut site_cache, stack);
        let counts = callers.entry(name.clone()).or_default();
        counts.0 += u64::from(slow);
        counts.1 += u64::from(failed);
    }

    let mut callers: Vec<SyscallCaller> = callers
        .into_iter()
        .map(|(site, (slow, failed))| SyscallCaller { site, slow, failed })
        .collect();

    callers.sort_by(|a, b| {
        (b.slow + b.failed)
            .cmp(&(a.slow + a.failed))
            .then_with(|| a.site.cmp(&b.site))
    });

    let mut histogram = selected.log2_ns.clone();
    histogram.resize(stats::BUCKETS, 0);

    Ok(SyscallDetail {
        whole,
        selected: histogram,
        errnos: counts(&selected.errors_by_errno),
        slow_ns,
        callers,
        unplaced,
    })
}

fn is_loggable_other(event: &TraceEvent) -> bool {
    match event {
        TraceEvent::Signal { info, .. } => {
            if info.as_deref().is_some_and(|info| info.len() >= 48) {
                present::other(event).is_some()
            } else {
                true
            }
        }
        TraceEvent::Exit { .. } | TraceEvent::ThreadExit { .. } | TraceEvent::Lost { .. } => true,
        TraceEvent::Call(_)
        | TraceEvent::Spawn { .. }
        | TraceEvent::Exec { .. }
        | TraceEvent::Descriptor { .. } => false,
    }
}

pub(in crate::view) fn log(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallLogQuery,
) -> Result<SyscallLog, ViewError> {
    let selection = Selection::new(capture, trace, &query.query)?;
    let first = query.offset;
    let last = first.saturating_add(u64::from(query.limit.min(MOST_LINES)));
    let mut total = 0_u64;
    let mut lines = Vec::new();
    let mut entry = Tokens::default();
    walk(trace, &selection, |seen| match seen {
        Seen::Before(_, call, fds) => {
            if (first..last).contains(&total) {
                entry = present::entry(
                    call,
                    Context {
                        fds: Some(fds),
                        before: true,
                    },
                );
            }
        }
        Seen::After(index, call, fds) => {
            if (first..last).contains(&total) {
                let mut line = std::mem::take(&mut entry);
                line.0.extend(
                    present::returned(
                        call,
                        Context {
                            fds: Some(fds),
                            before: false,
                        },
                    )
                    .0,
                );
                lines.push(SyscallLine {
                    event: index as u64,
                    time: trace.relative(call.time()),
                    tid: call.thread.get(),
                    pid: call.process.get(),
                    tokens: tokens(line),
                    duration_ns: call.duration(),
                    failed: matches!(call.outcome, Outcome::Failed(_)),
                    call: true,
                });
            }

            total += 1;
        }
        Seen::Other(index, event) => {
            if (first..last).contains(&total) {
                let Some(line) = present::other(event) else {
                    return;
                };
                let (tid, pid) = match *event {
                    TraceEvent::Signal {
                        thread, process, ..
                    }
                    | TraceEvent::ThreadExit {
                        thread, process, ..
                    } => (thread.get(), process.get()),
                    TraceEvent::Exit { process, .. } => (process.leader().get(), process.get()),
                    _ => (0, 0),
                };

                lines.push(SyscallLine {
                    event: index as u64,
                    time: trace.relative(event.time()),
                    tid,
                    pid,
                    tokens: tokens(line),
                    duration_ns: None,
                    failed: false,
                    call: false,
                });
                total += 1;
            } else if is_loggable_other(event) {
                total += 1;
            }
        }
    });

    Ok(SyscallLog { total, lines })
}

pub(in crate::view) fn call(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallCallQuery,
    source_lines: Option<SourceLines<'_>>,
) -> Result<SyscallCall, ViewError> {
    let index = usize::try_from(query.event).unwrap_or(usize::MAX);
    let missing = || ViewError::Invalid(format!("no call {}", query.event));
    let event = trace.events.get(index).ok_or_else(missing)?;
    let TraceEvent::Call(target) = &event.event else {
        return Err(missing());
    };

    let mut fds = FdTable::default();
    for event in trace.events.iter().take(index) {
        match &event.event {
            TraceEvent::Call(call) => fds.apply_call(call),
            other => fds.apply(other),
        }
    }

    let before = Context {
        fds: Some(&fds),
        before: true,
    };

    let arguments = present::argument_values(target, before)
        .into_iter()
        .map(|(name, value)| SyscallArgument {
            name,
            tokens: tokens(value),
        })
        .collect();

    let mut after = fds.clone();
    after.apply_call(target);
    let result = present::returned(
        target,
        Context {
            fds: Some(&after),
            before: false,
        },
    );
    let (stack, stack_ends) = event
        .stack
        .map_or((Vec::new(), None), |row| sites::stack_names(capture, row));

    Ok(SyscallCall {
        arguments,
        dumps: present::dumps::dump(target),
        stack,
        stack_ends,
        entered: target.entered.map(|time| trace.relative(time)),
        exited: target.exited.map(|time| trace.relative(time)),
        result: tokens(result),
        site: event
            .stack
            .and_then(|row| sites::syscall_place(capture, row))
            .map(|place| {
                let line = u64::from(place.line);
                let first = line.saturating_sub(SITE_MARGIN).max(1);
                let (text, unread) = source_lines.map_or_else(
                    || {
                        (
                            None,
                            Some("source files are not available on this server".to_owned()),
                        )
                    },
                    |reader| match reader(place.file, first, line + SITE_MARGIN) {
                        Ok(text) => (Some(text), None),
                        Err(why) => (None, Some(why)),
                    },
                );

                SyscallSite {
                    function: place.function.to_owned(),
                    path: place.file.to_owned(),
                    line: place.line,
                    first: u32::try_from(first).unwrap_or(u32::MAX),
                    text,
                    unread,
                }
            }),
    })
}

fn opens(call: &SyscallEvent) -> bool {
    call.call
        .name()
        .is_some_and(|name| matches!(name, "open" | "openat" | "openat2" | "creat"))
}

pub(in crate::view) fn files(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallQuery,
) -> Result<SyscallFiles, ViewError> {
    let selection = Selection::new(capture, trace, query)?;
    let mut files: BTreeMap<String, SyscallFile> = BTreeMap::new();
    calls(trace, &selection, |_, call, fds| {
        if opens(call)
            && let Some(path) = named_path(call, fds)
        {
            let entry = files.entry(path.clone()).or_insert_with(|| SyscallFile {
                path,
                ..SyscallFile::default()
            });
            entry.opens += 1;
            entry.failed += u64::from(matches!(call.outcome, Outcome::Failed(_)));
            entry.total_ns += call.duration().unwrap_or(0);
            return;
        }

        let Some(fd) = filter::descriptors(call).next() else {
            return;
        };

        let Some(description) = fds.describe(call.process, fd) else {
            return;
        };

        if !matches!(description.kind, FdKind::File) {
            return;
        }

        let entry = files
            .entry(description.text.to_string())
            .or_insert_with(|| SyscallFile {
                path: description.text.to_string(),
                ..SyscallFile::default()
            });
        let took = call.duration().unwrap_or(0);
        entry.total_ns += took;
        if call
            .call
            .name()
            .is_some_and(|name| matches!(name, "fsync" | "fdatasync"))
        {
            entry.sync_ns += took;
        }

        match present::dumps::moves(call) {
            Some((_, present::dumps::Direction::Read)) => entry.read += bytes_moved(call),
            Some((_, present::dumps::Direction::Write)) => entry.written += bytes_moved(call),
            None => {}
        }
    });

    let mut files: Vec<SyscallFile> = files.into_values().collect();
    files.sort_by(|a, b| {
        b.total_ns
            .cmp(&a.total_ns)
            .then_with(|| a.path.cmp(&b.path))
    });

    Ok(SyscallFiles { files })
}

pub(in crate::view) fn network(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallQuery,
) -> Result<SyscallNetwork, ViewError> {
    let selection = Selection::new(capture, trace, query)?;
    let mut endpoints: BTreeMap<String, SyscallEndpoint> = BTreeMap::new();
    known(trace, &selection, |_, call, fds| {
        let connect = is_connect(call);

        let Some(fd) = filter::descriptors(call).next() else {
            return;
        };

        let Some(description) = fds.describe(call.process, fd) else {
            return;
        };

        if !matches!(description.kind, FdKind::Socket(_)) {
            return;
        }

        let endpoint = endpoints
            .entry(description.text.to_string())
            .or_insert_with(|| SyscallEndpoint {
                endpoint: description.text.to_string(),
                ..SyscallEndpoint::default()
            });
        if connect {
            endpoint.connects += 1;
            if matches!(call.outcome, Outcome::Failed(errno) if present::errno(errno).0 != "EINPROGRESS")
            {
                endpoint.failed += 1;
            }
            endpoint.connect_ns += call.duration().unwrap_or(0);
        }

        match present::dumps::moves(call) {
            Some((_, present::dumps::Direction::Read)) => endpoint.received += bytes_moved(call),
            Some((_, present::dumps::Direction::Write)) => endpoint.sent += bytes_moved(call),
            None => {}
        }
    });

    let mut endpoints: Vec<SyscallEndpoint> = endpoints.into_values().collect();
    endpoints.sort_by(|a, b| {
        (b.sent + b.received)
            .cmp(&(a.sent + a.received))
            .then_with(|| a.endpoint.cmp(&b.endpoint))
    });

    Ok(SyscallNetwork { endpoints })
}

pub(in crate::view) fn processes(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallQuery,
) -> Result<SyscallProcesses, ViewError> {
    let selection = Selection::new(capture, trace, query)?;
    let mut counts: BTreeMap<Pid, (u64, u64)> = BTreeMap::new();

    calls(trace, &selection, |_, call, _| {
        let entry = counts.entry(call.process).or_default();
        entry.0 += 1;
        entry.1 += u64::from(matches!(call.outcome, Outcome::Failed(_)));
    });

    let mut ordered: Vec<SyscallProcess> = Vec::new();
    let mut pending: Vec<Pid> = trace
        .processes
        .iter()
        .filter(|process| {
            process
                .parent
                .is_none_or(|parent| trace.processes.get(parent).is_none())
        })
        .map(|process| process.pid)
        .collect();
    pending.reverse();

    while let Some(pid) = pending.pop() {
        let Some(process) = trace.processes.get(pid) else {
            continue;
        };

        let (calls, errors) = counts.get(&pid).copied().unwrap_or_default();
        ordered.push(SyscallProcess {
            pid: pid.get(),
            parent: process.parent.map(Pid::get),
            images: process
                .images
                .iter()
                .map(|(_, image)| image.to_string())
                .collect(),
            threads: u32::try_from(process.threads.len()).unwrap_or(u32::MAX),
            calls,
            errors,
            started: trace.relative(process.started),
            ended: process.ended.map(|(time, _)| trace.relative(time)),
            end: process.ended.map(|(_, end)| end_text(end)),
        });

        let mut children: Vec<Pid> = trace
            .processes
            .iter()
            .filter(|child| child.parent == Some(pid))
            .map(|child| child.pid)
            .collect();
        children.reverse();
        pending.extend(children);
    }

    Ok(SyscallProcesses { processes: ordered })
}
