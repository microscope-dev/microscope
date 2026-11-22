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
    body: { name?: string; layout?: unknown; scope?: ViewScope; notes?: string; pin?: boolean },
  ) => request<SessionRecord>("PATCH", `/api/sessions/${id}`, body),
  pinEstimate: (id: number) => request<PinEstimate>("GET", `/api/sessions/${id}/pin-estimate`),
  keepSession: (id: number, body: { name?: string; pin?: boolean }) =>
    request<SessionRecord>("POST", `/api/sessions/${id}/keep`, body),
  unpinSession: (id: number) => request<undefined>("POST", `/api/sessions/${id}/unpin`, {}),
  deleteSession: (id: number) => request<undefined>("DELETE", `/api/sessions/${id}`),

  queryFlame: (
    scope: ViewScope,
    sessionId: number | null,
    invert = false,
    options?: { fresh?: boolean; flushTimeoutMs?: number },
  ) =>
    request<FlameResponse>("POST", "/api/query/flame", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
      invert,
      ...(options?.fresh ? { fresh: true } : {}),
      ...(options?.flushTimeoutMs === undefined
        ? {}
        : { flush_timeout_ms: options.flushTimeoutMs }),
    }),
  queryOverTime: (
    scope: ViewScope,
    sessionId: number | null,
    groupBy: string,
    options?: { fresh?: boolean; flushTimeoutMs?: number },
  ) =>
    request<OverTimeResponse>("POST", "/api/query/over-time", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
      group_by: groupBy,
      ...(options?.fresh ? { fresh: true } : {}),
      ...(options?.flushTimeoutMs === undefined
        ? {}
        : { flush_timeout_ms: options.flushTimeoutMs }),
    }),
  queryByMachine: (
    scope: ViewScope,
    sessionId: number | null,
    options?: { fresh?: boolean; flushTimeoutMs?: number },
  ) =>
    request<ByMachineResponse>("POST", "/api/query/by-machine", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
      ...(options?.fresh ? { fresh: true } : {}),
      ...(options?.flushTimeoutMs === undefined
        ? {}
        : { flush_timeout_ms: options.flushTimeoutMs }),
    }),
  queryBreakdown: (
    scope: ViewScope,
    sessionId: number | null,
    dimension: string,
    options?: { fresh?: boolean; flushTimeoutMs?: number },
  ) =>
    request<BreakdownResponse>("POST", "/api/query/breakdown", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
      dimension,
      ...(options?.fresh ? { fresh: true } : {}),
      ...(options?.flushTimeoutMs === undefined
        ? {}
        : { flush_timeout_ms: options.flushTimeoutMs }),
    }),
  queryDimensions: (
    scope: ViewScope,
    sessionId: number | null,
    options?: { fresh?: boolean; flushTimeoutMs?: number },
  ) =>
    request<DimensionsResponse>("POST", "/api/query/dimensions", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
      ...(options?.fresh ? { fresh: true } : {}),
      ...(options?.flushTimeoutMs === undefined
        ? {}
        : { flush_timeout_ms: options.flushTimeoutMs }),
    }),
  flush: (scope: ViewScope, sessionId: number | null, timeoutMs?: number) =>
    request<FlushReport>("POST", "/api/query/flush", {
      scope,
      ...(sessionId === null ? {} : { session_id: sessionId }),
      ...(timeoutMs === undefined ? {} : { timeout_ms: timeoutMs }),
    }),
};

export interface ParsedSseEvent {
  event: string;
  data: string;
}

