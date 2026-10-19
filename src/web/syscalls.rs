//! A profile's system call trace, as the Syscalls view asks about it.
//!
//! Each answer is one pass over the calls a query selects, with the
//! descriptor table as each call found it, so that paths and filters agree
//! with the CLI's; lines are formatted by `present` as the CLI prints them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use microscope::profiler::{Profile, RecordedEvent, StackEnding};
use microscope::syscalls::fds::{FdTable, Kind as FdKind};
use microscope::syscalls::filter::{self, Filter};
use microscope::syscalls::procs::ProcessTree;
use microscope::syscalls::specs::{self, Class};
use microscope::syscalls::stats::{self, Folded, SyscallStats, bytes_moved};
use microscope::syscalls::{
    Errno, EventFlags, InFlight, Outcome, Pid, ProcessEnd, SyscallEvent, Tid, TraceEvent,
};
use serde_json::{Value, json};

use super::protocol::{
    ErrorKind, Request, SyscallArgument, SyscallBlocked, SyscallCall, SyscallCallQuery,
    SyscallCaller, SyscallCount, SyscallDetail, SyscallDetailQuery, SyscallEndpoint, SyscallFile,
    SyscallFiles, SyscallGroup, SyscallHeatmap, SyscallHeatmapQuery, SyscallInFlight, SyscallLane,
    SyscallLanes, SyscallLanesQuery, SyscallLine, SyscallLog, SyscallLogQuery, SyscallNetwork,
    SyscallProcess, SyscallProcesses, SyscallQuery, SyscallRate, SyscallRateBy, SyscallRateQuery,
    SyscallRow, SyscallSeries, SyscallSite, SyscallSummary, SyscallSummaryQuery, SyscallToken,
};

use super::session::Failure;
use crate::present::syscalls::{self as present, Context, FdStyle, Style, Tokens};

/// The working directory, as a `dirfd`.
const AT_FDCWD: i64 = -100;

/// The most slices a chart is cut into.
const MOST_BUCKETS: u32 = 4096;

/// The most lines a page of the log holds.
const MOST_LINES: u32 = 2000;

/// Lines shown on each side of a call's source line.
const SITE_MARGIN: u64 = 3;

/// The most series a rate chart stacks; the rest are `other`.
const MOST_SERIES: usize = 8;

/// The classes a call is shown under, first match first: what it does
/// rather than how it names its target.
const CLASSES: [Class; 8] = [
    Class::Net,
    Class::File,
    Class::Desc,
    Class::Memory,
    Class::Process,
    Class::Signal,
    Class::Ipc,
    Class::Clock,
];

/// A profile's trace, read once.
pub struct Trace {
    events: Vec<RecordedEvent>,
    counted: Folded,
    lost: u64,
    calls: u64,

    /// The profile's span, on the trace's clock.
    start: i64,
    end: i64,
    engine: Option<String>,
    processes: ProcessTree,
}

impl Trace {
    pub fn of(profile: &Profile) -> Self {
        let recorded = profile.syscalls();
        let (start, end) = profile.span();
        let mut processes = ProcessTree::default();
        let mut calls = 0;

        for event in &recorded.events {
            match &event.event {
                TraceEvent::Call(call) => {
                    calls += 1;
                    if processes.get(call.process).is_none() {
                        processes.root(call.process, call.time(), None);
                    }
                }
                other => processes.apply(other),
            }
        }

        Self {
            events: recorded.events,
            counted: recorded.counted,
            lost: recorded.lost,
            calls,
            start,
            end,
            engine: profile.setting("syscalls").map(str::to_owned),
            processes,
        }
    }

    /// Nanoseconds from the profile's start.
    const fn relative(&self, time: u64) -> i64 {
        time.cast_signed().saturating_sub(self.start)
    }

    /// The program a process ran last.
    fn program(&self, pid: Pid) -> Option<String> {
        let (_, image) = self.processes.get(pid)?.images.last()?;
        Some(image.rsplit('/').next().unwrap_or(image).to_owned())
    }
}

