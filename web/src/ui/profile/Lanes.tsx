import {
  type MouseEvent as ReactMouseEvent,
  memo,
  type PointerEvent,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { assert } from "../../assert";
import { blockedIn, kernelLines, stepAt, waitAt } from "../../profile/kernel";
import {
  formatRange,
  type Instance,
  parseBand,
  parseInstance,
  rangeOf,
  withInstance,
  withLook,
} from "../../profile/look";
import { formatCount, formatDuration, formatInstant, hue, largest } from "../../profile/paint";
import {
  blockAt,
  decodeZones,
  describeFlags,
  type ZoneBlocks,
  indexBlocks,
  laneAt,
  layLanes,
  nesting,
  panned,
  plotPath,
  threadName,
  timeTicks,
  zoomed,
} from "../../profile/zones";
import type {
  ProfileLane,
  ProfileMark,
  ProfileMarker,
  ProfilePlot,
  ProfileSummary,
  ProfileWait,
} from "../../protocol";
import { useShownScheme } from "../../theme";
import { NAMES, NAMES_STYLE, prepare, useWidth } from "./canvas";
import { type ProfileFocus, token, useAnswer, useProfile } from "./context";

const STRIP = 18;

const RIBBON = 8;
const ZONED_RIBBON = 4;
const ROW = 16;
const PLOT = 40;

const KERNEL = 12;

const DRAG = 4;

const MOST_TASK_LANES = 50;

const SETTLE = 80;

interface Row {
  id: string;
  lane: number;
  label: string;
  dim?: string;
  title: string;
  zoned?: boolean;
  nested?: boolean;
  others?: Row[];
}

const OTHERS = -2;

function rows(
  summary: ProfileSummary,
  src: ProfileFocus["look"]["src"],
  lanes: ProfileFocus["look"]["lanes"],
): Row[] {
  const waits = src === "waits";
  if (!waits && lanes !== "tasks") {
    const threads = summary.threads
      .toSorted((a, b) => b.weight - a.weight)
      .map((thread) => ({
        id: String(thread.row),
        lane: thread.row,
        label: thread.name ?? "thread",
        dim: String(thread.tid),
        title: `pid ${thread.pid}, tid ${thread.tid}: ${formatCount(thread.samples, "sample")}${offCpu(thread)}`,
        zoned: thread.zones > 0,
      }));
    const zoned = threads.filter((row) => row.zoned);
    const others: Row[] = threads.filter((row) => !row.zoned);
    if (zoned.length === 0 || others.length === 0) {
      return threads;
    }
    for (const row of others) {
      row.nested = true;
    }
    const label = `${others.length} without zones`;
    return [
      ...zoned,
      { id: "others", lane: OTHERS, label, title: `${label}: their mean CPU`, others },
    ];
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

interface Hit {
  time: number;
  lane: number;
  block: number | null;
  mark: ProfileMark | null;
  kernel: string[] | null;
  wait: ProfileWait | null;
  step: ProfileMarker | null;
}

function useSettled<T>(value: T, wait: number): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setSettled(value), wait);
    return () => clearTimeout(timer);
  }, [value, wait]);
  return settled;
}

