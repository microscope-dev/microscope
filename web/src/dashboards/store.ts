import { account } from "../account";
import { createState, useSelector } from "../state";
import { dashboardsApi } from "./api";
import { normalizeScope } from "./scope";
import {
  type CaptureMeta,
  DEFAULT_SCOPE,
  type DimensionsResponse,
  type Selection,
  type SessionRecord,
  type StreamedPaneData,
  type ViewScope,
} from "./types";

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

/**
 * Active dashboard/session state and background persistence.
 *
 * Opening a dashboard immediately creates a backing session in the background
 * (`pendingCreate`). The first scope or layout edit promotes the view from
 * `/d/<id>` to `/s/<sessionId>` (`wantMoveToSession` -> `movedToSession`) so
 * the user's exploration becomes shareable without mutating the source dashboard.
 */
export interface ActiveViewState {
  active: boolean;
  mode: "dashboard" | "session";
  dashboardId: string | null;
  sessionId: number | null;
  captureId: number | null;
  capture: CaptureMeta | null;
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
  captureId: null,
  capture: null,
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

/**
 * Snapshot and enqueue any unsaved edits before switching views or unmounting.
 * Captures state synchronously before resetting dirty flags so route transitions
 * cannot drop pending changes, and awaits `pendingCreate` if the session ID has
 * not arrived yet.
 */
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

/**
 * Serializes all session create/update requests so out-of-order responses cannot
 * overwrite newer scope or layout edits.
 */
function enqueueSave(task: () => Promise<void>): Promise<void> {
  const next = saveChain.then(task, task);
  saveChain = next.catch(() => {});
  return next;
}

/**
 * Keeps the user's relative time range across saves; the server normalizes/resolves
 * other fields (like org_id) but should not replace a sliding relative window.
 */
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
    // Only clear dirty state or adopt the server's scope if no newer local edit
    // happened while this save was in flight.
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
