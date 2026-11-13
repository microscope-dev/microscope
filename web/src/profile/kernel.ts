// What a thread's kernel lane says under the pointer: the call it waited
// in, or how many calls returned in the slice; and the budget's steps.

import type { ProfileLane, ProfileWait } from "../protocol";
import { formatCount, formatDuration, formatInstant } from "./paint";

/** The call `lane`'s thread waited in at `time`, give or take `slack`,
 * the nearest when several are that close. */
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

/** The slice of `count` across `range` that holds `time`, or -1. */
export function sliceAt(range: readonly [number, number], count: number, time: number): number {
  const [start, end] = range;
  if (count <= 0 || end <= start || time < start || time >= end) {
    return -1;
  }
  return Math.min(Math.floor(((time - start) * count) / (end - start)), count - 1);
}

/** What a kernel lane says at `time`, or null where it has nothing. */
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

/** The budget's step nearest `time`, give or take `slack`. */
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