export function parseSseChunk(
  buffer: string,
  chunk: string,
): { events: ParsedSseEvent[]; rest: string } {
  const combined = (buffer + chunk).replaceAll("\r\n", "\n");
  const events: ParsedSseEvent[] = [];
  let offset = 0;
  for (;;) {
    const sep = combined.indexOf("\n\n", offset);
    if (sep === -1) {
      break;
    }
    const block = combined.slice(offset, sep);
    offset = sep + 2;
    let eventName = "message";
    const dataLines: string[] = [];
    for (const line of block.split("\n")) {
      if (line === "" || line.startsWith(":")) {
        continue;
      }
      const colon = line.indexOf(":");
      const field = colon === -1 ? line : line.slice(0, colon);
      let value = colon === -1 ? "" : line.slice(colon + 1);
      if (value.startsWith(" ")) {
        value = value.slice(1);
      }
      if (field === "event") {
        eventName = value;
      } else if (field === "data") {
        dataLines.push(value);
      }
    }
    if (dataLines.length > 0) {
      events.push({ event: eventName, data: dataLines.join("\n") });
    }
  }
  return { events, rest: combined.slice(offset) };
}

export interface DashboardStreamCallbacks {
  onOpen?: () => void;
  onStatus?: (event: StreamStatusEvent) => void;
  onPane?: (event: StreamPaneEvent) => void;
  onDimensions?: (event: StreamDimensionsEvent) => void;
  onHeartbeat?: (event: StreamHeartbeatEvent) => void;
  onDisconnect?: () => void;
  onFatal?: (status: number) => void;
}

export function openDashboardStream(
  body: { scope: ViewScope; session_id?: number; panes: StreamPaneSpec[] },
  callbacks: DashboardStreamCallbacks,
): { close: () => void } {
  let closed = false;
  let controller: AbortController | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let backoffMs = 500;

  const scheduleReconnect = () => {
    if (closed) {
      return;
    }
    const delay = backoffMs;
    backoffMs = Math.min(backoffMs * 2, 5_000);
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      void connect();
    }, delay);
  };

  const dispatch = (ev: ParsedSseEvent) => {
    try {
      const parsed = JSON.parse(ev.data) as unknown;
      if (ev.event === "status") {
        callbacks.onStatus?.(parsed as StreamStatusEvent);
      } else if (ev.event === "pane") {
        callbacks.onPane?.(parsed as StreamPaneEvent);
      } else if (ev.event === "dimensions") {
        callbacks.onDimensions?.(parsed as StreamDimensionsEvent);
      } else if (ev.event === "heartbeat") {
        callbacks.onHeartbeat?.(parsed as StreamHeartbeatEvent);
      }
    } catch {}
  };

  const connect = async () => {
    if (closed) {
      return;
    }
    const ctrl = new AbortController();
    controller = ctrl;
    let readyTimer: ReturnType<typeof setTimeout> | null = setTimeout(() => {
      readyTimer = null;
      ctrl.abort();
    }, 5_000);
    const clearReadyTimer = () => {
      if (readyTimer !== null) {
        clearTimeout(readyTimer);
        readyTimer = null;
      }
    };
    try {
      const response = await fetch("/api/query/stream", {
        method: "POST",
        credentials: "same-origin",
        headers: {
          "Content-Type": "application/json",
          Accept: "text/event-stream",
        },
        body: JSON.stringify(body),
        signal: ctrl.signal,
      });
      if (closed) {
        return;
      }
      if (!response.ok || !response.body) {
        clearReadyTimer();
        if (
          response.status === 400 ||
          response.status === 401 ||
          response.status === 403 ||
          response.status === 404 ||
          response.status === 503
        ) {
          closed = true;
          callbacks.onFatal?.(response.status);
          return;
        }
        callbacks.onDisconnect?.();
        scheduleReconnect();
        return;
      }
      backoffMs = 500;
      callbacks.onOpen?.();

      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      const handleEvent = (ev: ParsedSseEvent) => {
        if (ev.event === "status") {
          clearReadyTimer();
        }
        dispatch(ev);
      };
      const pump = async (): Promise<void> => {
        const { done, value } = await reader.read();
        if (done || closed) {
          return;
        }
        const chunk = decoder.decode(value, { stream: true });
        const parsed = parseSseChunk(buffer, chunk);
        buffer = parsed.rest;
        for (const ev of parsed.events) {
          handleEvent(ev);
        }
        await pump();
      };
      try {
        await pump();
        const trailing = decoder.decode();
        if (trailing !== "" && !closed) {
          const parsed = parseSseChunk(buffer, trailing);
          for (const ev of parsed.events) {
            handleEvent(ev);
          }
        }
      } finally {
        reader.releaseLock();
      }
      if (!closed) {
        callbacks.onDisconnect?.();
        scheduleReconnect();
      }
    } catch {
      if (!closed) {
        callbacks.onDisconnect?.();
        scheduleReconnect();
      }
    } finally {
      clearReadyTimer();
      if (controller === ctrl) {
        controller = null;
      }
    }
  };

  void connect();

  return {
    close: () => {
      closed = true;
      if (reconnectTimer !== null) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      controller?.abort();
      controller = null;
    },
  };
}

