import type { JSX } from "@solidjs/web";
import { createMemo, createSignal, Match, Show, Switch } from "solid-js";

import { checkAccount, useAccount } from "../../account";
import {
  DEFAULT_SCOPE,
  normalizeScope,
  scopeKey,
  useActiveView,
  type TimeRange,
  type ViewScope,
} from "../../dashboards";
import { KIND } from "../../documents";
import { openMachines } from "../../fleet";
import type { Json, Pane, PaneId } from "../../workbench/layout";
import { change } from "../../workbench/store";
import { showPane } from "../panes";

export function updatePaneParams(id: PaneId, patch: Record<string, Json | undefined>): void {
  change((layout) => {
    const existing = layout.panes[id];
    if (!existing) {
      return layout;
    }
    const base: Record<string, Json> =
      existing.params !== null &&
      typeof existing.params === "object" &&
      !Array.isArray(existing.params)
        ? { ...(existing.params as Record<string, Json>) }
        : {};
    for (const [key, value] of Object.entries(patch)) {
      if (value === undefined) {
        delete base[key];
      } else {
        base[key] = value;
      }
    }
    return {
      ...layout,
      panes: {
        ...layout.panes,
        [id]: { ...existing, params: base },
      },
    };
  });
}

function freezeTimeRangeNow(range: TimeRange, nowNs: number): TimeRange {
  if (range.kind !== "relative") {
    return range;
  }
  return {
    kind: "absolute",
    start_time: nowNs - range.duration_secs * 1_000_000_000,
    end_time: nowNs + 60_000_000_000,
  };
}

function freezeScopeNow(scope: ViewScope): ViewScope {
  const nowNs = Date.now() * 1_000_000;
  const compare =
    scope.compare && scope.compare.kind === "custom" && scope.compare.time_range
      ? {
          ...scope.compare,
          time_range: freezeTimeRangeNow(scope.compare.time_range, nowNs),
        }
      : scope.compare;
  return {
    ...scope,
    filters: [...scope.filters],
    project_ids: [...scope.project_ids],
    time_range: freezeTimeRangeNow(scope.time_range, nowNs),
    compare: compare ?? null,
  };
}

export interface PaneScopeResult {
  scope: () => ViewScope;
  sessionId: () => number | null;
  revision: () => number;
  linked: () => boolean;
  locked: () => boolean;
  toggleLinked: () => void;
  setLocalScope: (updater: (prev: ViewScope) => ViewScope) => void;
  queryKey: () => string;
  signedIn: () => boolean;
}

export function usePaneScope(id: () => PaneId, pane: () => Pane): PaneScopeResult {
  const now = useAccount();
  const barScope = useActiveView((v) => v.scope);
  const barSessionId = useActiveView((v) => v.sessionId);
  const barRevision = useActiveView((v) => v.revision);
  const barLocked = useActiveView((v) => v.locked);
  const barActive = useActiveView((v) => v.active);

  const [localRev, setLocalRev] = createSignal(0);

  const local = createMemo<ViewScope | null>(() => {
    const params = pane().params as { unlinkedScope?: Partial<ViewScope> } | null;
    return params?.unlinkedScope ? normalizeScope(params.unlinkedScope) : null;
  });

  const linked = createMemo(() => local() === null);
  const scope = createMemo(() => local() ?? (barActive() ? barScope() : DEFAULT_SCOPE));
  const sessionId = createMemo(() => (linked() ? barSessionId() : null));
  const revision = createMemo(() => (linked() ? barRevision() : localRev()));
  const locked = createMemo(() => linked() && barLocked());
  const signedIn = createMemo(() => now().kind === "in");

  const toggleLinked = () => {
    if (local() === null) {
      const copy = freezeScopeNow(barScope());
      updatePaneParams(id(), { unlinkedScope: copy as unknown as Json });
    } else {
      updatePaneParams(id(), { unlinkedScope: undefined });
    }
    setLocalRev((r) => r + 1);
  };

  const setLocalScope = (updater: (prev: ViewScope) => ViewScope) => {
    const next = updater(local() ?? barScope());
    updatePaneParams(id(), { unlinkedScope: next as unknown as Json });
    setLocalRev((r) => r + 1);
  };

  const queryKey = createMemo(() =>
    signedIn() && !locked() ? scopeKey(scope(), sessionId(), revision()) : "idle",
  );

  return {
    scope,
    sessionId,
    revision,
    linked,
    locked,
    toggleLinked,
    setLocalScope,
    queryKey,
    signedIn,
  };
}

export function LinkToggle(props: { linked: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      class={`tb ghost fleet-link-toggle ${props.linked ? "is-linked" : "is-unlinked"}`}
      title={
        props.linked
          ? "Linked to scope bar (click to unlink)"
          : "Independent scope (click to re-link)"
      }
      onClick={props.onToggle}
    >
      {props.linked ? "Linked" : "Unlinked"}
    </button>
  );
}

export function PaneLockedState() {
  return (
    <div class="empty fleet-locked" data-testid="pane-locked">
      <div class="fleet-locked-icon" aria-hidden="true">
        🔒
      </div>
      <p>You do not have permission to view this project.</p>
    </div>
  );
}

function openAddMachines() {
  openMachines({ view: "add" });
}

function openAccountSignIn() {
  void checkAccount();
  showPane(KIND.account);
}

export function PaneEmptyState(props: { message?: string }) {
  return (
    <div class="empty muted fleet-empty">
      <p>{props.message ?? "No recorded data in this scope yet."}</p>
      <div class="actions">
        <button type="button" class="link-button" onClick={openAddMachines}>
          Add machines…
        </button>
      </div>
    </div>
  );
}

export function PaneAuthGate(props: { children: () => JSX.Element }) {
  const now = useAccount();
  return (
    <Show
      when={now().kind === "in"}
      fallback={
        <div class="pane-body pane-text">
          <Switch
            fallback={
              <>
                <p>Sign in to view recorded fleet data.</p>
                <div class="actions">
                  <button type="button" class="button" onClick={openAccountSignIn}>
                    Sign in
                  </button>
                </div>
              </>
            }
          >
            <Match when={now().kind === "checking"}>
              <p class="muted">Checking whether you are signed in…</p>
            </Match>
            <Match when={now().kind === "unavailable"}>
              <p class="muted">Recorded dashboards require the services backend.</p>
            </Match>
          </Switch>
        </div>
      }
    >
      {props.children()}
    </Show>
  );
}
