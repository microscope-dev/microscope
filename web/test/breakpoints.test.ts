import { describe, expect, it } from "vitest";
import { breakpointAt, firstBreakpointPlace, knownBreakpointPlaces } from "../src/breakpoints";
import type { Breakpoint, Place } from "../src/protocol";
import { breakpointTitle, placeLabel } from "../src/ui/Breakpoints";
import { breakpointLines } from "../src/ui/CodeArea";

const source: Place = { address: "", function: null, path: "/src/app.py", line: 3 };
const reason = "waiting for a runtime acknowledgement";

function waiting(): Breakpoint {
  return {
    id: 1,
    location: "/src/app.py:3",
    condition: null,
    hitCondition: null,
    logMessage: null,
    hits: 0,
    places: [],
    pending: { message: `pending: ${reason}`, places: [{ place: source, reason }] },
  };
}

describe("runtime breakpoint arming", () => {
  it("shows the arming reason and source while no location is ready", () => {
    const breakpoint = waiting();
    expect(breakpointTitle(breakpoint)).toContain(reason);
    expect(breakpointTitle(breakpoint)).not.toContain("no loaded code");
    expect(placeLabel(breakpoint)).toBe("app.py:3");
  });

  it("retains a ready native location alongside a pending runtime location", () => {
    const breakpoint = waiting();
    breakpoint.places = [{ address: "0x1234", function: "native", path: "/src/native.c", line: 4 }];
    breakpoint.pending = {
      message: `some locations pending: ${reason}`,
      places: [{ place: source, reason }],
    };
    expect(breakpointTitle(breakpoint)).toContain("native.c:4");
    expect(breakpointTitle(breakpoint)).toContain(reason);
    expect(placeLabel(breakpoint)).toBe("native.c:4 +1");
  });

  it("accepts an older server without arming metadata and a recovered breakpoint", () => {
    const breakpoint = waiting();
    delete breakpoint.pending;
    expect(breakpointTitle(breakpoint)).toContain("no loaded code has it yet");
    expect(breakpointAt([breakpoint], "/src/app.py", 3)?.id).toBe(1);
    expect(breakpointAt([breakpoint], "/src/app.py", 4)).toBeUndefined();
    breakpoint.places = [source];
    expect(breakpointTitle(breakpoint)).toContain("app.py:3");
    expect(breakpointTitle(breakpoint)).not.toContain("pending");
  });
  it("keeps pending source markers and lets gutter and keyboard actions find the existing intent", () => {
    const breakpoint = waiting();
    breakpoint.location = "app.py:3";
    expect(breakpointLines([breakpoint], "/src/app.py").get(3)).toBe("pending");
    expect(breakpointAt([breakpoint], "/src/app.py", 3)?.id).toBe(1);
    expect(firstBreakpointPlace(breakpoint)).toEqual(source);
    expect(knownBreakpointPlaces(breakpoint)).toEqual([source]);
    breakpoint.places = [{ address: "0x1234", function: "native", path: "/src/native.c", line: 4 }];
    expect(breakpointLines([breakpoint], "/src/app.py").get(3)).toBe("pending");
    expect(breakpointLines([breakpoint], "/src/native.c").get(4)).toBe("plain");
    expect(knownBreakpointPlaces(breakpoint)).toHaveLength(2);
  });

  it("marks only the resolved pending line when resolution moved from the requested line", () => {
    const breakpoint = waiting();
    breakpoint.location = "/src/app.py:2";
    expect(breakpointLines([breakpoint], "/src/app.py").has(2)).toBe(false);
    expect(breakpointLines([breakpoint], "/src/app.py").get(3)).toBe("pending");
    expect(breakpointAt([breakpoint], "/src/app.py", 2)).toBeUndefined();
    expect(breakpointAt([breakpoint], "/src/app.py", 3)?.id).toBe(1);
  });

  it("keeps a ready marker when another breakpoint on the same line is pending", () => {
    const pending = waiting();
    const ready = { ...pending, id: 2, places: [source] };
    delete ready.pending;
    expect(breakpointLines([ready, pending], "/src/app.py").get(3)).toBe("plain");
    expect(breakpointLines([pending, ready], "/src/app.py").get(3)).toBe("plain");
  });
});
