// A profile's view: its address round-trips, and its call tree's columns
// decode into geometry that nests, as real servers' answers do.

import { describe, expect, it } from "vitest";
import { parseSearch, stringifySearch } from "../src/focus";
import {
  type ProfileLook,
  parseRange,
  parseTransform,
  queryOf,
  rangeOf,
  transformed,
  untransformed,
  validateProfileLook,
} from "../src/profile/look";
import {
  diffFill,
  formatBytes,
  formatChange,
  formatCount,
  formatDuration,
  formatInstant,
  formatPercent,
  formatSpan,
  formatWeight,
  largest,
  shareChange,
} from "../src/profile/paint";
import { type CallTree, decodeTree, findPath, hit, layout, pathOf } from "../src/profile/tree";
import {
  blockAt,
  decodeZones,
  describeFlags,
  panned,
  plotPath,
  threadName,
  ticks,
  tracks,
  zoomed,
} from "../src/profile/zones";
import type { ProfileKey, ProfileTree, ServerMessage } from "../src/protocol";
import { binaries, transcript } from "./transcripts";

const roundTrip = (look: ProfileLook) =>
  validateProfileLook(parseSearch(stringifySearch(look as Record<string, unknown>)));

describe("a profile's address", () => {
  it("holds the whole view, names with any characters included", () => {
    const look: ProfileLook = {
      view: "functions",
      range: "1000-250000",
      th: ["0", "2"],
      src: "waits",
      tk: ["3", "none"],
      bt: "1",
      lb: ["route=/users?id=7", "round=2"],
      bl: "route",
      zs: "db::query",
      lanes: "tasks",
      tf: ["focus:<Pin<Box<F>> as Future>::poll", "drop:std::sys::backtrace::__rust_begin"],
      invert: "1",
      native: "1",
      py: "1",
      by: "line",
      fn: "app::db::decode",
      z: ["main", "a & b", "c=d?"],
      bp: "f00d",
      br: "5-90",
      ann: "code",
    };
    expect(roundTrip(look)).toEqual(look);
    expect(queryOf("p", look)).toEqual({
      profile: "p",
      range: [1000, 250000],
      threads: [0, 2],
      source: "waits",
      tasks: [3, null],
      byTask: true,
      labels: ["route=/users?id=7", "round=2"],
      byLabel: "route",
      transforms: [
        { op: "focus", name: "<Pin<Box<F>> as Future>::poll" },
        { op: "drop", name: "std::sys::backtrace::__rust_begin" },
      ],
      invert: true,
      look: "native",
      granularity: "line",
      base: { profile: "f00d", range: [5, 90] },
    });
    // A base range alone is of the profile itself.
    expect(queryOf("p", { br: "5-90" }).base).toEqual({ profile: "p", range: [5, 90] });
  });

  it("drops what it cannot mean", () => {
    expect(
      validateProfileLook({
        view: "flame",
        range: "9-3",
        th: ["x"],
        src: "cpu",
        tk: ["x", "-1"],
        bt: "yes",
        lb: ["=x", "nokey"],
        lanes: "threads",
        tf: ["spin:main", ":main", "merge:"],
        invert: "yes",
        py: "yes",
        by: "function",
        bp: "",
        br: "9-3",
        ann: "lines-of-code",
      }),
    ).toEqual({});
    expect(queryOf("p", {})).toEqual({
      profile: "p",
      source: "samples",
      byTask: false,
      labels: [],
      transforms: [],
      invert: false,
      look: "logical",
      granularity: "function",
    });
  });

  it("looks at stacks as Python sees them unless it looks at native ones", () => {
    expect(queryOf("p", { py: "1" }).look).toBe("python");
    expect(queryOf("p", { py: "1", native: "1" }).look).toBe("native");
  });

  it("keeps transforms in order, and forgets the zoom they move", () => {
    const focused = transformed({ z: ["main"], fn: "leaf" }, { op: "focus", name: "outer" });
    expect(focused).toEqual({ fn: "leaf", tf: ["focus:outer"] });
    const both = transformed(focused, { op: "merge", name: "middle" });
    expect(untransformed(both, 1)).toEqual(focused);
    expect(untransformed(both, 0)).toEqual({ fn: "leaf" });
  });
});

