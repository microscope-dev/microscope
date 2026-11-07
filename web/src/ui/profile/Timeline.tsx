import { type PointerEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";

import { rangeOf, withLook } from "../../profile/look";
import { formatCount, formatDuration, formatInstant, largest } from "../../profile/paint";
import { timeTicks } from "../../profile/zones";
import type { ProfileLane, ProfileSummary, ProfileThread } from "../../protocol";
import { useShownScheme } from "../../theme";
import { NAMES, NAMES_STYLE, prepare, shade, timeAt, useRangeDrag, useWidth } from "./canvas";
import { type ProfileFocus, token, useAnswer, useProfile } from "./context";

/** One lane, in CSS pixels. */
const LANE = 18;
/** Show only the tasks with the greatest weight. */
const MOST_TASK_LANES = 50;

/** A lane: what the look selects it by, and the answer's lane id. */
interface Row {
  id: string;
  lane: number;
  label: string;
  dim?: string;
  title: string;
}

/** Waits always use tasks. Task-based samples add a lane for unknown tasks. */
function rows(summary: ProfileSummary, look: ProfileFocus["look"]): Row[] {
  const waits = look.src === "waits";
  if (!waits && look.lanes !== "tasks") {
    return summary.threads
      .toSorted((a, b) => b.weight - a.weight)
      .map((thread) => ({
        id: String(thread.row),
        lane: thread.row,
        label: thread.name ?? "thread",
        dim: String(thread.tid),
        title: `pid ${thread.pid}, tid ${thread.tid}: ${formatCount(thread.samples, "sample")}${offCpu(thread)}`,
      }));
  }
  const weight = (task: ProfileSummary["tasks"][number]) => (waits ? task.waited : task.weight);
  const tasks: Row[] = summary.tasks
    .filter((task) => weight(task) > 0)
    .toSorted((a, b) => weight(b) - weight(a))
    .slice(0, MOST_TASK_LANES)
    .map((task) => ({
      id: String(task.row),
      lane: task.row,
      label: task.name,
      title: `${task.name}: ${formatDuration(task.weight)} sampled, ${formatDuration(task.waited)} waiting`,
    }));
  if (!waits) {
    tasks.push({
      id: "none",
      lane: -1,
      label: "no task known",
      title: "Samples whose task is not known",
    });
  }
  return tasks;
}

/**
 * Thread or task samples and waits across the whole profile.
 * Off-CPU bands are grey for blocking and amber for preemption;
 * intensity shows the off-CPU fraction. Drag to select a shared time range,
 * click to clear it, or click a lane name to toggle its thread or task.
 */
export function Timeline() {
  const { summary, query, look, setLook } = useProfile();
  const [box, width] = useWidth();
  const canvas = useRef<HTMLCanvasElement>(null);
  const scheme = useShownScheme();

  // Every thread, over the whole profile: the selection is drawn over it.
  const plot = Math.max(width - NAMES, 0);
  const buckets = Math.max(1, Math.min(2048, Math.floor(plot / 2)));
  const byTask = look.src === "waits" || look.lanes === "tasks";
  const params = useMemo(() => {
    const { range: _range, threads: _threads, tasks: _tasks, ...rest } = query;
    return plot > 0 ? { ...rest, buckets, taskLanes: byTask } : null;
  }, [query, buckets, plot, byTask]);
  const answer = useAnswer("profileTimeline", params);
  const lanes = useMemo(() => rows(summary, look), [summary, look]);
  const chosen = (byTask ? look.tk : look.th)?.join(",");
  // Kept between renders, so painting runs again only when they change.
  const selected = useMemo(() => (chosen ? new Set(chosen.split(",")) : null), [chosen]);
  const range = useMemo(() => rangeOf({ range: look.range }), [look.range]);
  const span = Math.max(summary.duration, 1);
  const height = lanes.length * LANE;
  const { handlers, shown } = useRangeDrag(span);
  const marks = useMemo(() => timeTicks(span, plot), [span, plot]);
  // Where the pointer is over the lanes, in nanoseconds from the start.
  const [pointer, setPointer] = useState<number | null>(null);

  useEffect(() => {
    const element = canvas.current;
    const context = prepare(element, plot, height);
    if (!element || !context) {
      return;
    }
    // Its colors are the scheme's, so a change of scheme paints it again.
    element.dataset.scheme = scheme;
    const answered = new Map(
      (answer.data?.lanes ?? []).map((lane) => [(byTask ? lane.task : lane.thread) ?? -1, lane]),
    );
    let most = 1;
    for (const lane of answered.values()) {
      most = largest(lane.weights, most);
    }
    const bar = token("--accent");
    const [start, end] = answer.data?.range ?? [0, span];
    const step = (plot * (end - start)) / span / Math.max(1, buckets);
    const slice = Math.max(1, (end - start) / Math.max(1, buckets));
    const bands: [keyof Pick<ProfileLane, "blocked" | "preempted">, string][] = [
      ["blocked", token("--line-strong")],
      ["preempted", token("--warn-line")],
    ];
    // A faint line down from each of the ruler's round times.
    context.fillStyle = token("--line");
    for (const { at } of marks) {
      if (at > 0 && at < 1) {
        context.fillRect(Math.round(at * plot), 0, 1, height);
      }
    }
    for (const [index, row] of lanes.entries()) {
      const lane = answered.get(row.lane);
      const dim = selected && !selected.has(row.id) ? 0.3 : 1;
      for (const [kind, color] of bands) {
        context.fillStyle = color;
        for (const [bucket, off] of (lane?.[kind] ?? []).entries()) {
          if (off > 0) {
            context.globalAlpha = dim * Math.min(1, off / slice);
            const x = (plot * start) / span + bucket * step;
            context.fillRect(x, index * LANE + 1, Math.max(step, 1), LANE - 2);
          }
        }
      }
      const weights = lane?.weights ?? [];
      context.globalAlpha = dim * 0.85;
      context.fillStyle = bar;
      for (const [bucket, weight] of weights.entries()) {
        if (weight > 0) {
          const h = Math.max(1, ((LANE - 3) * weight) / most);
          const x = (plot * start) / span + bucket * step;
          context.fillRect(x, index * LANE + LANE - 1 - h, Math.max(step, 1), h);
        }
      }
    }
    // Spans not sampled cover every lane.
    context.fillStyle = token("--line");
    context.globalAlpha = 0.7;
    for (const [from, to] of [...summary.gaps, ...summary.behind]) {
      const x = (plot * from) / span;
      context.fillRect(x, 0, Math.max((plot * (to - from)) / span, 1), height);
    }
    context.globalAlpha = 1;
    const marked = shown ?? range;
    if (marked) {
      shade(context, marked, span, plot, height);
      const from = (plot * marked[0]) / span;
      const to = (plot * marked[1]) / span;
      // A range too short to see between its shades still shows where it is.
      const wide = Math.max(to - from, 3);
      context.fillStyle = token("--accent");
      context.fillRect((from + to) / 2 - wide / 2, 0, wide, 2);
      context.fillRect((from + to) / 2 - wide / 2, height - 2, wide, 2);
    }
  }, [
    answer.data,
    lanes,
    byTask,
    plot,
    height,
    span,
    buckets,
    selected,
    range,
    shown,
    summary.gaps,
    summary.behind,
    scheme,
    marks,
  ]);

  const toggle = useCallback(
    (id: string) =>
      setLook((prev) => {
        const key = byTask ? "tk" : "th";
        const ids = new Set(prev[key] ?? []);
        if (!ids.delete(id)) {
          ids.add(id);
        }
        return withLook(prev, key, ids.size > 0 ? [...ids].toSorted() : undefined);
      }),
    [setLook, byTask],
  );
  const hover = useCallback(
    (event: PointerEvent<HTMLCanvasElement>) => {
      handlers.onPointerMove(event);
      setPointer(timeAt(event, span));
    },
    [handlers, span],
  );
  const leave = useCallback(() => setPointer(null), []);

  // A range's ends sit outside it, with its length measured between them,
  // and the pointer's time over it. Round times step out of their way.
  const resolution = span / Math.max(plot, 1);
  const pixel = (at: number) => (at * plot) / span;
  const instant = (at: number) => formatInstant(at, resolution, span);
  const pills: Pill[] = [];
  const taken: [number, number][] = [];
  const marked = shown ?? range;
  let measure: Measure | null = null;
  if (marked) {
    const [left, right] = [pixel(marked[0]), pixel(marked[1])];
    const [from, to] = [Math.max(left, PILL), Math.min(right, plot - PILL)];
    pills.push(
      { id: "from", x: from, label: instant(marked[0]) },
      { id: "to", x: to, label: instant(marked[1]) },
    );
    taken.push([from - PILL, to + PILL]);
    if (right - left >= MEASURED) {
      measure = { x: left, width: right - left, label: instant(marked[1] - marked[0]) };
    }
  }
  if (pointer !== null && !shown) {
    const x = Math.min(Math.max(pixel(pointer), PILL / 2), plot - PILL / 2);
    pills.push({ id: "at", x, label: instant(pointer) });
    taken.push([x - PILL / 2, x + PILL / 2]);
    // The pointer's time stands in for the length it would cover.
    if (measure && Math.abs(x - (measure.x + measure.width / 2)) < (PILL + LABEL) / 2) {
      measure = { ...measure, label: null };
    }
  }
  const covered = (x: number) =>
    taken.some(([from, to]) => x > from - LABEL / 2 && x < to + LABEL / 2);

  const plotStyle = useMemo(() => ({ width: plot }), [plot]);
  const canvasStyle = useMemo(() => ({ width: plot, height }), [plot, height]);
  return (
    <div className="profile-timeline" ref={box} data-testid="timeline">
      <div className="timeline-ruler" data-testid="ruler">
        <div className="ruler-gap" style={NAMES_STYLE} />
        <div className="ruler-marks" style={plotStyle}>
          {marks.map(({ at, label }, index) =>
            covered(at * plot) ? null : (
              <RulerMark
                key={label}
                x={at * plot}
                label={label}
                className={index === 0 ? "first" : index === marks.length - 1 ? "last" : undefined}
              />
            ),
          )}
          {measure ? <Measured {...measure} /> : null}
          {pills.map((pill) => (
            <RulerMark
              key={pill.id}
              x={pill.x}
              label={pill.label}
              className={`marker ${pill.id}`}
            />
          ))}
        </div>
      </div>
      <div className="timeline-body">
        <div className="lane-names" style={NAMES_STYLE}>
          {lanes.map((row) => (
            <LaneName
              key={row.id}
              row={row}
              selected={selected?.has(row.id) ?? false}
              toggle={toggle}
            />
          ))}
        </div>
        {answer.error ? (
          <div className="empty error" style={plotStyle}>
            {answer.error.message}
          </div>
        ) : (
          <div className="timeline-plot" style={plotStyle}>
            <canvas
              ref={canvas}
              className="lanes"
              style={canvasStyle}
              // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a canvas drawn here, not an image file
              role="img"
              aria-label={`Samples and time off CPU over ${formatDuration(span)}`}
              {...handlers}
              onPointerMove={hover}
              onPointerLeave={leave}
            />
            {pointer !== null && !shown ? (
              <Cursor x={(pointer * plot) / span} height={height} />
            ) : null}
          </div>
        )}
      </div>
    </div>
  );
}

/** About how wide, in pixels, a pill and a round time's label are. */
const PILL = 64;
const LABEL = 56;
/** The narrowest range, in pixels, with room to show its length. */
const MEASURED = 72;

/** A time the ruler names, `x` pixels in: a range's ends, or the pointer's. */
interface Pill {
  id: "from" | "to" | "at";
  x: number;
  label: string;
}

/** A range's length, drawn as a measure across its `width` pixels. */
interface Measure {
  x: number;
  width: number;
  label: string | null;
}

function Measured({ x, width, label }: Measure) {
  const style = useMemo(() => ({ left: x, width }), [x, width]);
  return (
    <div className="ruler-measure" style={style} data-testid="range-length">
      {label === null ? null : <span>{label}</span>}
    </div>
  );
}

/** A label on the ruler, `x` pixels in. */
function RulerMark({
  x,
  label,
  className,
}: {
  x: number;
  label: string;
  className?: string | undefined;
}) {
  const style = useMemo(() => ({ left: x }), [x]);
  return (
    <span className={className} style={style}>
      {label}
    </span>
  );
}

/** The line under the pointer, down through every lane. */
function Cursor({ x, height }: { x: number; height: number }) {
  const style = useMemo(() => ({ left: Math.round(x), height }), [x, height]);
  return <div className="timeline-cursor" style={style} />;
}

/** A lane's name, which toggles its thread or task in the selection. */
function LaneName({
  row,
  selected,
  toggle,
}: {
  row: Row;
  selected: boolean;
  toggle: (id: string) => void;
}) {
  const click = useCallback(() => toggle(row.id), [toggle, row.id]);
  return (
    <button
      type="button"
      className={`lane-name ${selected ? "selected" : ""}`}
      aria-pressed={selected}
      title={row.title}
      onClick={click}
    >
      {row.label} {row.dim ? <span className="dim">{row.dim}</span> : null}
    </button>
  );
}

/** What a lane's name says of its thread's time off its CPU, if any. */
function offCpu(thread: ProfileThread): string {
  if (thread.blocked === 0 && thread.preempted === 0) {
    return "";
  }
  const blocked = formatDuration(thread.blocked);
  const preempted = formatDuration(thread.preempted);
  return `; off its CPU ${blocked} blocked, ${preempted} preempted`;
}