export function computeLagMs(
  newestDataNs: number | null,
  serverTimeNs: number | null,
  receivedAtMs: number | null,
  nowMs: number,
): number | null {
  if (newestDataNs === null) {
    return null;
  }
  if (serverTimeNs !== null && receivedAtMs !== null) {
    const serverLag = Math.max(0, (serverTimeNs - newestDataNs) / 1_000_000);
    const elapsed = Math.max(0, nowMs - receivedAtMs);
    return Math.round(serverLag + elapsed);
  }
  return Math.max(0, Math.round(nowMs - newestDataNs / 1_000_000));
}

export function formatDataLag(lagMs: number | null): string | null {
  if (lagMs === null || !Number.isFinite(lagMs)) {
    return null;
  }
  if (lagMs < 2_000) {
    return "fresh";
  }
  const secs = Math.round(lagMs / 1_000);
  if (secs < 60) {
    return `${secs}s behind`;
  }
  const mins = Math.round(secs / 60);
  if (mins < 60) {
    return `${mins}m behind`;
  }
  const hours = Math.round(mins / 60);
  return `${hours}h behind`;
}

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
  streamActive: boolean;
  streamConnected: boolean;
  streamReady: boolean;
  streamKey: string | null;
  watching: boolean;
  watchedMachines: number;
  effectiveIntervalMs: number | null;
  newestDataNs: number | null;
  serverTimeNs: number | null;
  streamUpdatedAt: number | null;
  streamedPanes: Record<string, StreamedPaneData>;
  clockMs: number;
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
  streamActive: false,
  streamConnected: false,
  streamReady: false,
  streamKey: null,
  watching: false,
  watchedMachines: 0,
  effectiveIntervalMs: null,
  newestDataNs: null,
  serverTimeNs: null,
  streamUpdatedAt: null,
  streamedPanes: {},
  clockMs: Date.now(),
};

export const activeView = createState<ActiveViewState>(() => ({ ...INACTIVE_VIEW }));

export function useActiveView<T>(pick: (state: ActiveViewState) => T): () => T {
  return useSelector(activeView, pick);
}

let onMoveToSession: ((sessionId: number) => void) | null = null;
let saveTimer: ReturnType<typeof setTimeout> | null = null;
let currentLayoutSerializer: (() => unknown) | null = null;
let pendingCreate: Promise<SessionRecord | null> | null = null;
let saveChain: Promise<void> = Promise.resolve();
let viewDirty = false;
let wantMoveToSession = false;
let viewMutationSeq = 0;

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

export function setPendingSessionCreation(
  promise: Promise<SessionRecord | null>,
  isCancelled: () => boolean,
): void {
  pendingCreate = promise;
  const finish = async () => {
    const session = await promise;
    if (pendingCreate === promise) {
      pendingCreate = null;
    }
    if (isCancelled() || !session) {
      return;
    }
    activeView.setState({
      sessionId: session.id,
      expiresAt: session.expiresAt,
    });
    if (viewDirty || wantMoveToSession) {
      await flushSessionSaveNow(wantMoveToSession);
    }
  };
  void finish();
}

