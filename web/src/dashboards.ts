import { account } from "./account";
import { request } from "./orgs";
import type { ProfileKey } from "./protocol";
import { createState, useSelector } from "./state";

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
  layout: unknown;
  scope: ViewScope;
  notes: string;
  sharedAt: number | null;
  keptAt: number | null;
  pinStatus: "none" | "pinned" | "cap_exceeded";
  pinnedBytes: number;
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

const queryString = (params: Record<string, string | number | null | undefined>): string => {
  const entries = Object.entries(params).filter(
    (entry): entry is [string, string | number] => entry[1] !== null && entry[1] !== undefined,
  );
  return entries.length === 0
    ? ""
    : `?${new URLSearchParams(entries.map(([k, v]) => [k, String(v)]))}`;
};

export const dashboardsApi = {
  listDashboards: (org: number | null) =>
    request<DashboardsListResponse>("GET", `/api/dashboards${queryString({ org })}`),
  getDashboard: (id: string, org: number | null) =>
    request<DashboardRecord>(
      "GET",
      `/api/dashboards/${encodeURIComponent(id)}${queryString({ org })}`,
    ),
  createDashboard: (body: {
    org_id: number | null;
    project_id: number | null;
    name: string;
    layout: unknown;
    scope: ViewScope;
    notes?: string;
  }) => request<DashboardRecord>("POST", "/api/dashboards", body),
  updateDashboard: (
    id: string,
    body: { name?: string; layout?: unknown; scope?: ViewScope; notes?: string },
  ) => request<DashboardRecord>("PATCH", `/api/dashboards/${encodeURIComponent(id)}`, body),
  deleteDashboard: (id: string) =>
    request<undefined>("DELETE", `/api/dashboards/${encodeURIComponent(id)}`),

  listSessions: (org: number | null) =>
    request<SessionsListResponse>("GET", `/api/sessions${queryString({ org })}`),
  getSession: (id: number) => request<SessionRecord>("GET", `/api/sessions/${id}`),
  createSession: (body: {
    org_id: number | null;
    project_id?: number | null;
    source_dashboard_id?: number | null;
    source_builtin?: string | null;
    name?: string | null;
    layout: unknown;
    scope: ViewScope;
    notes?: string;
  }) => request<SessionRecord>("POST", "/api/sessions", body),
  updateSession: (
    id: number,
    body: { name?: string; layout?: unknown; scope?: ViewScope; notes?: string },
  ) => request<SessionRecord>("PATCH", `/api/sessions/${id}`, body),
  pinEstimate: (id: number) => request<PinEstimate>("GET", `/api/sessions/${id}/pin-estimate`),
  keepSession: (id: number, body: { name?: string; pin?: boolean }) =>
    request<SessionRecord>("POST", `/api/sessions/${id}/keep`, body),
  unpinSession: (id: number) => request<undefined>("POST", `/api/sessions/${id}/unpin`, {}),
  deleteSession: (id: number) => request<undefined>("DELETE", `/api/sessions/${id}`),

  queryFlame: (scope: ViewScope, sessionId: number | null, invert = false) =>
    request<FlameResponse>("POST", "/api/query/flame", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
      invert,
    }),
  queryOverTime: (scope: ViewScope, sessionId: number | null, groupBy: string) =>
    request<OverTimeResponse>("POST", "/api/query/over-time", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
      group_by: groupBy,
    }),
  queryByMachine: (scope: ViewScope, sessionId: number | null) =>
    request<ByMachineResponse>("POST", "/api/query/by-machine", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
    }),
  queryBreakdown: (scope: ViewScope, sessionId: number | null, dimension: string) =>
    request<BreakdownResponse>("POST", "/api/query/breakdown", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
      dimension,
    }),
  queryDimensions: (scope: ViewScope, sessionId: number | null) =>
    request<DimensionsResponse>("POST", "/api/query/dimensions", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
    }),
};

export function normalizeScope(raw: Partial<ViewScope> | null | undefined): ViewScope {
  if (!raw) {
    return { ...DEFAULT_SCOPE };
  }
  return {
    org_id: raw.org_id ?? null,
    project_ids: Array.isArray(raw.project_ids) ? raw.project_ids : [],
    filters: Array.isArray(raw.filters) ? raw.filters : [],
    time_range: raw.time_range ?? DEFAULT_SCOPE.time_range,
    refresh: raw.refresh ?? "m1",
    compare: raw.compare ?? null,
    selection: raw.selection ?? null,
  };
}