export function Lanes() {
  const { profile, summary, query, look, setLook } = useProfile();
  const [measure, width] = useWidth();
  const box = useRef<HTMLDivElement | null>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const scheme = useShownScheme();
  const plot = Math.max(width - NAMES, 0);
  const total = Math.max(summary.duration, 1);

  const [view, setView] = useState<[number, number]>([0, total]);
  const whole = view[0] <= 0 && view[1] >= total;

  const shownView = useMemo<[number, number]>(
    () => (whole ? [0, total] : [Math.round(view[0]), Math.round(Math.min(view[1], total))]),
    [whole, view, total],
  );
  const asked = useSettled(shownView, SETTLE);
  const span = Math.max(shownView[1] - shownView[0], 1);

  const byTask = look.src === "waits" || look.lanes === "tasks";
  const buckets = Math.max(1, Math.min(2048, Math.floor(plot / 2)));
  const timelineParams = useMemo(() => {
    const {
      range: _range,
      threads: _threads,
      tasks: _tasks,
      zones: _zones,
      base: _base,
      ...rest
    } = query;
    return plot > 0 ? { ...rest, range: asked, buckets, taskLanes: byTask } : null;
  }, [query, buckets, plot, byTask, asked]);
  const timeline = useAnswer("profileTimeline", timelineParams);

  const zoned = !byTask && (summary.zoneSites.length > 0 || summary.probes.events > 0);

  const zonesParams = useMemo(() => {
    const band = look.zs && look.zb ? parseBand(look.zb) : null;
    return zoned && plot > 0
      ? {
          profile,
          revision: query.revision ?? 0,
          range: asked,
          width: Math.round(plot),
          site: look.zs ?? null,
          band,
        }
      : null;
  }, [zoned, plot, profile, query.revision, asked, look.zs, look.zb]);
  const zones = useAnswer("profileZones", zonesParams);
  const blocks = useMemo(
    () => (zones.data ? decodeZones(zones.data, zones.data.payload) : null),
    [zones.data],
  );
  const index = useMemo(() => (blocks ? indexBlocks(blocks) : null), [blocks]);
  const marks = useMemo(() => zones.data?.marks ?? [], [zones.data]);
  const plots = useMemo(() => zones.data?.plots ?? [], [zones.data]);

  const [othersOpen, setOthersOpen] = useState(false);
  const lanes = useMemo(() => {
    const all = rows(summary, look.src, look.lanes);
    return othersOpen ? all.flatMap((row) => [row, ...(row.others ?? [])]) : all;
  }, [summary, look.src, look.lanes, othersOpen]);
  const chosen = (byTask ? look.tk : look.th)?.join(",");
  const selected = useMemo(() => (chosen ? new Set(chosen.split(",")) : null), [chosen]);

  const [depths, setDepths] = useState<ReadonlyMap<number, number>>(() => new Map());
  const [collapsed, setCollapsed] = useState<ReadonlySet<number>>(() => new Set());

  const [merged, setMerged] = useState(index);
  if (merged !== index) {
    setMerged(index);
    const deeper = [...(index?.depths ?? [])].filter(
      ([thread, depth]) => depth > (depths.get(thread) ?? 0),
    );
    if (deeper.length > 0) {
      setDepths(new Map([...depths, ...deeper]));
    }
  }
  const shownRows = useMemo(
    () =>
      lanes.map((row) => {
        const depth = byTask ? 0 : (depths.get(row.lane) ?? 0);
        return collapsed.has(row.lane) ? Math.min(depth, 1) : depth;
      }),
    [lanes, byTask, depths, collapsed],
  );

  const kernel = !byTask && summary.syscalls > 0;
  const head = kernel ? STRIP + KERNEL : STRIP;
  const laid = useMemo(() => layLanes(shownRows, head, ROW), [shownRows, head]);
  const answered = useMemo(
    () =>
      new Map(
        (timeline.data?.lanes ?? []).map((lane) => [
          (byTask ? lane.task : lane.thread) ?? -1,
          lane,
        ]),
      ),
    [timeline.data, byTask],
  );
  const laneOf = useMemo(() => new Map(lanes.map((row, at) => [row.lane, at])), [lanes]);
  const height = laid.height;

  const range = useMemo(() => rangeOf({ range: look.range }), [look.range]);
  const instance = useMemo(() => (look.zi ? parseInstance(look.zi) : null), [look.zi]);
  const [dragging, setDragging] = useState<[number, number] | null>(null);
  const [pointer, setPointer] = useState<{ time: number; x: number; y: number } | null>(null);
  const [hit, setHit] = useState<Hit | null>(null);

  const pressed = useRef<{
    x: number;
    view: [number, number];
    moved: boolean;
    pan: boolean;
  } | null>(null);
  const [panning, setPanning] = useState(false);

  const rulerFrom = useRef<number | null>(null);

  const latest = useRef({ view: shownView, total, plot });
  useLayoutEffect(() => {
    latest.current = { view: shownView, total, plot };
  });
  const wanted = useRef<[number, number] | null>(null);
  const frame = useRef(0);
  const show = useCallback((next: [number, number]) => {
    wanted.current = next;
    if (frame.current !== 0) {
      return;
    }
    frame.current = requestAnimationFrame(() => {
      frame.current = 0;
      const chosenView = wanted.current;
      wanted.current = null;
      if (chosenView) {
        setView(chosenView);
      }
    });
  }, []);
  useEffect(() => () => cancelAnimationFrame(frame.current), []);

  useEffect(() => {
    if (!instance) {
      return;
    }
    const [start, end] = instance.span;
    const now = latest.current;
    const per = (now.view[1] - now.view[0]) / Math.max(now.plot, 1);
    if (start < now.view[0] || end > now.view[1] || (end - start) / per < 4) {
      const margin = Math.max((end - start) / 3, 1000);
      const from = Math.max(start - margin, 0);
      setView([from, Math.min(Math.max(end + margin, from + 1000), now.total)]);
    }
    setCollapsed((was) => {
      if (!was.has(instance.thread)) {
        return was;
      }
      const next = new Set(was);
      next.delete(instance.thread);
      return next;
    });
  }, [instance]);

  const scrolledTo = useRef<string | null>(null);
  useLayoutEffect(() => {
    const element = box.current;
    const drawn = canvas.current;
    const lane = instance ? laneOf.get(instance.thread) : undefined;
    if (!look.zi || look.zi === scrolledTo.current || !element || !drawn || lane === undefined) {
      return;
    }
    scrolledTo.current = look.zi;
    const ruler = element.querySelector(".timeline-ruler")?.getBoundingClientRect().height ?? 0;
    const top =
      drawn.getBoundingClientRect().top -
      element.getBoundingClientRect().top +
      element.scrollTop +
      (laid.tops[lane] ?? 0);
    const bottom = top + head + (shownRows[lane] ?? 0) * ROW;
    if (top < element.scrollTop + ruler || bottom > element.scrollTop + element.clientHeight) {
      element.scrollTop = top - ruler;
    }
  }, [look.zi, instance, laneOf, laid, shownRows, head]);

  useEffect(() => {
    const element = box.current;
    if (!element) {
      return;
    }
    const wheel = (event: WheelEvent) => {
      const now = latest.current;
      const sideways = Math.abs(event.deltaX) > Math.abs(event.deltaY);
      if (!event.ctrlKey && !event.metaKey && !sideways) {
        return;
      }
      event.preventDefault();
      const from = wanted.current ?? now.view;
      const per = (from[1] - from[0]) / Math.max(now.plot, 1);
      if (sideways) {
        show(panned(from, event.deltaX * per, now.total));
        return;
      }
      const left = canvas.current?.getBoundingClientRect().left ?? 0;
      const x = Math.min(Math.max(event.clientX - left, 0), now.plot);
      show(zoomed(from, from[0] + x * per, Math.exp(event.deltaY * 0.002), now.total));
    };
    element.addEventListener("wheel", wheel, { passive: false });
    return () => element.removeEventListener("wheel", wheel);
  }, [show]);

  const palette = useMemo(() => {
    const names = blocks?.names ?? [];
    const sites = names.map((name) => name.split("=")[0] ?? name);
    const dark = scheme === "dark";
    return {
      sites,

      fills: sites.map((site) =>
        dark ? `oklch(0.5 0.13 ${hue(site)})` : `oklch(0.8 0.13 ${hue(site)})`,
      ),
    };
  }, [blocks, scheme]);

  useEffect(() => {
    const element = canvas.current;
    const context = prepare(element, plot, Math.max(height, 1));
    if (!element || !context) {
      return;
    }
    element.dataset.scheme = scheme;
    const x = (time: number) => ((time - shownView[0]) * plot) / span;
    context.font = `11px ${token("--mono")}`;
    context.textBaseline = "middle";

    context.fillStyle = token("--line");
    for (const { at } of timeTicks(shownView, plot)) {
      if (at > 0 && at < 1) {
        context.fillRect(Math.round(at * plot), 0, 1, height);
      }
    }

    const marked = dragging ?? (instance ? instance.span : range);
    if (marked) {
      const [from, to] = [x(marked[0]), x(marked[1])];

      const light = scheme === "light";
      context.fillStyle = token(light ? "--accent" : "--accent-soft");
      context.globalAlpha = light ? 0.12 : 0.6;
      context.fillRect(0, 0, Math.max(from, 0), height);
      context.fillRect(to, 0, Math.max(plot - to, 0), height);
      context.globalAlpha = 1;
    }

    const sliced = timeline.data?.range ?? asked;
    paintCpu(context, { answered, rows: lanes, tops: laid.tops, selected, x, range: sliced });
    if (kernel) {
      paintKernel(context, { answered, rows: lanes, tops: laid.tops, selected, x, range: sliced });
    }

    if (blocks && index) {
      const folded = token("--line-strong");
      const flagged = token("--changed");
      const found = token("--accent");
      const ink = token("--ink");
      const glyph = context.measureText("0").width;
      const lit = (at: number) => !look.zs || blocks.chosen[at] === 1;
      for (let at = 0; at < blocks.count; at += 1) {
        const thread = blocks.thread[at] ?? 0;
        const lane = laneOf.get(thread);
        const depth = blocks.depth[at] ?? 0;
        if (lane === undefined || depth >= (shownRows[lane] ?? 0)) {
          continue;
        }
        const top = (laid.tops[lane] ?? 0) + head + depth * ROW;
        const start = blocks.start[at] ?? 0;
        const end = blocks.end[at] ?? 0;
        const from = x(start);
        const to = x(end);
        if (to < 0 || from > plot) {
          continue;
        }
        const wide = Math.max(to - from, 1);
        const name = blocks.name[at] ?? 0;
        const label = blocks.names[name] ?? "";
        const many = (blocks.zones[at] ?? 1) > 1;
        const fill = many && label === "zones" ? folded : (palette.fills[name] ?? folded);
        const dim = lit(at) ? 1 : 0.3;
        const covered = blocks.covered[at] ?? 0;
        const running = onCpu(blocks, at);
        context.fillStyle = fill;
        if (many) {
          context.globalAlpha = dim * (0.25 + (0.75 * covered) / Math.max(end - start, 1));
          context.fillRect(from, top + 1, wide, ROW - 2);
        } else if (running === null) {
          context.globalAlpha = dim;
          context.fillRect(from, top + 1, wide, ROW - 2);
        } else {
          context.globalAlpha = dim * 0.2;
          context.fillRect(from, top + 1, wide, ROW - 2);
          paintRunning(context, {
            lane: answered.get(thread),
            range: sliced,
            span: [start, end],
            share: running,
            alpha: dim,
            x,
            top: top + 1,
            height: ROW - 2,
          });
          context.globalAlpha = dim;
          context.strokeStyle = fill;
          context.lineWidth = 1;
          context.strokeRect(from + 0.5, top + 1.5, Math.max(wide - 1, 1), ROW - 3);
        }
        context.globalAlpha = dim;
        if (look.zs && dim === 1) {
          context.strokeStyle = found;
          context.lineWidth = 1.5;
          context.strokeRect(from + 0.75, top + 1.75, Math.max(wide - 1.5, 1), ROW - 3.5);
        } else if ((blocks.flags[at] ?? 0) !== 0) {
          context.strokeStyle = flagged;
          context.lineWidth = 1;
          context.setLineDash([3, 2]);
          context.strokeRect(from + 0.5, top + 1.5, Math.max(wide - 1, 1), ROW - 3);
          context.setLineDash([]);
        }
        const room = Math.min(to, plot) - Math.max(from, 0) - 6;
        const text =
          room > 16
            ? many
              ? fitting([formatCount(blocks.zones[at] ?? 0, "zone")], room, glyph)
              : fitting(zoneLabels(label, end - start, running), room, glyph)
            : null;
        if (text) {
          context.globalAlpha = dim;
          context.fillStyle = ink;
          context.fillText(text, Math.max(from, 0) + 3, top + ROW / 2);
        }
      }
      context.globalAlpha = 1;

      for (const mark of marks) {
        const lane = laneOf.get(mark.thread);
        if (lane === undefined) {
          continue;
        }
        context.fillStyle =
          mark.kind === "trigger" ? flagged : mark.kind === "frame" ? folded : found;
        const at = Math.min(Math.max(x(mark.time) - 0.5, 0), plot - 1.5);
        context.fillRect(at, (laid.tops[lane] ?? 0) + 1, 1.5, 6);
      }
    }

    context.fillStyle = token("--line");
    context.globalAlpha = 0.7;
    for (const [from, to] of summary.behind) {
      context.fillRect(x(from), 0, Math.max(x(to) - x(from), 1), height);
    }
    context.globalAlpha = 1;

    context.strokeStyle = token("--changed");
    context.lineWidth = 1;
    context.setLineDash([2, 3]);
    for (const step of summary.budgetChanges) {
      const at = Math.round(x(step.time)) + 0.5;
      if (at >= 0 && at <= plot) {
        context.beginPath();
        context.moveTo(at, 0);
        context.lineTo(at, height);
        context.stroke();
      }
    }
    context.setLineDash([]);

    if (marked) {
      const [from, to] = [x(marked[0]), x(marked[1])];
      const wide = Math.max(to - from, 3);
      context.fillStyle = token("--accent");
      context.fillRect((from + to) / 2 - wide / 2, 0, wide, 2);
      context.fillRect((from + to) / 2 - wide / 2, height - 2, wide, 2);
    }
    const lane = instance ? laneOf.get(instance.thread) : undefined;
    if (instance && lane !== undefined && blocks) {
      for (let at = 0; at < blocks.count; at += 1) {
        if (
          blocks.thread[at] === instance.thread &&
          blocks.start[at] === instance.span[0] &&
          blocks.end[at] === instance.span[1] &&
          (blocks.depth[at] ?? 0) < (shownRows[lane] ?? 0)
        ) {
          const top = (laid.tops[lane] ?? 0) + head + (blocks.depth[at] ?? 0) * ROW;
          const from = x(instance.span[0]);
          context.strokeStyle = token("--ink");
          context.lineWidth = 2;
          context.strokeRect(from, top + 1, Math.max(x(instance.span[1]) - from, 2), ROW - 2);
        }
      }
    }
  }, [
    timeline.data,
    answered,
    kernel,
    head,
    asked,
    blocks,
    index,
    marks,
    lanes,
    laid,
    laneOf,
    shownRows,
    selected,
    plot,
    height,
    span,
    shownView,
    range,
    instance,
    dragging,
    look.zs,
    palette,
    summary.behind,
    summary.budgetChanges,
    scheme,
  ]);

  const at = useCallback(
    (px: number, py: number): Hit | null => {
      if (px < 0 || px >= plot || py < 0 || py >= height) {
        return null;
      }
      const time = shownView[0] + (px * span) / Math.max(plot, 1);
      const lane = laneAt(laid.tops, py);
      const row = lanes[lane];
      if (!row) {
        return null;
      }
      const inside = py - (laid.tops[lane] ?? 0);
      const slack = (2 * span) / Math.max(plot, 1);
      const step = stepAt(summary.budgetChanges, time, slack);
      const none = { block: null, mark: null, kernel: null, wait: null, step };
      if (inside >= STRIP && inside < head) {
        const calls = row.others ? undefined : answered.get(row.lane);
        const sliced = timeline.data?.range ?? asked;
        return {
          ...none,
          time,
          lane,
          kernel: calls ? kernelLines(calls, sliced, time, slack) : null,
          wait: calls ? waitAt(calls, time, slack) : null,
        };
      }
      if (inside < STRIP) {
        let mark: ProfileMark | null = null;
        for (const each of marks) {
          if (
            each.thread === row.lane &&
            Math.abs(each.time - time) <= slack * 2 &&
            (!mark || Math.abs(each.time - time) < Math.abs(mark.time - time))
          ) {
            mark = each;
          }
        }
        return { ...none, time, lane, mark };
      }
      const depth = Math.floor((inside - head) / ROW);
      const block = blocks && index ? blockAt(blocks, index, row.lane, depth, time, slack) : null;
      return { ...none, time, lane, block };
    },
    [
      plot,
      height,
      shownView,
      span,
      laid,
      lanes,
      marks,
      blocks,
      index,
      head,
      answered,
      timeline.data,
      asked,
      summary.budgetChanges,
    ],
  );

  const canvasLeft = useCallback(() => canvas.current?.getBoundingClientRect().left ?? 0, []);
  const local = useCallback((event: { clientX: number; clientY: number }) => {
    const bounds = canvas.current?.getBoundingClientRect() ?? { left: 0, top: 0 };
    return [event.clientX - bounds.left, event.clientY - bounds.top] as const;
  }, []);

  const press = useCallback(
    (event: PointerEvent<HTMLDivElement>) => {
      const body = event.currentTarget;
      const bounds = body.getBoundingClientRect();

      const scrollbar =
        event.clientX - bounds.left >= body.clientLeft + body.clientWidth ||
        event.clientY - bounds.top >= body.clientTop + body.clientHeight;
      const target = event.target as Element;
      if (
        event.button !== 0 ||
        scrollbar ||
        target.closest(".lane-names, .timeline-ruler, .plot-name")
      ) {
        return;
      }
      body.setPointerCapture(event.pointerId);
      pressed.current = {
        x: event.clientX,
        view: shownView,
        moved: false,
        pan: event.ctrlKey || event.metaKey,
      };
    },
    [shownView],
  );
  const select = useCallback(
    (block: number, zoom: boolean) => {
      assert(blocks, "a zone was chosen with none drawn");
      const zone: [number, number] = [blocks.start[block] ?? 0, blocks.end[block] ?? 0];
      if ((blocks.zones[block] ?? 1) > 1 || zoom) {
        const margin = (zone[1] - zone[0]) / 20;
        show([Math.max(zone[0] - margin, 0), Math.min(zone[1] + margin, total)]);
        if ((blocks.zones[block] ?? 1) > 1) {
          return;
        }
      }
      if (zone[1] <= zone[0]) {
        return;
      }
      const chosenOne: Instance = { thread: blocks.thread[block] ?? 0, span: zone };
      const site = palette.sites[blocks.name[block] ?? 0] ?? "";
      setLook((prev) => {
        const again = prev.zi === `${chosenOne.thread}:${formatRange(zone)}` && !zoom;
        const next = withInstance(prev, again ? null : chosenOne);

        return prev.zs === site ? next : withLook(clearBand(next), "zs", site);
      }, true);
    },
    [blocks, palette, setLook, show, total],
  );

  const chooseRange = useCallback(
    (from: number, to: number) => {
      const [start, end] = [Math.min(from, to), Math.max(from, to)];
      const chosenRange =
        ((end - start) * plot) / span >= 3 ? formatRange([start, end]) : undefined;
      setLook((prev) => withInstance(withLook(prev, "range", chosenRange), null), true);
    },
    [plot, span, setLook],
  );
  const release = useCallback(
    (event: PointerEvent<HTMLDivElement>) => {
      const was = pressed.current;
      pressed.current = null;
      setPanning(false);
      setDragging(null);
      if (was?.moved && !was.pan) {
        chooseRange(
          timeAt(was.view, plot, was.x - canvasLeft()),
          timeAt(was.view, plot, event.clientX - canvasLeft()),
        );
        return;
      }
      if (!was || was.moved) {
        return;
      }
      const found = at(...local(event));
      if (found?.block !== null && found?.block !== undefined) {
        select(found.block, false);
      } else if (found?.wait) {
        chooseRange(found.wait.start, found.wait.end);
      } else {
        setLook((prev) => {
          const { range: _range, ...rest } = prev;
          return prev.range ? rest : prev;
        }, true);
      }
    },
    [at, local, select, chooseRange, plot, canvasLeft, setLook],
  );
  const doubleClick = useCallback(
    (event: ReactMouseEvent<HTMLDivElement>) => {
      const found = at(...local(event));
      if (found?.block !== null && found?.block !== undefined) {
        select(found.block, true);
      }
    },
    [at, local, select],
  );
  const hover = useCallback(
    (event: PointerEvent<HTMLDivElement>) => {
      const held = pressed.current;
      if (held) {
        const by = event.clientX - held.x;
        if (held.moved || Math.abs(by) >= DRAG) {
          if (!held.moved) {
            held.moved = true;
            setPanning(held.pan);
          }
          setHit(null);
          if (held.pan) {
            const per = (held.view[1] - held.view[0]) / Math.max(plot, 1);
            show(panned(held.view, -by * per, total));
            return;
          }
          const [from, to] = [
            timeAt(held.view, plot, held.x - canvasLeft()),
            timeAt(held.view, plot, event.clientX - canvasLeft()),
          ];
          setDragging([Math.min(from, to), Math.max(from, to)]);
          setPointer({ time: to, x: event.clientX, y: event.clientY });
          return;
        }
      }
      const [px, py] = local(event);
      const found = at(px, py);
      setHit(found);
      setPointer(
        px >= 0 && px < plot
          ? {
              time: shownView[0] + (px * span) / Math.max(plot, 1),
              x: event.clientX,
              y: event.clientY,
            }
          : null,
      );
    },
    [plot, show, total, local, at, shownView, span, canvasLeft],
  );
  const leave = useCallback(() => {
    setHit(null);
    setPointer(null);
  }, []);
  const cancel = useCallback(() => {
    pressed.current = null;
    setPanning(false);
    setDragging(null);
  }, []);

  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && (pressed.current || rulerFrom.current !== null)) {
        event.preventDefault();
        pressed.current = null;
        rulerFrom.current = null;
        setPanning(false);
        setDragging(null);
      }
    };
    window.addEventListener("keydown", escape, { capture: true });
    return () => window.removeEventListener("keydown", escape, { capture: true });
  }, []);

  const menu = useCallback((event: ReactMouseEvent<HTMLDivElement>) => {
    if (event.ctrlKey) {
      event.preventDefault();
    }
  }, []);

  const rulerTime = useCallback(
    (event: PointerEvent<HTMLElement>) => {
      const left = canvas.current?.getBoundingClientRect().left ?? 0;
      const px = Math.min(Math.max(event.clientX - left, 0), plot);
      return shownView[0] + (px * span) / Math.max(plot, 1);
    },
    [plot, shownView, span],
  );
  const rulerDown = useCallback(
    (event: PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0) {
        return;
      }
      event.currentTarget.setPointerCapture(event.pointerId);
      rulerFrom.current = rulerTime(event);
    },
    [rulerTime],
  );
  const rulerMove = useCallback(
    (event: PointerEvent<HTMLDivElement>) => {
      const from = rulerFrom.current;
      const to = rulerTime(event);
      setPointer({ time: to, x: event.clientX, y: event.clientY });
      if (from !== null) {
        setDragging([Math.min(from, to), Math.max(from, to)]);
      }
    },
    [rulerTime],
  );
  const rulerUp = useCallback(
    (event: PointerEvent<HTMLDivElement>) => {
      const from = rulerFrom.current;
      rulerFrom.current = null;
      setDragging(null);
      if (from === null) {
        return;
      }
      chooseRange(from, rulerTime(event));
    },
    [rulerTime, chooseRange],
  );
  const rulerCancel = useCallback(() => {
    rulerFrom.current = null;
    setDragging(null);
  }, []);

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
  const expand = useCallback(
    (thread: number) =>
      setCollapsed((was) => {
        const next = new Set(was);
        if (!next.delete(thread)) {
          next.add(thread);
        }
        return next;
      }),
    [],
  );

  const flipOthers = useCallback(() => setOthersOpen((was) => !was), []);

  const ref = useCallback(
    (element: HTMLDivElement | null) => {
      box.current = element;
      return measure(element);
    },
    [measure],
  );
  const plotStyle = useMemo(() => ({ width: plot }), [plot]);
  const canvasStyle = useMemo(() => ({ width: plot, height }), [plot, height]);

  const kernelLanes = kernel ? answered : NO_LANES;
  const tip =
    hit && !panning
      ? tipLines(hit, { blocks, index, summary, span, plot, answered: kernelLanes })
      : null;
  const tipStyle = useMemo(
    () => (pointer ? { left: pointer.x + 12, top: pointer.y + 16 } : undefined),
    [pointer],
  );

  return (
    <div
      className={`profile-timeline ${panning ? "panning" : ""}`}
      ref={ref}
      data-testid="timeline"
      onPointerDown={press}
      onPointerUp={release}
      onPointerCancel={cancel}
      onPointerMove={hover}
      onPointerLeave={leave}
      onDoubleClick={doubleClick}
      onContextMenu={menu}
    >
      <Ruler
        view={shownView}
        plot={plot}
        marked={dragging ?? (instance ? instance.span : range)}
        pointer={pointer && !dragging ? pointer.time : null}
        onPointerDown={rulerDown}
        onPointerMove={rulerMove}
        onPointerUp={rulerUp}
        onPointerCancel={rulerCancel}
      />
      <div className="timeline-body">
        <div className="lane-names" style={NAMES_STYLE}>
          {lanes.map((row, lane) => (
            <LaneName
              key={row.id}
              row={row}
              selected={selected?.has(row.id) ?? false}
              toggle={toggle}
              depth={byTask ? 0 : (depths.get(row.lane) ?? 0)}
              open={row.others ? othersOpen : !collapsed.has(row.lane)}
              expand={row.others ? flipOthers : expand}
              rows={shownRows[lane] ?? 0}
              kernel={kernel}
            />
          ))}
        </div>
        {timeline.error ? (
          <div className="empty error" style={plotStyle}>
            {timeline.error.message}
          </div>
        ) : (
          <div className="timeline-plot" style={plotStyle}>
            <canvas
              ref={canvas}
              className={`lanes ${zonesParams && !zones.current ? "stale" : ""}`}
              style={canvasStyle}
              // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a canvas drawn here, not an image file
              role="img"
              aria-label={`CPU${zoned ? " and zones" : ""} over ${formatDuration(span)}`}
            />
            {pointer !== null && !dragging && !panning ? (
              <Cursor x={((pointer.time - shownView[0]) * plot) / span} height={height} />
            ) : null}
          </div>
        )}
      </div>
      {zones.error ? <div className="empty error">{zones.error.message}</div> : null}
      {plots.map((series) => (
        <PlotLane
          key={series.name}
          series={series}
          origin={shownView[0]}
          span={span}
          width={plot}
        />
      ))}
      {tip ? (
        <div className="lanes-tip" style={tipStyle} data-testid="zone-tip">
          {tip.map((line) => (
            <div key={line}>{line}</div>
          ))}
        </div>
      ) : null}
    </div>
  );
}

