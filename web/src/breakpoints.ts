// Resolved intent includes ready placements and runtime locations awaiting arming.
import type { Breakpoint, Place } from "./protocol";

export function knownBreakpointPlaces(breakpoint: Breakpoint): readonly Place[] {
  const pending = breakpoint.pending?.places;
  return pending?.length
    ? [...breakpoint.places, ...pending.map((pending) => pending.place)]
    : breakpoint.places;
}

export function firstBreakpointPlace(breakpoint: Breakpoint): Place | undefined {
  return breakpoint.places[0] ?? breakpoint.pending?.places[0]?.place;
}

export function breakpointAt(
  breakpoints: readonly Breakpoint[],
  path: string,
  line: number,
): Breakpoint | undefined {
  return breakpoints.find((breakpoint) => {
    const places = knownBreakpointPlaces(breakpoint);
    return (
      places.some((place) => place.path === path && place.line === line) ||
      (places.length === 0 && breakpoint.location === `${path}:${line}`)
    );
  });
}
