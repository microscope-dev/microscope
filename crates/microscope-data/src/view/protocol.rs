use serde::{Deserialize, Serialize};

use crate::syscalls::Losses;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRef {
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub revision: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileQuery {
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub revision: u32,
    #[serde(default)]
    pub range: Option<[i64; 2]>,
    #[serde(default)]
    pub threads: Option<Vec<u32>>,
    #[serde(default)]
    pub source: ProfileSource,
    #[serde(default)]
    pub tasks: Option<Vec<Option<u32>>>,
    #[serde(default)]
    pub by_task: bool,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub by_label: Option<String>,
    #[serde(default)]
    pub transforms: Vec<ProfileTransform>,
    #[serde(default)]
    pub invert: bool,
    #[serde(default)]
    pub look: ProfileLook,
    #[serde(default)]
    pub granularity: ProfileGranularity,
    #[serde(default)]
    pub zones: Option<ProfileZoneScope>,
    #[serde(default)]
    pub base: Option<ProfileBase>,
    #[serde(default)]
    pub during: Option<ProfileDuring>,
    #[serde(default)]
    pub syscall_leaves: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileDuring {
    pub filter: String,
    #[serde(default)]
    pub latency: Option<u32>,
    #[serde(default)]
    pub row: Option<SyscallRowKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProfileZoneScope {
    pub name: String,
    #[serde(default)]
    pub band: Option<[u64; 2]>,
    #[serde(default)]
    pub outside: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProfileBase {
    pub profile: String,
    #[serde(default)]
    pub range: Option<[i64; 2]>,
    #[serde(default)]
    pub zones: Option<ProfileZoneScope>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProfileBaseWeight {
    pub weight: u64,
    pub samples: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfileSource {
    #[default]
    Samples,
    Waits,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileTransform {
    pub op: TransformOp,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransformOp {
    Focus,
    Merge,
    Drop,
    CollapseCycles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfileLook {
    #[default]
    Logical,
    Native,
    Python,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfileGranularity {
    #[default]
    Function,
    Physical,
    Line,
    Address,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileFunctionQuery {
    #[serde(flatten)]
    pub query: ProfileQuery,
    pub function: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileLines {
    pub weight: u64,
    pub files: Vec<ProfileSourceFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileSourceFile {
    pub path: Option<String>,
    pub lines: Vec<[u64; 3]>,
    pub first: u32,
    pub text: Option<Vec<String>>,
    pub unread: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileCode {
    pub weight: u64,
    pub module: Option<String>,
    pub blocks: Vec<ProfileCodeBlock>,
    pub unplaced: Vec<[u64; 3]>,
    pub unavailable: Option<String>,
    pub functions: Vec<String>,
    pub files: Vec<ProfileSourceFile>,
    pub unlined: Option<String>,
    pub inlined_into: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileCodeBlock {
    pub symbol: String,
    pub instructions: Vec<ProfileInstruction>,
    pub stopped: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileInstruction {
    pub address: u64,
    pub tokens: Vec<ProfileToken>,
    pub target: Option<String>,
    pub own: u64,
    pub total: u64,
    pub lines: Vec<ProfileCodeLine>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileToken(pub String, pub String);

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProfileCodeLine(pub u32, pub Option<u32>, pub u32);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileTimelineQuery {
    #[serde(flatten)]
    pub query: ProfileQuery,
    pub buckets: u32,
    #[serde(default)]
    pub task_lanes: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub program: Option<String>,
    pub command: Option<ProfileCommand>,
    pub unit: String,
    pub source: Option<String>,
    pub samples: u64,
    pub weight: u64,
    pub syscalls: u64,
    pub syscalls_lost: SyscallLosses,
    pub duration: i64,
    pub threads: Vec<ProfileThread>,
    pub lost: u64,
    pub throttles: u64,
    pub truncations: Vec<ProfileCount>,
    pub settings: Vec<ProfileSetting>,
    pub recorder_cpu: u64,
    pub recorder_loading: u64,
    pub behind: Vec<[i64; 2]>,
    pub budget_changes: Vec<ProfileMarker>,
    pub tasks: Vec<ProfileTask>,
    pub interpreted: bool,
    pub snapshots: u64,
    pub snapshot_pause: u64,
    pub task_samples: u64,
    pub snapshot_problems: Vec<ProfileMarker>,
    pub waited: u64,
    pub probes: ProfileProbes,
    pub zone_sites: Vec<ProfileZoneSite>,
    pub label_keys: Vec<String>,
    pub probe_problems: Vec<ProfileMarker>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileProbes {
    pub events: u64,
    pub overwritten: u64,
    pub no_ring: u64,
    pub interrupted: u64,
    pub malformed: u64,
    pub plotted: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileZoneSite {
    pub name: String,
    pub label: bool,
    pub count: u64,
    pub total: u64,
    pub longest: u64,
    pub flagged: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileZonesQuery {
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub revision: u32,
    #[serde(default)]
    pub range: Option<[i64; 2]>,
    #[serde(default)]
    pub threads: Option<Vec<u32>>,
    pub width: u32,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub band: Option<[u64; 2]>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileZones {
    pub range: [i64; 2],
    pub blocks: u32,
    pub off_cpu: bool,
    pub names: Vec<String>,
    pub marks: Vec<ProfileMark>,
    pub plots: Vec<ProfilePlot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfilePlot {
    pub name: String,
    pub values: u64,
    pub unshown: u64,
    pub bins: Vec<[f64; 5]>,
    pub held: Option<f64>,
    pub continues: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileMark {
    pub thread: u32,
    pub time: i64,
    pub kind: ProfileMarkKind,
    pub name: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfileMarkKind {
    Message,
    Frame,
    Trigger,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileZoneStatsQuery {
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub revision: u32,
    pub name: String,
    #[serde(default)]
    pub range: Option<[i64; 2]>,
    #[serde(default)]
    pub threads: Option<Vec<u32>>,
    #[serde(default)]
    pub band: Option<[u64; 2]>,
    #[serde(default)]
    pub rank_of: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileZoneStats {
    pub count: u64,
    pub total: u64,
    pub flagged: u64,
    pub min: u64,
    pub median: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
    pub histogram: Vec<[u64; 2]>,
    pub longest: Vec<ProfileZoneAt>,
    pub banded: [u64; 2],
    pub shorter: u64,
    pub values: Vec<ProfileLabelValue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileZoneAt {
    pub thread: u32,
    pub start: i64,
    pub end: i64,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileLabelValue {
    pub value: String,
    pub total: u64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileTask {
    pub row: u32,
    pub name: String,
    pub weight: u64,
    pub waited: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileMarker {
    pub time: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileInspection {
    pub processes: Vec<ProfileProcess>,
    pub modules: Vec<ProfileModule>,
    pub depths: Vec<u64>,
    pub over_time: Vec<u64>,
    pub distinct_stacks: u64,
    pub frames: u64,
    pub frames_with_lines: u64,
    pub counted: u64,
    pub stack_bytes: u64,
    pub shortened: u64,
    pub recorder_setup: u64,
    pub recorder_finishing: u64,
    pub tables: Vec<ProfileTable>,
    pub file: Option<ProfileFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileCommand {
    pub arguments: Option<Vec<String>>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileProcess {
    pub pid: u32,
    pub name: String,
    pub command: Option<ProfileCommand>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileModule {
    pub path: String,
    pub size: u64,
    pub build_id: Option<String>,
    pub frames: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileTable {
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileFile {
    pub bytes: Option<u64>,
    pub chunks: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileThread {
    pub row: u32,
    pub pid: u32,
    pub tid: u32,
    pub name: Option<String>,
    pub samples: u64,
    pub weight: u64,
    pub zones: u64,
    pub blocked: u64,
    pub preempted: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileCount {
    pub what: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileSetting {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileTree {
    pub nodes: u32,
    pub weight: u64,
    pub samples: u64,
    pub keys: Vec<ProfileKey>,
    pub base: Option<ProfileBaseWeight>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileKey {
    pub name: String,
    pub kind: ProfileKeyKind,
    pub module: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfileKeyKind {
    Function,
    Line,
    Address,
    Truncation,
    Task,
    Group,
    Label,
    Syscall,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileFunctions {
    pub weight: u64,
    pub rows: Vec<ProfileCost>,
    pub keys: Vec<ProfileKey>,
    pub base: Option<ProfileBaseWeight>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileCost {
    pub key: u32,
    pub own: u64,
    pub total: u64,
    pub base_own: u64,
    pub base_total: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileTimeline {
    pub range: [i64; 2],
    pub lanes: Vec<ProfileLane>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileLane {
    pub thread: Option<u32>,
    pub task: Option<u32>,
    pub weights: Vec<u64>,
    pub blocked: Vec<u64>,
    pub preempted: Vec<u64>,
    pub syscalls: Vec<u32>,
    pub failed: Vec<u32>,
    pub waits: Vec<ProfileWait>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileWait {
    pub call: String,
    pub start: i64,
    pub end: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallQuery {
    #[serde(flatten)]
    pub link: ProfileQuery,
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub latency: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct SyscallRowKey {
    pub group: SyscallGroup,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallSummaryQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,
    #[serde(default)]
    pub group: SyscallGroup,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Copy, Serialize)]
pub struct SyscallLosses {
    pub dropped: u64,
    pub unseen: u64,
    pub capped: u64,
    pub overflowed: u64,
}

impl From<Losses> for SyscallLosses {
    fn from(lost: Losses) -> Self {
        Self {
            dropped: lost.dropped,
            unseen: lost.unseen,
            capped: lost.capped,
            overflowed: lost.overflowed,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct SyscallCount {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallHeatmapQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,
    pub buckets: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallHeatmap {
    pub start: i64,
    pub width: i64,
    pub rows: u32,
    pub counts: Vec<u32>,
    pub most: u32,
    pub failed: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallDetailQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,
    pub call: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallDetail {
    pub whole: Vec<u64>,
    pub selected: Vec<u64>,
    pub errnos: Vec<SyscallCount>,
    pub slow_ns: Option<u64>,
    pub callers: Vec<SyscallCaller>,
    pub unplaced: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyscallCaller {
    pub site: String,
    pub slow: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallLogQuery {
    #[serde(flatten)]
    pub query: SyscallQuery,
    pub offset: u64,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallLog {
    pub total: u64,
    pub lines: Vec<SyscallLine>,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct SyscallToken {
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallCallQuery {
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub revision: u32,
    pub event: u64,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallSite {
    pub function: String,
    pub path: String,
    pub line: u32,
    pub first: u32,
    pub text: Option<Vec<String>>,
    pub unread: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyscallArgument {
    pub name: String,
    pub tokens: Vec<SyscallToken>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyscallFiles {
    pub files: Vec<SyscallFile>,
}

#[derive(Debug, Clone, Default, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct SyscallNetwork {
    pub endpoints: Vec<SyscallEndpoint>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallEndpoint {
    pub endpoint: String,
    pub connects: u64,
    pub failed: u64,
    pub connect_ns: u64,
    pub sent: u64,
    pub received: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyscallProcesses {
    pub processes: Vec<SyscallProcess>,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallInFlightQuery {
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub revision: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyscallInFlight {
    pub live: bool,
    pub calls: Vec<SyscallBlocked>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyscallBlocked {
    pub tid: u32,
    pub pid: u32,
    pub tokens: Vec<SyscallToken>,
    pub blocked_ns: u64,
}