function timeAt(view: readonly [number, number], plot: number, x: number): number {
  const px = Math.min(Math.max(x, 0), plot);
  return view[0] + (px * (view[1] - view[0])) / Math.max(plot, 1);
}

function clearBand(look: ProfileFocus["look"]): ProfileFocus["look"] {
  const { zb: _zb, bz: _bz, zf, ...rest } = look;
  return zf === "all" ? { ...rest, zf } : rest;
}

function paintCpu(
  context: CanvasRenderingContext2D,
  {
    answered,
    rows: shown,
    tops,
    selected,
    x,
    range,
  }: {
    answered: ReadonlyMap<number, ProfileLane>;
    rows: Row[];
    tops: Float64Array;
    selected: ReadonlySet<string> | null;
    x: (time: number) => number;
    range: readonly [number, number];
  },
) {
  let most = 1;
  for (const lane of answered.values()) {
    most = largest(lane.weights, most);
  }
  context.fillStyle = token("--ink-2");
  for (const [at, row] of shown.entries()) {
    const lanes = (row.others ?? [row]).flatMap((each) => answered.get(each.lane) ?? []);
    const count = largest(
      lanes.map((lane) => lane.weights.length),
      0,
    );
    if (count === 0) {
      continue;
    }
    const slice = (range[1] - range[0]) / count;
    const step = Math.max(x(range[0] + slice) - x(range[0]), 1);
    const tall = row.zoned ? ZONED_RIBBON : RIBBON;
    const top = (tops[at] ?? 0) + STRIP - 3 - tall;
    const dim = selected && !selected.has(row.id) ? 0.3 : 1;
    for (let bucket = 0; bucket < count; bucket += 1) {
      let weight = 0;
      for (const lane of lanes) {
        weight += lane.weights[bucket] ?? 0;
      }
      if (weight > 0) {
        context.globalAlpha = dim * (0.15 + (0.75 * weight) / (most * lanes.length));
        context.fillRect(x(range[0] + bucket * slice), top, step, tall);
      }
    }
  }
  context.globalAlpha = 1;
}

