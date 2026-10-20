// What the Syscalls view draws, apart from React: each class's hue, the
// heat map's ramp, histogram buckets' names, and stacked series.

import { formatDuration, hue } from "./paint";

/** Each class the server shows calls under, with a hue of its own. Red is
 * left for errors. */
const CLASS_HUES: Record<string, number> = {
  file: 212,
  net: 160,
  desc: 250,
  memory: 38,
  process: 290,
  signal: 190,
  ipc: 95,
  clock: 60,
};

/** A series' color: its class's hue, or for a call or anything else a hue
 * of its name; `other` is grey. */
export function seriesColor(name: string, dark: boolean): string {
  if (name === "other") {
    return dark ? "hsl(215 10% 45%)" : "hsl(215 10% 70%)";
  }
  const shade = CLASS_HUES[name] ?? hue(name);
  return dark ? `hsl(${shade} 55% 55%)` : `hsl(${shade} 60% 52%)`;
}

/** The heat map's color for a cell of `count` calls when the fullest holds
 * `most`: none when empty, then from faint to the accent, by log scale, so
 * that rare calls still show. */
export function heatAlpha(count: number, most: number): number {
  if (count <= 0 || most <= 0) {
    return 0;
  }
  return 0.15 + (0.85 * Math.log1p(count)) / Math.log1p(most);
}

/** The shortest duration a log2 bucket holds, as the server buckets: 0,
 * then `[2^(i-1), 2^i)` nanoseconds. */
export function bucketFloor(bucket: number): number {
  return bucket === 0 ? 0 : 2 ** (bucket - 1);
}

/** A bucket's name, by the shortest duration it holds. */
export function bucketLabel(bucket: number): string {
  return bucket === 0 ? "0" : `≥ ${formatDuration(bucketFloor(bucket))}`;
}

/** Each series' running total in each slice, bottom to top, for drawing
 * them stacked: `tops[i][j]` is where series `i` ends in slice `j`. */
export function stacked(series: readonly (readonly number[])[]): number[][] {
  const below: number[] = [];
  return series.map((counts) =>
    counts.map((count, slice) => {
      below[slice] = (below[slice] ?? 0) + count;
      return below[slice];
    }),
  );
}

/** The histogram's buckets worth showing: from the first that holds a
 * call to the last, so a chart is not mostly empty. */
export function shownBuckets(...histograms: readonly (readonly number[])[]): [number, number] {
  let first = Number.POSITIVE_INFINITY;
  let last = -1;
  for (const histogram of histograms) {
    for (const [bucket, count] of histogram.entries()) {
      if (count > 0) {
        first = Math.min(first, bucket);
        last = Math.max(last, bucket);
      }
    }
  }
  return last < 0 ? [0, -1] : [first, last];
}

/** A capture's setting, as the server lists them. */
interface Setting {
  name: string;
  value: string;
}

/** A part of a trace's footer, with more on hover. */
interface TraceNote {
  text: string;
  title?: string;
  warn?: boolean;
}

/** How the calls were traced: the engine and, for BPF, how it attached. */
export function tracedBy(settings: readonly Setting[]): TraceNote {
  const setting = (name: string) => settings.find((each) => each.name === name)?.value;
  const engine = setting("syscalls");
  if (!engine) {
    return { text: "" };
  }
  const attach = setting("attach");
  const reason = setting("attach reason");
  return {
    text: attach ? ` by ${engine} through ${attach}` : ` by ${engine}`,
    ...(reason ? { title: reason } : {}),
  };
}

/** What else shaped the trace: its cgroup, each step the budget took, and
 * what saved a window. */
export function traceNotes(settings: readonly Setting[]): TraceNote[] {
  const notes: TraceNote[] = [];
  const cgroup = settings.find((each) => each.name === "cgroup")?.value;
  if (cgroup) {
    notes.push({ text: `cgroup ${cgroup}` });
  }
  const steps = settings
    .flatMap((each) => {
      const step = /^budget (\d+)$/.exec(each.name);
      return step ? [{ step: Number(step[1]), note: each.value }] : [];
    })
    .sort((a, b) => a.step - b.step)
    .map((each) => each.note);
  if (steps.length > 0) {
    notes.push({ text: `budget: ${steps[steps.length - 1]}`, title: steps.join("\n"), warn: true });
  }
  const trigger = settings.find((each) => each.name === "trigger")?.value;
  if (trigger) {
    notes.push({ text: `saved when ${trigger}`, warn: true });
  }
  return notes;
}