/// The profile a syscall request asks about.
pub fn profile_of(request: &Request) -> Option<&str> {
    Some(match request {
        Request::SyscallSummary(query) => &query.query.profile,
        Request::SyscallRate(query) => &query.query.profile,
        Request::SyscallHeatmap(query) => &query.query.profile,
        Request::SyscallDetail(query) => &query.query.profile,
        Request::SyscallLog(query) => &query.query.profile,
        Request::SyscallLanes(query) => &query.query.profile,
        Request::SyscallCall(query) => &query.profile,
        Request::SyscallFiles(query)
        | Request::SyscallNetwork(query)
        | Request::SyscallProcesses(query) => &query.profile,
        Request::SyscallInFlight(query) => &query.profile,
        _ => return None,
    })
}

/// Answers a syscall request about `profile`, whose trace is `trace`.
pub fn answer(profile: &Profile, trace: &Trace, request: Request) -> Result<Value, Failure> {
    match request {
        Request::SyscallSummary(query) => Ok(json(&summary(profile, trace, &query)?)),
        Request::SyscallRate(query) => Ok(json(&rate(trace, &query)?)),
        Request::SyscallHeatmap(query) => Ok(json(&heatmap(trace, &query)?)),
        Request::SyscallDetail(query) => Ok(json(&detail(profile, trace, &query)?)),
        Request::SyscallLog(query) => Ok(json(&log(trace, &query)?)),
        Request::SyscallCall(query) => Ok(json(&call(profile, trace, &query)?)),
        Request::SyscallLanes(query) => Ok(json(&lanes(trace, &query)?)),
        Request::SyscallFiles(query) => Ok(json(&files(trace, &query)?)),
        Request::SyscallNetwork(query) => Ok(json(&network(trace, &query)?)),
        Request::SyscallProcesses(query) => Ok(json(&processes(trace, &query)?)),
        _ => Err(Failure::new(
            ErrorKind::Failed,
            "internal: only syscall requests come here, and in-flight ones go to their recording",
        )),
    }
}

fn json(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|error| json!({ "error": error.to_string() }))
}

/// How a process ended, as strace's last line says it.
pub fn end_text(end: ProcessEnd) -> String {
    match end {
        ProcessEnd::Exited(code) => format!("exited with {code}"),
        ProcessEnd::Killed { signal, core } => format!(
            "killed by {}{}",
            present::signal(signal),
            if core { " (core dumped)" } else { "" }
        ),
    }
}

/// The calls `blocked` lists, as a recording last saw them at `at`.
pub fn in_flight(blocked: &[InFlight], at: u64, live: bool) -> SyscallInFlight {
    let style = style();
    let calls = blocked
        .iter()
        .map(|call| {
            let event = SyscallEvent {
                thread: call.thread,
                process: call.process,
                call: call.call,
                args: call.args,
                captured: Vec::new(),
                outcome: Outcome::Unfinished,
                entered: Some(call.entered),
                exited: None,
                stack: None,
                flags: EventFlags::default(),
            };

            SyscallBlocked {
                tid: call.thread.get(),
                pid: call.process.get(),
                tokens: tokens(present::entry(
                    &event,
                    &style,
                    Context {
                        fds: None,
                        before: true,
                    },
                )),
                blocked_ns: at.saturating_sub(call.entered),
            }
        })
        .collect();

    SyscallInFlight { live, calls }
}

/// The calls a query selects.
struct Selection {
    filter: Filter,

    /// Calls that ended in `[from, to)`, on the trace's clock.
    from: i64,
    to: i64,
    threads: Option<BTreeSet<u32>>,

    /// Calls whose duration falls in this log2 bucket.
    latency: Option<usize>,

    /// Whether a call that overlaps the range is in it, as lanes draw
    /// it, rather than only one that ended there.
    overlapping: bool,
}