function paintKernel(
  context: CanvasRenderingContext2D,
  {
    answered,
    rows: shown,
    tops,
    selected,
    x,
    range,
  }: {
    answered: ReadonlyMap<number, ProfileLane>;
    rows: Row[];
    tops: Float64Array;
    selected: ReadonlySet<string> | null;
    x: (time: number) => number;
    range: readonly [number, number];
  },
) {
  let most = 1;
  for (const lane of answered.values()) {
    most = largest(lane.syscalls, most);
  }
  const calls = token("--accent");
  const failed = token("--error");
  const ink = token("--ink");
  const glyph = context.measureText("0").width;
  for (const [at, row] of shown.entries()) {
    const lane = row.others ? undefined : answered.get(row.lane);
    if (!lane) {
      continue;
    }
    const top = (tops[at] ?? 0) + STRIP;
    const dim = selected && !selected.has(row.id) ? 0.3 : 1;
    const count = lane.syscalls.length;
    const slice = (range[1] - range[0]) / Math.max(count, 1);
    const step = Math.max(x(range[0] + slice) - x(range[0]), 1);

    context.fillStyle = calls;
    for (const wait of lane.waits) {
      const from = x(wait.start);
      const wide = Math.max(x(wait.end) - from, 1);
      context.globalAlpha = dim * 0.35;
      context.fillRect(from, top + 1, wide, KERNEL - 2);
      const text = fitting([wait.call], wide - 6, glyph);
      if (text) {
        context.globalAlpha = dim;
        context.fillStyle = ink;
        context.fillText(text, Math.max(from, 0) + 3, top + KERNEL / 2);
        context.fillStyle = calls;
      }
    }
    for (let bucket = 0; bucket < count; bucket += 1) {
      const n = lane.syscalls[bucket] ?? 0;
      if (n > 0) {
        const tall = Math.max(2, ((KERNEL - 2) * Math.log1p(n)) / Math.log1p(most));
        context.globalAlpha = dim * 0.8;
        context.fillStyle = (lane.failed[bucket] ?? 0) > 0 ? failed : calls;
        context.fillRect(x(range[0] + bucket * slice), top + KERNEL - 1 - tall, step, tall);
      }
    }
  }
  context.globalAlpha = 1;
}

