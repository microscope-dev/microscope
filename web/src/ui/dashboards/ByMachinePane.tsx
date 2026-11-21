import { createMemo, For, Match, Show, Switch } from "solid-js";

import {
  type ByMachineResponse,
  dashboardsApi,
  markViewFetched,
  setSingleScopeFilter,
} from "../../dashboards";
import type { PaneProps } from "../../workbench/kinds";
import { useLoaded } from "../Orgs";
import {
  LinkToggle,
  PaneAuthGate,
  PaneEmptyState,
  PaneLockedState,
  usePaneScope,
} from "./paneScope";

function formatMachineMetric(val: number, unit: string): string {
  return unit === "percent" ? `${val.toFixed(1)}%` : `${val.toFixed(2)} cores`;
}

export function ByMachinePane(props: PaneProps) {
  const ps = usePaneScope(() => props.pane);
  const loadKey = createMemo(() => ps.queryKey());

  const loaded = useLoaded<ByMachineResponse | null>(async () => {
    if (loadKey() === "idle") {
      return null;
    }
    const res = await dashboardsApi.queryByMachine(ps.scope(), ps.sessionId());
    markViewFetched();
    return res;
  }, loadKey);

  const data = loaded.data;

  const maxCell = createMemo(() => {
    const res = data();
    if (!res) {
      return 1;
    }
    let m = 0.001;
    for (const row of res.machines) {
      for (const v of row.values) {
        if (v > m) {
          m = v;
        }
      }
    }
    return m;
  });

  return (
    <PaneAuthGate>
      {() => (
        <div class="pane grow" data-testid="fleet-by-machine-pane">
          <div class="profile-details">
            <span class="fleet-pane-label">By machine (outliers first)</span>
            <LinkToggle linked={ps.linked()} onToggle={ps.toggleLinked} />
            <Show when={data()}>
              {(res) => (
                <span class="muted">
                  {res().machines.length} machines · median{" "}
                  {formatMachineMetric(res().median, res().unit)}
                </span>
              )}
            </Show>
          </div>
          <div class="pane-body">
            <Switch fallback={<div class="empty muted">Loading machines…</div>}>
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
                  <Show when={res().machines.length > 0} fallback={<PaneEmptyState />}>
                    <div class="fleet-machine-list">
                      <For each={res().machines}>
                        {(machine) => {
                          const cols = Math.max(machine.values.length, 1);
                          const cellW = 160 / cols;
                          return (
                            <button
                              type="button"
                              class="fleet-machine-row"
                              data-testid="machine-heatmap-row"
                              onClick={() =>
                                ps.linked() && setSingleScopeFilter("host.name", machine.hostname)
                              }
                            >
                              <span class="fleet-machine-name">
                                {machine.hostname}
                                <Show when={machine.environment !== ""}>
                                  <span class="fleet-env-pill">{machine.environment}</span>
                                </Show>
                              </span>
                              <svg
                                class="fleet-heatmap-strip"
                                viewBox="0 0 160 16"
                                preserveAspectRatio="none"
                                aria-hidden="true"
                              >
                                <For each={machine.values}>
                                  {(val, idx) => {
                                    const intensity = Math.min(1, Math.max(0.08, val / maxCell()));
                                    return (
                                      <rect
                                        x={idx() * cellW}
                                        y={1}
                                        width={Math.max(cellW - 0.8, 1)}
                                        height={14}
                                        rx={1}
                                        fill="var(--accent)"
                                        fill-opacity={val > 0 ? intensity.toFixed(2) : "0.06"}
                                      />
                                    );
                                  }}
                                </For>
                              </svg>
                              <span class="fleet-machine-val num">
                                {formatMachineMetric(machine.overall, res().unit)}
                              </span>
                            </button>
                          );
                        }}
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
