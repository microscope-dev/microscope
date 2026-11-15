import type { ProfileLane, ProfileWait } from "../protocol";
import { formatCount, formatDuration, formatInstant } from "./paint";

export function waitAt(lane: ProfileLane, time: number, slack: number): ProfileWait | null {
  let found: ProfileWait | null = null;
  let best = Number.POSITIVE_INFINITY;
  for (const wait of lane.waits) {
    const off = time < wait.start ? wait.start - time : time > wait.end ? time - wait.end : 0;
    if (off <= slack && off < best) {
      found = wait;
      best = off;
    }
  }
  return found;
}

export function sliceAt(range: readonly [number, number], count: number, time: number): number {
  const [start, end] = range;
  if (count <= 0 || end <= start || time < start || time >= end) {
    return -1;
  }
  return Math.min(Math.floor(((time - start) * count) / (end - start)), count - 1);
}

export function kernelLines(
  lane: ProfileLane,
  range: readonly [number, number],
  time: number,
  slack: number,
): string[] | null {
  const wait = waitAt(lane, time, slack);
  if (wait) {
    return [
      `waited in ${wait.call} for ${formatDuration(wait.end - wait.start)}`,
      `from ${formatInstant(wait.start, slack)}: click to select it`,
    ];
  }
  const slice = sliceAt(range, lane.syscalls.length, time);
  const calls = lane.syscalls[slice] ?? 0;
  if (calls === 0) {
    return null;
  }
  const failed = lane.failed[slice] ?? 0;
  return [
    `${formatCount(calls, "system call")}${failed > 0 ? `, ${formatCount(failed)} failed` : ""}`,
  ];
}

export function blockedIn(lane: ProfileLane, start: number, end: number): string | null {
  const by = new Map<string, { count: number; ns: number }>();
  for (const wait of lane.waits) {
    const overlap = Math.min(wait.end, end) - Math.max(wait.start, start);
    if (overlap > 0) {
      const seen = by.get(wait.call) ?? { count: 0, ns: 0 };
      by.set(wait.call, { count: seen.count + 1, ns: seen.ns + overlap });
    }
  }
  if (by.size === 0) {
    return null;
  }
  const calls = [...by.entries()]
    .toSorted(([, a], [, b]) => b.ns - a.ns)
    .map(
      ([call, { count, ns }]) => `${call}${count > 1 ? ` ×${count}` : ""} ${formatDuration(ns)}`,
    );
  return `blocked in ${calls.join(", ")}`;
}

export function stepAt(
  steps: readonly { time: number; text: string }[],
  time: number,
  slack: number,
): { time: number; text: string } | null {
  let found: { time: number; text: string } | null = null;
  for (const step of steps) {
    const off = Math.abs(step.time - time);
    if (off <= slack && (!found || off < Math.abs(found.time - time))) {
      found = step;
    }
  }
  return found;
}
