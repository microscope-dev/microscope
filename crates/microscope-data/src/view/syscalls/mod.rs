mod extract;
mod queries;
mod sites;

use std::collections::{BTreeMap, HashMap};

use crate::analysis::Keys;
use crate::capture::Capture;
use crate::syscalls::fds::FdTable;
use crate::syscalls::filter::{self, Filter};
use crate::syscalls::present::{self, Context, Tokens};
use crate::syscalls::procs::ProcessTree;
use crate::syscalls::specs::{self, Class};
use crate::syscalls::stats::{self, Folded, SyscallStats};
use crate::syscalls::{Errno, Outcome, Pid, ProcessEnd, SyscallEvent, TraceEvent};

use super::ViewError;
use super::protocol::{
    ProfileDuring, SyscallBlocked, SyscallCount, SyscallGroup, SyscallInFlight, SyscallRow,
    SyscallToken,
};

pub(super) use extract::{syscall_count, syscall_name, syscalls_lost};
pub(super) use queries::{call, detail, files, heatmap, log, network, processes, summary};

const AT_FDCWD: i64 = -100;
const MOST_BUCKETS: u32 = 4096;
const MOST_LINES: u32 = 2000;
const SITE_MARGIN: u64 = 3;
const BLOCKED_AT: &str = "syscalls blocked at";

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

pub(super) struct RecordedEvent {
    pub(super) event: TraceEvent,
    pub(super) stack: Option<u32>,
}

pub(super) struct Trace {
    pub(super) events: Vec<RecordedEvent>,
    pub(super) counted: Folded,
    pub(super) lost: crate::syscalls::Losses,
    pub(super) calls: u64,
    pub(super) start: i64,
    pub(super) end: i64,
    pub(super) engine: Option<String>,
    pub(super) processes: ProcessTree,
}