function onCpu(blocks: ZoneBlocks, at: number): number | null {
  const covered = blocks.covered[at] ?? 0;
  if (!blocks.offCpu || covered <= 0) {
    return null;
  }
  const off = (blocks.blocked[at] ?? 0) + (blocks.preempted[at] ?? 0);
  return Math.min(Math.max(1 - off / covered, 0), 1);
}

function paintRunning(
  context: CanvasRenderingContext2D,
  {
    lane,
    range,
    span: [start, end],
    share,
    alpha,
    x,
    top,
    height,
  }: {
    lane: ProfileLane | undefined;
    range: readonly [number, number];
    span: [number, number];
    share: number;
    alpha: number;
    x: (time: number) => number;
    top: number;
    height: number;
  },
) {
  const count = lane?.blocked.length ?? 0;
  if (!lane || count === 0 || lane.preempted.length !== count) {
    context.globalAlpha = alpha * share;
    context.fillRect(x(start), top, Math.max(x(end) - x(start), 1), height);
    return;
  }
  const slice = (range[1] - range[0]) / count;
  const first = Math.max(Math.floor((start - range[0]) / slice), 0);
  const last = Math.min(Math.ceil((end - range[0]) / slice), count);
  for (let bucket = first; bucket < last; bucket += 1) {
    const from = Math.max(range[0] + bucket * slice, start);
    const to = Math.min(range[0] + (bucket + 1) * slice, end);
    const off = (lane.blocked[bucket] ?? 0) + (lane.preempted[bucket] ?? 0);
    const ran = 1 - Math.min(off / Math.max(slice, 1), 1);

    const [left, right] = [Math.round(x(from)), Math.round(x(to))];
    if (ran > 0 && right > left) {
      context.globalAlpha = alpha * ran;
      context.fillRect(left, top, right - left, height);
    }
  }
}

