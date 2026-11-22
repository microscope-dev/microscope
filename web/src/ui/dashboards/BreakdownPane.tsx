import { createMemo, For, Match, Show, Switch } from "solid-js";

import {
  addScopeFilter,
  type BreakdownResponse,
  dashboardsApi,
  formatDimensionLabel,
  markViewFetched,
  useActiveView,
} from "../../dashboards";
import { formatPercent, formatWeight } from "../../profile/paint";
import type { PaneProps } from "../../workbench/kinds";
import { useLoaded } from "../Orgs";
import {
  LinkToggle,
  PaneAuthGate,
  PaneEmptyState,
  PaneLagBadge,
  PaneLockedState,
  updatePaneParams,
  usePaneScope,
} from "./paneScope";

const BASE_DIMENSIONS = [
  "service.name",
  "service.version",
  "deployment.environment",
  "host.name",
  "process.name",
  "k8s.pod.name",
  "container.name",
];

export function BreakdownPane(props: PaneProps) {
  const dimension = createMemo(() => {
    const p = props.pane.params as { dimension?: string } | null;
    return p?.dimension ?? "service.name";
  });
  const setDimension = (next: string) => updatePaneParams(props.id, { dimension: next });
  const barDims = useActiveView((v) => v.dimensions);

  const dimOptions = createMemo(() => {
    const set = new Set<string>(BASE_DIMENSIONS);
    const d = barDims();
    if (d) {
      for (const k of Object.keys(d.adhoc)) {
        set.add(k);
      }
    }
    return [...set];
  });

  const ps = usePaneScope(
    () => props.id,
    () => props.pane,
  );
  const loadKey = createMemo(() => {
    const base = ps.queryKey();
    return base === "idle" ? "idle" : `${base}:dim=${dimension()}`;
  });

  const loaded = useLoaded<BreakdownResponse | null>(async () => {
    if (loadKey() === "idle") {
      return null;
    }
    const res = await dashboardsApi.queryBreakdown(ps.scope(), ps.sessionId(), dimension());
    markViewFetched();
    return res;
  }, loadKey);

  const data = createMemo<BreakdownResponse | null>(() => {
    if (ps.streamActive()) {
      const entry = ps.streamed();
      return entry && entry.kind === "breakdown" ? (entry.data as BreakdownResponse) : null;
    }
    return loaded.data();
  });

  return (
    <PaneAuthGate>
      {() => (
        <div class="pane grow" data-testid="fleet-breakdown-pane">
          <div class="profile-details">
            <span class="fleet-pane-label">Breakdown</span>
            <select
              class="input compact"
              aria-label="Breakdown dimension"
              value={dimension()}
              onChange={(e) => setDimension(e.currentTarget.value)}
            >
              <For each={dimOptions()}>
                {(d) => <option value={d}>{formatDimensionLabel(d)}</option>}
              </For>
            </select>
            <LinkToggle linked={ps.linked()} onToggle={ps.toggleLinked} />
            <PaneLagBadge linked={ps.linked()} streamed={ps.streamed()} />
          </div>
          <div class="pane-body">
            <Switch fallback={<div class="empty muted">Loading breakdown…</div>}>
              <Match when={ps.locked() || data()?.locked}>
                <PaneLockedState />
              </Match>
              <Match when={loaded.error()}>
                {(err) => (
                  <div class="empty error" role="alert">
                    {err()}
                  </div>
                )}
              </Match>
              <Match when={data()}>
                {(res) => (
                  <Show when={res().rows.length > 0} fallback={<PaneEmptyState />}>
                    <div class="fleet-breakdown-list">
                      <For each={res().rows}>
                        {(row) => (
                          <button
                            type="button"
                            class="fleet-breakdown-row"
                            onClick={() =>
                              ps.linked() && addScopeFilter(res().dimension, row.value)
                            }
                          >
                            <span class="fleet-breakdown-label">{row.value}</span>
                            <span class="fleet-breakdown-bar-wrap">
                              <span
                                class="fleet-breakdown-bar"
                                style={{ width: `${Math.max(row.share * 100, 1).toFixed(1)}%` }}
                              />
                            </span>
                            <span class="num fleet-breakdown-val">
                              {formatWeight(row.total, res().unit)} ({formatPercent(row.share, 1)})
                            </span>
                            <Show when={row.deltaPct !== undefined}>
                              <span
                                class={`num fleet-breakdown-delta ${(row.delta ?? 0) > 0 ? "fleet-pos" : (row.delta ?? 0) < 0 ? "fleet-neg" : ""}`}
                              >
                                {(row.deltaPct ?? 0) >= 0 ? "+" : ""}
                                {(row.deltaPct ?? 0).toFixed(1)}%
                              </span>
                            </Show>
                          </button>
                        )}
                      </For>
                    </div>
                  </Show>
                )}
              </Match>
            </Switch>
          </div>
        </div>
      )}
    </PaneAuthGate>
  );
}
