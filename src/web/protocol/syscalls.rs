//! What a tab asks about a profile's system call trace, and the answers.

use serde::{Deserialize, Serialize};

/// Which of a trace's calls a view shows.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallQuery {
    pub profile: String,

    /// The revision the tab has seen, which keeps its cached answers
    /// apart; the server answers from the newest.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    #[expect(dead_code, reason = "only the tab's cache reads it")]
    pub revision: u32,

    /// Calls that ended in `[start, end)`, in nanoseconds from the
    /// profile's start, or all.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub range: Option<[i64; 2]>,

    /// strace's selection, as `-e` takes it, expressions apart by spaces:
    /// `trace=%file status=failed`.
    #[serde(default)]
    pub filter: String,

    /// Calls of these threads, by id, or of all.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub threads: Option<Vec<u32>>,

    /// Calls whose duration falls in this log2 bucket, as a heat map's
    /// row names it, or all.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub latency: Option<u32>,
}

/// What the summary's rows are of.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum SyscallGroup {
    #[default]
    Syscall,
    Class,
    Process,
    Thread,

    /// The file, socket, or pipe a call names, by path or descriptor.
    Path,

    /// The innermost frame with a source line.
    Site,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallSummaryQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,

    #[serde(default)]
    pub group: SyscallGroup,
}

/// What a trace's selected calls cost, as `-c` counts it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallSummary {
    /// Every call the trace holds, and those the query selects.
    pub total: u64,
    pub calls: u64,
    pub errors: u64,

    pub lost: SyscallLosses,

    /// Whether the kernel counted the calls without reporting them, so
    /// that only the summary has them.
    pub counted: bool,

    /// The engine that traced, `bpf` or `ptrace`.
    pub engine: Option<String>,
    pub processes: u32,
    pub rows: Vec<SyscallRow>,
}

/// Events a trace's engine lost: records it dropped and returns it never
/// saw, which make every count a lower bound, and calls over a counting
/// recording's cap or past a full ring, which were counted but not kept.
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallLosses {
    pub dropped: u64,
    pub unseen: u64,
    pub capped: u64,
    pub overflowed: u64,
}

impl From<microscope::syscalls::Losses> for SyscallLosses {
    fn from(lost: microscope::syscalls::Losses) -> Self {
        Self {
            dropped: lost.dropped,
            unseen: lost.unseen,
            capped: lost.capped,
            overflowed: lost.overflowed,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallRow {
    /// What the row is of, as a filter or query names it: a call's name,
    /// a class, a pid, a tid, a path, or a site.
    pub key: String,
    pub name: String,
    pub calls: u64,
    pub errors: u64,

    /// Calls with a duration, which the times cover.
    pub timed: u64,
    pub total_ns: u64,
    pub p50_ns: Option<u64>,
    pub p90_ns: Option<u64>,
    pub p99_ns: Option<u64>,
    pub max_ns: u64,
    pub bytes: u64,

    /// Failures by errno name, most first.
    pub errnos: Vec<SyscallCount>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallCount {
    pub name: String,
    pub count: u64,
}

/// What a rate chart stacks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum SyscallRateBy {
    #[default]
    Class,
    Syscall,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallRateQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,
    pub buckets: u32,

    #[serde(default)]
    pub by: SyscallRateBy,
}

/// Calls per slice of the query's range.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallRate {
    /// The range's start and each slice's nanoseconds.
    pub start: i64,
    pub width: i64,

    /// Most calls first; the rest past the first few as `other`.
    pub series: Vec<SyscallSeries>,
    pub errors: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallSeries {
    pub name: String,
    pub counts: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallHeatmapQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,
    pub buckets: u32,
}

/// Calls by when they ended and how long they took.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallHeatmap {
    pub start: i64,
    pub width: i64,

    /// Rows of log2 latency, as a histogram's buckets.
    pub rows: u32,

    /// Each slice's rows of counts, slice after slice.
    pub counts: Vec<u32>,
    pub most: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallDetailQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,

    /// The call, by name.
    pub call: String,
}

/// One call's latency, errors, and callers.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallDetail {
    /// Log2 histograms of the whole trace's calls and of the query's.
    pub whole: Vec<u64>,
    pub selected: Vec<u64>,
    pub errnos: Vec<SyscallCount>,

    /// What counts as slow: the selection's 90th percentile.
    pub slow_ns: Option<u64>,

    /// Where the slow and failed calls came from, most first.
    pub callers: Vec<SyscallCaller>,

    /// Slow or failed calls without a stack.
    pub unplaced: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallCaller {
    /// The innermost frame with a source line, or the stack's innermost.
    pub site: String,
    pub slow: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallLogQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,
    pub offset: u64,
    pub limit: u32,
}

/// A page of the trace as strace prints it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallLog {
    /// Lines the query selects.
    pub total: u64,
    pub lines: Vec<SyscallLine>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallLine {
    /// The event's place in the trace, which `syscallCall` names it by.
    pub event: u64,

    /// Nanoseconds from the profile's start.
    pub time: i64,
    pub tid: u32,
    pub pid: u32,
    pub tokens: Vec<SyscallToken>,
    pub duration_ns: Option<u64>,
    pub failed: bool,

    /// Whether it is a call, which expands; signals and exits do not.
    pub call: bool,
}

/// Formatted text of one kind, as the CLI colors it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallToken {
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallCallQuery {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    #[expect(dead_code, reason = "only the tab's cache reads it")]
    pub revision: u32,
    pub event: u64,
}

/// One call, expanded.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallCall {
    pub arguments: Vec<SyscallArgument>,

    /// Hex dumps of the bytes it read or wrote, as `-e read=` prints them.
    pub dumps: Vec<String>,

    /// Its stack's functions, innermost first.
    pub stack: Vec<String>,

    /// Why the stack ends early, when it does.
    pub stack_ends: Option<String>,

    /// When it entered and returned, from the profile's start.
    pub entered: Option<i64>,
    pub exited: Option<i64>,

    /// What its result means, as `ENOENT (No such file or directory)`.
    pub result: Vec<SyscallToken>,

    /// Where in the source it was made, when its stack says.
    pub site: Option<SyscallSite>,
}

/// A call's source line, with the lines around it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallSite {
    pub function: String,
    pub path: String,
    pub line: u32,