function zoneLabels(name: string, length: number, running: number | null): string[] {
  const took = formatDuration(length);
  if (running === null) {
    return [`${name} ${took}`, took];
  }
  const cpu = `${Math.round(running * 100)}%`;
  return [`${name} ${took}, ${cpu} CPU`, `${took}, ${cpu} CPU`, `${cpu} CPU`, cpu];
}

function fitting(labels: readonly string[], room: number, glyph: number): string | null {
  return labels.find((label) => label.length * glyph <= room) ?? null;
}

const NO_LANES: ReadonlyMap<number, ProfileLane> = new Map();

function tipLines(
  hit: Hit,
  {
    blocks,
    index,
    summary,
    span,
    plot,
    answered,
  }: {
    blocks: ReturnType<typeof decodeZones> | null;
    index: ReturnType<typeof indexBlocks> | null;
    summary: ProfileSummary;
    span: number;
    plot: number;
    answered: ReadonlyMap<number, ProfileLane>;
  },
): string[] | null {
  const pixel = span / Math.max(plot, 1);
  const step = hit.step
    ? [`budget at ${formatInstant(hit.step.time, pixel)}: ${hit.step.text}`]
    : [];
  const lines = ownLines(hit, { blocks, index, summary, pixel, answered });
  return lines || step.length > 0 ? [...(lines ?? []), ...step] : null;
}

