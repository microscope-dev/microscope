import type { ProfileKey } from "../protocol";

export function formatWeight(weight: number, unit: string): string {
  if (unit === "nanoseconds") {
    return formatDuration(weight);
  }
  const count = Math.round(weight).toLocaleString("en-US");
  return `${count} ${weight === 1 ? "execution" : "executions"}`;
}

/**
 * Maps a baseline comparison to a red (growth) / blue (reduction) fill.
 * Normalizing by `max(share, baseShare)` keeps new and removed frames symmetric
 * in `[-1, 1]`; changes under 2% stay neutral gray.
 */
export function diffFill(share: number, baseShare: number, dark: boolean): string {
  const larger = Math.max(share, baseShare);
  const change = larger > 0 ? (share - baseShare) / larger : 0;
  const strength = Math.min(Math.abs(change), 1);
  if (strength < 0.02) {
    return dark ? "hsl(0, 0%, 30%)" : "hsl(0, 0%, 84%)";
  }
  const angle = change > 0 ? 0 : 215;
  const lightness = dark ? 30 - 10 * strength : 86 - 36 * strength;
  return `hsl(${angle}, ${Math.round(40 + 40 * strength)}%, ${lightness.toFixed(1)}%)`;
}

export function shareChange(
  part: number,
  whole: number,
  basePart: number,
  baseWhole: number,
): number | null {
  return whole > 0 && baseWhole > 0 ? (part / whole - basePart / baseWhole) * 100 : null;
}

export function formatChange(points: number | null): string {
  if (points === null) {
    return "–";
  }
  const shown = Math.abs(points) >= 10 ? points.toFixed(1) : points.toFixed(2);
  return Number(shown) === 0 ? "±0" : `${points > 0 ? "+" : "−"}${shown.replace("-", "")} pp`;
}

export type DiffMode = "time" | "share";

export interface Changed {
  text: string;
  way: "up" | "down" | "";
}

export function weightChange(weight: number, baseWeight: number, unit: string): string {
  if (weight === baseWeight) {
    return "±0";
  }
  const change = weight - baseWeight;
  const sign = change > 0 ? "+" : "−";
  const amount = `${sign}${formatWeight(Math.abs(change), unit)}`;
  if (baseWeight === 0) {
    return `${amount} new`;
  }
  if (weight === 0) {
    return `${amount} gone`;
  }
  const percent = (Math.abs(change) * 100) / baseWeight;
  return `${amount} ${sign}${percent.toFixed(1)}%`;
}

export function changeOf(
  mode: DiffMode,
  [part, whole]: readonly [number, number],
  [basePart, baseWhole]: readonly [number, number],
  unit: string,
): Changed {
  if (mode === "time") {
    return {
      text: weightChange(part, basePart, unit),
      way: part > basePart ? "up" : part < basePart ? "down" : "",
    };
  }
  const points = shareChange(part, whole, basePart, baseWhole);
  return {
    text: formatChange(points),
    way: points === null || Math.abs(points) < 0.005 ? "" : points > 0 ? "up" : "down",
  };
}

export function changeFill(
  mode: DiffMode,
  [part, whole]: readonly [number, number],
  [basePart, baseWhole]: readonly [number, number],
  dark: boolean,
): string {
  return mode === "time"
    ? diffFill(part, basePart, dark)
    : diffFill(part / Math.max(whole, 1), basePart / Math.max(baseWhole, 1), dark);
}

function scaled(
  value: number,
  units: readonly (readonly [number, string])[],
  base: string,
): string {
  for (const [size, name] of units) {
    const shown = Number((value / size).toPrecision(3));
    if (Math.abs(shown) >= 1) {
      return Math.abs(shown) >= 1000
        ? `${Math.round(value / size).toLocaleString("en-US")} ${name}`
        : `${(value / size).toPrecision(3)} ${name}`;
    }
  }
  return `${Math.round(value)} ${base}`;
}

export function formatBytes(bytes: number): string {
  return scaled(
    bytes,
    [
      [2 ** 30, "GiB"],
      [2 ** 20, "MiB"],
      [2 ** 10, "KiB"],
    ],
    "B",
  );
}

export function formatDuration(nanoseconds: number): string {
  return scaled(
    nanoseconds,
    [
      [1e9, "s"],
      [1e6, "ms"],
      [1e3, "µs"],
    ],
    "ns",
  );
}

export function formatInstant(
  nanoseconds: number,
  resolution: number,
  scale = nanoseconds,
): string {
  if (nanoseconds === 0) {
    return "0 s";
  }
  const [size, name] = (
    [
      [1e9, "s"],
      [1e6, "ms"],
      [1e3, "µs"],
    ] as const
  ).find(([limit]) => Math.abs(scale) >= limit) ?? [1, "ns"];
  const decimals = Math.min(
    Math.max(Math.ceil(Math.log10(size / Math.max(resolution, 1)) - 1e-9), 0),
    9,
  );
  return `${(nanoseconds / size).toFixed(decimals)} ${name}`;
}

export function formatSpan([start, end]: readonly [number, number]): string {
  const resolution = (end - start) / 100;
  return `${formatInstant(start, resolution)} – ${formatInstant(end, resolution)} (${formatDuration(end - start)})`;
}

export function formatPercent(part: number, whole: number): string {
  if (whole <= 0) {
    return "0%";
  }
  const percent = (part * 100) / whole;
  return `${percent >= 10 ? percent.toFixed(1) : percent.toFixed(2)}%`;
}

export function formatCount(count: number, singular?: string, plural = `${singular}s`): string {
  const digits = count.toLocaleString("en-US");
  return singular === undefined ? digits : `${digits} ${count === 1 ? singular : plural}`;
}

export function largest(values: Iterable<number>, floor: number): number {
  let most = floor;
  for (const value of values) {
    if (value > most) {
      most = value;
    }
  }
  return most;
}

export function family(key: ProfileKey): string {
  const name = key.name.replace(/^\[task\] /u, "").replace(/^</u, "");
  const scope = /^([A-Za-z_][\w]*)(::|\.)/u.exec(name);
  return scope?.[1] ?? key.module ?? name;
}

export function hue(text: string): number {
  let hash = 0x811c9dc5;
  for (let at = 0; at < text.length; at += 1) {
    // oxlint-disable-next-line unicorn/prefer-code-point -- the hash is over UTF-16 code units; code points would change every hue
    hash ^= text.charCodeAt(at);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0) % 360;
}

export function fill(key: ProfileKey, dark: boolean): string {
  if (key.kind === "truncation") {
    return dark ? "hsl(40 70% 30%)" : "hsl(42 90% 75%)";
  }
  if (key.kind === "syscall") {
    return dark ? "hsl(210 8% 32%)" : "hsl(210 10% 82%)";
  }
  const h = key.kind === "task" ? 225 : hue(family(key));
  const saturation = key.kind === "address" ? 10 : 50;
  return dark ? `hsl(${h} ${saturation - 15}% 30%)` : `hsl(${h} ${saturation}% 78%)`;
}
