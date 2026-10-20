import { useEffect, useMemo, useRef, useState } from "react";
import { formatRange, rangeOf, withLook } from "../../profile/look";
import { formatCount, formatDuration, formatInstant, formatSpan, hue } from "../../profile/paint";
import {
  blockAt,
  decodeZones,
  describeFlags,
  panned,
  plotPath,
  type Track,
  threadName,
  ticks,
  tracks,
  type ZoneBlocks,
  zoomed,
} from "../../profile/zones";
import type { ProfileMark, ProfilePlot } from "../../protocol";
import { useShownScheme } from "../../theme";
import { NAMES, prepare, useWidth } from "./canvas";
import { token, useAnswer, useProfile } from "./context";

/** A depth's row and the gap between tracks, in CSS pixels. */
const ROW = 16;
const GAP = 6;
const PLOT = 40;
const RULER = 18;
/** How far a pointer moves before a press pans rather than clicks. */
const DRAG = 4;

/** What the pointer is over. */
interface Tip {
  x: number;
  y: number;
  lines: string[];
}

/**
 * Each thread's zones and labels over the selected range, a row per
 * depth, with the program's messages and frames as ticks above. Zones
 * under three pixels wide fold into one grey block; a click on a block
 * selects its span, which every view then reads. Ctrl and the wheel, or a
 * pinch, zoom about the pointer, and dragging pans.
 */