function ownLines(
  hit: Hit,
  {
    blocks,
    index,
    summary,
    pixel,
    answered,
  }: {
    blocks: ReturnType<typeof decodeZones> | null;
    index: ReturnType<typeof indexBlocks> | null;
    summary: ProfileSummary;
    pixel: number;
    answered: ReadonlyMap<number, ProfileLane>;
  },
): string[] | null {
  if (hit.kernel) {
    return hit.kernel;
  }
  if (hit.mark) {
    return [
      `${hit.mark.kind} ${hit.mark.name} at ${formatInstant(hit.mark.time, pixel)}`,
      ...(hit.mark.text ? [hit.mark.text] : []),
    ];
  }
  if (hit.block === null || !blocks || !index) {
    return null;
  }
  const at = hit.block;
  const length = (blocks.end[at] ?? 0) - (blocks.start[at] ?? 0);
  const count = blocks.zones[at] ?? 1;
  const label = blocks.names[blocks.name[at] ?? 0] ?? "";
  const thread = blocks.thread[at] ?? 0;
  const head =
    count > 1
      ? `${formatCount(count)} ${label === "zones" ? "zones" : label} over ${formatDuration(length)}: click to zoom in`
      : `${label}: ${formatDuration(length)}`;
  const inside = nesting(blocks, index, thread, blocks.depth[at] ?? 0, blocks.start[at] ?? 0);
  const running = count > 1 ? null : onCpu(blocks, at);
  const lane = answered.get(thread);
  const waited =
    running === null || !lane ? null : blockedIn(lane, blocks.start[at] ?? 0, blocks.end[at] ?? 0);
  return [
    head,
    ...(running === null
      ? []
      : [
          `${Math.round(running * 100)}% on CPU, ${formatDuration(blocks.blocked[at] ?? 0)} blocked, ${formatDuration(blocks.preempted[at] ?? 0)} preempted`,
        ]),
    ...(waited ? [waited] : []),
    `${threadName(summary.threads, thread)}, from ${formatInstant(blocks.start[at] ?? 0, pixel)}`,
    ...(inside.length > 0 ? [`in ${inside.join(" › ")}`] : []),
    ...describeFlags(blocks.flags[at] ?? 0),
  ];
}

const LETTER = 6.5;
const LABEL = 56;

const pillWidth = (label: string) => label.length * LETTER + 12;

const MEASURED = 72;

