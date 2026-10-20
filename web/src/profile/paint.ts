// How profile views name numbers and color frames. Colors follow a frame's
// family, its module or its outermost namespace, so they stay the same
// across views, profiles, and queries.

import type { ProfileKey } from "../protocol";

/** A weight in its unit: CPU time, bytes, or a count of what the
 * profile counts, such as executions or an imported profile's goroutines. */
export function formatWeight(weight: number, unit: string): string {
  if (unit === "nanoseconds") {
    return formatDuration(weight);
  }
  if (unit === "bytes") {
    return formatBytes(weight);
  }
  const count = Math.round(weight).toLocaleString("en-US");
  const one = unit.endsWith("s") ? unit.slice(0, -1) : unit;
  return `${count} ${weight === 1 ? one : unit.endsWith("s") ? unit : `${unit}s`}`;
}

/** A diff's fill for a frame's share of its samples now and in the base:
 * red where it grew, blue where it shrank, as strong as the change is to
 * the larger share, and grey where it held. */
export function diffFill(share: number, baseShare: number, dark: boolean): string {
  const larger = Math.max(share, baseShare);
  const change = larger > 0 ? (share - baseShare) / larger : 0;
  const strength = Math.min(Math.abs(change), 1);
  if (strength < 0.02) {
    return dark ? "hsl(0, 0%, 30%)" : "hsl(0, 0%, 84%)";
  }
  const hue = change > 0 ? 0 : 215;
  const lightness = dark ? 30 - 10 * strength : 86 - 36 * strength;
  return `hsl(${hue}, ${Math.round(40 + 40 * strength)}%, ${lightness.toFixed(1)}%)`;
}

/** How a share of the samples changed from the base's, in percentage
 * points, or `null` when either side weighs nothing. */
export function shareChange(
  part: number,
  whole: number,
  basePart: number,
  baseWhole: number,
): number | null {
  return whole > 0 && baseWhole > 0 ? (part / whole - basePart / baseWhole) * 100 : null;
}

/** A change in percentage points, signed. */
export function formatChange(points: number | null): string {
  if (points === null) {
    return "–";
  }
  const shown = Math.abs(points) >= 10 ? points.toFixed(1) : points.toFixed(2);
  return Number(shown) === 0 ? "±0" : `${points > 0 ? "+" : "−"}${shown.replace("-", "")} pp`;
}

/** `value` to three significant digits in the largest of `units`, largest
 * first, that reads one or more once rounded, or whole in `base`. */
function scaled(
  value: number,
  units: readonly (readonly [number, string])[],
  base: string,
): string {
  for (const [size, name] of units) {
    const shown = Number((value / size).toPrecision(3));
    if (Math.abs(shown) >= 1) {
      // `toPrecision` writes 1200 as 1.20e+3.
      return Math.abs(shown) >= 1000
        ? `${Math.round(value / size).toLocaleString("en-US")} ${name}`
        : `${(value / size).toPrecision(3)} ${name}`;
    }
  }
  return `${Math.round(value)} ${base}`;
}

/** Bytes, in the largest binary unit that keeps them above one. */
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

/** Nanoseconds, in the largest unit that keeps them above one. */
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

/** A time since the profile began, in the largest unit that keeps it, or
 * `scale`, above one, with as many decimals as tell apart times
 * `resolution` nanoseconds apart. */
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
  ).find(([size]) => Math.abs(scale) >= size) ?? [1, "ns"];
  const decimals = Math.min(
    Math.max(Math.ceil(Math.log10(size / Math.max(resolution, 1)) - 1e-9), 0),
    9,
  );
  return `${(nanoseconds / size).toFixed(decimals)} ${name}`;
}

/** A time range, each end as fine as a hundredth of its length, and that
 * length. */
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

/** An exact count, its digits grouped, and with a noun, the noun for one
 * or for many. */
export function formatCount(count: number, singular?: string, plural = `${singular}s`): string {
  const digits = count.toLocaleString("en-US");
  return singular === undefined ? digits : `${digits} ${count === 1 ? singular : plural}`;
}

/** The largest of `values`, or `floor` if none is larger. Spreading them
 * into `Math.max` overflows the stack for a lane per thread of buckets. */
export function largest(values: Iterable<number>, floor: number): number {
  let most = floor;
  for (const value of values) {
    if (value > most) {
      most = value;
    }
  }
  return most;
}

/** What a frame's color follows: its module, or failing that the first
 * part of its name, such as a Rust crate or a Go package. */
export function family(key: ProfileKey): string {
  const name = key.name.replace(/^\[task\] /, "").replace(/^</, "");
  const scope = /^([A-Za-z_][\w]*)(::|\.)/.exec(name);
  return scope?.[1] ?? key.module ?? name;
}

/** A stable hue for a family. */
export function hue(text: string): number {
  // FNV-1a, which spreads similar names apart.
  let hash = 0x811c9dc5;
  for (let at = 0; at < text.length; at += 1) {
    hash ^= text.charCodeAt(at);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0) % 360;
}

/** A frame's fill: warm warnings for truncations, the family's hue for
 * code. Dark themes darken every fill. */
export function fill(key: ProfileKey, dark: boolean): string {
  if (key.kind === "truncation") {
    return dark ? "hsl(40 70% 30%)" : "hsl(42 90% 75%)";
  }
  const h = key.kind === "task" ? 225 : hue(family(key));
  const saturation = key.kind === "address" ? 10 : 50;
  return dark ? `hsl(${h} ${saturation - 15}% 30%)` : `hsl(${h} ${saturation}% 78%)`;
}
