import type { ProfileKey } from "../protocol";

export interface DimensionFilter {
  dimension: string;
  value: string;
}

export type TimeRange =
  | { kind: "relative"; duration_secs: number }
  | { kind: "absolute"; start_time: number; end_time: number };

export type RefreshInterval = "off" | "s5" | "s15" | "s30" | "m1" | "m5" | "m15";

export type CompareSpec =
  | { kind: "previous_version" }
  | { kind: "rest_of_group" }
  | { kind: "shift"; offset_secs: number }
  | { kind: "custom"; filters: DimensionFilter[]; time_range?: TimeRange | null };

export type Selection =
  | { kind: "function"; name: string }
  | { kind: "syscall"; abi?: number | null; number?: number | null; name?: string | null }
  | { kind: "zone"; name: string };

export interface ViewScope {
  org_id?: number | null;
  project_ids: number[];
  filters: DimensionFilter[];
  time_range: TimeRange;
  refresh: RefreshInterval;
  compare?: CompareSpec | null;
  selection?: Selection | null;
}

export interface StorageQuota {
  pinnedBytes: number;
  pinCapBytes: number;
  detailDays: number;
  sessionDays: number;
}

export interface DashboardRecord {
  id: string;
  builtin: boolean;
  orgId: number;
  projectId: number | null;
  name: string;
  description: string;
  ownerUsername: string | null;
  canEdit: boolean;
  locked: boolean;
  layout: unknown;
  scope: ViewScope;
  updatedAt: number | null;
}

export interface CaptureMeta {
  id: number;
  name: string;
  projectId: number;
  projectName: string;
  byteLen: number;
  samples: number;
  durationNs: number;
  redactedCount: number;
  createdAt: number;
  expiresAt: number | null;
  deleted: boolean;
}

export interface SessionRecord {
  id: number;
  orgId: number;
  projectId: number | null;
  ownerUserId: number;
  ownerUsername: string;
  isOwner: boolean;
  name: string | null;
  sourceDashboardId: number | null;
  sourceBuiltin: string | null;
  captureId?: number | null;
  capture?: CaptureMeta | null;
  layout: unknown;
  scope: ViewScope;
  notes: string;
  sharedAt: number | null;
  keptAt: number | null;
  pinStatus: "none" | "pinned" | "cap_exceeded";
  pinnedBytes: number;
  pinAttempt?: string;
  expiresAt: number | null;
  lastOpenedAt: number;
  createdAt: number;
  updatedAt: number;
  visitorCount: number;
  locked: boolean;
}

export interface DashboardsListResponse {
  orgId: number;
  dashboards: DashboardRecord[];
  recentSessions: SessionRecord[];
  storage: StorageQuota;
}

export interface SessionsListResponse {
  orgId: number;
  sessions: SessionRecord[];
  storage: StorageQuota;
}

export interface PinEstimate {
  estimatedBytes: number;
  orgPinnedBytes: number;
  pinCapBytes: number;
  wouldExceedCap: boolean;
  machineCount: number;
  dataExpiresAt: number | null;
}

export interface TimedOutMachine {
  machineId: number;
  hostname: string;
}

export interface FlushReport {
  flushId: number;
  requested: number;
  flushed: number;
  timedOut: TimedOutMachine[];
}

export interface FlameResponse {
  nodes: number;
  weight: number;
  samples: number;
  unit: string;
  blocks: number;
  prunedBlocks: number;
  cacheHits: number;
  truncated: boolean;
  coarse: boolean;
  locked: boolean;
  keys: ProfileKey[];
  own: number[];
  total: number[];
  key: number[];
  depth: number[];
  baseOwn?: number[];
  baseTotal?: number[];
  baseWeight?: number;
  flush?: FlushReport;
}

export interface OverTimeSeries {
  label: string;
  values: number[];
}

export interface DeployMarker {
  timeNs: number;
  version: string;
  service: string;
}

export interface OverTimeResponse {
  timestampsNs: number[];
  bucketNs: number;
  unit: string;
  groupBy: string;
  series: OverTimeSeries[];
  compareSeries?: OverTimeSeries[];
  deploys: DeployMarker[];
  blocks: number;
  prunedBlocks: number;
  cacheHits: number;
  locked: boolean;
  flush?: FlushReport;
}