impl Trace {
    pub(super) fn of(capture: &Capture) -> Self {
        let _span = crate::span!("web syscall trace");
        let recorded = extract::extract_syscalls(capture);
        let (start, end) = capture.span();
        let mut processes = ProcessTree::default();
        let mut calls = 0;

        for event in &recorded.events {
            match &event.event {
                TraceEvent::Call(call) => {
                    calls += u64::from(!call.flags.kept());
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
            engine: capture.setting("syscall engine").map(str::to_owned),
            processes,
        }
    }

    pub(super) const fn relative(&self, time: u64) -> i64 {
        time.cast_signed().saturating_sub(self.start)
    }

    pub(super) fn program(&self, pid: Pid) -> Option<String> {
        let (_, image) = self.processes.get(pid)?.images.last()?;
        Some(image.rsplit('/').next().unwrap_or(image).to_owned())
    }
}

#[must_use]
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

pub(super) fn in_flight(capture: &Capture, live: bool) -> SyscallInFlight {
    if !live {
        return SyscallInFlight {
            live,
            calls: Vec::new(),
        };
    }

    let at = capture
        .setting(BLOCKED_AT)
        .and_then(|at| at.parse().ok())
        .unwrap_or_else(|| capture.span().1.cast_unsigned());
    let calls = extract::extract_syscalls(capture)
        .events
        .iter()
        .filter_map(|event| match &event.event {
            TraceEvent::Call(call) if call.outcome == Outcome::Unfinished => Some(call),
            _ => None,
        })
        .map(|call| SyscallBlocked {
            tid: call.thread.get(),
            pid: call.process.get(),
            tokens: tokens(present::entry(
                call,
                Context {
                    fds: None,
                    before: true,
                },
            )),
            blocked_ns: call.entered.map_or(0, |entered| at.saturating_sub(entered)),
        })
        .collect();

    SyscallInFlight { live, calls }
}

pub(super) struct Selection {
    pub(super) filter: Filter,
    from: i64,
    to: i64,
    kept: Option<Vec<bool>>,
    latency: Option<usize>,
}

impl Selection {
    fn calls(filter: &str, latency: Option<u32>) -> Result<Self, ViewError> {
        let expressions: Vec<String> = filter.split_whitespace().map(str::to_owned).collect();
        let filter = Filter::parse(&expressions, &present::signal_named)
            .map_err(|error| ViewError::Invalid(error.to_string()))?;
        Ok(Self {
            filter,
            from: i64::MIN,
            to: i64::MAX,
            kept: None,
            latency: latency.map(|bucket| usize::try_from(bucket).unwrap_or(usize::MAX)),
        })
    }

    pub(super) fn new(
        capture: &Capture,
        trace: &Trace,
        query: &super::protocol::SyscallQuery,
    ) -> Result<Self, ViewError> {
        let calls = Self::calls(&query.filter, query.latency)?;
        let link = &query.link;
        let narrows = link.threads.is_some()
            || !link.labels.is_empty()
            || link.zones.is_some()
            || !link.transforms.is_empty();
        let kept = if narrows {
            let mut unranged = link.clone();
            unranged.range = None;
            unranged.during = None;
            unranged.syscall_leaves = false;
            let keys = Keys::new(capture, super::profile::granularity(link.granularity));
            let resolved = super::profile::resolve(capture, &keys, &unranged)?;
            Some(keys.syscalls(&resolved))
        } else {
            None
        };

        let (from, to) = link.range.map_or((i64::MIN, i64::MAX), |[from, to]| {
            (
                trace.start.saturating_add(from),
                trace.start.saturating_add(to),
            )
        });

        Ok(Self {
            from,
            to,
            kept,
            ..calls
        })
    }

    fn holds(&self, time: u64) -> bool {
        (self.from..self.to).contains(&time.cast_signed())
    }

    fn shows(&self, row: usize, call: &SyscallEvent, fds: &FdTable) -> bool {
        if !self.holds(call.time())
            || self
                .kept
                .as_ref()
                .is_some_and(|kept| !kept.get(row).copied().unwrap_or(false))
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

    pub(super) const fn span(&self, trace: &Trace) -> (i64, i64) {
        if self.from == i64::MIN {
            (trace.start, trace.end.saturating_add(1))
        } else {
            (self.from, self.to)
        }
    }
}

pub(super) enum Seen<'a> {
    Before(usize, &'a SyscallEvent, &'a FdTable),
    After(usize, &'a SyscallEvent, &'a FdTable),
    Other(usize, &'a TraceEvent),
}

pub(super) fn walk(trace: &Trace, selection: &Selection, mut visit: impl FnMut(Seen<'_>)) {
    let mut fds = FdTable::default();
    let mut row = 0;
    for (index, recorded) in trace.events.iter().enumerate() {
        match &recorded.event {
            TraceEvent::Call(call) => {
                let shown = selection.shows(row, call, &fds);
                row += 1;
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

pub(super) fn rows(
    capture: &Capture,
    trace: &Trace,
    during: &ProfileDuring,
) -> Result<Vec<u32>, ViewError> {
    let selection = Selection::calls(&during.filter, during.latency)?;
    let mut row = during
        .row
        .as_ref()
        .map(|row| (Grouping::new(capture, trace, row.group), row.key.as_str()));
    let mut matched = Vec::new();
    known(trace, &selection, |index, call, fds| {
        if row
            .as_mut()
            .is_none_or(|(grouping, key)| grouping.row(index, call, fds).0 == *key)
        {
            matched.push(index);
        }
    });

    let mut matched = matched.into_iter().peekable();
    let mut rows = Vec::new();
    let mut calls = 0;
    for (index, recorded) in trace.events.iter().enumerate() {
        if matches!(recorded.event, TraceEvent::Call(_)) {
            if matched.next_if_eq(&index).is_some() {
                rows.push(calls);
            }
            calls += 1;
        }
    }

    Ok(rows)
}

pub(super) fn calls(
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

pub(super) fn known(
    trace: &Trace,
    selection: &Selection,
    mut visit: impl FnMut(usize, &SyscallEvent, &FdTable),
) {
    walk(trace, selection, |seen| match seen {
        Seen::Before(index, call, fds) if !is_connect(call) => visit(index, call, fds),
        Seen::After(index, call, fds) if is_connect(call) => visit(index, call, fds),
        _ => {}
    });
}

pub(super) fn is_connect(call: &SyscallEvent) -> bool {
    call.call.name() == Some("connect")
}

pub(super) fn site<'a>(
    capture: &Capture,
    sites_map: &'a mut HashMap<u32, String>,
    row: u32,
) -> &'a String {
    sites_map
        .entry(row)
        .or_insert_with(|| sites::syscall_site_name(capture, row))
}

pub(super) fn named_path(call: &SyscallEvent, fds: &FdTable) -> Option<String> {
    let path = filter::paths(call).into_iter().next()?;
    let dirfd = match specs::spec(call.call).and_then(|spec| {
        spec.args
            .iter()
            .position(|arg| matches!(arg, specs::Arg::DirFd))
    }) {
        Some(index) => filter::fd_value(*call.args.get(index)?),
        None => AT_FDCWD,
    };

    Some(fds.resolve(call.process, dirfd, &path))
}

fn target(call: &SyscallEvent, fds: &FdTable) -> Option<String> {
    named_path(call, fds).or_else(|| {
        let spec = specs::spec(call.call)?;
        let index = spec
            .args
            .iter()
            .position(|arg| matches!(arg, specs::Arg::Fd))?;
        let fd = filter::fd_value(*call.args.get(index)?);
        Some(fds.describe(call.process, fd).map_or_else(
            || fd.to_string(),
            |description| description.text.to_string(),
        ))
    })
}

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

pub(super) fn counts(errnos: &BTreeMap<u16, u64>) -> Vec<SyscallCount> {
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

pub(super) fn row(key: String, name: String, stats: &SyscallStats) -> SyscallRow {
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

pub(super) fn tokens(tokens: Tokens) -> Vec<SyscallToken> {
    tokens
        .0
        .into_iter()
        .map(|token| SyscallToken {
            kind: token.kind.name().to_owned(),
            text: token.text,
        })
        .collect()
}

pub(super) struct Grouping<'a> {
    capture: &'a Capture,
    trace: &'a Trace,
    group: SyscallGroup,
    sites: HashMap<u32, String>,
}

impl<'a> Grouping<'a> {
    pub(super) fn new(capture: &'a Capture, trace: &'a Trace, group: SyscallGroup) -> Self {
        Self {
            capture,
            trace,
            group,
            sites: HashMap::new(),
        }
    }

    pub(super) fn row(
        &mut self,
        index: usize,
        call: &SyscallEvent,
        fds: &FdTable,
    ) -> (String, String) {
        match self.group {
            SyscallGroup::Path => {
                let path = target(call, fds).unwrap_or_else(|| "none".to_owned());
                (path.clone(), path)
            }
            SyscallGroup::Site => {
                let site = self
                    .trace
                    .events
                    .get(index)
                    .and_then(|event| event.stack)
                    .map_or_else(
                        || "no stack".to_owned(),
                        |stack| site(self.capture, &mut self.sites, stack).clone(),
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
                self.trace.program(call.process).map_or_else(
                    || call.process.to_string(),
                    |program| format!("{} {program}", call.process),
                ),
            ),
            SyscallGroup::Thread => (call.thread.to_string(), call.thread.to_string()),
        }
    }
}