impl Selection {
    fn new(trace: &Trace, query: &SyscallQuery) -> Result<Self, Failure> {
        let expressions: Vec<String> = query.filter.split_whitespace().map(str::to_owned).collect();
        let filter = Filter::parse(&expressions, &present::signal_named)
            .map_err(|error| Failure::new(ErrorKind::Invalid, error.to_string()))?;
        let (from, to) = query.range.map_or((i64::MIN, i64::MAX), |[from, to]| {
            (
                trace.start.saturating_add(from),
                trace.start.saturating_add(to),
            )
        });

        Ok(Self {
            filter,
            from,
            to,
            threads: query
                .threads
                .as_ref()
                .map(|threads| threads.iter().copied().collect()),
            latency: query
                .latency
                .map(|bucket| usize::try_from(bucket).unwrap_or(usize::MAX)),
            overlapping: false,
        })
    }

    fn holds(&self, time: u64) -> bool {
        (self.from..self.to).contains(&time.cast_signed())
    }

    fn shows(&self, call: &SyscallEvent, fds: &FdTable) -> bool {
        let inside = if self.overlapping {
            let entered = call.entered.unwrap_or_else(|| call.time()).cast_signed();
            entered < self.to && self.from <= call.time().cast_signed()
        } else {
            self.holds(call.time())
        };

        if !inside
            || self
                .threads
                .as_ref()
                .is_some_and(|threads| !threads.contains(&call.thread.get()))
            || self
                .latency
                .is_some_and(|band| call.duration().map(stats::bucket) != Some(band))
        {
            return false;
        }

        let paths = filter::paths(call);
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();

        self.filter.shows(call, &paths, &|fd| {
            fds.describe(call.process, fd)
                .map(|description| description.text.to_string())
        })
    }

    /// The range the query asks about, or the whole profile's.
    const fn span(&self, trace: &Trace) -> (i64, i64) {
        if self.from == i64::MIN {
            (trace.start, trace.end.saturating_add(1))
        } else {
            (self.from, self.to)
        }
    }
}

/// What a pass over a trace sees.
enum Seen<'a> {
    /// A selected call, with the descriptors as it found them.
    Before(usize, &'a SyscallEvent, &'a FdTable),

    /// The same call, with the descriptors as it left them.
    After(usize, &'a SyscallEvent, &'a FdTable),

    /// Another event in the selection's range.
    Other(usize, &'a TraceEvent),
}

/// Walks the trace in order, keeping its descriptor table.
fn walk(trace: &Trace, selection: &Selection, mut visit: impl FnMut(Seen<'_>)) {
    let mut fds = FdTable::default();
    for (index, recorded) in trace.events.iter().enumerate() {
        match &recorded.event {
            TraceEvent::Call(call) => {
                let shown = selection.shows(call, &fds);
                if shown {
                    visit(Seen::Before(index, call, &fds));
                }

                fds.apply_call(call);
                if shown {
                    visit(Seen::After(index, call, &fds));
                }
            }
            other => {
                fds.apply(other);
                if selection.holds(other.time()) {
                    visit(Seen::Other(index, other));
                }
            }
        }
    }
}

/// Each selected call, with the descriptors as it found them.
fn calls(
    trace: &Trace,
    selection: &Selection,
    mut visit: impl FnMut(usize, &SyscallEvent, &FdTable),
) {
    walk(trace, selection, |seen| {
        if let Seen::Before(index, call, fds) = seen {
            visit(index, call, fds);
        }
    });
}

/// Where stack `row` was made, as a caller's row names it; read once.
fn site<'a>(profile: &Profile, sites: &'a mut HashMap<u32, String>, row: u32) -> &'a String {
    sites.entry(row).or_insert_with(|| {
        profile.syscall_site(row).unwrap_or_else(|| {
            profile
                .syscall_stack(row)
                .0
                .into_iter()
                .next()
                .unwrap_or_else(|| "?".to_owned())
        })
    })
}