/** The columns of a tree of `nodes`, each [name, own, total, depth]. */
function encode(nodes: [string, number, number, number][]): CallTree {
  const names = [...new Set(nodes.map(([name]) => name))];
  const keys: ProfileKey[] = names.map((name) => ({
    name,
    kind: "function",
    module: null,
    file: null,
    line: null,
  }));
  const count = nodes.length;
  const bytes = new Uint8Array(count * 24);
  const view = new DataView(bytes.buffer);
  for (const [node, [name, own, total, depth]] of nodes.entries()) {
    view.setFloat64(node * 8, own, true);
    view.setFloat64((count + node) * 8, total, true);
    view.setUint32(count * 16 + node * 4, names.indexOf(name), true);
    view.setUint32(count * 20 + node * 4, depth, true);
  }
  const weight = nodes.filter(([, , , depth]) => depth === 0).reduce((sum, [, , t]) => sum + t, 0);
  const answer: ProfileTree = { nodes: count, weight, samples: weight, keys, base: null };
  return decodeTree(answer, bytes);
}

const tree = encode([
  ["main", 0, 10, 0],
  ["work", 2, 6, 1],
  ["leaf", 4, 4, 2],
  ["idle", 4, 4, 1],
  ["thread", 0, 2, 0],
  ["leaf", 2, 2, 1],
]);

describe("a call tree's geometry", () => {
  it("finds each node's caller and subtree", () => {
    expect([...tree.parent]).toEqual([-1, 0, 1, 0, -1, 4]);
    expect([...tree.end]).toEqual([4, 3, 3, 4, 6, 6]);
    expect(pathOf(tree, 2)).toEqual(["main", "work", "leaf"]);
    expect(findPath(tree, ["thread", "leaf"])).toBe(5);
    expect(findPath(tree, ["main", "leaf"])).toBeNull();
  });

  it("nests every node inside its caller, siblings side by side", () => {
    const rects = layout(tree, null, 0);
    // In twelfths of the width, the weight of everything.
    const twelfths = (fraction: number) => Math.round(fraction * 12 * 1e9) / 1e9;
    expect(rects.map(({ node, x, width }) => [node, twelfths(x), twelfths(width)])).toEqual([
      [0, 0, 10],
      [1, 0, 6],
      [2, 0, 4],
      [3, 6, 4],
      [4, 10, 2],
      [5, 10, 2],
    ]);
    expect(hit(rects, 1, 0.7)?.node).toBe(3);
    expect(hit(rects, 2, 0.7)).toBeNull();
  });

  it("zooms a node to the whole width, under its ancestors", () => {
    const rects = layout(tree, 1, 0);
    expect(rects.map(({ node, x, width }) => [node, x, width])).toEqual([
      [0, 0, 1],
      [1, 0, 1],
      [2, 0, 4 / 6],
    ]);
  });

  it("leaves out what is too narrow to see, and what it calls", () => {
    expect(layout(tree, null, 0.2).map(({ node }) => node)).toEqual([0, 1, 2, 3]);
  });

  it("refuses columns that do not fit the tree", () => {
    const answer: ProfileTree = { nodes: 2, weight: 1, samples: 1, keys: [], base: null };
    expect(() => decodeTree(answer, new Uint8Array(24))).toThrow(/48 bytes/);
  });

  it("decodes a diff's base weights after its own columns", () => {
    const bytes = new Uint8Array(40);
    const view = new DataView(bytes.buffer);
    view.setFloat64(0, 3, true);
    view.setFloat64(8, 3, true);
    view.setFloat64(24, 1, true);
    view.setFloat64(32, 5, true);
    const keys: ProfileKey[] = [
      { name: "a", kind: "function", module: null, file: null, line: null },
    ];
    const answer: ProfileTree = {
      nodes: 1,
      weight: 3,
      samples: 3,
      keys,
      base: { weight: 5, samples: 5 },
    };
    const diff = decodeTree(answer, bytes);
    expect([diff.own[0], diff.baseOwn?.[0], diff.baseTotal?.[0], diff.baseWeight]).toEqual([
      3, 1, 5, 5,
    ]);
    expect(() => decodeTree(answer, bytes.slice(0, 24))).toThrow(/40 bytes/);
  });
});