export function ZoneTracks() {
  const { profile, summary, query, look, setLook } = useProfile();
  const [box, width] = useWidth();
  const canvas = useRef<HTMLCanvasElement>(null);
  const [tip, setTip] = useState<Tip | null>(null);
  const scheme = useShownScheme();
  const [pressed, setPressed] = useState<{
    x: number;
    view: [number, number];
    moved: boolean;
  } | null>(null);

  const plot = Math.max(width - NAMES, 0);
  const selected = rangeOf(look);
  const params = useMemo(
    () =>
      plot > 0
        ? {
            profile,
            revision: query.revision ?? 0,
            ...(selected ? { range: selected } : {}),
            ...(query.threads ? { threads: query.threads } : {}),
            width: Math.round(plot),
          }
        : null,
    [profile, query.revision, query.threads, selected?.[0], selected?.[1], plot],
  );
  const answer = useAnswer("profileZones", params);
  const blocks = useMemo(
    () => (answer.data ? decodeZones(answer.data, answer.data.payload) : null),
    [answer.data],
  );
  const order = useMemo(
    () => [...summary.threads].sort((a, b) => b.weight - a.weight).map((thread) => thread.row),
    [summary.threads],
  );
  const laid = useMemo(() => (blocks ? tracks(blocks, order) : []), [blocks, order]);
  const marks = answer.data?.marks ?? [];
  // Drawn over the range asked for, so a zoom moves what is shown at once
  // and the answer only sharpens it.
  const total = Math.max(summary.duration, 1);
  const range: [number, number] = selected ?? [0, total];
  const span = Math.max(range[1] - range[0], 1);
  const height =
    laid.length === 0
      ? 0
      : trackTop(laid.at(-1) as Track, laid.length - 1) + ROW * (laid.at(-1)?.depths ?? 0);
  const origin = range[0];
  const name = (thread: number) => threadName(summary.threads, thread);
  // One pixel's time, which is as finely as a time here needs telling.
  const pixel = span / Math.max(plot, 1);

  // Views asked for while zooming or panning, applied once a frame.
  const latest = useRef({ range, total, plot });
  latest.current = { range, total, plot };
  const wanted = useRef<[number, number] | null>(null);
  const frame = useRef(0);
  const show = (view: [number, number]) => {
    wanted.current = view;
    if (frame.current !== 0) {
      return;
    }
    frame.current = requestAnimationFrame(() => {
      frame.current = 0;
      const next = wanted.current;
      // From here the address bar holds it, and the next zoom starts there.
      wanted.current = null;
      if (!next) {
        return;
      }
      const whole = next[0] <= 0 && next[1] >= latest.current.total;
      setLook((look) => withLook(look, "range", whole ? undefined : formatRange(next)));
    });
  };
  useEffect(() => () => cancelAnimationFrame(frame.current), []);

  useEffect(() => {
    const element = canvas.current;
    if (!element) {
      return;
    }
    const wheel = (event: WheelEvent) => {
      const { range, total, plot } = latest.current;
      const sideways = Math.abs(event.deltaX) > Math.abs(event.deltaY);
      if (!event.ctrlKey && !event.metaKey && !sideways) {
        return;
      }
      event.preventDefault();
      const view = wanted.current ?? range;
      const per = (view[1] - view[0]) / Math.max(plot, 1);
      if (sideways) {
        show(panned(view, event.deltaX * per, total));
        return;
      }
      const at = view[0] + (event.clientX - element.getBoundingClientRect().left) * per;
      show(zoomed(view, at, Math.exp(event.deltaY * 0.002), total));
    };
    element.addEventListener("wheel", wheel, { passive: false });
    return () => element.removeEventListener("wheel", wheel);
  });

  useEffect(() => {
    const context = blocks ? prepare(canvas.current, plot, Math.max(height, 1)) : null;
    if (!context || !blocks) {
      return;
    }
    const x = (time: number) => ((time - origin) * plot) / span;
    // A canvas cannot read CSS variables, so the font's is read here.
    context.font = `11px ${token("--mono")}`;
    context.textBaseline = "middle";
    // Painted in the scheme shown, and again when it changes.
    const dark = scheme === "dark";
    const ink = token("--ink");
    const folded = token("--line-strong");
    const flagged = token("--changed");
    const found = token("--accent");
    const positions = new Map(laid.map((track, index) => [track.thread, trackTop(track, index)]));
    for (let at = 0; at < blocks.count; at += 1) {
      const y = positions.get(blocks.thread[at] ?? 0);
      if (y === undefined) {
        continue;
      }
      const row = y + (blocks.depth[at] ?? 0) * ROW;
      const from = x(blocks.start[at] ?? 0);
      const wide = Math.max(x(blocks.end[at] ?? 0) - from, 1);
      const label = blocks.names[blocks.name[at] ?? 0] ?? "";
      const many = (blocks.zones[at] ?? 1) > 1;
      context.fillStyle = many
        ? folded
        : dark
          ? `hsl(${hue(label.split("=")[0] ?? label)} 35% 32%)`
          : `hsl(${hue(label.split("=")[0] ?? label)} 55% 76%)`;
      context.fillRect(from, row + 1, wide, ROW - 2);
      const site = label.split("=")[0];
      if (look.zs && site === look.zs && !many) {
        context.strokeStyle = found;
        context.lineWidth = 2;
        context.strokeRect(from + 1, row + 2, Math.max(wide - 2, 1), ROW - 4);
      } else if ((blocks.flags[at] ?? 0) !== 0) {
        context.strokeStyle = flagged;
        context.lineWidth = 1;
        context.setLineDash([3, 2]);
        context.strokeRect(from + 0.5, row + 1.5, Math.max(wide - 1, 1), ROW - 3);
        context.setLineDash([]);
      }
      if (wide > 40) {
        context.save();
        context.beginPath();
        context.rect(from, row, wide, ROW);
        context.clip();
        context.fillStyle = ink;
        const length = (blocks.end[at] ?? 0) - (blocks.start[at] ?? 0);
        const text = many
          ? formatCount(blocks.zones[at] ?? 0, "zone")
          : `${label} ${formatDuration(length)}`;
        context.fillText(text, from + 3, row + ROW / 2);
        context.restore();
      }
    }
    // The program's marks, as ticks above each track.
    for (const mark of marks) {
      const y = positions.get(mark.thread);
      if (y === undefined) {
        continue;
      }
      context.fillStyle =
        mark.kind === "trigger" ? flagged : mark.kind === "frame" ? folded : found;
      const at = Math.min(Math.max(x(mark.time) - 0.5, 0), plot - 1.5);
      context.fillRect(at, y - ROW + 3, 1.5, ROW - 4);
    }
  }, [blocks, laid, marks, plot, height, look.zs, origin, span, scheme]);

  const point = (event: React.MouseEvent<HTMLCanvasElement>) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const px = event.clientX - bounds.left;
    const py = event.clientY - bounds.top;
    const time = range[0] + (px * span) / Math.max(plot, 1);
    const slack = (2 * span) / Math.max(plot, 1);
    for (const [index, track] of laid.entries()) {
      const first = trackTop(track, index);
      if (py >= first - ROW && py < first) {
        const near = nearest(marks, track.thread, time, slack * 2);
        return { px, py, time, block: null, mark: near };
      }
      if (py >= first && py < first + track.depths * ROW) {
        const depth = Math.floor((py - first) / ROW);
        return {
          px,
          py,
          time,
          block: blocks ? blockAt(blocks, track, depth, time, slack) : null,
          mark: null,
        };
      }
    }
    return { px, py, time, block: null, mark: null };
  };

  return (
    <div className="pane grow zones-tracks">
      <div className="pane-head">
        Zones
        <span className="count">
          {formatCount(summary.zoneSites.reduce((sum, site) => sum + site.count, 0))}
        </span>
        {selected ? <span className="subject">{formatSpan(selected)}</span> : null}
        <span className="end">ctrl+wheel zooms · drag pans · click a block to select it</span>
      </div>
      {answer.error ? (
        <div className="empty error">{answer.error.message}</div>
      ) : (
        <div className={`pane-body zones-body ${answer.current ? "" : "stale"}`} ref={box}>
          <Ruler origin={range[0]} span={span} width={plot} />
          {blocks ? null : <div className="empty">Reading zones…</div>}
          <div className="zone-lane">
            <div className="zone-names" style={{ width: NAMES, height }}>
              {laid.map((track, index) => (
                <div
                  key={track.thread}
                  className="zone-name"
                  style={{ top: trackTop(track, index) - ROW }}
                >
                  {name(track.thread)}
                </div>
              ))}
            </div>
            <canvas
              ref={canvas}
              style={{ width: plot, height }}
              role="img"
              aria-label={`Zones over ${formatDuration(span)}`}
              className={pressed?.moved ? "panning" : undefined}
              onPointerDown={(event) => {
                if (event.button !== 0) {
                  return;
                }
                event.currentTarget.setPointerCapture(event.pointerId);
                setPressed({ x: event.clientX, view: range, moved: false });
              }}
              onPointerUp={(event) => {
                const was = pressed;
                setPressed(null);
                if (!was || was.moved) {
                  return;
                }
                const at = point(event);
                if (at.block !== null && blocks) {
                  const start = blocks.start[at.block] ?? 0;
                  const end = blocks.end[at.block] ?? 0;
                  if (end > start) {
                    setLook((look) => withLook(look, "range", formatRange([start, end])), true);
                  }
                }
              }}
              onPointerCancel={() => setPressed(null)}
              onPointerMove={(event) => {
                if (pressed) {
                  const by = event.clientX - pressed.x;
                  if (pressed.moved || Math.abs(by) >= DRAG) {
                    if (!pressed.moved) {
                      setPressed({ ...pressed, moved: true });
                    }
                    setTip(null);
                    show(panned(pressed.view, -by * pixel, total));
                    return;
                  }
                }
                const at = point(event);
                const lines =
                  at.block !== null && blocks
                    ? blockLines(blocks, at.block, name, pixel)
                    : at.mark
                      ? [
                          `${at.mark.kind} ${at.mark.name} at ${formatInstant(at.mark.time, pixel)}`,
                          ...(at.mark.text ? [at.mark.text] : []),
                        ]
                      : null;
                setTip(lines ? { x: at.px + NAMES + 12, y: at.py + 30, lines } : null);
              }}
              onPointerLeave={() => setTip(null)}
            />
          </div>
          {(answer.data?.plots ?? []).map((series) => (
            <PlotLane key={series.name} series={series} origin={origin} span={span} width={plot} />
          ))}
          {tip ? (
            <div className="flame-tip" style={{ left: tip.x, top: tip.y }} data-testid="zone-tip">
              {tip.lines.map((line) => (
                <div key={line}>{line}</div>
              ))}
            </div>
          ) : null}
          {blocks && blocks.count === 0 && (answer.data?.plots.length ?? 0) === 0 ? (
            <div className="empty">No zones in this range.</div>
          ) : null}
        </div>
      )}
    </div>
  );
}