export function scopeToSearch(scope: ViewScope): string {
  const params = new URLSearchParams();
  if (scope.org_id !== null && scope.org_id !== undefined) {
    params.set("org", String(scope.org_id));
  }
  if (scope.project_ids.length > 0) {
    params.set("projects", scope.project_ids.join(","));
  }
  for (const f of scope.filters) {
    params.append("f", `${f.dimension}:${f.value}`);
  }
  const tr = scope.time_range;
  if (tr.kind === "relative") {
    const preset = RANGE_PRESETS.find((p) => p.secs === tr.duration_secs);
    params.set("range", preset ? preset.slug : `${tr.duration_secs}s`);
  } else {
    params.set("start", String(tr.start_time));
    params.set("end", String(tr.end_time));
  }
  if (scope.refresh !== "m1") {
    params.set("refresh", scope.refresh);
  }
  if (scope.compare) {
    if (scope.compare.kind === "shift") {
      params.set("cmp", `shift:${scope.compare.offset_secs}`);
    } else if (scope.compare.kind !== "custom") {
      params.set("cmp", scope.compare.kind);
    }
  }
  if (scope.selection) {
    if (scope.selection.kind === "function") {
      params.set("fn", scope.selection.name);
    } else if (scope.selection.kind === "zone") {
      params.set("zone", scope.selection.name);
    } else if (scope.selection.name) {
      params.set("syscall", scope.selection.name);
    }
  }
  const text = params.toString();
  return text === "" ? "" : `?${text}`;
}

export function searchToScope(search: string, base: ViewScope): ViewScope {
  const params = new URLSearchParams(search.startsWith("?") ? search.slice(1) : search);
  if ([...params.keys()].length === 0) {
    return base;
  }
  const next: ViewScope = {
    ...base,
    filters: [...base.filters],
    project_ids: [...base.project_ids],
  };
  const org = params.get("org");
  if (org !== null && /^[1-9]\d*$/u.test(org)) {
    next.org_id = Number(org);
  }
  const projects = params.get("projects");
  if (projects !== null) {
    next.project_ids = projects
      .split(",")
      .map((s) => Number(s.trim()))
      .filter((n) => Number.isInteger(n) && n > 0);
  }
  const fList = params.getAll("f");
  if (fList.length > 0) {
    next.filters = [];
    for (const pair of fList) {
      const idx = pair.indexOf(":");
      if (idx > 0) {
        next.filters.push({ dimension: pair.slice(0, idx), value: pair.slice(idx + 1) });
      }
    }
  }
  const start = params.get("start");
  const end = params.get("end");
  const range = params.get("range");
  if (start !== null && end !== null && /^\d+$/u.test(start) && /^\d+$/u.test(end)) {
    next.time_range = { kind: "absolute", start_time: Number(start), end_time: Number(end) };
  } else if (range !== null) {
    const preset = RANGE_PRESETS.find((p) => p.slug === range);
    if (preset) {
      next.time_range = { kind: "relative", duration_secs: preset.secs };
    } else if (/^\d+s$/u.test(range)) {
      next.time_range = { kind: "relative", duration_secs: Number(range.slice(0, -1)) };
    }
  }
  const refresh = params.get("refresh");
  if (refresh && REFRESH_OPTIONS.some((o) => o.value === refresh)) {
    next.refresh = refresh as RefreshInterval;
  }
  const cmp = params.get("cmp");
  if (cmp === "previous_version") {
    next.compare = { kind: "previous_version" };
  } else if (cmp === "rest_of_group") {
    next.compare = { kind: "rest_of_group" };
  } else if (cmp?.startsWith("shift:")) {
    const secs = Number(cmp.slice(6));
    if (Number.isFinite(secs) && secs >= 60) {
      next.compare = { kind: "shift", offset_secs: secs };
    }
  }
  const fn = params.get("fn");
  const zone = params.get("zone");
  const syscall = params.get("syscall");
  if (fn) {
    next.selection = { kind: "function", name: fn };
  } else if (zone) {
    next.selection = { kind: "zone", name: zone };
  } else if (syscall) {
    next.selection = { kind: "syscall", name: syscall };
  }
  return next;
}

export type DashboardsTab = "dashboards" | "sessions";

export interface DashboardsWindowState {
  open: boolean;
  tab: DashboardsTab;
  org: number | null;
  q: string;
  selectedSessionId: number | null;
  saveDialogOpen: boolean;
  pushed: boolean;
  waiting: { tab: DashboardsTab; org: number | null } | null;
}

export const dashboardsWindow = createState<DashboardsWindowState>(() => ({
  open: false,
  tab: "dashboards",
  org: null,
  q: "",
  selectedSessionId: null,
  saveDialogOpen: false,
  pushed: false,
  waiting: null,
}));

