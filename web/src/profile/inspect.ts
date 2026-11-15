import type { ProfileCommand, ProfileSetting } from "../protocol";

export function shellQuote(argument: string): string {
  return /^[\w./=:@%+,-]+$/u.test(argument) ? argument : `'${argument.replaceAll("'", `'\\''`)}'`;
}

export function commandLine(command: ProfileCommand): string {
  return command.arguments?.map(shellQuote).join(" ") ?? command.text;
}

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

export function recordingSettings(settings: readonly ProfileSetting[]): ProfileSetting[] {
  return settings.filter(
    ({ name }) => !SHOWN_ELSEWHERE.has(name) && !/^(?:command|arguments) \d+$/u.test(name),
  );
}
