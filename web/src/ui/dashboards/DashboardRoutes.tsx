import { useLocation, useNavigate, useParams } from "@solidjs/router";
import { createEffect, createMemo, onCleanup, Show } from "solid-js";

import { checkAccount, useAccount } from "../../account";
import {
  activeView,
  clearActiveView,
  dashboardsApi,
  flushActiveViewSave,
  markViewLayoutDirty,
  normalizeScope,
  registerViewHooks,
  searchToScope,
  setPendingSessionCreation,
  useActiveView,
} from "../../dashboards";
import { KIND } from "../../documents";
import { toHref } from "../../history";
import { loadLayout, saveLayout } from "../../workbench/saved";
import { workbench } from "../../workbench/store";
import { fleetOverviewLayout, showPane } from "../panes";
import { Workbench } from "../Workspace";

let applyingLayout = false;

function applySavedLayout(raw: unknown): void {
  applyingLayout = true;
  try {
    const parsed = loadLayout(JSON.stringify(raw));
    if (parsed.ok) {
      workbench.setState({ layout: parsed.layout });
    } else {
      workbench.setState({ layout: fleetOverviewLayout() });
    }
  } finally {
    applyingLayout = false;
  }
}

/**
 * Bridges workbench layout mutations with session persistence.
 * Ignores programmatic layout loads (`applyingLayout`) and focus-only changes so
 * clicking between panes does not mark a dashboard dirty; `preserveOnSessionMove`
 * keeps `activeView` intact across the `/d/<id>` -> `/s/<id>` promotion.
 */
function useViewLifecycle(preserveOnSessionMove: boolean) {
  const navigate = useNavigate();
  onCleanup(
    registerViewHooks({
      moveToSession: (sessionId) => {
        navigate(toHref(`/s/${sessionId}`), { replace: false });
      },
      serializeLayout: () => JSON.parse(saveLayout(workbench.getState().layout)) as unknown,
    }),
  );
  onCleanup(
    workbench.subscribe((state, prev) => {
      if (applyingLayout || state.layout === prev.layout) {
        return;
      }
      if (
        JSON.stringify({ ...state.layout, focused: "" }) ===
        JSON.stringify({ ...prev.layout, focused: "" })
      ) {
        return;
      }
      if (activeView.getState().active) {
        markViewLayoutDirty();
      }
    }),
  );
  onCleanup(() => {
    if (preserveOnSessionMove && activeView.getState().movedToSession) {
      return;
    }
    clearActiveView();
  });
}

export function DashboardRoute() {
  const params = useParams();
  const location = useLocation();
  const now = useAccount();
  useViewLifecycle(true);

  const dashId = createMemo(() => params.dashboard ?? "fleet-overview");
  const orgParam = createMemo(() => {
    const raw = new URLSearchParams(location.search).get("org");
    return raw !== null && /^[1-9]\d*$/u.test(raw) ? Number(raw) : null;
  });

  createEffect(
    () => [now().kind, dashId(), orgParam()] as const,
    ([kind, id, org]) => {
      if (kind === "checking") {
        void checkAccount();
        return;
      }
      if (kind !== "in") {
        return;
      }
      flushActiveViewSave(true);
      let cancelled = false;
      activeView.setState({ loading: true, error: null });
      const load = async () => {
        try {
          const record = await dashboardsApi.getDashboard(id, org);
          if (cancelled) {
            return;
          }
          flushActiveViewSave();
          const baseScope = normalizeScope({ ...record.scope, org_id: record.orgId });
          const effectiveScope = searchToScope(location.search, baseScope);
          applySavedLayout(record.layout);
          activeView.setState({
            active: true,
            mode: "dashboard",
            dashboardId: record.id,
            sessionId: null,
            captureId: null,
            capture: null,
            title: record.name,
            builtin: record.builtin,
            canEdit: record.canEdit,
            isOwner: true,
            locked: record.locked,
            orgId: record.orgId,
            projectId: record.projectId,
            pinStatus: "none",
            pinnedBytes: 0,
            keptAt: null,
            sharedAt: null,
            expiresAt: null,
            visitorCount: 0,
            notes: record.description,
            scope: effectiveScope,
            loading: false,
            error: null,
            movedToSession: false,
            revision: activeView.getState().revision + 1,
          });
          const createPromise = dashboardsApi
            .createSession({
              org_id: record.orgId,
              project_id: record.projectId,
              source_builtin: record.builtin ? record.id : null,
              source_dashboard_id: record.builtin ? null : Number(record.id),
              layout: record.layout,
              scope: effectiveScope,
            })
            .catch(() => null);
          setPendingSessionCreation(createPromise, () => cancelled);
        } catch (err: unknown) {
          if (!cancelled) {
            activeView.setState({
              loading: false,
              error: err instanceof Error ? err.message : String(err),
            });
          }
        }
      };
      void load();
      return () => {
        cancelled = true;
      };
    },
  );

  const error = useActiveView((v) => v.error);

  return (
    <Show
      when={now().kind === "in"}
      fallback={
        <Workbench focus={null} wants="start">
          {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- styled notice */}
          <div class="workbench-notice" role="status">
            <Show
              when={now().kind === "checking"}
              fallback={
                <>
                  Sign in to view recorded dashboards.{" "}
                  <button type="button" class="link-button" onClick={() => showPane(KIND.account)}>
                    Sign in
                  </button>
                </>
              }
            >
              Checking whether you are signed in…
            </Show>
          </div>
        </Workbench>
      }
    >
      <Workbench focus={null} wants="fleet" start={fleetOverviewLayout}>
        <Show when={error()}>
          {(msg) => (
            <div class="workbench-notice error" role="alert">
              {msg()}
            </div>
          )}
        </Show>
      </Workbench>
    </Show>
  );
}

