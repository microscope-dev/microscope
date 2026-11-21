import type { JSX } from "@solidjs/web";
import { createMemo, createSignal, Match, Show, Switch } from "solid-js";

import { checkAccount, useAccount } from "../../account";
import {
  DEFAULT_SCOPE,
  normalizeScope,
  scopeKey,
  useActiveView,
  type ViewScope,
} from "../../dashboards";
import { KIND } from "../../documents";
import { openMachines } from "../../fleet";
import type { Pane } from "../../workbench/layout";
import { showPane } from "../panes";

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

export function usePaneScope(pane: () => Pane): PaneScopeResult {
  const now = useAccount();
  const barScope = useActiveView((v) => v.scope);
  const barSessionId = useActiveView((v) => v.sessionId);
  const barRevision = useActiveView((v) => v.revision);
  const barLocked = useActiveView((v) => v.locked);
  const barActive = useActiveView((v) => v.active);

  const initialUnlinked = () => {
    const params = pane().params as { unlinkedScope?: Partial<ViewScope> } | null;
    return params?.unlinkedScope ? normalizeScope(params.unlinkedScope) : null;
  };
  const [local, setLocal] = createSignal<ViewScope | null>(initialUnlinked());
  const [localRev, setLocalRev] = createSignal(0);

  const linked = createMemo(() => local() === null);
  const scope = createMemo(() => local() ?? (barActive() ? barScope() : DEFAULT_SCOPE));
  const sessionId = createMemo(() => (linked() ? barSessionId() : null));
  const revision = createMemo(() => (linked() ? barRevision() : localRev()));
  const locked = createMemo(() => linked() && barLocked());
  const signedIn = createMemo(() => now().kind === "in");

  const toggleLinked = () => {
    if (local() === null) {
      setLocal({
        ...barScope(),
        filters: [...barScope().filters],
        project_ids: [...barScope().project_ids],
      });
    } else {
      setLocal(null);
    }
  };

  const setLocalScope = (updater: (prev: ViewScope) => ViewScope) => {
    setLocal((prev) => updater(prev ?? barScope()));
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