/** Round times across the view, above the tracks. */
function Ruler({ origin, span, width }: { origin: number; span: number; width: number }) {
  const { step, times } = ticks([origin, origin + span], width);
  return (
    <div className="zone-lane zone-ruler">
      <div className="zone-names" style={{ width: NAMES, height: RULER }} />
      <svg width={width} height={RULER} role="img" aria-label="Time">
        {times.map((time) => {
          const x = ((time - origin) * width) / span;
          return (
            <g key={time}>
              <line x1={x} x2={x} y1={RULER - 5} y2={RULER} />
              {x < width - 60 ? (
                <text x={x + 3} y={RULER - 6}>
                  {formatInstant(time, step, origin + span)}
                </text>
              ) : null}
            </g>
          );
        })}
      </svg>
    </div>
  );
}

/** One plotted series as a line from its lowest value to its highest. */
function PlotLane({
  series,
  origin,
  span,
  width,
}: {
  series: ProfilePlot;
  origin: number;
  span: number;
  width: number;
}) {
  let [low, high] = [Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY];
  for (const bin of series.bins) {
    low = Math.min(low, bin[3] ?? low);
    high = Math.max(high, bin[4] ?? high);
  }
  if (series.held !== null) {
    low = Math.min(low, series.held);
    high = Math.max(high, series.held);
  }
  const extent = Number.isFinite(low) ? `${formatValue(low)} – ${formatValue(high)}` : "";
  return (
    <div className="zone-lane zone-plot">
      <div className="zone-names" style={{ width: NAMES, height: PLOT }}>
        <div className="zone-name">{series.name}</div>
        <div className="zone-name dim" style={{ top: 16 }}>
          {extent}
          {series.unshown > 0 ? ` · ${formatCount(series.unshown)} not finite` : ""}
        </div>
      </div>
      <svg
        width={width}
        height={PLOT}
        role="img"
        aria-label={`Plot ${series.name}, ${formatCount(series.values, "value")}${extent ? ` from ${extent}` : ""}`}
      >
        <path
          d={plotPath(
            series.bins,
            origin,
            span,
            width,
            PLOT,
            low,
            high,
            series.held,
            series.continues,
          )}
        />
      </svg>
    </div>
  );
}