describe("a diff's colors", () => {
  it("are red where a share grew, blue where it shrank, stronger the more", () => {
    expect(diffFill(0.5, 0.5, false)).toBe(diffFill(0.2, 0.2, false));
    const hueOf = (fill: string) => Number(/^hsl\((\d+)/.exec(fill)?.[1]);
    const lightness = (fill: string) => Number(/(\d+(?:\.\d+)?)%\)$/.exec(fill)?.[1]);
    expect(hueOf(diffFill(0.5, 0.25, false))).toBe(0);
    expect(hueOf(diffFill(0.25, 0.5, false))).toBe(215);
    expect(lightness(diffFill(0.5, 0, false))).toBeLessThan(lightness(diffFill(0.5, 0.4, false)));
  });
});

describe("a real server's call trees", () => {
  const lines = transcript("profile");
  const frames = binaries("profile");
  const trees = lines.flatMap((line) => {
    if (line.to !== "page" || !("message" in line)) {
      return [];
    }
    const message: ServerMessage = line.message;
    if (message.type !== "result" || !frames.has(message.id)) {
      return [];
    }
    return [decodeTree(message.result as ProfileTree, frames.get(message.id))];
  });

  it("decodes, and each node's total is its own and its callees'", () => {
    expect(trees.length).toBeGreaterThanOrEqual(3);
    for (const tree of trees) {
      let roots = 0;
      for (let node = 0; node < tree.count; node += 1) {
        let callees = 0;
        for (let child = node + 1; child < (tree.end[node] ?? 0); child = tree.end[child] ?? 0) {
          callees += tree.total[child] ?? 0;
        }
        expect(tree.total[node]).toBe((tree.own[node] ?? 0) + callees);
        roots += tree.parent[node] === -1 ? (tree.total[node] ?? 0) : 0;
      }
      expect(roots).toBe(tree.weight);
      for (const rect of layout(tree, null, 0)) {
        const parent = tree.parent[rect.node] ?? -1;
        expect(rect.x + rect.width).toBeLessThanOrEqual(1 + 1e-9);
        if (parent >= 0) {
          const outer = layout(tree, null, 0).find((each) => each.node === parent);
          expect(rect.x).toBeGreaterThanOrEqual(outer?.x ?? 0);
        }
      }
    }
  });
});

describe("a zones answer", () => {
  it("decodes its columns into a track per thread, a row per depth", () => {
    // [start, end, thread, depth, name, count, flags]
    const blocks: number[][] = [
      [0, 100, 4, 0, 0, 1, 0],
      [10, 40, 4, 1, 1, 1, 2],
      [50, 52, 4, 1, 2, 7, 0],
      [5, 30, 9, 0, 0, 1, 0],
    ];
    const count = blocks.length;
    const bytes = new Uint8Array(count * 36);
    const view = new DataView(bytes.buffer);
    for (const [at, block] of blocks.entries()) {
      for (const [column, value] of block.entries()) {
        if (column < 2) {
          view.setFloat64((column * count + at) * 8, value, true);
        } else {
          view.setUint32(count * 16 + ((column - 2) * count + at) * 4, value, true);
        }
      }
    }
    const zones = decodeZones(
      { range: [0, 100], blocks: count, names: ["outer", "inner", "zones"], marks: [], plots: [] },
      bytes,
    );
    expect([...zones.zones]).toEqual([1, 1, 7, 1]);
    expect([...zones.flags]).toEqual([0, 2, 0, 0]);
    // The heavier thread first, then any other with zones.
    const laid = tracks(zones, [9, 4]);
    expect(laid).toEqual([
      { thread: 9, row: 0, depths: 1 },
      { thread: 4, row: 1, depths: 2 },
    ]);
    const track = laid[1] ?? laid[0];
    expect(track && blockAt(zones, track, 1, 20, 0)).toBe(1);
    expect(track && blockAt(zones, track, 1, 45, 0)).toBeNull();
    expect(() =>
      decodeZones({ range: [0, 1], blocks: 2, names: [], marks: [], plots: [] }, bytes),
    ).toThrow();
  });

  it("a plot's bin strokes from its first value through its extremes to its last, which holds until the next", () => {
    // Two bins over 100 ns, 100 px wide and 24 tall, of values 0 to 10.
    const path = plotPath(
      [
        [10, 5, 0, 0, 10],
        [60, 10, 10, 10, 10],
      ],
      0,
      100,
      100,
      24,
      0,
      10,
    );
    expect(path).toBe(
      "M10.0 12.0L10.0 22.0L10.0 2.0L10.0 22.0L60.0 22.0L60.0 2.0L60.0 2.0L60.0 2.0L60.0 2.0",
    );
    expect(plotPath([[0, 3, 3, 3, 3]], 0, 1, 10, 20, 3, 3)).toBe(
      "M0.0 10.0L0.0 10.0L0.0 10.0L0.0 10.0",
    );
    // What held before the range starts the line at its left edge, and a
    // value after the range holds the last to its right.
    expect(plotPath([[40, 10, 10, 10, 10]], 0, 100, 100, 24, 0, 10, 0, true)).toBe(
      "M0.0 22.0L40.0 22.0L40.0 2.0L40.0 2.0L40.0 2.0L40.0 2.0L100.0 2.0",
    );
    expect(plotPath([], 0, 100, 100, 24, 0, 10, 5, true)).toBe("M0.0 12.0L100.0 12.0");
    expect(plotPath([], 0, 100, 100, 24, 0, 10, 5, false)).toBe("M0.0 12.0L100.0 12.0");
  });
});