export interface MachineRow {
  machineId: number;
  hostname: string;
  environment: string;
  overall: number;
  distanceFromMedian: number;
  values: number[];
}

export interface ByMachineResponse {
  timestampsNs: number[];
  bucketNs: number;
  unit: string;
  median: number;
  machines: MachineRow[];
  blocks: number;
  prunedBlocks: number;
  cacheHits: number;
  locked: boolean;
  flush?: FlushReport;
}

export interface BreakdownRow {
  value: string;
  total: number;
  share: number;
  baseTotal?: number;
  delta?: number;
  deltaPct?: number;
}

export interface BreakdownResponse {
  dimension: string;
  unit: string;
  total: number;
  baseTotal?: number;
  rows: BreakdownRow[];
  blocks: number;
  prunedBlocks: number;
  cacheHits: number;
  locked: boolean;
  flush?: FlushReport;
}

export interface ScopeProject {
  id: number;
  name: string;
  permission: string;
}

export interface ScopeMachine {
  id: number;
  hostname: string;
  environment: string;
  tags: Record<string, string>;
}

export interface DimensionsResponse {
  orgId: number;
  locked: boolean;
  projects: ScopeProject[];
  machines: ScopeMachine[];
  indexed: Record<string, string[]>;
  adhoc: Record<string, string[]>;
  flush?: FlushReport;
}

export type StreamPaneSpec =
  | { kind: "flame"; id: string; invert?: boolean }
  | { kind: "over_time"; id: string; group_by?: string }
  | { kind: "by_machine"; id: string }
  | { kind: "breakdown"; id: string; dimension?: string };

export interface StreamStatusEvent {
  watching: boolean;
  watchedMachines: number;
  effectiveIntervalMs: number | null;
  locked: boolean;
  serverTimeNs: number;
  newestDataNs: number | null;
}

export interface StreamPaneEvent {
  id: string;
  kind: "flame" | "over_time" | "by_machine" | "breakdown";
  serverTimeNs: number;
  newestDataNs: number | null;
  data: unknown;
}

export interface StreamDimensionsEvent {
  serverTimeNs: number;
  data: DimensionsResponse;
}

export interface StreamHeartbeatEvent {
  serverTimeNs: number;
  newestDataNs: number | null;
  watchedMachines: number;
  effectiveIntervalMs: number | null;
}

export interface StreamedPaneData {
  kind: "flame" | "over_time" | "by_machine" | "breakdown";
  data: unknown;
  serverTimeNs: number;
  newestDataNs: number | null;
  receivedAt: number;
  streamKey?: string;
}

export const DEFAULT_SCOPE: ViewScope = {
  org_id: null,
  project_ids: [],
  filters: [],
  time_range: { kind: "relative", duration_secs: 3600 },
  refresh: "m1",
  compare: null,
  selection: null,
};

export const REFRESH_MS: Record<RefreshInterval, number | null> = {
  off: null,
  s5: 5_000,
  s15: 15_000,
  s30: 30_000,
  m1: 60_000,
  m5: 300_000,
  m15: 900_000,
};

export const REFRESH_OPTIONS: readonly { value: RefreshInterval; label: string }[] = [
  { value: "off", label: "Off" },
  { value: "s5", label: "5s" },
  { value: "s15", label: "15s" },
  { value: "s30", label: "30s" },
  { value: "m1", label: "1m" },
  { value: "m5", label: "5m" },
  { value: "m15", label: "15m" },
];

export const RANGE_PRESETS: readonly { secs: number; label: string; slug: string }[] = [
  { secs: 15 * 60, label: "Last 15m", slug: "15m" },
  { secs: 60 * 60, label: "Last 1h", slug: "1h" },
  { secs: 6 * 60 * 60, label: "Last 6h", slug: "6h" },
  { secs: 24 * 60 * 60, label: "Last 24h", slug: "24h" },
  { secs: 7 * 24 * 60 * 60, label: "Last 7d", slug: "7d" },
  { secs: 30 * 24 * 60 * 60, label: "Last 30d", slug: "30d" },
];