/** A plotted value, short. */
function formatValue(value: number): string {
  return Number.isInteger(value) ? String(value) : value.toPrecision(4);
}

/** Where a track's first row begins: its marks take the row above. */
function trackTop(track: Track, index: number): number {
  return (track.row + 1) * ROW + index * GAP;
}

/** A tooltip's lines for a block. */
function blockLines(
  blocks: ZoneBlocks,
  at: number,
  name: (thread: number) => string,
  pixel: number,
): string[] {
  const length = (blocks.end[at] ?? 0) - (blocks.start[at] ?? 0);
  const count = blocks.zones[at] ?? 1;
  const label = blocks.names[blocks.name[at] ?? 0] ?? "";
  const head =
    count > 1
      ? `${formatCount(count)} ${label === "zones" ? "zones" : label} over ${formatDuration(length)}: click to see them`
      : `${label}: ${formatDuration(length)}`;
  return [
    head,
    `${name(blocks.thread[at] ?? 0)}, from ${formatInstant(blocks.start[at] ?? 0, pixel)}`,
    ...describeFlags(blocks.flags[at] ?? 0),
  ];
}

/** The mark of a thread nearest `time`, within `slack`. */
function nearest(marks: ProfileMark[], thread: number, time: number, slack: number) {
  let best: ProfileMark | null = null;
  for (const mark of marks) {
    if (mark.thread === thread && Math.abs(mark.time - time) <= slack) {
      if (!best || Math.abs(mark.time - time) < Math.abs(best.time - time)) {
        best = mark;
      }
    }
  }
  return best;
}
