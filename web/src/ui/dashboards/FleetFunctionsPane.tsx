import { createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import {
  dashboardsApi,
  type FlameResponse,
  markViewFetched,
  setScopeSelection,
} from "../../dashboards";
import { formatPercent, formatWeight } from "../../profile/paint";
import { nameOf, treeFromColumns } from "../../profile/tree";
import type { PaneProps } from "../../workbench/kinds";
import { useLoaded } from "../Orgs";
import {
  LinkToggle,
  PaneAuthGate,
  PaneEmptyState,
  PaneLockedState,
  updatePaneParams,
  usePaneScope,
} from "./paneScope";

interface FnRow {
  name: string;
  self: number;
  total: number;
  share: number;
  baseTotal: number | null;
  delta: number | null;
}

type SortCol = "total" | "self" | "delta";

export function FleetFunctionsPane(props: PaneProps) {
  const [filter, setFilter] = createSignal("");
  const sortBy = createMemo<SortCol>(() => {
    const p = props.pane.params as { sortBy?: string } | null;
    return p?.sortBy === "self" || p?.sortBy === "delta" ? p.sortBy : "total";
  });
  const setSortBy = (next: SortCol) => updatePaneParams(props.id, { sortBy: next });
  const ps = usePaneScope(
    () => props.id,
    () => props.pane,
  );
  const loadKey = createMemo(() => ps.queryKey());

  const loaded = useLoaded<{
    rows: FnRow[];
    unit: string;
    isDiff: boolean;
    locked: boolean;
  } | null>(async () => {
    if (loadKey() === "idle") {
      return null;
    }
    const res: FlameResponse = await dashboardsApi.queryFlame(ps.scope(), ps.sessionId(), false);
    markViewFetched();
    const isDiff = Array.isArray(res.baseTotal) && res.baseWeight !== undefined;
    const tree = treeFromColumns({
      own: Float64Array.from(res.own),
      total: Float64Array.from(res.total),
      key: Uint32Array.from(res.key),
      depth: Uint32Array.from(res.depth),
      keys: res.keys,
      weight: res.weight,
      samples: res.samples,
      baseOwn: isDiff && res.baseOwn ? Float64Array.from(res.baseOwn) : null,
      baseTotal: isDiff && res.baseTotal ? Float64Array.from(res.baseTotal) : null,
      baseWeight: isDiff ? (res.baseWeight ?? 0) : null,
    });

    const agg = new Map<string, { self: number; total: number; baseTotal: number }>();
    for (let i = 0; i < tree.count; i += 1) {
      const name = nameOf(tree, i);
      const entry = agg.get(name) ?? { self: 0, total: 0, baseTotal: 0 };
      entry.self += tree.own[i] ?? 0;
      let ancestorSame = false;
      let p = tree.parent[i] ?? -1;
      while (p >= 0) {
        if (nameOf(tree, p) === name) {
          ancestorSame = true;
          break;
        }
        p = tree.parent[p] ?? -1;
      }
      if (!ancestorSame) {
        entry.total += tree.total[i] ?? 0;
        entry.baseTotal += tree.baseTotal?.[i] ?? 0;
      }
      agg.set(name, entry);
    }

    const rows: FnRow[] = [...agg.entries()].map(([name, val]) => ({
      name,
      self: val.self,
      total: val.total,
      share: tree.weight > 0 ? val.total / tree.weight : 0,
      baseTotal: isDiff ? val.baseTotal : null,
      delta: isDiff ? val.total - val.baseTotal : null,
    }));

    return { rows, unit: res.unit, isDiff, locked: res.locked };
  }, loadKey);

  const current = loaded.data;

  const shown = createMemo(() => {
    const cur = current();
    if (!cur) {
      return [];
    }
    const q = filter().trim().toLowerCase();
    const list = q === "" ? cur.rows : cur.rows.filter((r) => r.name.toLowerCase().includes(q));
    const col = sortBy();
    return list
      .toSorted((a, b) => {
        if (col === "self") {
          return b.self - a.self || a.name.localeCompare(b.name);
        }
        if (col === "delta") {
          return Math.abs(b.delta ?? 0) - Math.abs(a.delta ?? 0) || a.name.localeCompare(b.name);
        }
        return b.total - a.total || a.name.localeCompare(b.name);
      })
      .slice(0, 200);
  });

  return (
    <PaneAuthGate>
      {() => (
        <div class="pane grow" data-testid="fleet-functions-pane">
          <div class="profile-details">
            <input
              type="search"
              class="input profile-search"
              placeholder="Filter functions"
              aria-label="Filter functions"
              value={filter()}
              onInput={(e) => setFilter(e.currentTarget.value)}
            />
            <LinkToggle linked={ps.linked()} onToggle={ps.toggleLinked} />
          </div>
          <div class="pane-body">
            <Switch fallback={<div class="empty muted">Loading functions…</div>}>
              <Match when={ps.locked() || current()?.locked}>
                <PaneLockedState />
              </Match>
              <Match when={loaded.error()}>
                {(err) => (
                  <div class="empty error" role="alert">
                    {err()}
                  </div>
                )}
              </Match>
              <Match when={current()}>
                {(cur) => (
                  <Show when={cur().rows.length > 0} fallback={<PaneEmptyState />}>
                    <table class="fleet-table">
                      <thead>
                        <tr>
                          <th>Function</th>
                          <th class="num">
                            <button
                              type="button"
                              class="link-button"
                              onClick={() => setSortBy("total")}
                            >
                              Total{sortBy() === "total" ? " ↓" : ""}
                            </button>
                          </th>
                          <th class="num">
                            <button
                              type="button"
                              class="link-button"
                              onClick={() => setSortBy("self")}
                            >
                              Self{sortBy() === "self" ? " ↓" : ""}
                            </button>
                          </th>
                          <th class="num">Share</th>
                          <Show when={cur().isDiff}>
                            <th class="num">
                              <button
                                type="button"
                                class="link-button"
                                onClick={() => setSortBy("delta")}
                              >
                                Change{sortBy() === "delta" ? " ↓" : ""}
                              </button>
                            </th>
                          </Show>
                        </tr>
                      </thead>
                      <tbody>
                        <For each={shown()}>
                          {(row) => (
                            <tr>
                              <td>
                                <button
                                  type="button"
                                  class="link-button fleet-fn-link"
                                  onClick={() =>
                                    ps.linked() &&
                                    setScopeSelection({ kind: "function", name: row.name })
                                  }
                                >
                                  {row.name}
                                </button>
                              </td>
                              <td class="num">{formatWeight(row.total, cur().unit)}</td>
                              <td class="num">{formatWeight(row.self, cur().unit)}</td>
                              <td class="num">{formatPercent(row.share, 1)}</td>
                              <Show when={cur().isDiff}>
                                <td
                                  class={`num ${(row.delta ?? 0) > 0 ? "fleet-pos" : (row.delta ?? 0) < 0 ? "fleet-neg" : ""}`}
                                >
                                  {(row.delta ?? 0) >= 0 ? "+" : ""}
                                  {formatWeight(row.delta ?? 0, cur().unit)}
                                </td>
                              </Show>
                            </tr>
                          )}
                        </For>
                      </tbody>
                    </table>
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