describe("a weight", () => {
  it("reads in its unit: time, bytes, executions, or what a profile counts", () => {
    expect(formatWeight(1_500_000, "nanoseconds")).toBe("1.50 ms");
    expect(formatWeight(64_000, "bytes")).toBe("62.5 KiB");
    expect(formatWeight(512, "bytes")).toBe("512 B");
    expect(formatWeight(1, "executions")).toBe("1 execution");
    expect(formatWeight(3, "goroutine")).toBe("3 goroutines");
    expect(formatWeight(1, "goroutine")).toBe("1 goroutine");
    expect(formatWeight(1200, "samples")).toBe("1,200 samples");
  });

  it("never reads in exponents, however large", () => {
    expect(formatDuration(999_499)).toBe("999 µs");
    expect(formatDuration(999_999)).toBe("1.00 ms");
    expect(formatDuration(1.2e12)).toBe("1,200 s");
    expect(formatDuration(-999_999)).toBe("-1.00 ms");
    expect(formatBytes(1023 * 1024 + 1000)).toBe("1.00 MiB");
    expect(formatBytes(1500 * 2 ** 30)).toBe("1,500 GiB");
    expect(formatBytes(1023.6)).toBe("1.00 KiB");
  });
});

describe("a time in the profile", () => {
  it("reads as finely as what it is shown beside", () => {
    // A zone of a few milliseconds four seconds in still tells its start.
    expect(formatInstant(4_385_412_363, 30_000)).toBe("4.38541 s");
    expect(formatInstant(84_360_994, 50_000_000)).toBe("84 ms");
    expect(formatInstant(5_082_923_590, 50_000_000)).toBe("5.08 s");
    expect(formatInstant(0, 1000)).toBe("0 s");
    expect(formatInstant(1500, 1)).toBe("1.500 µs");
    // A ruler's ticks share the unit of its last.
    expect(formatInstant(500_000_000, 500_000_000, 5_680_000_000)).toBe("0.5 s");
    expect(formatSpan([4_385_412_363, 4_388_392_651])).toBe("4.38541 s – 4.38839 s (2.98 ms)");
    expect(formatSpan([84_360_994, 5_082_923_590])).toBe("84 ms – 5.08 s (5.00 s)");
  });
});

describe("a zones view's time", () => {
  it("zooms about the pointer, and never past the profile or below a microsecond", () => {
    // Zooming in twice at a quarter of the way keeps that time where it was.
    expect(zoomed([0, 1000_000], 250_000, 0.5, 1000_000)).toEqual([125_000, 625_000]);
    // Out from near an end slides inside the profile rather than past it.
    expect(zoomed([0, 600_000], 100_000, 2, 1000_000)).toEqual([0, 1000_000]);
    expect(zoomed([800_000, 1000_000], 900_000, 1.5, 1000_000)).toEqual([700_000, 1000_000]);
    expect(zoomed([500_000, 500_400], 500_200, 0.5, 1000_000)).toEqual([499_700, 500_700]);
  });

  it("pans by a time, stopping at either end", () => {
    expect(panned([100, 300], 50, 1000)).toEqual([150, 350]);
    expect(panned([100, 300], -500, 1000)).toEqual([0, 200]);
    expect(panned([100, 300], 5000, 1000)).toEqual([800, 1000]);
  });

  it("ticks at round times about a hundred pixels apart", () => {
    expect(ticks([0, 1000_000_000], 1000)).toEqual({
      step: 100_000_000,
      times: [0, 1e8, 2e8, 3e8, 4e8, 5e8, 6e8, 7e8, 8e8, 9e8, 1e9],
    });
    expect(ticks([4_385_412_363, 4_388_392_651], 600).step).toBe(500_000);
    expect(ticks([4_385_412_363, 4_388_392_651], 600).times[0]).toBe(4_385_500_000);
  });
});