/// The path a call names, made absolute against the directory it names
/// it from, as the descriptor it makes will be known.
fn named_path(call: &SyscallEvent, fds: &FdTable) -> Option<String> {
    let path = filter::paths(call).into_iter().next()?;
    let dirfd = specs::spec(call.call)
        .and_then(|spec| {
            spec.args
                .iter()
                .position(|arg| matches!(arg, specs::Arg::DirFd))
        })
        .map_or(AT_FDCWD, |index| filter::fd_value(call.args[index]));

    Some(fds.resolve(call.process, dirfd, &path))
}

/// What a call works on: the path it names, or what its first descriptor
/// refers to.
fn target(call: &SyscallEvent, fds: &FdTable) -> Option<String> {
    named_path(call, fds).or_else(|| {
        let spec = specs::spec(call.call)?;
        let index = spec
            .args
            .iter()
            .position(|arg| matches!(arg, specs::Arg::Fd))?;
        let fd = filter::fd_value(call.args[index]);
        Some(fds.describe(call.process, fd).map_or_else(
            || fd.to_string(),
            |description| description.text.to_string(),
        ))
    })
}

/// The class a call is shown under.
fn class(call: &SyscallEvent) -> &'static str {
    specs::spec(call.call)
        .and_then(|spec| {
            CLASSES
                .into_iter()
                .find(|&class| spec.classes.contains(class))
        })
        .map_or("other", Class::name)
}

fn errno_name(errno: u16) -> String {
    present::errno(Errno(errno)).0
}

fn counts(errnos: &BTreeMap<u16, u64>) -> Vec<SyscallCount> {
    let mut counts: Vec<SyscallCount> = errnos
        .iter()
        .map(|(&errno, &count)| SyscallCount {
            name: errno_name(errno),
            count,
        })
        .collect();
    counts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    counts
}

fn row(key: String, name: String, stats: &SyscallStats) -> SyscallRow {
    SyscallRow {
        key,
        name,
        calls: stats.calls,
        errors: stats.errors,
        timed: stats.timed,
        total_ns: stats.total_ns,
        p50_ns: stats.quantile_ns(0.5),
        p90_ns: stats.quantile_ns(0.9),
        p99_ns: stats.quantile_ns(0.99),
        max_ns: stats.max_ns,
        bytes: stats.bytes,
        errnos: counts(&stats.errors_by_errno),
    }
}

fn summary(
    profile: &Profile,
    trace: &Trace,
    query: &SyscallSummaryQuery,
) -> Result<SyscallSummary, Failure> {
    let selection = Selection::new(trace, &query.query)?;
    let mut sites: HashMap<u32, String> = HashMap::new();
    let mut group = |index: usize, call: &SyscallEvent, fds: &FdTable| -> (String, String) {
        match query.group {
            SyscallGroup::Path => {
                let path = target(call, fds).unwrap_or_else(|| "none".to_owned());
                (path.clone(), path)
            }
            SyscallGroup::Site => {
                let site = trace.events[index].stack.map_or_else(
                    || "no stack".to_owned(),
                    |stack| site(profile, &mut sites, stack).clone(),
                );
                (site.clone(), site)
            }
            SyscallGroup::Syscall => {
                let name = call.call.display_name().into_owned();
                (name.clone(), name)
            }
            SyscallGroup::Class => (format!("%{}", class(call)), class(call).to_owned()),
            SyscallGroup::Process => (
                call.process.to_string(),
                trace.program(call.process).map_or_else(
                    || call.process.to_string(),
                    |program| format!("{} {program}", call.process),
                ),
            ),
            SyscallGroup::Thread => (call.thread.to_string(), call.thread.to_string()),
        }
    };

    let mut groups: BTreeMap<String, (String, SyscallStats)> = BTreeMap::new();
    let mut processes = BTreeSet::new();
    let mut add = |key: String, name: String, stats: &SyscallStats| {
        groups
            .entry(key)
            .or_insert_with(|| (name, SyscallStats::default()))
            .1
            .merge(stats);
    };

    calls(trace, &selection, |index, call, fds| {
        processes.insert(call.process);
        let (key, name) = group(index, call, fds);
        let mut one = SyscallStats::default();
        one.add(call.outcome, call.duration(), bytes_moved(call));
        add(key, name, &one);
    });

    // A trace the kernel counted holds its calls only as statistics, of
    // each process and call over the whole trace.
    let counted = !trace.counted.by_call.is_empty();
    for (&(pid, id), stats) in &trace.counted.by_call {
        if !selection.filter.calls.contains(id) {
            continue;
        }

        processes.insert(pid);
        let name = id.display_name().into_owned();
        let (key, name) = match query.group {
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
        lost: trace.lost,
        counted,
        engine: trace.engine.clone(),
        processes: u32::try_from(processes.len()).unwrap_or(u32::MAX),
        rows,
    })
}

