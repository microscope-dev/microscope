use serde::{Deserialize, Serialize};

use super::ProfileQuery;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallQuery {
    #[serde(flatten)]
    pub link: ProfileQuery,

    #[serde(default)]
    pub filter: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub latency: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum SyscallGroup {
    #[default]
    Syscall,
    Class,
    Process,
    Thread,

    Path,

    Site,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallRowKey {
    pub group: SyscallGroup,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallSummaryQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,

    #[serde(default)]
    pub group: SyscallGroup,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallSummary {
    pub total: u64,
    pub calls: u64,
    pub errors: u64,

    pub lost: SyscallLosses,

    pub counted: bool,

    pub engine: Option<String>,
    pub processes: u32,
    pub rows: Vec<SyscallRow>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallLosses {
    pub dropped: u64,
    pub unseen: u64,
    pub capped: u64,
    pub overflowed: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallRow {
    pub key: String,
    pub name: String,
    pub calls: u64,
    pub errors: u64,

    pub timed: u64,
    pub total_ns: u64,
    pub p50_ns: Option<u64>,
    pub p90_ns: Option<u64>,
    pub p99_ns: Option<u64>,
    pub max_ns: u64,
    pub bytes: u64,

    pub errnos: Vec<SyscallCount>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallCount {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallHeatmapQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,
    pub buckets: u32,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallHeatmap {
    pub start: i64,
    pub width: i64,

    pub rows: u32,

    pub counts: Vec<u32>,
    pub most: u32,

    pub failed: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallDetailQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,

    pub call: String,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallDetail {
    pub whole: Vec<u64>,
    pub selected: Vec<u64>,
    pub errnos: Vec<SyscallCount>,

    pub slow_ns: Option<u64>,

    pub callers: Vec<SyscallCaller>,

    pub unplaced: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallCaller {
    pub site: String,
    pub slow: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallLogQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,
    pub offset: u64,
    pub limit: u32,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallLog {
    pub total: u64,
    pub lines: Vec<SyscallLine>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallLine {
    pub event: u64,

    pub time: i64,
    pub tid: u32,
    pub pid: u32,
    pub tokens: Vec<SyscallToken>,
    pub duration_ns: Option<u64>,
    pub failed: bool,

    pub call: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallToken {
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallCallQuery {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub revision: u32,
    pub event: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallCall {
    pub arguments: Vec<SyscallArgument>,

    pub dumps: Vec<String>,

    pub stack: Vec<String>,

    pub stack_ends: Option<String>,

    pub entered: Option<i64>,
    pub exited: Option<i64>,

    pub result: Vec<SyscallToken>,

    pub site: Option<SyscallSite>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallSite {
    pub function: String,
    pub path: String,
    pub line: u32,

    pub first: u32,
    pub text: Option<Vec<String>>,
    pub unread: Option<String>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallArgument {
    pub name: String,
    pub tokens: Vec<SyscallToken>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallFiles {
    pub files: Vec<SyscallFile>,
}

#[cfg(test)]
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallFile {
    pub path: String,
    pub opens: u64,
    pub failed: u64,
    pub read: u64,
    pub written: u64,

    pub sync_ns: u64,

    pub total_ns: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallNetwork {
    pub endpoints: Vec<SyscallEndpoint>,
}

#[cfg(test)]
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallEndpoint {
    pub endpoint: String,
    pub connects: u64,
    pub failed: u64,

    pub connect_ns: u64,
    pub sent: u64,
    pub received: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallProcesses {
    pub processes: Vec<SyscallProcess>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallProcess {
    pub pid: u32,
    pub parent: Option<u32>,

    pub images: Vec<String>,
    pub threads: u32,
    pub calls: u64,
    pub errors: u64,

    pub started: i64,
    pub ended: Option<i64>,

    pub end: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallInFlightQuery {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub revision: u32,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SyscallInFlight {
    pub live: bool,
    pub calls: Vec<SyscallBlocked>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SyscallBlocked {
    pub tid: u32,
    pub pid: u32,

    pub tokens: Vec<SyscallToken>,

    pub blocked_ns: u64,
}