export function useDashboardsWindow<T>(pick: (state: DashboardsWindowState) => T): () => T {
  return useSelector(dashboardsWindow, pick);
}

export function openDashboards(tab: DashboardsTab = "dashboards", org?: number | null): void {
  const cur = dashboardsWindow.getState();
  dashboardsWindow.setState({
    open: true,
    tab,
    org: org === undefined ? cur.org : org,
    waiting: null,
  });
}

export function closeDashboards(): void {
  dashboardsWindow.setState({ open: false, waiting: null });
}

export function openSaveDashboardDialog(): void {
  dashboardsWindow.setState({ saveDialogOpen: true });
}

export function closeSaveDashboardDialog(): void {
  dashboardsWindow.setState({ saveDialogOpen: false });
}

export function dashboardsHash(tab: DashboardsTab, org: number | null): string {
  const params = new URLSearchParams();
  if (org !== null) {
    params.set("org", String(org));
  }
  const qs = params.toString();
  return qs === "" ? tab : `${tab}?${qs}`;
}

export function parseDashboardsHash(
  hash: string,
): { tab: DashboardsTab; org: number | null } | null {
  const raw = hash.startsWith("#") ? hash.slice(1) : hash;
  const idx = raw.indexOf("?");
  const path = idx === -1 ? raw : raw.slice(0, idx);
  if (path !== "dashboards" && path !== "sessions") {
    return null;
  }
  const params = new URLSearchParams(idx === -1 ? "" : raw.slice(idx + 1));
  const orgRaw = params.get("org");
  const org = orgRaw !== null && /^[1-9]\d*$/u.test(orgRaw) ? Number(orgRaw) : null;
  return { tab: path, org };
}

export interface ActiveViewState {
  active: boolean;
  mode: "dashboard" | "session";
  dashboardId: string | null;
  sessionId: number | null;
  title: string;
  builtin: boolean;
  canEdit: boolean;
  isOwner: boolean;
  locked: boolean;
  orgId: number | null;
  projectId: number | null;
  pinStatus: "none" | "pinned" | "cap_exceeded";
  pinnedBytes: number;
  keptAt: number | null;
  sharedAt: number | null;
  expiresAt: number | null;
  visitorCount: number;
  notes: string;
  scope: ViewScope;
  dimensions: DimensionsResponse | null;
  revision: number;
  lastFetchedAt: number | null;
  loading: boolean;
  error: string | null;
  movedToSession: boolean;
}

const INACTIVE_VIEW: ActiveViewState = {
  active: false,
  mode: "dashboard",
  dashboardId: null,
  sessionId: null,
  title: "",
  builtin: false,
  canEdit: false,
  isOwner: true,
  locked: false,
  orgId: null,
  projectId: null,
  pinStatus: "none",
  pinnedBytes: 0,
  keptAt: null,
  sharedAt: null,
  expiresAt: null,
  visitorCount: 0,
  notes: "",
  scope: DEFAULT_SCOPE,
  dimensions: null,
  revision: 0,
  lastFetchedAt: null,
  loading: false,
  error: null,
  movedToSession: false,
};

export const activeView = createState<ActiveViewState>(() => ({ ...INACTIVE_VIEW }));

export function useActiveView<T>(pick: (state: ActiveViewState) => T): () => T {
  return useSelector(activeView, pick);
}

let onMoveToSession: ((sessionId: number) => void) | null = null;
let saveTimer: ReturnType<typeof setTimeout> | null = null;
let currentLayoutSerializer: (() => unknown) | null = null;

export function registerViewHooks(hooks: {
  moveToSession: (sessionId: number) => void;
  serializeLayout: () => unknown;
}): () => void {
  onMoveToSession = hooks.moveToSession;
  currentLayoutSerializer = hooks.serializeLayout;
  return () => {
    if (onMoveToSession === hooks.moveToSession) {
      onMoveToSession = null;
    }
    if (currentLayoutSerializer === hooks.serializeLayout) {
      currentLayoutSerializer = null;
    }
  };
}

export function clearActiveView(): void {
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
    saveTimer = null;
  }
  activeView.setState({ ...INACTIVE_VIEW });
}

export function bumpViewRevision(): void {
  activeView.setState((s) => ({ revision: s.revision + 1 }));
}

export function markViewFetched(): void {
  activeView.setState({ lastFetchedAt: Date.now() });
}