/// A range cut into slices: its start, each slice's width, and how many.
struct Slices {
    start: i64,
    width: i64,
    count: usize,
}

impl Slices {
    fn new(span: (i64, i64), buckets: u32) -> Self {
        let count = buckets.clamp(1, MOST_BUCKETS);
        let length = span.1.saturating_sub(span.0).max(1);

        Self {
            start: span.0,
            width: (length + i64::from(count) - 1) / i64::from(count),
            count: count as usize,
        }
    }

    /// The slice a time falls in, if any.
    fn of(&self, time: u64) -> Option<usize> {
        let offset = time.cast_signed().checked_sub(self.start)?;
        usize::try_from(offset / self.width)
            .ok()
            .filter(|&slice| offset >= 0 && slice < self.count)
    }
}

fn rate(trace: &Trace, query: &SyscallRateQuery) -> Result<SyscallRate, Failure> {
    let selection = Selection::new(trace, &query.query)?;
    let slices = Slices::new(selection.span(trace), query.buckets);
    let mut series: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut errors = vec![0; slices.count];
    calls(trace, &selection, |_, call, _| {
        let Some(slice) = slices.of(call.time()) else {
            return;
        };

        let name = match query.by {
            SyscallRateBy::Class => class(call).to_owned(),
            SyscallRateBy::Syscall => call.call.display_name().into_owned(),
        };

        series.entry(name).or_insert_with(|| vec![0; slices.count])[slice] += 1;
        if matches!(call.outcome, Outcome::Failed(_)) {
            errors[slice] += 1;
        }
    });

    let mut series: Vec<SyscallSeries> = series
        .into_iter()
        .map(|(name, counts)| SyscallSeries { name, counts })
        .collect();
    series.sort_by_key(|series| std::cmp::Reverse(series.counts.iter().sum::<u64>()));
    if series.len() > MOST_SERIES {
        let rest = series.split_off(MOST_SERIES - 1);
        let mut other = vec![0; slices.count];
        for series in rest {
            for (total, count) in other.iter_mut().zip(series.counts) {
                *total += count;
            }
        }

        series.push(SyscallSeries {
            name: "other".to_owned(),
            counts: other,
        });
    }

    Ok(SyscallRate {
        start: slices.start - trace.start,
        width: slices.width,
        series,
        errors,
    })
}

fn heatmap(trace: &Trace, query: &SyscallHeatmapQuery) -> Result<SyscallHeatmap, Failure> {
    let selection = Selection::new(trace, &query.query)?;
    let slices = Slices::new(selection.span(trace), query.buckets);
    let rows = stats::BUCKETS;
    let mut counts = vec![0_u32; slices.count * rows];
    calls(trace, &selection, |_, call, _| {
        if let (Some(duration), Some(slice)) = (call.duration(), slices.of(call.time())) {
            counts[slice * rows + stats::bucket(duration)] += 1;
        }
    });

    Ok(SyscallHeatmap {
        start: slices.start - trace.start,
        width: slices.width,
        rows: u32::try_from(rows).unwrap_or(u32::MAX),
        most: counts.iter().copied().max().unwrap_or(0),
        counts,
    })
}

