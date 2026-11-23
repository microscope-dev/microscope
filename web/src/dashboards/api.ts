import { request } from "../orgs";
import type {
  BreakdownResponse,
  ByMachineResponse,
  DashboardRecord,
  DashboardsListResponse,
  DimensionsResponse,
  FlameResponse,
  FlushReport,
  OverTimeResponse,
  PinEstimate,
  SessionRecord,
  SessionsListResponse,
  ViewScope,
} from "./types";

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
