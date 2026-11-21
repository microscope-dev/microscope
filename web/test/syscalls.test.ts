import { describe, expect, it } from "vitest";

import { parseSearch, stringifySearch } from "../src/focus";
import { type ProfileLook, syscallQueryOf, validateProfileLook } from "../src/profile/look";
import {
  bucketFloor,
  bucketLabel,
  heatAlpha,
  lossNotes,
  shownBuckets,
  tracedBy,
  PTRACE_DISTORTS,
  traceNotes,
  unkept,
} from "../src/profile/syscalls";

const roundTrip = (look: ProfileLook) =>
  validateProfileLook(parseSearch(stringifySearch(look as Record<string, unknown>)));

describe("the Syscalls view's address", () => {
  it("holds the selection, grouping, call, tab, and expanded line", () => {
    const look: ProfileLook = {
      view: "kernel",
      range: "10-90",
      sf: "trace=%file,!close status=failed path=/a b&c",
      sg: "process",
      sc: "openat",
      st: "log",
      ev: "42",
      du: "all",
      sk: "1",
    };
    expect(roundTrip(look)).toEqual(look);

    expect(syscallQueryOf("p", look, 3)).toEqual({
      profile: "p",
      revision: 3,
      range: [10, 90],
      source: "samples",
      look: "logical",
      granularity: "function",
      byTask: false,
      invert: false,
      labels: [],
      transforms: [],
      syscallLeaves: false,
      filter: "trace=%file,!close status=failed path=/a b&c",
    });
  });

  it("drops what it cannot read", () => {
    expect(validateProfileLook({ sg: "syscall", st: "flame", ev: "-1", sf: "" })).toEqual({});
  });
});

describe("what the Syscalls view draws", () => {
  it("names buckets by the durations the server puts in them", () => {
    expect([0, 1, 2, 11].map((bucket) => bucketFloor(bucket))).toEqual([0, 1, 2, 1024]);
    expect(bucketLabel(0)).toBe("0");
    expect(bucketLabel(11)).toMatch(/^≥ 1\.0\d* µs$/u);
  });

  it("shows only the buckets that hold calls, across histograms", () => {
    expect(shownBuckets([0, 0, 1, 0], [0, 0, 0, 0, 5])).toEqual([2, 4]);
    expect(shownBuckets([0, 0])).toEqual([0, -1]);
  });

  it("shades a cell by log scale, faint but visible for one call", () => {
    expect(heatAlpha(0, 100)).toBe(0);
    expect(heatAlpha(1, 100)).toBeGreaterThan(0.15);
    expect(heatAlpha(100, 100)).toBeCloseTo(1);
    expect(heatAlpha(10, 100)).toBeLessThan(heatAlpha(50, 100));
  });
});

const settings = (pairs: [string, string][]) => pairs.map(([name, value]) => ({ name, value }));

describe("what a trace's footer says of how it was made", () => {
  it("names the engine and how it attached, with the reason on hover", () => {
    expect(tracedBy(settings([["syscall engine", "ptrace"]]))).toEqual({
      text: " by ptrace",
      title: PTRACE_DISTORTS,
      warn: true,
    });
    expect(
      tracedBy(
        settings([
          ["syscalls", "calls"],
          ["syscall engine", "bpf"],
          ["attach", "tp_btf"],
          ["attach reason", "the list names 40 calls"],
        ]),
      ),
    ).toEqual({ text: " by bpf through tp_btf", title: "the list names 40 calls" });
    expect(tracedBy([])).toEqual({ text: "" });
  });

  it("says the cgroup and a window's trigger", () => {
    expect(
      traceNotes(
        settings([
          ["program", "make"],
          ["cgroup", "/sys/fs/cgroup/app.slice"],
          ["trigger", "close failed with errno 9"],
        ]),
      ),
    ).toEqual([
      { text: "cgroup /sys/fs/cgroup/app.slice" },
      { text: "saved when close failed with errno 9", warn: true },
    ]);
    expect(traceNotes(settings([["syscalls", "bpf"]]))).toEqual([]);
  });
});

describe("what a recording did not keep of its calls", () => {
  it("names the flag that would have kept each call, by level and reason", () => {
    expect(unkept(settings([["syscalls", "calls"]]))).toBeNull();
    expect(unkept(settings([["syscalls", "full"]]))).toBeNull();
    expect(unkept(settings([["syscalls", "counts"]]))?.flag).toBe("--syscalls calls");
    expect(unkept(settings([["syscalls", "off"]]))).toEqual({
      text: "System calls were not recorded.",
      flag: "--syscalls calls",
    });

    expect(
      unkept(
        settings([
          ["syscalls", "off"],
          ["syscalls not recorded", "CAP_BPF missing"],
        ]),
      ),
    ).toEqual({
      text: "System calls were not recorded: CAP_BPF missing.",
      flag: "--syscall-engine ptrace",
    });
  });
});

describe("a trace's losses", () => {
  it("warn only of those that leave the counts short", () => {
    expect(lossNotes({ dropped: 0, unseen: 0, capped: 0, overflowed: 0, skipped: 0 })).toEqual([]);
    const notes = lossNotes({ dropped: 1200, unseen: 1, capped: 40, overflowed: 7, skipped: 3 });
    expect(notes.map((note) => [note.text, note.warn ?? false])).toEqual([
      ["1,200 dropped", true],
      ["1 unseen", true],
      ["40 over the cap", false],
      ["7 not kept", false],
      ["3 skipped on the host", true],
    ]);
  });
});
