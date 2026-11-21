import { createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import {
  dashboardsApi,
  formatTimeShort,
  markViewFetched,
  type OverTimeResponse,
  updateViewScope,
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

const PALETTE = [
  "#3b82f6",
  "#10b981",
  "#f59e0b",
  "#8b5cf6",
  "#ec4899",
  "#14b8a6",
  "#f97316",
  "#6366f1",
];

const GROUP_OPTIONS = [
  { value: "service.version", label: "Version" },
  { value: "service.name", label: "Service" },
  { value: "deployment.environment", label: "Environment" },
  { value: "host.name", label: "Machine" },
  { value: "total", label: "Total" },
];

export function OverTimePane(props: PaneProps) {
  const initialGroup = () => {
    const p = props.pane.params as { groupBy?: string } | null;
    return p?.groupBy ?? "service.version";
  };
  const [groupBy, setGroupBy] = createSignal(initialGroup());
  const [hoverIdx, setHoverIdx] = createSignal<number | null>(null);
  const [dragStart, setDragStart] = createSignal<number | null>(null);
  const [dragEnd, setDragEnd] = createSignal<number | null>(null);

  const ps = usePaneScope(() => props.pane);
  const loadKey = createMemo(() => {
    const base = ps.queryKey();
    return base === "idle" ? "idle" : `${base}:g=${groupBy()}`;
  });

  const loaded = useLoaded<OverTimeResponse | null>(async () => {
    if (loadKey() === "idle") {
      return null;
    }
    const res = await dashboardsApi.queryOverTime(ps.scope(), ps.sessionId(), groupBy());
    markViewFetched();
    return res;
  }, loadKey);

  const data = loaded.data;

  const chart = createMemo(() => {
    const res = data();
    if (!res || res.timestampsNs.length === 0 || res.series.length === 0) {
      return null;
    }
    const n = res.timestampsNs.length;
    const w = 680;
    const h = 190;
    const padL = 44;
    const padR = 14;
    const padT = 14;
    const padB = 26;
    const plotW = w - padL - padR;
    const plotH = h - padT - padB;

    const stacked: { label: string; color: string; tops: number[]; bottoms: number[] }[] = [];
    const running = Array.from({ length: n }, () => 0);
    for (const [idx, s] of res.series.entries()) {
      const bottoms = [...running];
      const tops = s.values.map((v, i) => {
        const next = (running[i] ?? 0) + v;
        running[i] = next;
        return next;
      });
      stacked.push({
        label: s.label,
        color: PALETTE[idx % PALETTE.length] as string,
        tops,
        bottoms,
      });
    }

    const cmpVals = res.compareSeries?.[0]?.values ?? [];
    const maxVal = Math.max(...running, ...cmpVals, 0.001);

    const xOf = (i: number) => (n <= 1 ? padL + plotW / 2 : padL + (i / (n - 1)) * plotW);
    const yOf = (v: number) => padT + plotH - (v / maxVal) * plotH;

    const areas = stacked.map((layer) => {
      const topPts = layer.tops.map((v, i) => `${xOf(i).toFixed(1)},${yOf(v).toFixed(1)}`);
      const botPts = layer.bottoms
        .map((v, i) => `${xOf(i).toFixed(1)},${yOf(v).toFixed(1)}`)
        .toReversed();
      return {
        label: layer.label,
        color: layer.color,
        d: `M ${topPts.join(" L ")} L ${botPts.join(" L ")} Z`,
      };
    });

    const compareLine =
      cmpVals.length === n
        ? `M ${cmpVals.map((v, i) => `${xOf(i).toFixed(1)},${yOf(v).toFixed(1)}`).join(" L ")}`
        : null;

    const firstTs = res.timestampsNs[0] ?? 0;
    const lastTs = res.timestampsNs.at(-1) ?? firstTs;
    const span = Math.max(lastTs - firstTs, 1);
    const deploys = res.deploys.map((d) => ({
      ...d,
      x: n <= 1 ? padL + plotW / 2 : padL + ((d.timeNs - firstTs) / span) * plotW,
    }));

    return {
      w,
      h,
      padL,
      padR,
      padT,
      padB,
      plotW,
      plotH,
      maxVal,
      n,
      areas,
      compareLine,
      deploys,
      timestamps: res.timestampsNs,
      series: res.series,
      unit: res.unit,
      xOf,
    };
  });

  const indexFromPointer = (event: PointerEvent & { currentTarget: SVGSVGElement }) => {
    const c = chart();
    if (!c) {
      return null;
    }
    const rect = event.currentTarget.getBoundingClientRect();
    const relX = ((event.clientX - rect.left) / Math.max(rect.width, 1)) * c.w;
    const frac = Math.max(0, Math.min(1, (relX - c.padL) / Math.max(c.plotW, 1)));
    return Math.round(frac * Math.max(c.n - 1, 0));
  };

  const onPointerDown = (event: PointerEvent & { currentTarget: SVGSVGElement }) => {
    const idx = indexFromPointer(event);
    if (idx !== null) {
      setDragStart(idx);
      setDragEnd(idx);
    }
  };

  const onPointerMove = (event: PointerEvent & { currentTarget: SVGSVGElement }) => {
    const idx = indexFromPointer(event);
    setHoverIdx(idx);
    if (dragStart() !== null && idx !== null) {
      setDragEnd(idx);
    }
  };

  const onPointerUp = () => {
    const s = dragStart();
    const e = dragEnd();
    const c = chart();
    setDragStart(null);
    setDragEnd(null);
    if (s !== null && e !== null && s !== e && c) {
      const lo = Math.min(s, e);
      const hi = Math.max(s, e);
      const startTime = c.timestamps[lo] ?? 0;
      const endTime = (c.timestamps[hi] ?? startTime) + 60_000_000_000;
      if (ps.linked()) {
        updateViewScope((prev) => ({
          ...prev,
          time_range: { kind: "absolute", start_time: startTime, end_time: endTime },
        }));
      } else {
        ps.setLocalScope((prev) => ({
          ...prev,
          time_range: { kind: "absolute", start_time: startTime, end_time: endTime },
        }));
      }
    }
  };

  const selectionTitle = createMemo(() => {
    const sel = ps.scope().selection;
    if (!sel) {
      return "CPU over time";
    }
    if (sel.kind === "function") {
      return `${sel.name} over time`;
    }
    if (sel.kind === "zone") {
      return `Zone ${sel.name} over time`;
    }
    return `Syscall ${sel.name ?? sel.number ?? ""} over time`;
  });

  return (
    <PaneAuthGate>
      {() => (
        <div class="pane grow" data-testid="fleet-over-time-pane">
          <div class="profile-details">
            <span class="fleet-pane-label">{selectionTitle()}</span>
            <select
              class="input compact"
              aria-label="Group by"
              value={groupBy()}
              onChange={(e) => setGroupBy(e.currentTarget.value)}
            >
              <For each={GROUP_OPTIONS}>
                {(opt) => <option value={opt.value}>By {opt.label}</option>}
              </For>
            </select>
            <LinkToggle linked={ps.linked()} onToggle={ps.toggleLinked} />
            <Show when={data()}>
              {(res) => (
                <span class="muted">
                  {res().blocks} blocks
                  {res().prunedBlocks > 0 ? ` · ${res().prunedBlocks} pruned` : ""}
                </span>
              )}
            </Show>
          </div>
          <div class="pane-body fleet-chart-body">
            <Switch fallback={<div class="empty muted">Loading time series…</div>}>
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
              <Match when={data() && !chart()}>
                <PaneEmptyState />
              </Match>
              <Match when={chart()}>
                {(c) => (
                  <div class="fleet-chart-wrap">
                    <svg
                      class="fleet-chart-svg"
                      viewBox={`0 0 ${c().w} ${c().h}`}
                      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- interactive SVG chart
                      role="img"
                      aria-label={selectionTitle()}
                      onPointerDown={onPointerDown}
                      onPointerMove={onPointerMove}
                      onPointerUp={onPointerUp}
                      onPointerLeave={() => setHoverIdx(null)}
                    >
                      <line
                        x1={c().padL}
                        y1={c().padT}
                        x2={c().padL}
                        y2={c().padT + c().plotH}
                        stroke="var(--line-strong)"
                      />
                      <line
                        x1={c().padL}
                        y1={c().padT + c().plotH}
                        x2={c().w - c().padR}
                        y2={c().padT + c().plotH}
                        stroke="var(--line-strong)"
                      />
                      <text
                        x={c().padL - 6}
                        y={c().padT + 8}
                        text-anchor="end"
                        class="fleet-axis-text"
                      >
                        {c().maxVal.toFixed(2)}
                      </text>
                      <text
                        x={c().padL - 6}
                        y={c().padT + c().plotH}
                        text-anchor="end"
                        class="fleet-axis-text"
                      >
                        0
                      </text>
                      <text x={c().padL} y={c().h - 6} class="fleet-axis-text">
                        {formatTimeShort(c().timestamps[0] ?? 0)}
                      </text>
                      <text
                        x={c().w - c().padR}
                        y={c().h - 6}
                        text-anchor="end"
                        class="fleet-axis-text"
                      >
                        {formatTimeShort(c().timestamps.at(-1) ?? 0)}
                      </text>

                      <For each={c().areas}>
                        {(area) => (
                          <path
                            d={area.d}
                            fill={area.color}
                            fill-opacity="0.55"
                            stroke={area.color}
                            stroke-width="1.5"
                          />
                        )}
                      </For>

                      <Show when={c().compareLine}>
                        {(d) => (
                          <path
                            d={d()}
                            fill="none"
                            stroke="var(--ink-2)"
                            stroke-width="1.5"
                            stroke-dasharray="4 3"
                          />
                        )}
                      </Show>

                      <For each={c().deploys}>
                        {(dep) => (
                          <g>
                            <line
                              x1={dep.x}
                              y1={c().padT}
                              x2={dep.x}
                              y2={c().padT + c().plotH}
                              stroke="var(--changed)"
                              stroke-dasharray="3 2"
                            />
                            <text x={dep.x + 3} y={c().padT + 10} class="fleet-deploy-text">
                              {dep.version}
                            </text>
                          </g>
                        )}
                      </For>

                      <Show when={dragStart() !== null && dragEnd() !== null}>
                        <rect
                          x={c().xOf(Math.min(dragStart() ?? 0, dragEnd() ?? 0))}
                          y={c().padT}
                          width={Math.max(
                            c().xOf(Math.max(dragStart() ?? 0, dragEnd() ?? 0)) -
                              c().xOf(Math.min(dragStart() ?? 0, dragEnd() ?? 0)),
                            2,
                          )}
                          height={c().plotH}
                          fill="var(--accent)"
                          fill-opacity="0.18"
                        />
                      </Show>

                      <Show when={hoverIdx() !== null}>
                        <line
                          x1={c().xOf(hoverIdx() ?? 0)}
                          y1={c().padT}
                          x2={c().xOf(hoverIdx() ?? 0)}
                          y2={c().padT + c().plotH}
                          stroke="var(--ink-3)"
                          stroke-dasharray="2 2"
                        />
                      </Show>
                    </svg>

                    <div class="fleet-legend">
                      <For each={c().areas}>
                        {(area) => (
                          <span class="fleet-legend-item">
                            <span class="fleet-legend-swatch" style={{ background: area.color }} />
                            <span>{area.label}</span>
                          </span>
                        )}
                      </For>
                      <Show when={c().compareLine}>
                        <span class="fleet-legend-item muted">--- compare</span>
                      </Show>
                      <Show when={hoverIdx() !== null}>
                        <span class="fleet-legend-hover muted">
                          {formatTimeShort(c().timestamps[hoverIdx() ?? 0] ?? 0)} ·{" "}
                          {c()
                            .series.reduce((acc, s) => acc + (s.values[hoverIdx() ?? 0] ?? 0), 0)
                            .toFixed(3)}{" "}
                          {c().unit}
                        </span>
                      </Show>
                    </div>
                  </div>
                )}
              </Match>
            </Switch>
          </div>
        </div>
      )}
    </PaneAuthGate>
  );
}
