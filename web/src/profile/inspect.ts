// What the Inspect dialog derives from a profile's answers.

import type { ProfileCommand, ProfileSetting } from "../protocol";

/** An argument as a POSIX shell reads it back: bare when it is plainly
 * one word, otherwise in single quotes. */
export function shellQuote(argument: string): string {
  return /^[\w./=:@%+,-]+$/u.test(argument) ? argument : `'${argument.replaceAll("'", `'\\''`)}'`;
}

/** The command as one line a shell runs, or as recorded when the
 * recording kept only the joined line. */
export function commandLine(command: ProfileCommand): string {
  return command.arguments?.map(shellQuote).join(" ") ?? command.text;
}

/** The depth half of the samples reach, given samples by depth. */
export function medianDepth(depths: readonly number[]): number | null {
  const total = depths.reduce((sum, count) => sum + count, 0);
  if (total === 0) {
    return null;
  }
  let seen = 0;
  for (const [depth, count] of depths.entries()) {
    seen += count;
    if (seen * 2 >= total) {
      return depth;
    }
  }
  return depths.length - 1;
}

/** Where microscope's CPU time went, in nanoseconds. Reading debug
 * information after exit is part of reading it at all, and setup may
 * overlap it, so what remains is never below zero. */
export function recorderBreakdown(
  total: number,
  loading: number,
  setup: number,
  finishing: number,
): [string, number][] {
  const after = Math.min(finishing, loading);
  return [
    ["Setup", setup],
    ["While recording", Math.max(total - setup - loading, 0)],
    ["Debug info after exit", after],
    ["Other debug info", loading - after],
  ];
}

/** Settings Inspect shows elsewhere, or that only the recorder reads. */
const SHOWN_ELSEWHERE = new Set([
  "arguments",
  "program",
  "start",
  "end",
  "pid",
  "recorder",
  "weight",
  "final debug enrichment cpu ns",
]);

/** How the profile was recorded, as a person reads it. */
export function recordingSettings(settings: readonly ProfileSetting[]): ProfileSetting[] {
  return settings.filter(
    ({ name }) => !SHOWN_ELSEWHERE.has(name) && !/^(?:command|arguments) \d+$/u.test(name),
  );
}
