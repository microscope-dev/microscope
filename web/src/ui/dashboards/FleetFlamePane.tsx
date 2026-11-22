import { createMemo, createSignal, Match, Show, Switch } from "solid-js";

import {
  dashboardsApi,
  type FlameResponse,
  markViewFetched,
  setScopeSelection,
} from "../../dashboards";
import { type DiffMode, formatCount } from "../../profile/paint";
import {
  type CallTree,
  findPath,
  matching,
  nameOf,
  pathOf,
  treeFromColumns,
} from "../../profile/tree";
import type { PaneProps } from "../../workbench/kinds";
import { useLoaded } from "../Orgs";
import { FlameGraph } from "../profile/FlameView";
import { useTreeSearch } from "../profile/Search";
import {
  LinkToggle,
  PaneAuthGate,
  PaneEmptyState,
  PaneLagBadge,
  PaneLockedState,
  updatePaneParams,
  usePaneScope,
} from "./paneScope";

interface LoadedFlame {
  tree: CallTree;
  unit: string;
  blocks: number;
  prunedBlocks: number;
  truncated: boolean;
  locked: boolean;
  isDiff: boolean;
}

function buildLoadedFlame(res: FlameResponse): LoadedFlame {
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
    blocks: res.blocks,
    prunedBlocks: res.prunedBlocks,
    truncated: res.truncated,
    locked: res.locked,
    isDiff,
  };
}

export function FleetFlamePane(props: PaneProps) {
  const [search, setSearch] = createSignal("");
  const [zoomPath, setZoomPath] = createSignal<string[] | null>(null);
  const invert = createMemo(() => {
    const p = props.pane.params as { invert?: boolean } | null;
    return p?.invert === true;
  });
  const diffMode = createMemo<DiffMode>(() => {
    const p = props.pane.params as { diffMode?: string } | null;
    return p?.diffMode === "share" ? "share" : "time";
  });
  const setInvert = (next: boolean | ((v: boolean) => boolean)) => {
    const val = typeof next === "function" ? next(invert()) : next;
    updatePaneParams(props.id, { invert: val });
  };
  const setDiffMode = (next: DiffMode) => {
    updatePaneParams(props.id, { diffMode: next });
  };

  const ps = usePaneScope(
    () => props.id,
    () => props.pane,
  );
  const loadKey = createMemo(() => {
    const base = ps.queryKey();
    return base === "idle" ? "idle" : `${base}:inv=${invert() ? 1 : 0}`;
  });

  const loaded = useLoaded<LoadedFlame | null>(async () => {
    if (loadKey() === "idle") {
      return null;
    }
    const res: FlameResponse = await dashboardsApi.queryFlame(ps.scope(), ps.sessionId(), invert());
    markViewFetched();
    return buildLoadedFlame(res);
  }, loadKey);

  const streamedFlame = createMemo<LoadedFlame | null>(() => {
    const entry = ps.streamed();
    if (!entry || entry.kind !== "flame") {
      return null;
    }
    return buildLoadedFlame(entry.data as FlameResponse);
  });

  const current = createMemo<LoadedFlame | null>(() =>
    ps.streamActive() ? streamedFlame() : loaded.data(),
  );
  const tree = createMemo(() => current()?.tree ?? null);
  const searched = useTreeSearch(tree, search);

  const zoom = createMemo(() => {
    const t = tree();
    const path = zoomPath();
    return t && path ? findPath(t, path) : null;
  });

  const selected = createMemo(() => {
    const t = tree();
    const sel = ps.scope().selection;
    if (!t || sel?.kind !== "function") {
      return null;
    }
    return matching(t, (candidate) => candidate === sel.name).nodes;
  });

  const onZoom = (zoomed: CallTree, node: number) => {
    setZoomPath(pathOf(zoomed, node));
    const name = nameOf(zoomed, node);
    if (ps.linked() && name !== "") {
      setScopeSelection({ kind: "function", name });
    }
  };

  const onClear = () => {
    setSearch("");
    setZoomPath(null);
    if (ps.linked()) {
      setScopeSelection(null);
    }
  };

  return (
    <PaneAuthGate>
      {() => (
        <div class="pane grow" data-testid="fleet-flame-pane">
          <div class="profile-details">
            <input
              type="search"
              class="input profile-search"
              placeholder="Search frames"
              aria-label="Search frames"
              value={search()}
              onInput={(e) => setSearch(e.currentTarget.value)}
            />
            <button
              type="button"
              class={`tb ghost ${invert() ? "active" : ""}`}
              onClick={() => {
                setInvert((v) => !v);
                setZoomPath(null);
              }}
            >
              {invert() ? "Bottom-up" : "Top-down"}
            </button>
            <Show when={current()?.isDiff}>
              <select
                class="input compact"
                aria-label="Diff mode"
                value={diffMode()}
                onChange={(e) => setDiffMode(e.currentTarget.value as DiffMode)}
              >
                <option value="time">Diff: time</option>
                <option value="share">Diff: share</option>
              </select>
            </Show>
            <Show when={search() !== "" || zoomPath() !== null}>
              <button type="button" class="tb ghost" onClick={onClear}>
                Clear
              </button>
            </Show>
            <LinkToggle linked={ps.linked()} onToggle={ps.toggleLinked} />
            <Show when={searched().note}>
              {(note) => (
                <span class="muted" data-testid="matched">
                  {note()}
                </span>
              )}
            </Show>
            <Show when={current()}>
              {(flame) => (
                <span class="muted" data-testid="recorded-summary">
                  {formatCount(flame().blocks, "block")} ·{" "}
                  {formatCount(flame().tree.samples, "sample")}
                  {flame().prunedBlocks > 0 ? ` · ${flame().prunedBlocks} pruned` : ""}
                  {flame().truncated ? " · truncated" : ""}
                </span>
              )}
            </Show>
            <PaneLagBadge linked={ps.linked()} streamed={ps.streamed()} />
          </div>
          <div class="pane-body">
            <Switch fallback={<div class="empty muted">Reading the flame graph…</div>}>
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
                {(flame) => (
                  <Show when={flame().tree.count > 0} fallback={<PaneEmptyState />}>
                    <FlameGraph
                      tree={flame().tree}
                      zoom={zoom()}
                      matched={searched().matched}
                      selected={selected()}
                      onZoom={onZoom}
                      unit={flame().unit}
                      mode={flame().isDiff ? diffMode() : undefined}
                    />
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
