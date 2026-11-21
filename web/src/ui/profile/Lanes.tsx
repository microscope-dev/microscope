import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  onSettled,
  Show,
  untrack,
} from "solid-js";

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
import { shallowEqual } from "../../state";
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

function useSettled<T>(value: () => T, wait: number): () => T {
  const [settled, setSettled] = createSignal<{ current: T }>({ current: untrack(value) });
  createEffect(
    () => value(),
    (next) => {
      const timer = setTimeout(() => setSettled({ current: next }), wait);
      return () => clearTimeout(timer);
    },
  );
  return () => settled().current;
}

function menu(event: MouseEvent) {
  if (event.ctrlKey) {
    event.preventDefault();
  }
}

export function Lanes() {
  const focus = useProfile();
  const [measure, width] = useWidth();
  let box: HTMLDivElement | undefined;
  let canvas: HTMLCanvasElement | undefined;
  const scheme = useShownScheme();
  const summary = createMemo(() => focus().summary);
  const profileId = createMemo(() => focus().profile);
  const lookSrc = createMemo(() => focus().look.src);
  const lookLanes = createMemo(() => focus().look.lanes);
  const lookZs = createMemo(() => focus().look.zs);
  const lookZb = createMemo(() => focus().look.zb);
  const lookZi = createMemo(() => focus().look.zi);
  const lookRange = createMemo(() => focus().look.range);
  const lookTh = createMemo(() => focus().look.th);
  const lookTk = createMemo(() => focus().look.tk);
  const queryRevision = createMemo(() => focus().query.revision ?? 0);
  const behind = createMemo(() => summary().behind);
  const budgetChanges = createMemo(() => summary().budgetChanges);

  const plot = createMemo(() => Math.max(width() - NAMES, 0));
  const total = createMemo(() => Math.max(summary().duration, 1));

  const [view, setView] = createSignal<[number, number]>([0, untrack(total)]);
  const whole = createMemo(() => view()[0] <= 0 && view()[1] >= total());

  const shownView = createMemo<[number, number]>(
    () => {
      const currentTotal = total();
      const currentView = view();
      return whole()
        ? [0, currentTotal]
        : [Math.round(currentView[0]), Math.round(Math.min(currentView[1], currentTotal))];
    },
    { equals: (a, b) => a[0] === b[0] && a[1] === b[1] },
  );
  const asked = useSettled(shownView, SETTLE);
  const span = createMemo(() => Math.max(shownView()[1] - shownView()[0], 1));

  const byTask = createMemo(() => lookSrc() === "waits" || lookLanes() === "tasks");
  const buckets = createMemo(() => Math.max(1, Math.min(2048, Math.floor(plot() / 2))));
  const timelineParams = createMemo(
    () => {
      const {
        range: _range,
        threads: _threads,
        tasks: _tasks,
        zones: _zones,
        base: _base,
        ...rest
      } = focus().query;
      return plot() > 0
        ? { ...rest, range: asked(), buckets: buckets(), taskLanes: byTask() }
        : null;
    },
    { equals: shallowEqual },
  );
  const timeline = useAnswer("profileTimeline", timelineParams);
  const timelineData = createMemo(() => timeline().data);

  const zoned = createMemo(
    () => !byTask() && (summary().zoneSites.length > 0 || summary().probes.events > 0),
  );

  const zonesParams = createMemo(
    () => {
      const zs = lookZs();
      const zb = lookZb();
      const band = zs && zb ? parseBand(zb) : null;
      return zoned() && plot() > 0
        ? {
            profile: profileId(),
            revision: queryRevision(),
            range: asked(),
            width: Math.round(plot()),
            site: zs ?? null,
            band,
          }
        : null;
    },
    { equals: shallowEqual },
  );
  const zones = useAnswer("profileZones", zonesParams);
  const zonesData = createMemo(() => zones().data);
  const blocks = createMemo(() => {
    const data = zonesData();
    return data ? decodeZones(data, data.payload) : null;
  });
  const index = createMemo(() => {
    const current = blocks();
    return current ? indexBlocks(current) : null;
  });
  const marks = createMemo(() => zonesData()?.marks ?? []);
  const plots = createMemo(() => zonesData()?.plots ?? []);

  const [othersOpen, setOthersOpen] = createSignal(false);
  const lanes = createMemo(() => {
    const all = rows(summary(), lookSrc(), lookLanes());
    return othersOpen() ? all.flatMap((row) => [row, ...(row.others ?? [])]) : all;
  });
  const chosen = createMemo(() => (byTask() ? lookTk() : lookTh())?.join(","));
  const selected = createMemo(() => {
    const text = chosen();
    return text ? new Set(text.split(",")) : null;
  });

  let seenDepths = new Map<number, number>();
  const depths = createMemo<ReadonlyMap<number, number>>(() => {
    const current = index();
    if (!current) {
      return seenDepths;
    }
    const deeper = [...current.depths].filter(
      ([thread, depth]) => depth > (seenDepths.get(thread) ?? 0),
    );
    if (deeper.length === 0) {
      return seenDepths;
    }
    seenDepths = new Map([...seenDepths, ...deeper]);
    return seenDepths;
  });
  const [collapsed, setCollapsed] = createSignal<ReadonlySet<number>>(new Set());

  const shownRows = createMemo(() => {
    const taskMode = byTask();
    const currentDepths = depths();
    const currentCollapsed = collapsed();
    return lanes().map((row) => {
      const depth = taskMode ? 0 : (currentDepths.get(row.lane) ?? 0);
      return currentCollapsed.has(row.lane) ? Math.min(depth, 1) : depth;
    });
  });

  const kernel = createMemo(() => !byTask() && summary().syscalls > 0);
  const head = createMemo(() => (kernel() ? STRIP + KERNEL : STRIP));
  const laid = createMemo(() => layLanes(shownRows(), head(), ROW));
  const answered = createMemo(
    () =>
      new Map(
        (timelineData()?.lanes ?? []).map((lane) => [
          (byTask() ? lane.task : lane.thread) ?? -1,
          lane,
        ]),
      ),
  );
  const laneOf = createMemo(() => new Map(lanes().map((row, at) => [row.lane, at])));
  const height = createMemo(() => laid().height);

  const range = createMemo(() => rangeOf({ range: lookRange() }), {
    equals: (a, b) => a === b || (a !== null && b !== null && a[0] === b[0] && a[1] === b[1]),
  });
  const instance = createMemo(() => {
    const zi = lookZi();
    return zi ? parseInstance(zi) : null;
  });
  const [dragging, setDragging] = createSignal<[number, number] | null>(null);
  const [pointer, setPointer] = createSignal<{ time: number; x: number; y: number } | null>(null);
  const [hit, setHit] = createSignal<Hit | null>(null);

  let pressed: {
    x: number;
    view: [number, number];
    moved: boolean;
    pan: boolean;
  } | null = null;
  const [panning, setPanning] = createSignal(false);

  let rulerFrom: number | null = null;

  let wanted: [number, number] | null = null;
  let frame = 0;
  const show = (next: [number, number]) => {
    wanted = next;
    if (frame !== 0) {
      return;
    }
    frame = requestAnimationFrame(() => {
      frame = 0;
      const chosenView = wanted;
      wanted = null;
      if (chosenView) {
        setView(chosenView);
      }
    });
  };
  onCleanup(() => cancelAnimationFrame(frame));

  createEffect(
    () => instance(),
    (currentInstance) => {
      if (!currentInstance) {
        return;
      }
      const [start, end] = currentInstance.span;
      const [currentView, currentPlot, currentTotal] = untrack(() => [
        shownView(),
        plot(),
        total(),
      ]);
      const per = (currentView[1] - currentView[0]) / Math.max(currentPlot, 1);
      if (start < currentView[0] || end > currentView[1] || (end - start) / per < 4) {
        const margin = Math.max((end - start) / 3, 1000);
        const from = Math.max(start - margin, 0);
        setView([from, Math.min(Math.max(end + margin, from + 1000), currentTotal)]);
      }
      setCollapsed((was) => {
        if (!was.has(currentInstance.thread)) {
          return was;
        }
        const next = new Set(was);
        next.delete(currentInstance.thread);
        return next;
      });
    },
  );

  let scrolledTo: string | null = null;
  createEffect(
    () => ({
      zi: focus().look.zi,
      instance: instance(),
      laneOf: laneOf(),
      laid: laid(),
      shownRows: shownRows(),
      head: head(),
    }),
    ({
      zi,
      instance: currentInstance,
      laneOf: lanesMap,
      laid: layout,
      shownRows: rowsShown,
      head: headHeight,
    }) => {
      const element = box;
      const drawn = canvas;
      const lane = currentInstance ? lanesMap.get(currentInstance.thread) : undefined;
      if (!zi || zi === scrolledTo || !element || !drawn || lane === undefined) {
        return;
      }
      scrolledTo = zi;
      const ruler = element.querySelector(".timeline-ruler")?.getBoundingClientRect().height ?? 0;
      const top =
        drawn.getBoundingClientRect().top -
        element.getBoundingClientRect().top +
        element.scrollTop +
        (layout.tops[lane] ?? 0);
      const bottom = top + headHeight + (rowsShown[lane] ?? 0) * ROW;
      if (top < element.scrollTop + ruler || bottom > element.scrollTop + element.clientHeight) {
        element.scrollTop = top - ruler;
      }
    },
  );

  const wheel = (event: WheelEvent) => {
    const currentView = shownView();
    const currentPlot = plot();
    const currentTotal = total();
    const sideways = Math.abs(event.deltaX) > Math.abs(event.deltaY);
    if (!event.ctrlKey && !event.metaKey && !sideways) {
      return;
    }
    event.preventDefault();
    const from = wanted ?? currentView;
    const per = (from[1] - from[0]) / Math.max(currentPlot, 1);
    if (sideways) {
      show(panned(from, event.deltaX * per, currentTotal));
      return;
    }
    const left = canvas?.getBoundingClientRect().left ?? 0;
    const x = Math.min(Math.max(event.clientX - left, 0), currentPlot);
    show(zoomed(from, from[0] + x * per, Math.exp(event.deltaY * 0.002), currentTotal));
  };

  onSettled(() => {
    const element = box;
    if (!element) {
      return;
    }
    element.addEventListener("wheel", wheel, { passive: false });
    return () => element.removeEventListener("wheel", wheel);
  });

  const palette = createMemo(() => {
    const names = blocks()?.names ?? [];
    const sites = names.map((name) => name.split("=")[0] ?? name);
    const dark = scheme() === "dark";
    return {
      sites,
      fills: sites.map((site) =>
        dark ? `oklch(0.5 0.13 ${hue(site)})` : `oklch(0.8 0.13 ${hue(site)})`,
      ),
    };
  });

  const paintState = createMemo(
    () => ({
      timelineData: timelineData(),
      answered: answered(),
      kernel: kernel(),
      head: head(),
      asked: asked(),
      blocks: blocks(),
      index: index(),
      marks: marks(),
      lanes: lanes(),
      laid: laid(),
      laneOf: laneOf(),
      shownRows: shownRows(),
      selected: selected(),
      plot: plot(),
      height: height(),
      span: span(),
      shownView: shownView(),
      range: range(),
      instance: instance(),
      dragging: dragging(),
      zs: lookZs(),
      palette: palette(),
      behind: behind(),
      budgetChanges: budgetChanges(),
      scheme: scheme(),
    }),
    { equals: shallowEqual },
  );

  createEffect(paintState, (s) => {
    const element = canvas;
    const context = prepare(element, s.plot, Math.max(s.height, 1));
    if (!element || !context) {
      return;
    }
    element.dataset.scheme = s.scheme;
    const x = (time: number) => ((time - s.shownView[0]) * s.plot) / s.span;
    context.font = `11px ${token("--mono")}`;
    context.textBaseline = "middle";

    context.fillStyle = token("--line");
    for (const { at } of timeTicks(s.shownView, s.plot)) {
      if (at > 0 && at < 1) {
        context.fillRect(Math.round(at * s.plot), 0, 1, s.height);
      }
    }

    const marked = s.dragging ?? (s.instance ? s.instance.span : s.range);
    if (marked) {
      const [from, to] = [x(marked[0]), x(marked[1])];

      const light = s.scheme === "light";
      context.fillStyle = token(light ? "--accent" : "--accent-soft");
      context.globalAlpha = light ? 0.12 : 0.6;
      context.fillRect(0, 0, Math.max(from, 0), s.height);
      context.fillRect(to, 0, Math.max(s.plot - to, 0), s.height);
      context.globalAlpha = 1;
    }

    const sliced = s.timelineData?.range ?? s.asked;
    paintCpu(context, {
      answered: s.answered,
      rows: s.lanes,
      tops: s.laid.tops,
      selected: s.selected,
      x,
      range: sliced,
    });
    if (s.kernel) {
      paintKernel(context, {
        answered: s.answered,
        rows: s.lanes,
        tops: s.laid.tops,
        selected: s.selected,
        x,
        range: sliced,
      });
    }

    if (s.blocks && s.index) {
      const currentBlocks = s.blocks;
      const folded = token("--line-strong");
      const flagged = token("--changed");
      const found = token("--accent");
      const ink = token("--ink");
      const glyph = context.measureText("0").width;
      const lit = (at: number) => !s.zs || currentBlocks.chosen[at] === 1;
      for (let at = 0; at < s.blocks.count; at += 1) {
        const thread = s.blocks.thread[at] ?? 0;
        const lane = s.laneOf.get(thread);
        const depth = s.blocks.depth[at] ?? 0;
        if (lane === undefined || depth >= (s.shownRows[lane] ?? 0)) {
          continue;
        }
        const top = (s.laid.tops[lane] ?? 0) + s.head + depth * ROW;
        const start = s.blocks.start[at] ?? 0;
        const end = s.blocks.end[at] ?? 0;
        const from = x(start);
        const to = x(end);
        if (to < 0 || from > s.plot) {
          continue;
        }
        const wide = Math.max(to - from, 1);
        const name = s.blocks.name[at] ?? 0;
        const label = s.blocks.names[name] ?? "";
        const many = (s.blocks.zones[at] ?? 1) > 1;
        const fill = many && label === "zones" ? folded : (s.palette.fills[name] ?? folded);
        const dim = lit(at) ? 1 : 0.3;
        const covered = s.blocks.covered[at] ?? 0;
        const running = onCpu(s.blocks, at);
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
            lane: s.answered.get(thread),
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
        if (s.zs && dim === 1) {
          context.strokeStyle = found;
          context.lineWidth = 1.5;
          context.strokeRect(from + 0.75, top + 1.75, Math.max(wide - 1.5, 1), ROW - 3.5);
        } else if ((s.blocks.flags[at] ?? 0) !== 0) {
          context.strokeStyle = flagged;
          context.lineWidth = 1;
          context.setLineDash([3, 2]);
          context.strokeRect(from + 0.5, top + 1.5, Math.max(wide - 1, 1), ROW - 3);
          context.setLineDash([]);
        }
        const room = Math.min(to, s.plot) - Math.max(from, 0) - 6;
        const text =
          room > 16
            ? many
              ? fitting([formatCount(s.blocks.zones[at] ?? 0, "zone")], room, glyph)
              : fitting(zoneLabels(label, end - start, running), room, glyph)
            : null;
        if (text) {
          context.globalAlpha = dim;
          context.fillStyle = ink;
          context.fillText(text, Math.max(from, 0) + 3, top + ROW / 2);
        }
      }
      context.globalAlpha = 1;

      for (const mark of s.marks) {
        const lane = s.laneOf.get(mark.thread);
        if (lane === undefined) {
          continue;
        }
        context.fillStyle =
          mark.kind === "trigger" ? flagged : mark.kind === "frame" ? folded : found;
        const at = Math.min(Math.max(x(mark.time) - 0.5, 0), s.plot - 1.5);
        context.fillRect(at, (s.laid.tops[lane] ?? 0) + 1, 1.5, 6);
      }
    }

    context.fillStyle = token("--line");
    context.globalAlpha = 0.7;
    for (const [from, to] of s.behind) {
      context.fillRect(x(from), 0, Math.max(x(to) - x(from), 1), s.height);
    }
    context.globalAlpha = 1;

    context.strokeStyle = token("--changed");
    context.lineWidth = 1;
    context.setLineDash([2, 3]);
    for (const step of s.budgetChanges) {
      const at = Math.round(x(step.time)) + 0.5;
      if (at >= 0 && at <= s.plot) {
        context.beginPath();
        context.moveTo(at, 0);
        context.lineTo(at, s.height);
        context.stroke();
      }
    }
    context.setLineDash([]);

    if (marked) {
      const [from, to] = [x(marked[0]), x(marked[1])];
      const wide = Math.max(to - from, 3);
      context.fillStyle = token("--accent");
      context.fillRect((from + to) / 2 - wide / 2, 0, wide, 2);
      context.fillRect((from + to) / 2 - wide / 2, s.height - 2, wide, 2);
    }
    const lane = s.instance ? s.laneOf.get(s.instance.thread) : undefined;
    if (s.instance && lane !== undefined && s.blocks) {
      for (let at = 0; at < s.blocks.count; at += 1) {
        if (
          s.blocks.thread[at] === s.instance.thread &&
          s.blocks.start[at] === s.instance.span[0] &&
          s.blocks.end[at] === s.instance.span[1] &&
          (s.blocks.depth[at] ?? 0) < (s.shownRows[lane] ?? 0)
        ) {
          const top = (s.laid.tops[lane] ?? 0) + s.head + (s.blocks.depth[at] ?? 0) * ROW;
          const from = x(s.instance.span[0]);
          context.strokeStyle = token("--ink");
          context.lineWidth = 2;
          context.strokeRect(from, top + 1, Math.max(x(s.instance.span[1]) - from, 2), ROW - 2);
        }
      }
    }
  });

  const at = (px: number, py: number): Hit | null => {
    const currentPlot = plot();
    const currentHeight = height();
    if (px < 0 || px >= currentPlot || py < 0 || py >= currentHeight) {
      return null;
    }
    const currentView = shownView();
    const currentSpan = span();
    const currentLaid = laid();
    const currentLanes = lanes();
    const time = currentView[0] + (px * currentSpan) / Math.max(currentPlot, 1);
    const lane = laneAt(currentLaid.tops, py);
    const row = currentLanes[lane];
    if (!row) {
      return null;
    }
    const inside = py - (currentLaid.tops[lane] ?? 0);
    const slack = (2 * currentSpan) / Math.max(currentPlot, 1);
    const step = stepAt(focus().summary.budgetChanges, time, slack);
    const none = { block: null, mark: null, kernel: null, wait: null, step };
    const currentHead = head();
    if (inside >= STRIP && inside < currentHead) {
      const calls = row.others ? undefined : answered().get(row.lane);
      const sliced = timeline().data?.range ?? asked();
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
      for (const each of marks()) {
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
    const depth = Math.floor((inside - currentHead) / ROW);
    const currentBlocks = blocks();
    const currentIndex = index();
    const block =
      currentBlocks && currentIndex
        ? blockAt(currentBlocks, currentIndex, row.lane, depth, time, slack)
        : null;
    return { ...none, time, lane, block };
  };

  const canvasLeft = () => canvas?.getBoundingClientRect().left ?? 0;
  const local = (event: { clientX: number; clientY: number }) => {
    const bounds = canvas?.getBoundingClientRect() ?? { left: 0, top: 0 };
    return [event.clientX - bounds.left, event.clientY - bounds.top] as const;
  };

  const press = (event: PointerEvent & { currentTarget: HTMLDivElement }) => {
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
    pressed = {
      x: event.clientX,
      view: shownView(),
      moved: false,
      pan: event.ctrlKey || event.metaKey,
    };
  };
  const select = (block: number, zoom: boolean) => {
    const currentBlocks = blocks();
    assert(currentBlocks, "a zone was chosen with none drawn");
    const zone: [number, number] = [currentBlocks.start[block] ?? 0, currentBlocks.end[block] ?? 0];
    if ((currentBlocks.zones[block] ?? 1) > 1 || zoom) {
      const margin = (zone[1] - zone[0]) / 20;
      show([Math.max(zone[0] - margin, 0), Math.min(zone[1] + margin, total())]);
      if ((currentBlocks.zones[block] ?? 1) > 1) {
        return;
      }
    }
    if (zone[1] <= zone[0]) {
      return;
    }
    const chosenOne: Instance = { thread: currentBlocks.thread[block] ?? 0, span: zone };
    const site = palette().sites[currentBlocks.name[block] ?? 0] ?? "";
    focus().setLook((prev) => {
      const again = prev.zi === `${chosenOne.thread}:${formatRange(zone)}` && !zoom;
      const next = withInstance(prev, again ? null : chosenOne);

      return prev.zs === site ? next : withLook(clearBand(next), "zs", site);
    }, true);
  };

  const chooseRange = (from: number, to: number) => {
    const [start, end] = [Math.min(from, to), Math.max(from, to)];
    const chosenRange =
      ((end - start) * plot()) / span() >= 3 ? formatRange([start, end]) : undefined;
    focus().setLook((prev) => withInstance(withLook(prev, "range", chosenRange), null), true);
  };
  const release = (event: PointerEvent) => {
    const was = pressed;
    pressed = null;
    setPanning(false);
    setDragging(null);
    if (was?.moved && !was.pan) {
      const left = canvasLeft();
      chooseRange(
        timeAt(was.view, plot(), was.x - left),
        timeAt(was.view, plot(), event.clientX - left),
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
      focus().setLook((prev) => {
        const { range: _range, ...rest } = prev;
        return prev.range ? rest : prev;
      }, true);
    }
  };
  const doubleClick = (event: MouseEvent) => {
    const found = at(...local(event));
    if (found?.block !== null && found?.block !== undefined) {
      select(found.block, true);
    }
  };
  const hover = (event: PointerEvent) => {
    const held = pressed;
    const currentPlot = plot();
    if (held) {
      const by = event.clientX - held.x;
      if (held.moved || Math.abs(by) >= DRAG) {
        if (!held.moved) {
          held.moved = true;
          setPanning(held.pan);
        }
        setHit(null);
        if (held.pan) {
          const per = (held.view[1] - held.view[0]) / Math.max(currentPlot, 1);
          show(panned(held.view, -by * per, total()));
          return;
        }
        const left = canvasLeft();
        const [from, to] = [
          timeAt(held.view, currentPlot, held.x - left),
          timeAt(held.view, currentPlot, event.clientX - left),
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
      px >= 0 && px < currentPlot
        ? {
            time: shownView()[0] + (px * span()) / Math.max(currentPlot, 1),
            x: event.clientX,
            y: event.clientY,
          }
        : null,
    );
  };
  const leave = () => {
    setHit(null);
    setPointer(null);
  };
  const cancel = () => {
    pressed = null;
    setPanning(false);
    setDragging(null);
  };

  const escape = (event: KeyboardEvent) => {
    if (event.key === "Escape" && (pressed || rulerFrom !== null)) {
      event.preventDefault();
      pressed = null;
      rulerFrom = null;
      setPanning(false);
      setDragging(null);
    }
  };
  onSettled(() => {
    window.addEventListener("keydown", escape, { capture: true });
    return () => window.removeEventListener("keydown", escape, { capture: true });
  });

  const rulerTime = (event: PointerEvent) => {
    const left = canvas?.getBoundingClientRect().left ?? 0;
    const currentPlot = plot();
    const px = Math.min(Math.max(event.clientX - left, 0), currentPlot);
    return shownView()[0] + (px * span()) / Math.max(currentPlot, 1);
  };
  const rulerDown = (event: PointerEvent & { currentTarget: HTMLDivElement }) => {
    if (event.button !== 0) {
      return;
    }
    event.currentTarget.setPointerCapture(event.pointerId);
    rulerFrom = rulerTime(event);
  };
  const rulerMove = (event: PointerEvent) => {
    const from = rulerFrom;
    const to = rulerTime(event);
    setPointer({ time: to, x: event.clientX, y: event.clientY });
    if (from !== null) {
      setDragging([Math.min(from, to), Math.max(from, to)]);
    }
  };
  const rulerUp = (event: PointerEvent) => {
    const from = rulerFrom;
    rulerFrom = null;
    setDragging(null);
    if (from === null) {
      return;
    }
    chooseRange(from, rulerTime(event));
  };
  const rulerCancel = () => {
    rulerFrom = null;
    setDragging(null);
  };

  const toggle = (id: string) =>
    focus().setLook((prev) => {
      const key = byTask() ? "tk" : "th";
      const ids = new Set(prev[key] ?? []);
      if (!ids.delete(id)) {
        ids.add(id);
      }
      return withLook(prev, key, ids.size > 0 ? [...ids].toSorted() : undefined);
    });
  const expand = (thread: number) =>
    setCollapsed((was) => {
      const next = new Set(was);
      if (!next.delete(thread)) {
        next.add(thread);
      }
      return next;
    });

  const flipOthers = () => setOthersOpen((was) => !was);

  const tip = createMemo(() => {
    const currentHit = hit();
    return currentHit && !panning()
      ? tipLines(currentHit, {
          blocks: blocks(),
          index: index(),
          summary: focus().summary,
          span: span(),
          plot: plot(),
          answered: kernel() ? answered() : NO_LANES,
        })
      : null;
  });

  const marked = createMemo(() => dragging() ?? instance()?.span ?? range());
  const rulerPointer = createMemo(() => (dragging() ? null : (pointer()?.time ?? null)));
  const cursorPointer = createMemo(() => (dragging() || panning() ? null : pointer()));
  const tipStyle = createMemo(() => {
    const current = pointer();
    return current ? { left: `${current.x + 12}px`, top: `${current.y + 16}px` } : undefined;
  });

  return (
    <div
      class={`profile-timeline ${panning() ? "panning" : ""}`}
      ref={(node) => {
        box = node;
        measure(node);
      }}
      data-testid="timeline"
      onPointerDown={press}
      onPointerUp={release}
      onPointerCancel={cancel}
      onPointerMove={hover}
      onPointerOut={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget as Node | null)) {
          leave();
        }
      }}
      onDblClick={doubleClick}
      onContextMenu={menu}
    >
      <Ruler
        view={shownView()}
        plot={plot()}
        marked={marked()}
        pointer={rulerPointer()}
        onPointerDown={rulerDown}
        onPointerMove={rulerMove}
        onPointerUp={rulerUp}
        onPointerCancel={rulerCancel}
      />
      <div class="timeline-body">
        <div class="lane-names" style={NAMES_STYLE}>
          <For each={lanes()} keyed={(row) => row.id}>
            {(row, lane) => (
              <LaneName
                row={row()}
                selected={selected()?.has(row().id) ?? false}
                toggle={toggle}
                depth={byTask() ? 0 : (depths().get(row().lane) ?? 0)}
                open={row().others ? othersOpen() : !collapsed().has(row().lane)}
                expand={row().others ? flipOthers : expand}
                rows={shownRows()[lane()] ?? 0}
                kernel={kernel()}
              />
            )}
          </For>
        </div>
        <Show
          when={!timeline().error}
          fallback={
            <div class="empty error" style={{ width: `${plot()}px` }}>
              {timeline().error?.message}
            </div>
          }
        >
          <div class="timeline-plot" style={{ width: `${plot()}px` }}>
            <canvas
              ref={(node) => {
                canvas = node;
              }}
              class={`lanes ${zonesParams() && !zones().current ? "stale" : ""}`}
              style={{ width: `${plot()}px`, height: `${height()}px` }}
              // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a canvas drawn here, not an image file
              role="img"
              aria-label={`CPU${zoned() ? " and zones" : ""} over ${formatDuration(span())}`}
            />
            <Show when={cursorPointer()}>
              {(current) => (
                <Cursor
                  x={((current().time - shownView()[0]) * plot()) / span()}
                  height={height()}
                />
              )}
            </Show>
          </div>
        </Show>
      </div>
      <Show when={zones().error}>
        {(error) => <div class="empty error">{error().message}</div>}
      </Show>
      <For each={plots()} keyed={(series) => series.name}>
        {(series) => (
          <PlotLane series={series()} origin={shownView()[0]} span={span()} width={plot()} />
        )}
      </For>
      <Show when={tip()}>
        {(lines) => (
          <div class="lanes-tip" style={tipStyle()} data-testid="zone-tip">
            <For each={lines()}>{(line) => <div>{line}</div>}</For>
          </div>
        )}
      </Show>
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

function Ruler(props: {
  view: [number, number];
  plot: number;
  marked: readonly [number, number] | null;
  pointer: number | null;
  onPointerDown: (event: PointerEvent & { currentTarget: HTMLDivElement }) => void;
  onPointerMove: (event: PointerEvent) => void;
  onPointerUp: (event: PointerEvent) => void;
  onPointerCancel: () => void;
}) {
  const layout = createMemo(() => {
    const view = props.view;
    const plot = props.plot;
    const marked = props.marked;
    const pointer = props.pointer;
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
    const visibleMarks = marks.flatMap(({ at, label }, index) =>
      covered(at * plot, label, index)
        ? []
        : [
            {
              at,
              x: at * plot,
              label,
              className: index === 0 ? "first" : index === marks.length - 1 ? "last" : undefined,
            },
          ],
    );
    return { visibleMarks, measure, pills };
  });

  return (
    <div class="timeline-ruler" data-testid="ruler">
      <div class="ruler-gap" style={NAMES_STYLE} />
      <div
        class="ruler-marks"
        style={{ width: `${props.plot}px` }}
        title="Drag to select a range; click to clear it"
        onPointerDown={(event) => props.onPointerDown(event)}
        onPointerMove={(event) => props.onPointerMove(event)}
        onPointerUp={(event) => props.onPointerUp(event)}
        onPointerCancel={() => props.onPointerCancel()}
      >
        <For each={layout().visibleMarks} keyed={(mark) => mark.at}>
          {(mark) => <RulerMark x={mark().x} label={mark().label} className={mark().className} />}
        </For>
        <Show when={layout().measure}>
          {(m) => <Measured x={m().x} width={m().width} label={m().label} />}
        </Show>
        <For each={layout().pills} keyed={(pill) => pill.id}>
          {(pill) => (
            <RulerMark x={pill().x} label={pill().label} className={`marker ${pill().id}`} />
          )}
        </For>
      </div>
    </div>
  );
}

function Measured(props: { x: number; width: number; label: string | null }) {
  return (
    <div
      class="ruler-measure"
      style={{ left: `${props.x}px`, width: `${props.width}px` }}
      data-testid="range-length"
    >
      <Show when={props.label !== null}>
        <span>{props.label}</span>
      </Show>
    </div>
  );
}

function RulerMark(props: { x: number; label: string; className?: string | undefined }) {
  return (
    <span class={props.className} style={{ left: `${props.x}px` }}>
      {props.label}
    </span>
  );
}

function Cursor(props: { x: number; height: number }) {
  return (
    <div
      class="timeline-cursor"
      style={{ left: `${Math.round(props.x)}px`, height: `${props.height}px` }}
    />
  );
}

function LaneName(props: {
  row: Row;
  selected: boolean;
  toggle: (id: string) => void;
  depth: number;
  open: boolean;
  expand: (thread: number) => void;
  rows: number;
  kernel: boolean;
}) {
  return (
    <Show
      when={!props.row.others}
      fallback={
        <button
          type="button"
          class="lane-name lane-others"
          aria-expanded={props.open ? "true" : "false"}
          aria-label={props.row.label}
          title={props.row.title}
          onClick={() => props.expand(props.row.lane)}
        >
          {props.open ? "▾" : "▸"} {props.row.label}
        </button>
      }
    >
      <button
        type="button"
        class={`lane-name ${props.selected ? "selected" : ""} ${props.row.nested ? "nested" : ""}`}
        aria-pressed={props.selected ? "true" : "false"}
        title={props.row.title}
        onClick={() => props.toggle(props.row.id)}
      >
        {props.row.label}{" "}
        <Show when={props.row.dim}>{(dim) => <span class="dim">{dim()}</span>}</Show>
      </button>
      <Show when={props.kernel}>
        <div class="lane-kernel" aria-hidden="true" />
      </Show>
      <Show when={props.rows > 0}>
        <div class="lane-zones" style={{ height: `${props.rows * ROW}px` }}>
          <Show when={props.depth > 1}>
            <button
              type="button"
              class="lane-chevron"
              aria-expanded={props.open ? "true" : "false"}
              aria-label={`${props.open ? "Hide" : "Show"} ${props.row.label} ${props.row.dim ?? ""}'s nested zones`}
              onClick={() => props.expand(props.row.lane)}
            >
              {props.open ? "▾" : `▸ +${props.depth - 1}`}
            </button>
          </Show>
        </div>
      </Show>
    </Show>
  );
}

function PlotLane(props: { series: ProfilePlot; origin: number; span: number; width: number }) {
  const bounds = createMemo(() => {
    let low = Number.POSITIVE_INFINITY;
    let high = Number.NEGATIVE_INFINITY;
    for (const bin of props.series.bins) {
      low = Math.min(low, bin[3] ?? low);
      high = Math.max(high, bin[4] ?? high);
    }
    if (props.series.held !== null) {
      low = Math.min(low, props.series.held);
      high = Math.max(high, props.series.held);
    }
    return {
      low,
      high,
      extent: Number.isFinite(low) ? `${formatValue(low)} – ${formatValue(high)}` : "",
    };
  });
  return (
    <div class="plot-lane">
      <div class="plot-name" style={NAMES_STYLE}>
        <div>{props.series.name}</div>
        <div class="dim">
          {bounds().extent}
          {props.series.unshown > 0 ? ` · ${formatCount(props.series.unshown)} not finite` : ""}
        </div>
      </div>
      <svg
        width={props.width}
        height={PLOT}
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an inline drawing, not an image file
        role="img"
        aria-label={`Plot ${props.series.name}, ${formatCount(props.series.values, "value")}${bounds().extent ? ` from ${bounds().extent}` : ""}`}
      >
        <path
          d={plotPath(
            props.series.bins,
            props.origin,
            props.span,
            props.width,
            PLOT,
            bounds().low,
            bounds().high,
            props.series.held,
            props.series.continues,
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
