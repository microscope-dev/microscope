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
  updatePaneParams,
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

function svgXFromPointer(
  svg: SVGSVGElement,
  clientX: number,
  clientY: number,
  viewW: number,
  viewH: number,
): number {
  const ctm = svg.getScreenCTM();
  if (ctm) {
    const pt = svg.createSVGPoint();
    pt.x = clientX;
    pt.y = clientY;
    return pt.matrixTransform(ctm.inverse()).x;
  }
  const rect = svg.getBoundingClientRect();
  const scale = Math.min(rect.width / viewW, rect.height / viewH);
  const renderedW = viewW * scale;
  const offsetX = rect.left + (rect.width - renderedW) / 2;
  return (clientX - offsetX) / Math.max(scale, 1e-6);
}

export function OverTimePane(props: PaneProps) {
  const groupBy = createMemo(() => {
    const p = props.pane.params as { groupBy?: string } | null;
    return p?.groupBy ?? "service.version";
  });
  const setGroupBy = (next: string) => updatePaneParams(props.id, { groupBy: next });
  const [hoverIdx, setHoverIdx] = createSignal<number | null>(null);
  const [dragStartNs, setDragStartNs] = createSignal<number | null>(null);
  const [dragEndNs, setDragEndNs] = createSignal<number | null>(null);

  const ps = usePaneScope(
    () => props.id,
    () => props.pane,
  );
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
    const hasCompare = (res?.compareSeries?.[0]?.values.length ?? 0) > 0;
    if (!res || res.timestampsNs.length === 0 || (res.series.length === 0 && !hasCompare)) {
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

    const bucketNs = Math.max(res.bucketNs || 60_000_000_000, 1);
    const firstTs = res.timestampsNs[0] ?? 0;
    const lastTs = res.timestampsNs.at(-1) ?? firstTs;
    const span = Math.max(lastTs - firstTs, 0);

    const xOfTime = (ts: number) => {
      if (span <= 0) {
        return padL + plotW / 2;
      }
      const clamped = Math.max(firstTs, Math.min(lastTs, ts));
      return padL + ((clamped - firstTs) / span) * plotW;
    };
    const xOf = (i: number) => xOfTime(res.timestampsNs[i] ?? firstTs);
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

    const deploys = res.deploys.map((d) => ({
      ...d,
      x: xOfTime(d.timeNs),
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
      firstTs,
      lastTs,
      span,
      bucketNs,
      areas,
      compareLine,
      deploys,
      timestamps: res.timestampsNs,
      series: res.series,
      unit: res.unit,
      xOf,
      xOfTime,
    };
  });

  const timeFromPointer = (event: PointerEvent & { currentTarget: SVGSVGElement }) => {
    const c = chart();
    if (!c) {
      return null;
    }
    const svgX = svgXFromPointer(event.currentTarget, event.clientX, event.clientY, c.w, c.h);
    const frac = Math.max(0, Math.min(1, (svgX - c.padL) / Math.max(c.plotW, 1)));
    const ts = c.span <= 0 ? c.firstTs : Math.round(c.firstTs + frac * c.span);
    let nearest = 0;
    let bestDist = Infinity;
    for (let i = 0; i < c.timestamps.length; i += 1) {
      const dist = Math.abs((c.timestamps[i] ?? 0) - ts);
      if (dist < bestDist) {
        bestDist = dist;
        nearest = i;
      }
    }
    return { ts, bucketTs: c.timestamps[nearest] ?? ts, nearest };
  };

  const onPointerDown = (event: PointerEvent & { currentTarget: SVGSVGElement }) => {
    const hit = timeFromPointer(event);
    if (hit !== null) {
      setDragStartNs(hit.bucketTs);
      setDragEndNs(hit.bucketTs);
    }
  };

  const onPointerMove = (event: PointerEvent & { currentTarget: SVGSVGElement }) => {
    const hit = timeFromPointer(event);
    setHoverIdx(hit ? hit.nearest : null);
    if (dragStartNs() !== null && hit !== null) {
      setDragEndNs(hit.bucketTs);
    }
  };

  const onPointerUp = () => {
    const s = dragStartNs();
    const e = dragEndNs();
    const c = chart();
    setDragStartNs(null);
    setDragEndNs(null);
    if (s !== null && e !== null && s !== e && c) {
      const startTime = Math.min(s, e);
      const endTime = Math.max(s, e) + c.bucketNs;
      if (endTime > startTime) {
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

                      <Show when={dragStartNs() !== null && dragEndNs() !== null}>
                        <rect
                          x={c().xOfTime(Math.min(dragStartNs() ?? 0, dragEndNs() ?? 0))}
                          y={c().padT}
                          width={Math.max(
                            c().xOfTime(Math.max(dragStartNs() ?? 0, dragEndNs() ?? 0)) -
                              c().xOfTime(Math.min(dragStartNs() ?? 0, dragEndNs() ?? 0)),
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
