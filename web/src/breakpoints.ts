// Where a breakpoint is: the places it is set at, and those it will be set
// at once a module or runtime is ready.
import type { Connection } from "./connection";
import type { Breakpoint, Place } from "./protocol";
import { flash } from "./tab";

/** Removes `existing`, or adds a breakpoint at `location` when there is none,
 * saying why when the server refuses. */
export async function toggleBreakpoint(
  connection: Connection,
  existing: Breakpoint | undefined,
  location: string,
): Promise<void> {
  try {
    if (existing) {
      await connection.request("removeBreakpoint", { id: existing.id });
    } else {
      await connection.request("addBreakpoint", { location });
    }
  } catch (failure) {
    flash(failure instanceof Error ? failure.message : String(failure));
  }
}

/** What a breakpoint no loaded code has waits for, as the CLI says it. */
export const PENDING = "pending until a module with its code loads";

export function knownBreakpointPlaces(breakpoint: Breakpoint): readonly Place[] {
  const pending = breakpoint.pending?.places;
  return pending?.length
    ? [...breakpoint.places, ...pending.map((pending) => pending.place)]
    : breakpoint.places;
}

export function firstBreakpointPlace(breakpoint: Breakpoint): Place | undefined {
  return breakpoint.places[0] ?? breakpoint.pending?.places[0]?.place;
}

/**
 * Whether a breakpoint's place, written as `named`, is the file at `path`:
 * the same path, or a relative one that names the file's whole trailing
 * components, `.` ones dropped, as the debugger and runtimes match it.
 */
export function namesFile(named: string, path: string): boolean {
  if (named === path) {
    return true;
  }
  if (named.startsWith("/")) {
    return false;
  }
  const parts = named.split("/").filter((part) => part !== "" && part !== ".");
  const whole = path.split("/");
  const start = whole.length - parts.length;
  return (
    parts.length > 0 && start >= 0 && parts.every((part, index) => whole[start + index] === part)
  );
}

/** The line a breakpoint was asked for in the file at `path`, if any. */
export function askedAt(breakpoint: Breakpoint, path: string): number | undefined {
  const asked = /^(.*):(\d+)$/.exec(breakpoint.location);
  return asked?.[1] && namesFile(asked[1], path) ? Number(asked[2]) : undefined;
}

export function breakpointAt(
  breakpoints: readonly Breakpoint[],
  path: string,
  line: number,
): Breakpoint | undefined {
  return breakpoints.find((breakpoint) => {
    const places = knownBreakpointPlaces(breakpoint);
    return (
      places.some((place) => place.path && namesFile(place.path, path) && place.line === line) ||
      (places.length === 0 && askedAt(breakpoint, path) === line)
    );
  });
}
