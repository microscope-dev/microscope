// Zones as the server sends them, in columns, and how the lanes find and
// lay them out: under each thread's samples, a row per depth.

import { assert } from "../assert";
import type { ProfileThread, ProfileZones } from "../protocol";
import { formatDuration, formatInstant } from "./paint";

export interface ZoneBlocks {
  count: number;
  /** Nanoseconds from the profile's start. */
  start: Float64Array;
  end: Float64Array;
  /** Nanoseconds its zones cover, and of those, off their thread's CPU. */
  covered: Float64Array;
  blocked: Float64Array;
  preempted: Float64Array;
  thread: Uint32Array;
  depth: Uint32Array;
  /** Indices into `names`. */
  name: Uint32Array;
  /** How many zones each block draws: more than one is a fold. */
  zones: Uint32Array;
  flags: Uint32Array;
  names: string[];
  /** Whether time off the CPU was traced, without which it reads zero. */
  offCpu: boolean;
}

/** Decodes an answer and the columns sent before it: starts, ends, and
 * times covered, blocked, and preempted as little-endian `f64`s, then
 * threads, depths, names, counts, and flags as `u32`s. */
export function decodeZones(answer: ProfileZones, payload: Uint8Array | undefined): ZoneBlocks {
  const count = answer.blocks;
  const bytes = payload ?? new Uint8Array();
  if (bytes.byteLength !== count * 60) {
    throw new Error(`${count} zone blocks need ${count * 60} bytes, not ${bytes.byteLength}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const floats = (column: number) =>
    Float64Array.from({ length: count }, (_, at) =>
      view.getFloat64((column * count + at) * 8, true),
    );
  const words = (column: number) =>
    Uint32Array.from({ length: count }, (_, at) =>
      view.getUint32(count * 40 + (column * count + at) * 4, true),
    );
  return {
    count,
    start: floats(0),
    end: floats(1),
    covered: floats(2),
    blocked: floats(3),
    preempted: floats(4),
    thread: words(0),
    depth: words(1),
    name: words(2),
    zones: words(3),
    flags: words(4),
    names: answer.names,
    offCpu: answer.offCpu,
  };
}

/** Each row of zones, a thread's at one depth, as the blocks in it by
 * start, and how deep each thread's zones go. */
export interface BlockIndex {
  rows: Map<number, Int32Array>;
  depths: Map<number, number>;
}

const rowKey = (thread: number, depth: number) => thread * 65_536 + depth;

/** Indexes the blocks by row, so the one at a time is found by bisection. */
export function indexBlocks(blocks: ZoneBlocks): BlockIndex {
  const lists = new Map<number, number[]>();
  const depths = new Map<number, number>();
  for (let at = 0; at < blocks.count; at += 1) {
    const thread = blocks.thread[at] ?? 0;
    const depth = blocks.depth[at] ?? 0;
    assert(depth < 65_536, `zone depth ${depth} is too deep to index`);
    const key = rowKey(thread, depth);
    const list = lists.get(key);
    if (list) {
      list.push(at);
    } else {
      lists.set(key, [at]);
    }
    depths.set(thread, Math.max(depths.get(thread) ?? 0, depth + 1));
  }
  const rows = new Map<number, Int32Array>();
  for (const [key, list] of lists) {
    const row = Int32Array.from(list).toSorted(
      (a, b) => (blocks.start[a] ?? 0) - (blocks.start[b] ?? 0),
    );
    rows.set(key, row);
  }
  return { rows, depths };
}

/** The block of a row at `time`, or failing that the nearest within
 * `slack` of it, if any. */
export function blockAt(
  blocks: ZoneBlocks,
  index: BlockIndex,
  thread: number,
  depth: number,
  time: number,
  slack: number,
): number | null {
  assert(slack >= 0, `slack ${slack} is negative`);
  const row = index.rows.get(rowKey(thread, depth));
  if (!row) {
    return null;
  }
  // The last block that starts by `time + slack`, and the one before it,
  // which may be the one `time` is in where the slack reaches the next.
  let [low, high] = [0, row.length];
  while (low < high) {
    const middle = (low + high) >>> 1;
    if ((blocks.start[row[middle] ?? 0] ?? 0) <= time + slack) {
      low = middle + 1;
    } else {
      high = middle;
    }
  }
  let best: number | null = null;
  let distance = Number.POSITIVE_INFINITY;
  for (const at of [low - 1, low - 2]) {
    const block = row[at];
    if (block === undefined) {
      continue;
    }
    const start = blocks.start[block] ?? 0;
    const end = blocks.end[block] ?? 0;
    const away = time < start ? start - time : time > end ? time - end : 0;
    if (away <= slack && away < distance) {
      [best, distance] = [block, away];
    }
  }
  return best;
}

/** The names of the zones around `time` above `depth` in a thread, outermost
 * first. */
export function nesting(
  blocks: ZoneBlocks,
  index: BlockIndex,
  thread: number,
  depth: number,
  time: number,
): string[] {
  const names: string[] = [];
  for (let above = 0; above < depth; above += 1) {
    const block = blockAt(blocks, index, thread, above, time, 0);
    if (block !== null && (blocks.zones[block] ?? 1) === 1) {
      names.push(blocks.names[blocks.name[block] ?? 0] ?? "");
    }
  }
  return names;
}

/** Where each lane begins, `strip` pixels of samples and then a row per
 * depth of zones it shows, one under another; and how tall they are. */
export function layLanes(
  shown: readonly number[],
  strip: number,
  row: number,
): { tops: Float64Array; height: number } {
  const tops = new Float64Array(shown.length);
  let top = 0;
  for (const [at, rows] of shown.entries()) {
    assert(rows >= 0, `lane ${at} shows ${rows} rows`);
    tops[at] = top;
    top += strip + rows * row;
  }
  return { tops, height: top };
}

/** The lane `y` pixels down, by bisection of their tops; -1 above them. */
export function laneAt(tops: Float64Array, y: number): number {
  let [low, high] = [0, tops.length];
  while (low < high) {
    const middle = (low + high) >>> 1;
    if ((tops[middle] ?? 0) <= y) {
      low = middle + 1;
    } else {
      high = middle;
    }
  }
  return low - 1;
}

/** Each histogram bucket's lengths, `[low, high]`: to just short of the next
 * bucket's low, the last to the longest. */
export function bucketSpans(histogram: readonly number[][], max: number): [number, number][] {
  return histogram.map(([low = 0], at) => {
    const next = histogram[at + 1]?.[0];
    return [low, next === undefined ? Math.max(max, low) : Math.max(next - 1, low)];
  });
}

/** The band from bucket `from` to bucket `to`, either way round. */
export function bandOf(
  spans: readonly [number, number][],
  from: number,
  to: number,
): [number, number] {
  const first = spans[Math.min(from, to)];
  const last = spans[Math.max(from, to)];
  assert(first && last, `buckets ${from}..${to} are outside ${spans.length}`);
  return [first[0], last[1]];
}

/** A thread's name and id, by its row: a pool's threads share a name. */
export function threadName(threads: readonly ProfileThread[], row: number): string {
  const thread = threads.find((each) => each.row === row);
  if (!thread) {
    return `thread ${row}`;
  }
  return `${thread.name ?? "thread"} ${thread.tid}`;
}

/** What a zone's flags say was not seen. */
export function describeFlags(flags: number): string[] {
  const said: string[] = [];
  if (flags & 1) {
    said.push("its begin was lost");
  }
  if (flags & 2) {
    said.push("its end was not seen");
  }
  if (flags & 4) {
    said.push("it does not nest");
  }
  if (flags & 8) {
    said.push("records were lost inside it");
  }
  return said;
}

/** An SVG path of a plot's bins, `width` by `height`, from `origin` over
 * `span` nanoseconds and from `low` to `high`: each bin a stroke from its
 * first value through its lowest and highest to its last, which holds
 * until the next bin's first. A value `held` from before the range holds
 * from its left edge, and when a value `continues` past the range, the
 * last holds to its right edge, as `held` does across a range it alone
 * covers. */
export function plotPath(
  bins: number[][],
  origin: number,
  span: number,
  width: number,
  height: number,
  low: number,
  high: number,
  held: number | null = null,
  continues = false,
): string {
  const x = (time: number) => (((time - origin) * width) / Math.max(span, 1)).toFixed(1);
  const y = (value: number) =>
    (high === low
      ? height / 2
      : height - 2 - ((value - low) * (height - 4)) / (high - low)
    ).toFixed(1);
  const points: string[] = [];
  let holding = held;
  if (holding !== null) {
    points.push(`${x(origin)} ${y(holding)}`);
  }
  for (const [time = 0, first = 0, last = 0, least = 0, most = 0] of bins) {
    const at = x(time);
    if (holding !== null) {
      points.push(`${at} ${y(holding)}`);
    }
    holding = last;
    for (const value of [first, least, most, last]) {
      points.push(`${at} ${y(value)}`);
    }
  }
  if (holding !== null && (continues || bins.length === 0)) {
    points.push(`${x(origin + span)} ${y(holding)}`);
  }
  return points.length > 0 ? `M${points.join("L")}` : "";
}

/** `view` zoomed by `factor` about the time `at`, which stays where it is
 * unless that would show past either end of a profile `total` long; never
 * shorter than `least` nanoseconds. */
export function zoomed(
  [start, end]: readonly [number, number],
  at: number,
  factor: number,
  total: number,
  least = 1000,
): [number, number] {
  assert(start <= end, `view ${start}..${end} is reversed`);
  assert(total > 0, `profile length ${total} is not positive`);
  const span = Math.min(Math.max((end - start) * factor, least), total);
  const from = at - (at - start) * (span / Math.max(end - start, 1));
  const kept = Math.min(Math.max(from, 0), total - span);
  return [Math.round(kept), Math.round(kept + span)];
}

/** `view` moved later by `by` nanoseconds, stopping at either end of a
 * profile `total` long. */
export function panned(
  [start, end]: readonly [number, number],
  by: number,
  total: number,
): [number, number] {
  assert(start <= end, `view ${start}..${end} is reversed`);
  const moved = Math.min(Math.max(start + by, 0), total - (end - start));
  return [Math.round(moved), Math.round(moved + end - start)];
}

/** Round times across `view`, drawn `width` pixels wide, about a hundred
 * pixels apart: a step of 1, 2, or 5 times a power of ten. */
export function ticks(
  [start, end]: readonly [number, number],
  width: number,
): { step: number; times: number[] } {
  assert(start <= end, `ruler ${start}..${end} is reversed`);
  const raw = ((end - start) * 100) / Math.max(width, 1);
  const power = 10 ** Math.floor(Math.log10(Math.max(raw, 1)));
  const step = power * ([1, 2, 5, 10].find((each) => each * power >= raw) ?? 10);
  const times: number[] = [];
  for (let time = Math.ceil(start / step) * step; time <= end; time += step) {
    times.push(time);
  }
  return { step, times };
}

/** Round times to mark along a chart of `[start, end]` drawn `width`
 * pixels wide, each with its share of the way across. The end is marked
 * itself, so a round time too near it to be read beside it is left out. */
export function timeTicks(
  [start, end]: readonly [number, number],
  width: number,
): { at: number; label: string }[] {
  if (end <= start || width <= 0) {
    return [];
  }
  const span = end - start;
  const { step, times } = ticks([start, end], width);
  const last = start === 0 ? formatDuration(end) : formatInstant(end, step, end);
  // The end, rounded to the step, can read as the round time before it.
  const marks = times
    .filter((time) => (time - start) / span <= 0.9)
    .map((time) => ({
      at: (time - start) / span,
      label: time === 0 ? "0" : formatInstant(time, step, end),
    }))
    .filter((mark) => mark.label !== last);
  return [...marks, { at: 1, label: last }];
}