export function updateViewScope(
  updater: (prev: ViewScope) => ViewScope,
  options?: { persist?: boolean },
): void {
  const cur = activeView.getState();
  if (!cur.active) {
    return;
  }
  const nextScope = updater(cur.scope);
  const shouldMove = cur.mode === "dashboard" && cur.sessionId !== null && !cur.movedToSession;
  activeView.setState({
    scope: nextScope,
    revision: cur.revision + 1,
    ...(shouldMove ? { mode: "session", movedToSession: true } : {}),
  });
  if (options?.persist !== false) {
    if (shouldMove) {
      flushSessionSaveNow();
    } else {
      scheduleSessionSave();
    }
  }
  if (shouldMove && cur.sessionId !== null && onMoveToSession) {
    onMoveToSession(cur.sessionId);
  }
}

function flushSessionSaveNow(): void {
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
    saveTimer = null;
  }
  const cur = activeView.getState();
  if (
    !cur.active ||
    cur.sessionId === null ||
    !cur.isOwner ||
    account.getState().now.kind !== "in"
  ) {
    return;
  }
  const layout = currentLayoutSerializer ? currentLayoutSerializer() : undefined;
  void dashboardsApi
    .updateSession(cur.sessionId, {
      scope: cur.scope,
      ...(layout === undefined ? {} : { layout }),
    })
    .catch(() => {});
}

export function scheduleSessionSave(): void {
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
  }
  saveTimer = setTimeout(() => {
    saveTimer = null;
    flushSessionSaveNow();
  }, 350);
}

export async function ensureSessionUrl(): Promise<string | null> {
  const cur = activeView.getState();
  if (!cur.active) {
    return null;
  }
  let sid = cur.sessionId;
  if (sid === null && currentLayoutSerializer) {
    const created = await dashboardsApi.createSession({
      org_id: cur.orgId,
      project_id: cur.projectId,
      source_builtin: cur.builtin ? cur.dashboardId : null,
      source_dashboard_id: !cur.builtin && cur.dashboardId ? Number(cur.dashboardId) : null,
      layout: currentLayoutSerializer(),
      scope: cur.scope,
    });
    sid = created.id;
    activeView.setState({
      sessionId: created.id,
      mode: "session",
      movedToSession: true,
      expiresAt: created.expiresAt,
      keptAt: created.keptAt,
    });
    onMoveToSession?.(created.id);
  } else if (sid !== null && cur.isOwner && currentLayoutSerializer) {
    await dashboardsApi.updateSession(sid, {
      scope: cur.scope,
      layout: currentLayoutSerializer(),
    });
    if (cur.mode === "dashboard" && !cur.movedToSession) {
      activeView.setState({ mode: "session", movedToSession: true });
      onMoveToSession?.(sid);
    }
  }
  return sid === null ? null : `${window.location.origin}/s/${sid}`;
}

export function addScopeFilter(dimension: string, value: string): void {
  updateViewScope((prev) => {
    const exists = prev.filters.some((f) => f.dimension === dimension && f.value === value);
    if (exists) {
      return prev;
    }
    return { ...prev, filters: [...prev.filters, { dimension, value }] };
  });
}

export function setSingleScopeFilter(dimension: string, value: string | null): void {
  updateViewScope((prev) => {
    const rest = prev.filters.filter((f) => f.dimension !== dimension);
    return {
      ...prev,
      filters: value === null || value === "" ? rest : [...rest, { dimension, value }],
    };
  });
}

export function removeScopeFilter(dimension: string, value: string): void {
  updateViewScope((prev) => ({
    ...prev,
    filters: prev.filters.filter((f) => !(f.dimension === dimension && f.value === value)),
  }));
}

export function setScopeSelection(selection: Selection | null): void {
  updateViewScope((prev) => ({ ...prev, selection }));
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let idx = 0;
  while (value >= 1024 && idx < units.length - 1) {
    value /= 1024;
    idx += 1;
  }
  return `${value >= 100 ? Math.round(value) : value.toFixed(1)} ${units[idx]}`;
}

export function formatDimensionLabel(dim: string): string {
  switch (dim) {
    case "service.name":
      return "service";
    case "deployment.environment":
      return "env";
    case "service.version":
      return "version";
    case "host.name":
      return "machine";
    case "process.name":
      return "process";
    case "thread.name":
      return "thread";
    case "k8s.pod.name":
      return "pod";
    case "container.name":
      return "container";
    default:
      return dim;
  }
}

export function formatTimeShort(ns: number): string {
  const date = new Date(ns / 1_000_000);
  const hh = String(date.getHours()).padStart(2, "0");
  const mm = String(date.getMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

export function scopeKey(scope: ViewScope, sessionId: number | null, revision: number): string {
  return `${revision}:${sessionId ?? "live"}:${JSON.stringify(scope)}`;
}
