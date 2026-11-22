import { createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import {
  dashboardsApi,
  type FlameResponse,
  markViewFetched,
  setScopeSelection,
} from "../../dashboards";
import { formatPercent, formatWeight } from "../../profile/paint";
import { type CallTree, nameOf, treeFromColumns } from "../../profile/tree";
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

function buildLoadedTree(res: FlameResponse) {
  const isDiff = Array.isArray(res.baseTotal) && res.baseWeight !== undefined;
  return {
    tree: treeFromColumns({
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
    }),
    unit: res.unit,
    locked: res.locked,
  };
}

export function FleetTreePane(props: PaneProps) {
  const [expanded, setExpanded] = createSignal<Set<number>>(new Set());
  const invert = createMemo(() => {
    const p = props.pane.params as { invert?: boolean } | null;
    return p?.invert === true;
  });
  const setInvert = (next: boolean | ((v: boolean) => boolean)) => {
    const val = typeof next === "function" ? next(invert()) : next;
    updatePaneParams(props.id, { invert: val });
  };
  const ps = usePaneScope(
    () => props.id,
    () => props.pane,
  );
  const loadKey = createMemo(() => {
    const base = ps.queryKey();
    return base === "idle" ? "idle" : `${base}:tree=${invert() ? 1 : 0}`;
  });

  const loaded = useLoaded<{ tree: CallTree; unit: string; locked: boolean } | null>(async () => {
    if (loadKey() === "idle") {
      return null;
    }
    const res: FlameResponse = await dashboardsApi.queryFlame(ps.scope(), ps.sessionId(), invert());
    markViewFetched();
    return buildLoadedTree(res);
  }, loadKey);

  const streamed = createMemo(() => {
    const entry = ps.streamed();
    if (!entry || entry.kind !== "flame") {
      return null;
    }
    return buildLoadedTree(entry.data as FlameResponse);
  });

  const current = createMemo(() => (ps.streamActive() ? streamed() : loaded.data()));

  const visibleRows = createMemo(() => {
    const cur = current();
    if (!cur || cur.tree.count === 0) {
      return [];
    }
    const { tree } = cur;
    const open = expanded();
    const out: { node: number; depth: number; hasChildren: boolean; isOpen: boolean }[] = [];
    const walk = (start: number, end: number, depth: number) => {
      let i = start;
      while (i < end) {
        const nodeEnd = tree.end[i] ?? i + 1;
        const hasChildren = nodeEnd > i + 1;
        const isOpen = open.has(i) || (depth === 0 && open.size === 0 && i === 0);
        out.push({ node: i, depth, hasChildren, isOpen });
        if (hasChildren && isOpen && out.length < 400) {
          walk(i + 1, nodeEnd, depth + 1);
        }
        i = nodeEnd;
      }
    };
    walk(0, tree.count, 0);
    return out;
  });

  const toggle = (node: number) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(node)) {
        next.delete(node);
      } else {
        next.add(node);
      }
      return next;
    });
  };

  return (
    <PaneAuthGate>
      {() => (
        <div class="pane grow" data-testid="fleet-tree-pane">
          <div class="profile-details">
            <button
              type="button"
              class={`tb ghost ${invert() ? "active" : ""}`}
              onClick={() => {
                setInvert((v) => !v);
                setExpanded(new Set<number>());
              }}
            >
              {invert() ? "Bottom-up" : "Top-down"}
            </button>
            <LinkToggle linked={ps.linked()} onToggle={ps.toggleLinked} />
            <PaneLagBadge linked={ps.linked()} streamed={ps.streamed()} />
          </div>
          <div class="pane-body">
            <Switch fallback={<div class="empty muted">Loading call tree…</div>}>
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
                  <Show when={cur().tree.count > 0} fallback={<PaneEmptyState />}>
                    <table class="fleet-table">
                      <thead>
                        <tr>
                          <th>Function</th>
                          <th class="num">Total</th>
                          <th class="num">Self</th>
                          <th class="num">Share</th>
                          <Show when={cur().tree.baseTotal !== null}>
                            <th class="num">Change</th>
                          </Show>
                        </tr>
                      </thead>
                      <tbody>
                        <For each={visibleRows()}>
                          {(row) => {
                            const t = cur().tree;
                            const tot = t.total[row.node] ?? 0;
                            const own = t.own[row.node] ?? 0;
                            const name = nameOf(t, row.node);
                            const share = t.weight > 0 ? tot / t.weight : 0;
                            const baseTot = t.baseTotal?.[row.node] ?? 0;
                            const delta = tot - baseTot;
                            return (
                              <tr>
                                <td style={{ "padding-left": `${row.depth * 14 + 6}px` }}>
                                  <Show
                                    when={row.hasChildren}
                                    fallback={<span class="fleet-tree-spacer" />}
                                  >
                                    <button
                                      type="button"
                                      class="fleet-tree-toggle"
                                      aria-label={row.isOpen ? "Collapse" : "Expand"}
                                      onClick={() => toggle(row.node)}
                                    >
                                      {row.isOpen ? "▾" : "▸"}
                                    </button>
                                  </Show>
                                  <button
                                    type="button"
                                    class="link-button fleet-fn-link"
                                    onClick={() =>
                                      ps.linked() && setScopeSelection({ kind: "function", name })
                                    }
                                  >
                                    {name}
                                  </button>
                                </td>
                                <td class="num">{formatWeight(tot, cur().unit)}</td>
                                <td class="num">{formatWeight(own, cur().unit)}</td>
                                <td class="num">{formatPercent(share, 1)}</td>
                                <Show when={t.baseTotal !== null}>
                                  <td
                                    class={`num ${delta > 0 ? "fleet-pos" : delta < 0 ? "fleet-neg" : ""}`}
                                  >
                                    {delta >= 0 ? "+" : ""}
                                    {formatWeight(delta, cur().unit)}
                                  </td>
                                </Show>
                              </tr>
                            );
                          }}
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
