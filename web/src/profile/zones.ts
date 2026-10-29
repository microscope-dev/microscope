// Zones as the server sends them, in columns, and how the zones view lays
// each thread's track out: a row per depth, tracks one under another.

import type { ProfileThread, ProfileZones } from "../protocol";

export interface ZoneBlocks {
  count: number;
  /** Nanoseconds from the profile's start. */
  start: Float64Array;
  end: Float64Array;
  thread: Uint32Array;
  depth: Uint32Array;
  /** Indices into `names`. */
  name: Uint32Array;
  /** How many zones each block draws: more than one is a fold. */
  zones: Uint32Array;
  flags: Uint32Array;
  names: string[];
}

/** Decodes an answer and the columns sent before it: starts and ends as
 * little-endian `f64`s, then threads, depths, names, counts, and flags as
 * `u32`s. */
export function decodeZones(answer: ProfileZones, payload: Uint8Array | undefined): ZoneBlocks {
  const count = answer.blocks;
  const bytes = payload ?? new Uint8Array();
  if (bytes.byteLength !== count * 36) {
    throw new Error(`${count} zone blocks need ${count * 36} bytes, not ${bytes.byteLength}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const floats = (column: number) =>
    Float64Array.from({ length: count }, (_, at) =>
      view.getFloat64((column * count + at) * 8, true),
    );
  const words = (column: number) =>
    Uint32Array.from({ length: count }, (_, at) =>
      view.getUint32(count * 16 + (column * count + at) * 4, true),
    );
  return {
    count,
    start: floats(0),
    end: floats(1),
    thread: words(0),
    depth: words(1),
    name: words(2),
    zones: words(3),
    flags: words(4),
    names: answer.names,
  };
}

/** One thread's track: its first row and how many rows it takes. */
export interface Track {
  thread: number;
  row: number;
  depths: number;
}

/** Each thread's track, in `order`, then any other thread with zones,
 * each as deep as its deepest block. */
export function tracks(blocks: ZoneBlocks, order: number[]): Track[] {
  const deepest = new Map<number, number>();
  for (let at = 0; at < blocks.count; at += 1) {
    const thread = blocks.thread[at] ?? 0;
    deepest.set(thread, Math.max(deepest.get(thread) ?? 0, (blocks.depth[at] ?? 0) + 1));
  }
  const threads = [
    ...order.filter((thread) => deepest.has(thread)),
    ...[...deepest.keys()].filter((thread) => !order.includes(thread)).sort((a, b) => a - b),
  ];
  let row = 0;
  return threads.map((thread) => {
    const depths = deepest.get(thread) ?? 1;
    const track = { thread, row, depths };
    row += depths;
    return track;
  });
}

/** The block drawn at `time` in a row, if any. */
export function blockAt(
  blocks: ZoneBlocks,
  track: Track,
  depth: number,
  time: number,
  slack: number,
): number | null {
  let best: number | null = null;
  for (let at = 0; at < blocks.count; at += 1) {
    if (blocks.thread[at] !== track.thread || blocks.depth[at] !== depth) {
      continue;
    }
    const start = blocks.start[at] ?? 0;
    const end = blocks.end[at] ?? 0;
    if (start - slack <= time && time <= end + slack) {
      if (best === null || Math.abs(time - start) < Math.abs(time - (blocks.start[best] ?? 0))) {
        best = at;
      }
    }
  }
  return best;
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
  const moved = Math.min(Math.max(start + by, 0), total - (end - start));
  return [Math.round(moved), Math.round(moved + end - start)];
}

/** Round times across `view`, drawn `width` pixels wide, about a hundred
 * pixels apart: a step of 1, 2, or 5 times a power of ten. */
export function ticks(
  [start, end]: readonly [number, number],
  width: number,
): { step: number; times: number[] } {
  const raw = ((end - start) * 100) / Math.max(width, 1);
  const power = 10 ** Math.floor(Math.log10(Math.max(raw, 1)));
  const step = power * ([1, 2, 5, 10].find((each) => each * power >= raw) ?? 10);
  const times: number[] = [];
  for (let time = Math.ceil(start / step) * step; time <= end; time += step) {
    times.push(time);
  }
  return { step, times };
}