function Ruler({
  view,
  plot,
  marked,
  pointer,
  ...handlers
}: {
  view: [number, number];
  plot: number;
  marked: readonly [number, number] | null;
  pointer: number | null;
  onPointerDown: (event: PointerEvent<HTMLDivElement>) => void;
  onPointerMove: (event: PointerEvent<HTMLDivElement>) => void;
  onPointerUp: (event: PointerEvent<HTMLDivElement>) => void;
  onPointerCancel: () => void;
}) {
  const span = Math.max(view[1] - view[0], 1);
  const marks = timeTicks(view, plot);
  const resolution = span / Math.max(plot, 1);
  const pixel = (time: number) => ((time - view[0]) * plot) / span;
  const instant = (time: number) => formatInstant(time, resolution, view[1]);
  const pills: { id: "from" | "to" | "at"; x: number; label: string; width: number }[] = [];
  const taken: [number, number][] = [];
  let measure: { x: number; width: number; label: string | null } | null = null;
  if (marked && marked[1] >= view[0] && marked[0] <= view[1]) {
    const [left, right] = [pixel(marked[0]), pixel(marked[1])];
    const [start, end] = [instant(marked[0]), instant(marked[1])];
    const [before, after] = [pillWidth(start), pillWidth(end)];

    let [from, to] = [Math.max(left, before), Math.min(right, plot - after)];
    if (to < from) {
      [from, to] = from > left ? [from, from] : [to, to];
    }
    pills.push(
      { id: "from", x: from, label: start, width: before },
      { id: "to", x: to, label: end, width: after },
    );
    taken.push([from - before, to + after]);
    if (right - left >= MEASURED) {
      measure = {
        x: Math.max(left, 0),
        width: Math.min(right, plot) - Math.max(left, 0),
        label: formatInstant(marked[1] - marked[0], resolution, span),
      };
    }
  }
  if (pointer !== null) {
    const label = instant(pointer);
    const width = pillWidth(label);
    const x = Math.min(Math.max(pixel(pointer), width / 2), plot - width / 2);

    for (let at = pills.length - 1; at >= 0; at -= 1) {
      const pill = pills[at];
      if (!pill) {
        continue;
      }
      const middle = pill.x + (pill.id === "from" ? -pill.width / 2 : pill.width / 2);
      if (Math.abs(middle - x) < (pill.width + width) / 2) {
        pills.splice(at, 1);
      }
    }
    pills.push({ id: "at", x, label, width });
    taken.push([x - width / 2, x + width / 2]);

    if (measure && Math.abs(x - (measure.x + measure.width / 2)) < (width + LABEL) / 2) {
      measure = { ...measure, label: null };
    }
  }

  const covered = (x: number, label: string, index: number) => {
    const width = label.length * LETTER + 8;
    const left = index === 0 ? x : index === marks.length - 1 ? x - width : x - width / 2;
    return taken.some(([from, to]) => left < to + 4 && left + width > from - 4);
  };
  const marksStyle = useMemo(() => ({ width: plot }), [plot]);
  return (
    <div className="timeline-ruler" data-testid="ruler">
      <div className="ruler-gap" style={NAMES_STYLE} />
      <div
        className="ruler-marks"
        style={marksStyle}
        title="Drag to select a range; click to clear it"
        {...handlers}
      >
        {marks.map(({ at, label }, index) =>
          covered(at * plot, label, index) ? null : (
            <RulerMark
              key={at}
              x={at * plot}
              label={label}
              className={index === 0 ? "first" : index === marks.length - 1 ? "last" : undefined}
            />
          ),
        )}
        {measure ? <Measured {...measure} /> : null}
        {pills.map((pill) => (
          <RulerMark key={pill.id} x={pill.x} label={pill.label} className={`marker ${pill.id}`} />
        ))}
      </div>
    </div>
  );
}

function Measured({ x, width, label }: { x: number; width: number; label: string | null }) {
  const style = useMemo(() => ({ left: x, width }), [x, width]);
  return (
    <div className="ruler-measure" style={style} data-testid="range-length">
      {label === null ? null : <span>{label}</span>}
    </div>
  );
}

const RulerMark = memo(function RulerMark({
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
});

function Cursor({ x, height }: { x: number; height: number }) {
  const style = useMemo(() => ({ left: Math.round(x), height }), [x, height]);
  return <div className="timeline-cursor" style={style} />;
}

const LaneName = memo(function LaneName({
  row,
  selected,
  toggle,
  depth,
  open,
  expand,
  rows: shown,
  kernel,
}: {
  row: Row;
  selected: boolean;
  toggle: (id: string) => void;
  depth: number;
  open: boolean;
  expand: (thread: number) => void;
  rows: number;
  kernel: boolean;
}) {
  const click = useCallback(() => toggle(row.id), [toggle, row.id]);
  const flip = useCallback(() => expand(row.lane), [expand, row.lane]);
  const style = useMemo(() => ({ height: shown * ROW }), [shown]);
  if (row.others) {
    return (
      <button
        type="button"
        className="lane-name lane-others"
        aria-expanded={open}
        aria-label={row.label}
        title={row.title}
        onClick={flip}
      >
        {open ? "▾" : "▸"} {row.label}
      </button>
    );
  }

  const calls = kernel ? <div className="lane-kernel" aria-hidden="true" /> : null;
  return (
    <>
      <button
        type="button"
        className={`lane-name ${selected ? "selected" : ""} ${row.nested ? "nested" : ""}`}
        aria-pressed={selected}
        title={row.title}
        onClick={click}
      >
        {row.label} {row.dim ? <span className="dim">{row.dim}</span> : null}
      </button>
      {calls}
      {shown > 0 ? (
        <div className="lane-zones" style={style}>
          {depth > 1 ? (
            <button
              type="button"
              className="lane-chevron"
              aria-expanded={open}
              aria-label={`${open ? "Hide" : "Show"} ${row.label} ${row.dim ?? ""}'s nested zones`}
              onClick={flip}
            >
              {open ? "▾" : `▸ +${depth - 1}`}
            </button>
          ) : null}
        </div>
      ) : null}
    </>
  );
});

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
    <div className="plot-lane">
      <div className="plot-name" style={NAMES_STYLE}>
        <div>{series.name}</div>
        <div className="dim">
          {extent}
          {series.unshown > 0 ? ` · ${formatCount(series.unshown)} not finite` : ""}
        </div>
      </div>
      <svg
        width={width}
        height={PLOT}
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an inline drawing, not an image file
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

function formatValue(value: number): string {
  return Number.isInteger(value) ? String(value) : value.toPrecision(4);
}

function offCpu(thread: ProfileSummary["threads"][number]): string {
  if (thread.blocked === 0 && thread.preempted === 0) {
    return "";
  }
  const blocked = formatDuration(thread.blocked);
  const preempted = formatDuration(thread.preempted);
  return `; off its CPU ${blocked} blocked, ${preempted} preempted`;
}
