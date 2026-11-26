import type { JSX } from "@solidjs/web";
import { createMemo, createSignal, Match, Show, Switch } from "solid-js";

import { checkAccount, useAccount } from "../../account";
import { copyText } from "../../clipboard";
import {
  buildCurlCommand,
  buildQueryCommand,
  computeLagMs,
  type CopyQuerySpec,
  type DataGap,
  DEFAULT_SCOPE,
  describeGaps,
  formatDataLag,
  normalizeScope,
  scopeKey,
  type StreamedPaneData,
  type TimeRange,
  useActiveView,
  type ViewScope,
} from "../../dashboards";
import { KIND } from "../../documents";
import { openMachines } from "../../fleet";
import { flash } from "../../tab";
import type { Json, Pane, PaneId } from "../../workbench/layout";
import { change } from "../../workbench/store";
import { useDismiss } from "../dialog";
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
  streamActive: () => boolean;
  streamed: () => StreamedPaneData | null;
}

export function usePaneScope(id: () => PaneId, pane: () => Pane): PaneScopeResult {
  const now = useAccount();
  const barScope = useActiveView((v) => v.scope);
  const barSessionId = useActiveView((v) => v.sessionId);
  const barRevision = useActiveView((v) => v.revision);
  const barLocked = useActiveView((v) => v.locked);
  const barActive = useActiveView((v) => v.active);
  const barStreamActive = useActiveView((v) => v.streamActive);
  const barStreamReady = useActiveView((v) => v.streamReady);
  const barStreamKey = useActiveView((v) => v.streamKey);
  const barStreamedPanes = useActiveView((v) => v.streamedPanes);

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
  const streamed = createMemo<StreamedPaneData | null>(() => {
    if (!linked()) {
      return null;
    }
    const entry = barStreamedPanes()[id()] ?? null;
    if (!entry) {
      return null;
    }
    const curKey = barStreamKey();
    if (entry.streamKey !== undefined && curKey !== null && entry.streamKey !== curKey) {
      return null;
    }
    return entry;
  });
  const streamActive = createMemo(
    () => linked() && barStreamActive() && (!barStreamReady() || streamed() !== null),
  );

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
    signedIn() && !locked() && !streamActive()
      ? scopeKey(scope(), sessionId(), revision())
      : "idle",
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
    streamActive,
    streamed,
  };
}

export function PaneGapsNote(props: { gaps: readonly DataGap[] | undefined }) {
  return (
    <Show when={describeGaps(props.gaps)}>
      {(text) => (
        <span
          class={`pane-gaps ${props.gaps?.some((g) => g.reason === "lost") ? "pane-gaps-lost" : ""}`}
          data-testid="pane-gaps"
          title="Some recorded data could not be read from storage. Retrying gaps fill in once the object is readable again; lost gaps will not."
        >
          {text()}
        </span>
      )}
    </Show>
  );
}

export function PaneLagBadge(props: { linked: boolean; streamed: StreamedPaneData | null }) {
  const clockMs = useActiveView((v) => v.clockMs);
  const label = createMemo(() => {
    if (!props.linked || !props.streamed) {
      return null;
    }
    const pane = props.streamed;
    const lag = computeLagMs(pane.newestDataNs, pane.serverTimeNs, pane.receivedAt, clockMs());
    return formatDataLag(lag);
  });

  return (
    <Show when={label()}>
      {(text) => (
        <span
          class={`pane-lag ${text() === "fresh" ? "pane-lag-fresh" : ""}`}
          data-testid="pane-lag"
        >
          {text()}
        </span>
      )}
    </Show>
  );
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

export function CopyQueryMenu(props: {
  spec: CopyQuerySpec;
  scope: ViewScope;
  sessionId: number | null;
}) {
  const [open, setOpen] = createSignal(false);
  let anchor: HTMLDivElement | undefined;
  useDismiss(
    () => anchor,
    open,
    () => setOpen(false),
  );

  const copyCli = async () => {
    const origin = typeof window === "undefined" ? undefined : window.location.origin;
    const cmd = buildQueryCommand(props.spec, props.scope, props.sessionId, origin);
    setOpen(false);
    if (await copyText(cmd)) {
      flash("Copied microscope query command.");
    } else {
      flash("Could not copy query command.");
    }
  };

  const copyCurl = async () => {
    const origin =
      typeof window === "undefined" ? "https://microscope.dev" : window.location.origin;
    const cmd = buildCurlCommand(props.spec, props.scope, props.sessionId, origin);
    setOpen(false);
    if (await copyText(cmd)) {
      flash("Copied curl API call.");
    } else {
      flash("Could not copy curl command.");
    }
  };

  return (
    <div
      class="anchor copy-query-wrap"
      ref={(node) => {
        anchor = node;
      }}
    >
      <button
        type="button"
        class="tb ghost"
        data-testid="copy-query-btn"
        aria-haspopup="menu"
        aria-expanded={open() ? "true" : "false"}
        onClick={() => setOpen((was) => !was)}
      >
        Copy query
      </button>
      <Show when={open()}>
        <div class="popover menu copy-query-menu" role="menu" aria-label="Copy query">
          <button
            type="button"
            role="menuitem"
            data-testid="copy-query-cli"
            onClick={() => void copyCli()}
          >
            Copy <code>microscope query</code>
          </button>
          <button
            type="button"
            role="menuitem"
            data-testid="copy-query-curl"
            onClick={() => void copyCurl()}
          >
            Copy <code>curl</code> API call
          </button>
        </div>
      </Show>
    </div>
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
