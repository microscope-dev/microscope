import { assert } from "../assert";
import type { SyscallLosses } from "../protocol.gen";
import { formatCount, formatDuration } from "./paint";

export function heatAlpha(count: number, most: number): number {
  if (count <= 0 || most <= 0) {
    return 0;
  }
  return 0.15 + (0.85 * Math.log1p(count)) / Math.log1p(most);
}

export function bucketFloor(bucket: number): number {
  assert(Number.isInteger(bucket) && bucket >= 0, `latency bucket ${bucket} is not an index`);
  return bucket === 0 ? 0 : 2 ** (bucket - 1);
}

export function bucketLabel(bucket: number): string {
  return bucket === 0 ? "0" : `≥ ${formatDuration(bucketFloor(bucket))}`;
}

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

interface Setting {
  name: string;
  value: string;
}

interface TraceNote {
  text: string;
  title?: string;
  warn?: boolean;
}

export const PTRACE_DISTORTS =
  "ptrace stops the program at each traced call: it ran slower than it would, and each call's time includes the stop.";

export function sampledKernel(settings: readonly Setting[]): boolean {
  return settings.some((each) => each.name === "kernel stacks" && each.value === "on");
}

export function tracedBy(settings: readonly Setting[]): TraceNote {
  const setting = (name: string) => settings.find((each) => each.name === name)?.value;
  const engine = setting("syscall engine");
  if (!engine) {
    return { text: "" };
  }
  if (engine === "ptrace") {
    return { text: " by ptrace", title: PTRACE_DISTORTS, warn: true };
  }
  const attach = setting("attach");
  const reason = setting("attach reason");
  return {
    text: attach ? ` by ${engine} through ${attach}` : ` by ${engine}`,
    ...(reason ? { title: reason } : {}),
  };
}

export function lossNotes(lost: SyscallLosses): TraceNote[] {
  const notes: TraceNote[] = [];
  if (lost.dropped > 0) {
    notes.push({
      text: `${formatCount(lost.dropped)} dropped`,
      title: "Records the kernel had no room for: some calls lack detail, and counts may be short.",
      warn: true,
    });
  }
  if (lost.unseen > 0) {
    notes.push({
      text: `${formatCount(lost.unseen)} unseen`,
      title: "Calls whose return the trace never saw: they are not counted.",
      warn: true,
    });
  }
  if (lost.capped > 0) {
    notes.push({
      text: `${formatCount(lost.capped)} over the cap`,
      title: "Failed or slow calls past the cap: counted, but not kept.",
    });
  }
  if (lost.overflowed > 0) {
    notes.push({
      text: `${formatCount(lost.overflowed)} not kept`,
      title: "Calls that came while the kernel's ring was full: counted, but not kept.",
    });
  }
  return notes;
}

export function unkept(settings: readonly Setting[]): { text: string; flag: string } | null {
  const setting = (name: string) => settings.find((each) => each.name === name)?.value;
  const level = setting("syscalls") ?? "off";
  if (level === "counts") {
    return {
      text: "The kernel counted every system call and kept only the failed and slow ones.",
      flag: "--syscalls calls",
    };
  }
  if (level !== "off") {
    return null;
  }
  const why = setting("syscalls not recorded");
  return why
    ? { text: `System calls were not recorded: ${why}.`, flag: "--syscall-engine ptrace" }
    : { text: "System calls were not recorded.", flag: "--syscalls calls" };
}

export function traceNotes(settings: readonly Setting[]): TraceNote[] {
  const notes: TraceNote[] = [];
  const cgroup = settings.find((each) => each.name === "cgroup")?.value;
  if (cgroup) {
    notes.push({ text: `cgroup ${cgroup}` });
  }
  const trigger = settings.find((each) => each.name === "trigger")?.value;
  if (trigger) {
    notes.push({ text: `saved when ${trigger}`, warn: true });
  }
  return notes;
}