fn detail(
    profile: &Profile,
    trace: &Trace,
    query: &SyscallDetailQuery,
) -> Result<SyscallDetail, Failure> {
    let named = |call: &SyscallEvent| call.call.display_name() == query.call;
    let mut whole = vec![0; stats::BUCKETS];
    for event in &trace.events {
        if let TraceEvent::Call(call) = &event.event
            && named(call)
            && let Some(duration) = call.duration()
        {
            whole[stats::bucket(duration)] += 1;
        }
    }

    let selection = Selection::new(trace, &query.query)?;
    let mut selected = SyscallStats::default();
    let mut chosen = Vec::new();
    calls(trace, &selection, |index, call, _| {
        if named(call) {
            selected.add(call.outcome, call.duration(), bytes_moved(call));
            chosen.push(index);
        }
    });

    // Slow is the selection's slowest tenth; failed is any error.
    let slow_ns = selected.quantile_ns(0.9);
    let mut sites: HashMap<u32, String> = HashMap::new();
    let mut callers: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut unplaced = 0;

    for index in chosen {
        let event = &trace.events[index];
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

        let site = site(profile, &mut sites, stack);
        let counts = callers.entry(site.clone()).or_default();
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

/// How the web shows calls: with what descriptors refer to, as `-y`.
fn style() -> Style {
    Style {
        fds: FdStyle::Paths,
        ..Style::default()
    }
}

fn tokens(tokens: Tokens) -> Vec<SyscallToken> {
    tokens
        .0
        .into_iter()
        .map(|token| SyscallToken {
            kind: token.kind.name().to_owned(),
            text: token.text,
        })
        .collect()
}

fn log(trace: &Trace, query: &SyscallLogQuery) -> Result<SyscallLog, Failure> {
    let selection = Selection::new(trace, &query.query)?;
    let first = query.offset;
    let last = first.saturating_add(u64::from(query.limit.min(MOST_LINES)));
    let style = style();
    let mut total = 0_u64;
    let mut lines = Vec::new();
    let mut entry = Tokens::default();
    walk(trace, &selection, |seen| match seen {
        Seen::Before(_, call, fds) => {
            if (first..last).contains(&total) {
                entry = present::entry(
                    call,
                    &style,
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
                        &style,
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
            let Some(line) = present::other(event) else {
                return;
            };

            if (first..last).contains(&total) {
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
            }

            total += 1;
        }
    });

    Ok(SyscallLog { total, lines })
}

fn call(
    profile: &Profile,
    trace: &Trace,
    query: &SyscallCallQuery,
) -> Result<SyscallCall, Failure> {
    let index = usize::try_from(query.event).unwrap_or(usize::MAX);
    let missing = || Failure::new(ErrorKind::Invalid, format!("no call {}", query.event));
    let event = trace.events.get(index).ok_or_else(missing)?;
    let TraceEvent::Call(target) = &event.event else {
        return Err(missing());
    };

    // The descriptors as the call found them, and as it left them.
    let mut fds = FdTable::default();
    for event in &trace.events[..index] {
        match &event.event {
            TraceEvent::Call(call) => fds.apply_call(call),
            other => fds.apply(other),
        }
    }

    let style = style();
    let before = Context {
        fds: Some(&fds),
        before: true,
    };

    let arguments = present::argument_values(target, &style, before)
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
        &style,
        Context {
            fds: Some(&after),
            before: false,
        },
    );
    let (stack, stack_ends) = event.stack.map_or((Vec::new(), None), |row| {
        let (functions, ending) = profile.syscall_stack(row);
        let ends = match ending {
            StackEnding::Complete => None,
            StackEnding::Truncated(reason) => Some(reason),
        };

        (functions, ends)
    });

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
            .and_then(|row| profile.syscall_place(row))
            .map(|place| {
                let line = u64::from(place.line);
                let first = line.saturating_sub(SITE_MARGIN).max(1);
                let (text, unread) =
                    match super::profiles::source_lines(place.file, first, line + SITE_MARGIN) {
                        Ok(text) => (Some(text), None),
                        Err(why) => (None, Some(why)),
                    };

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

fn lanes(trace: &Trace, query: &SyscallLanesQuery) -> Result<SyscallLanes, Failure> {
    let selection = Selection {
        overlapping: true,
        ..Selection::new(trace, &query.query)?
    };

    let slices = Slices::new(selection.span(trace), query.buckets);
    let mut lanes: BTreeMap<(Pid, Tid), Lane> = BTreeMap::new();
    calls(trace, &selection, |_, call, _| {
        let Lane {
            busy,
            failed,
            blocked,
        } = lanes
            .entry((call.process, call.thread))
            .or_insert_with(|| Lane {
                busy: vec![0; slices.count],
                failed: vec![0; slices.count],
                blocked: vec![false; slices.count],
            });
        if let (Some(entered), Some(exited)) = (call.entered, call.exited) {
            spread(&slices, entered, exited, busy, blocked);
        }

        if matches!(call.outcome, Outcome::Failed(_))
            && let Some(slice) = slices.of(call.time())
        {
            failed[slice] += 1;
        }
    });

    Ok(SyscallLanes {
        start: slices.start - trace.start,
        width: slices.width,
        lanes: lanes
            .into_iter()
            .map(|((pid, tid), lane)| SyscallLane {
                tid: tid.get(),
                pid: pid.get(),
                name: trace.program(pid),
                busy: lane.busy,
                failed: lane.failed,
                blocked: lane.blocked,
            })
            .collect(),
    })
}

/// One thread's slices.
struct Lane {
    busy: Vec<u64>,
    failed: Vec<u32>,
    blocked: Vec<bool>,
}

/// Adds the nanoseconds of `[from, to)` to each slice they overlap, and
/// marks those it covers whole.
fn spread(slices: &Slices, from: u64, to: u64, busy: &mut [u64], blocked: &mut [bool]) {
    let (from, to) = (from.cast_signed(), to.cast_signed());
    for ((slice, total), whole) in (0_i64..).zip(busy.iter_mut()).zip(blocked.iter_mut()) {
        let start = slices.start + slices.width * slice;
        let end = start + slices.width;
        let overlap = to.min(end) - from.max(start);
        if overlap > 0 {
            *total += overlap.unsigned_abs();
        }
        *whole |= from <= start && end <= to;
    }
}

/// Whether a call opens what its path names.
fn opens(call: &SyscallEvent) -> bool {
    call.call
        .name()
        .is_some_and(|name| matches!(name, "open" | "openat" | "openat2" | "creat"))
}

fn files(trace: &Trace, query: &SyscallQuery) -> Result<SyscallFiles, Failure> {
    let selection = Selection::new(trace, query)?;
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

fn network(trace: &Trace, query: &SyscallQuery) -> Result<SyscallNetwork, Failure> {
    let selection = Selection::new(trace, query)?;
    let mut endpoints: BTreeMap<String, SyscallEndpoint> = BTreeMap::new();
    walk(trace, &selection, |seen| {
        // A connect is known by the endpoint it made; the rest by the one
        // they used.
        let (call, fds, connect) = match seen {
            Seen::Before(_, call, fds) if !is_connect(call) => (call, fds, false),
            Seen::After(_, call, fds) if is_connect(call) => (call, fds, true),
            _ => return,
        };

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

            // A non-blocking connect that is under way has not failed.
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

fn is_connect(call: &SyscallEvent) -> bool {
    call.call.name() == Some("connect")
}

fn processes(trace: &Trace, query: &SyscallQuery) -> Result<SyscallProcesses, Failure> {
    let selection = Selection::new(trace, query)?;
    let mut counts: BTreeMap<Pid, (u64, u64)> = BTreeMap::new();

    calls(trace, &selection, |_, call, _| {
        let entry = counts.entry(call.process).or_default();
        entry.0 += 1;
        entry.1 += u64::from(matches!(call.outcome, Outcome::Failed(_)));
    });

    // Parents first: each process after the one that made it.
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