describe("a count", () => {
  it("groups its digits and names one or many", () => {
    expect(formatCount(0)).toBe("0");
    expect(formatCount(1_234_567)).toBe("1,234,567");
    expect(formatCount(1, "sample")).toBe("1 sample");
    expect(formatCount(2, "sample")).toBe("2 samples");
    expect(formatCount(1200, "sample")).toBe("1,200 samples");
    expect(formatCount(3, "process", "processes")).toBe("3 processes");
    expect(formatCount(1, "process", "processes")).toBe("1 process");
  });
});

describe("the largest value", () => {
  it("never falls below its floor", () => {
    expect(largest([], 1)).toBe(1);
    expect(largest([0.5, 0.25], 1)).toBe(1);
    expect(largest([3, 9, 2], 1)).toBe(9);
    expect(largest([-4, -2], Number.NEGATIVE_INFINITY)).toBe(-2);
  });

  it("reads more values than a call can take as arguments", () => {
    // A lane per thread of 2048 buckets, for 300 threads.
    const values = new Array<number>(300 * 2048).fill(1);
    values[123_456] = 7;
    expect(largest(values, 0)).toBe(7);
  });
});

describe("a share", () => {
  it("is a percentage, finer below ten, and nothing of nothing", () => {
    expect(formatPercent(1, 0)).toBe("0%");
    expect(formatPercent(5, -1)).toBe("0%");
    expect(formatPercent(1, 3)).toBe("33.3%");
    expect(formatPercent(1, 10)).toBe("10.0%");
    expect(formatPercent(999, 10_000)).toBe("9.99%");
    expect(formatPercent(0, 7)).toBe("0.00%");
  });

  it("changes by percentage points, or not at all when either side weighs nothing", () => {
    expect(shareChange(1, 2, 1, 4)).toBe(25);
    expect(shareChange(1, 0, 1, 4)).toBeNull();
    expect(shareChange(1, 2, 0, 0)).toBeNull();
    expect(formatChange(null)).toBe("–");
    expect(formatChange(25)).toBe("+25.0 pp");
    expect(formatChange(-2.5)).toBe("−2.50 pp");
    // Too small to print is no change, of either sign.
    expect(formatChange(0.001)).toBe("±0");
    expect(formatChange(-0.001)).toBe("±0");
    expect(formatChange(0)).toBe("±0");
  });
});

describe("a time range in the address", () => {
  it("is nonempty, and absent when it is not", () => {
    expect(parseRange("5-90")).toEqual([5, 90]);
    expect(parseRange("90-90")).toBeNull();
    expect(parseRange("90-5")).toBeNull();
    expect(parseRange("-5-90")).toBeNull();
    expect(rangeOf({ range: "5-90" })).toEqual([5, 90]);
    expect(rangeOf({})).toBeNull();
  });
});

describe("a transform in the address", () => {
  it("is a known operation, a colon, and a name that may hold colons", () => {
    expect(parseTransform("merge:a::b")).toEqual({ op: "merge", name: "a::b" });
    expect(parseTransform("merge")).toBeNull();
    expect(parseTransform("spin:main")).toBeNull();
    expect(parseTransform(":main")).toBeNull();
    expect(parseTransform("drop:")).toBeNull();
  });
});

describe("a zone's flags", () => {
  it("say each thing that was not seen", () => {
    expect(describeFlags(0)).toEqual([]);
    expect(describeFlags(2)).toEqual(["its end was not seen"]);
    expect(describeFlags(7)).toEqual([
      "its begin was lost",
      "its end was not seen",
      "it does not nest",
    ]);
  });
});

describe("a zone's thread", () => {
  it("is named by its row's thread, or by the row when it has no name", () => {
    const threads = [
      { row: 0, pid: 1, tid: 1, name: "main", samples: 0, weight: 0, blocked: 0, preempted: 0 },
      { row: 1, pid: 1, tid: 2, name: null, samples: 0, weight: 0, blocked: 0, preempted: 0 },
    ];
    expect(threadName(threads, 0)).toBe("main");
    expect(threadName(threads, 1)).toBe("thread 1");
    expect(threadName(threads, 5)).toBe("thread 5");
  });
});