export function RecordedSessionRoute(props: { sessionId: number }) {
  const now = useAccount();
  useViewLifecycle(false);

  createEffect(
    () => [now().kind, props.sessionId] as const,
    ([kind, id]) => {
      if (kind === "checking") {
        void checkAccount();
        return;
      }
      if (kind !== "in") {
        return;
      }
      const cur = activeView.getState();
      // Skip reloading when arriving via a dashboard -> session promotion;
      // `activeView` already holds the live layout and scope.
      if (cur.active && cur.sessionId === id && cur.movedToSession) {
        return;
      }
      flushActiveViewSave(true);
      let cancelled = false;
      activeView.setState({ loading: true, error: null });
      const load = async () => {
        try {
          const session = await dashboardsApi.getSession(id);
          if (cancelled) {
            return;
          }
          flushActiveViewSave();
          const scope = normalizeScope({ ...session.scope, org_id: session.orgId });
          const isCaptureSession = (session.captureId ?? null) !== null;
          if (!session.locked) {
            applySavedLayout(session.layout);
          }
          activeView.setState({
            active: true,
            mode: "session",
            dashboardId:
              session.sourceBuiltin ??
              (session.sourceDashboardId ? String(session.sourceDashboardId) : null),
            sessionId: session.id,
            captureId: session.captureId ?? null,
            capture: session.capture ?? null,
            title:
              session.name ??
              (isCaptureSession
                ? (session.capture?.name ?? `Session #${session.id}`)
                : `Session #${session.id}`),
            builtin: false,
            canEdit: session.isOwner,
            isOwner: session.isOwner,
            locked: session.locked,
            orgId: session.orgId,
            projectId: session.projectId,
            pinStatus: session.pinStatus,
            pinnedBytes: session.pinnedBytes,
            keptAt: session.keptAt,
            sharedAt: session.sharedAt,
            expiresAt: session.expiresAt,
            visitorCount: session.visitorCount,
            notes: session.notes,
            scope,
            loading: false,
            error: null,
            movedToSession: true,
            revision: activeView.getState().revision + 1,
          });
        } catch (err: unknown) {
          if (!cancelled) {
            activeView.setState({
              active: false,
              loading: false,
              error: err instanceof Error ? err.message : String(err),
            });
          }
        }
      };
      void load();
      return () => {
        cancelled = true;
      };
    },
  );

  const error = useActiveView((v) => v.error);
  const captureId = useActiveView((v) => v.captureId);
  const captureDeleted = useActiveView((v) => v.capture?.deleted === true);
  const locked = useActiveView((v) => v.locked);

  return (
    <Show
      when={now().kind === "in"}
      fallback={
        <Workbench focus={null} wants="start">
          {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- styled notice */}
          <div class="workbench-notice" role="status">
            <Show
              when={now().kind === "checking"}
              fallback={
                <>
                  Sign in to open this session.{" "}
                  <button type="button" class="link-button" onClick={() => showPane(KIND.account)}>
                    Sign in
                  </button>
                </>
              }
            >
              Checking whether you are signed in…
            </Show>
          </div>
        </Workbench>
      }
    >
      <Workbench
        focus={null}
        wants={captureId() === null ? "fleet" : "capture"}
        start={fleetOverviewLayout}
      >
        <Show when={error()}>
          {(msg) => (
            <div class="workbench-notice error" role="alert">
              {msg()}
            </div>
          )}
        </Show>
        <Show when={locked()}>
          {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- styled notice */}
          <div class="workbench-notice" role="status" data-testid="session-locked-notice">
            You do not have permission to view this project.
          </div>
        </Show>
        <Show when={captureDeleted()}>
          <div class="workbench-notice error" role="alert" data-testid="capture-deleted-notice">
            This capture has expired or been removed.
          </div>
        </Show>
      </Workbench>
    </Show>
  );
}