export function flushActiveViewSave(keepPendingCreate = false): void {
  const cur = activeView.getState();
  const shouldFlush =
    viewDirty && cur.active && cur.isOwner && account.getState().now.kind === "in";
  const snapshot = shouldFlush
    ? {
        sessionId: cur.sessionId,
        scope: cur.scope,
        layout: currentLayoutSerializer ? currentLayoutSerializer() : undefined,
        pending: pendingCreate,
      }
    : null;
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
    saveTimer = null;
  }
  viewDirty = false;
  wantMoveToSession = false;
  if (!keepPendingCreate) {
    pendingCreate = null;
  }
  if (snapshot !== null) {
    void enqueueSave(async () => {
      let sid = snapshot.sessionId;
      if (sid === null && snapshot.pending !== null) {
        const created = await snapshot.pending;
        sid = created?.id ?? null;
      }
      if (sid === null) {
        return;
      }
      await dashboardsApi.updateSession(sid, {
        scope: snapshot.scope,
        ...(snapshot.layout === undefined ? {} : { layout: snapshot.layout }),
        pin: false,
      });
    });
  }
}

export function clearActiveView(): void {
  flushActiveViewSave();
  activeView.setState({ ...INACTIVE_VIEW });
}

export function bumpViewRevision(): void {
  activeView.setState((s) => ({ revision: s.revision + 1 }));
}

export function markViewFetched(): void {
  activeView.setState({ lastFetchedAt: Date.now() });
}

export function markViewLayoutDirty(): void {
  const cur = activeView.getState();
  if (!cur.active || !cur.isOwner) {
    return;
  }
  viewDirty = true;
  viewMutationSeq += 1;
  const shouldMove = cur.mode === "dashboard" && !cur.movedToSession;
  if (shouldMove) {
    wantMoveToSession = true;
    if (cur.sessionId !== null) {
      void flushSessionSaveNow(true);
    }
  } else {
    scheduleSessionSave();
  }
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
  const shouldMove = options?.persist !== false && cur.mode === "dashboard" && !cur.movedToSession;
  const orgChanged = nextScope.org_id !== cur.scope.org_id;
  activeView.setState({
    scope: nextScope,
    revision: cur.revision + 1,
    ...(orgChanged ? { streamedPanes: {}, newestDataNs: null } : {}),
  });
  if (options?.persist !== false) {
    viewDirty = true;
    viewMutationSeq += 1;
    if (shouldMove) {
      wantMoveToSession = true;
      if (cur.sessionId !== null) {
        void flushSessionSaveNow(true);
      }
    } else {
      scheduleSessionSave();
    }
  }
}

function enqueueSave(task: () => Promise<void>): Promise<void> {
  const next = saveChain.then(task, task);
  saveChain = next.catch(() => {});
  return next;
}

function mergeSavedScope(current: ViewScope, saved: ViewScope, orgId: number): ViewScope {
  const normalized = normalizeScope({ ...saved, org_id: orgId });
  if (current.time_range.kind === "relative") {
    return { ...normalized, time_range: current.time_range };
  }
  return normalized;
}