    /// The text from line `first`, or why it could not be read.
    pub first: u32,
    pub text: Option<Vec<String>>,
    pub unread: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallArgument {
    pub name: String,
    pub tokens: Vec<SyscallToken>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallLanesQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,
    pub buckets: u32,
}

/// Each thread's calls over the range, as lanes beside the samples'.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallLanes {
    pub start: i64,
    pub width: i64,
    pub lanes: Vec<SyscallLane>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallLane {
    pub tid: u32,
    pub pid: u32,

    /// Its process's program.
    pub name: Option<String>,

    /// Each slice's nanoseconds spent in calls, and failed calls.
    pub busy: Vec<u64>,
    pub failed: Vec<u32>,

    /// Slices one call spans from end to end: the thread was blocked.
    pub blocked: Vec<bool>,
}

/// What the trace did with each file.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallFiles {
    pub files: Vec<SyscallFile>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallFile {
    pub path: String,
    pub opens: u64,
    pub failed: u64,
    pub read: u64,
    pub written: u64,

    /// Nanoseconds in `fsync` and `fdatasync`.
    pub sync_ns: u64,

    /// Nanoseconds in every call on it.
    pub total_ns: u64,
}

/// What the trace did over each connection.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallNetwork {
    pub endpoints: Vec<SyscallEndpoint>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallEndpoint {
    /// What `-y` prints of the socket, as `TCP:[10.0.0.1:5000->10.0.0.2:80]`.
    pub endpoint: String,
    pub connects: u64,
    pub failed: u64,

    /// Nanoseconds the connects took.
    pub connect_ns: u64,
    pub sent: u64,
    pub received: u64,
}

/// The processes the trace saw, parents first.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallProcesses {
    pub processes: Vec<SyscallProcess>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallProcess {
    pub pid: u32,
    pub parent: Option<u32>,

    /// The programs it ran, in order.
    pub images: Vec<String>,
    pub threads: u32,
    pub calls: u64,
    pub errors: u64,

    /// From the profile's start.
    pub started: i64,
    pub ended: Option<i64>,

    /// How it ended, as strace's last line says it.
    pub end: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallInFlightQuery {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    #[expect(dead_code, reason = "only the tab's cache reads it")]
    pub revision: u32,
}

/// The calls a live trace's threads were blocked in when it was last
/// published.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallInFlight {
    /// Whether the trace is still recording; an ended one has none.
    pub live: bool,
    pub calls: Vec<SyscallBlocked>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallBlocked {
    pub tid: u32,
    pub pid: u32,

    /// The call as strace prints its entry.
    pub tokens: Vec<SyscallToken>,

    /// How long it had been blocked.
    pub blocked_ns: u64,
}