function flushSessionSaveNow(
  moveAfterSave: boolean,
  expectedSessionId?: number | null,
): Promise<void> {
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
    saveTimer = null;
  }
  const boundSessionId =
    expectedSessionId === undefined ? activeView.getState().sessionId : expectedSessionId;
  return enqueueSave(async () => {
    if (pendingCreate !== null) {
      await pendingCreate;
    }
    const cur = activeView.getState();
    if (
      !cur.active ||
      cur.sessionId === null ||
      (boundSessionId !== null && cur.sessionId !== boundSessionId) ||
      !cur.isOwner ||
      account.getState().now.kind !== "in"
    ) {
      return;
    }
    const seqBefore = viewMutationSeq;
    const layout = currentLayoutSerializer ? currentLayoutSerializer() : undefined;
    const saved = await dashboardsApi.updateSession(cur.sessionId, {
      scope: cur.scope,
      ...(layout === undefined ? {} : { layout }),
      pin: false,
    });
    if (viewMutationSeq === seqBefore) {
      viewDirty = false;
    }
    const latest = activeView.getState();
    if (!latest.active || latest.sessionId !== cur.sessionId) {
      return;
    }
    const shouldMove = (moveAfterSave || wantMoveToSession) && !latest.movedToSession;
    const patch: Partial<ActiveViewState> = {
      expiresAt: saved.expiresAt,
      ...(viewMutationSeq === seqBefore
        ? { scope: mergeSavedScope(latest.scope, saved.scope, saved.orgId) }
        : {}),
      ...(shouldMove ? { mode: "session", movedToSession: true } : {}),
    };
    activeView.setState(patch);
    if (shouldMove) {
      wantMoveToSession = false;
      onMoveToSession?.(cur.sessionId);
    }
  });
}

export function scheduleSessionSave(): void {
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
  }
  const scheduledSessionId = activeView.getState().sessionId;
  saveTimer = setTimeout(() => {
    saveTimer = null;
    void flushSessionSaveNow(false, scheduledSessionId);
  }, 350);
}

export async function ensureSessionUrl(): Promise<string | null> {
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
    saveTimer = null;
  }
  let resolvedUrl: string | null = null;
  await enqueueSave(async () => {
    if (pendingCreate !== null) {
      await pendingCreate;
    }
    const cur = activeView.getState();
    if (!cur.active) {
      resolvedUrl = null;
      return;
    }
    let sid = cur.sessionId;
    if (sid === null && currentLayoutSerializer) {
      const seqBefore = viewMutationSeq;
      const created = await dashboardsApi.createSession({
        org_id: cur.orgId,
        project_id: cur.projectId,
        source_builtin: cur.builtin ? cur.dashboardId : null,
        source_dashboard_id: !cur.builtin && cur.dashboardId ? Number(cur.dashboardId) : null,
        layout: currentLayoutSerializer(),
        scope: cur.scope,
      });
      sid = created.id;
      if (viewMutationSeq === seqBefore) {
        viewDirty = false;
      }
      wantMoveToSession = false;
      const latest = activeView.getState();
      if (latest.active) {
        activeView.setState({
          sessionId: created.id,
          mode: "session",
          movedToSession: true,
          expiresAt: created.expiresAt,
          keptAt: created.keptAt,
          ...(viewMutationSeq === seqBefore
            ? { scope: mergeSavedScope(latest.scope, created.scope, created.orgId) }
            : {}),
        });
        onMoveToSession?.(created.id);
      }
    } else if (sid !== null && cur.isOwner && currentLayoutSerializer) {
      const seqBefore = viewMutationSeq;
      const saved = await dashboardsApi.updateSession(sid, {
        scope: cur.scope,
        layout: currentLayoutSerializer(),
        pin: false,
      });
      if (viewMutationSeq === seqBefore) {
        viewDirty = false;
      }
      wantMoveToSession = false;
      const latest = activeView.getState();
      if (latest.active && latest.sessionId === sid) {
        const shouldMove = latest.mode === "dashboard" && !latest.movedToSession;
        activeView.setState({
          ...(shouldMove ? { mode: "session", movedToSession: true } : {}),
          ...(viewMutationSeq === seqBefore
            ? { scope: mergeSavedScope(latest.scope, saved.scope, saved.orgId) }
            : {}),
        });
        if (shouldMove) {
          onMoveToSession?.(sid);
        }
      }
    }
    resolvedUrl = sid === null ? null : `${window.location.origin}/s/${sid}`;
  });
  return resolvedUrl;
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
    case "host.id":
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
