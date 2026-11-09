// A profile's view: its address round-trips, and its call tree's columns
// decode into geometry that nests, as real servers' answers do.

import { describe, expect, it } from "vitest";

import { parseSearch, stringifySearch } from "../src/focus";
import {
  commandLine,
  medianDepth,
  recorderBreakdown,
  recordingSettings,
  shellQuote,
} from "../src/profile/inspect";
import {
  cleared,
  type ProfileLook,
  parseBand,
  parseInstance,
  parseRange,
  parseTransform,
  queryOf,
  rangeOf,
  scopeOf,
  spanOf,
  transformed,
  untransformed,
  validateProfileLook,
  withInstance,
} from "../src/profile/look";
import {
  changeFill,
  changeOf,
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
  weightChange,
} from "../src/profile/paint";
import {
  type CallTree,
  decodeTree,
  findPath,
  hit,
  layout,
  matching,
  pathOf,
  revealed,
} from "../src/profile/tree";
import {
  bandOf,
  blockAt,
  bucketSpans,
  decodeZones,
  describeFlags,
  indexBlocks,
  laneAt,
  layLanes,
  nesting,
  panned,
  plotPath,
  threadName,
  ticks,
  timeTicks,
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
      dm: "share",
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
        dm: "time",
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
    expect(untransformed(focused, 0)).toEqual({ fn: "leaf" });
  });

  it("removes only the transform asked, keeping those before and after it", () => {
    const look = { z: ["main"], tf: ["focus:outer", "merge:middle", "drop:leaf"] };
    expect(untransformed(look, 1)).toEqual({ tf: ["focus:outer", "drop:leaf"] });
    expect(untransformed(look, 0)).toEqual({ tf: ["merge:middle", "drop:leaf"] });
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

const twelfths = (fraction: number) => Math.round(fraction * 12 * 1e9) / 1e9;

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
    expect(() => decodeTree(answer, new Uint8Array(24))).toThrow(/48 bytes/u);
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
    expect(() => decodeTree(answer, bytes.slice(0, 24))).toThrow(/40 bytes/u);
  });
});

const hueOf = (fill: string) => Number(/^hsl\((\d+)/u.exec(fill)?.[1]);
const lightness = (fill: string) => Number(/(\d+(?:\.\d+)?)%\)$/u.exec(fill)?.[1]);

describe("a search of a call tree", () => {
  it("marks every match, nested ones too, but counts a nested one's samples once", () => {
    // main (10) holds leaf (4); thread (2) holds leaf (2): of 12, all of it.
    const found = matching(tree, (name) => name.includes("a"));
    expect([...found.nodes].toSorted()).toEqual([0, 2, 4, 5]);
    expect(found.weight).toBe(12);
    expect(matching(tree, (name) => name === "leaf").weight).toBe(6);
  });

  it("opens every caller of each match, and nothing else", () => {
    expect([...revealed(tree, new Set(), [2])].toSorted()).toEqual([0, 1]);
    // An open caller under a closed one still opens the closed one.
    const open = new Set([1]);
    expect([...revealed(tree, open, [2, 5])].toSorted()).toEqual([0, 1, 4]);
    expect([...open]).toEqual([1]);
    expect([...revealed(tree, new Set(), [0, 4])]).toEqual([]);
  });
});

describe("a diff's colors", () => {
  it("are red where a share grew, blue where it shrank, stronger the more", () => {
    expect(diffFill(0.5, 0.5, false)).toBe(diffFill(0.2, 0.2, false));
    expect(hueOf(diffFill(0.5, 0.25, false))).toBe(0);
    expect(hueOf(diffFill(0.25, 0.5, false))).toBe(215);
    expect(lightness(diffFill(0.5, 0, false))).toBeLessThan(lightness(diffFill(0.5, 0.4, false)));
  });
});

describe("a diff in time", () => {
  const s = 1e9;
  // V8's scavenger in the ESLint diff: less time, in a smaller profile.
  const after = [29.323 * s, 222.541 * s] as const;
  const before = [30.322 * s, 280 * s] as const;

  it("reads as report --base does, whichever side is zero", () => {
    expect(weightChange(after[0], before[0], "nanoseconds")).toBe("−999 ms −3.3%");
    expect(weightChange(3 * s, 2 * s, "nanoseconds")).toBe("+1.00 s +50.0%");
    expect(weightChange(2 * s, 0, "nanoseconds")).toBe("+2.00 s new");
    expect(weightChange(0, 2 * s, "nanoseconds")).toBe("−2.00 s gone");
    expect(weightChange(0, 0, "nanoseconds")).toBe("±0");
    expect(weightChange(5, 5, "executions")).toBe("±0");
    expect(weightChange(3, 4, "executions")).toBe("−1 execution −25.0%");
    expect(weightChange(3 * 1024, 1024, "bytes")).toBe("+2.00 KiB +200.0%");
  });

  it("goes the way the cost went, where shares can go the other", () => {
    expect(changeOf("time", after, before, "nanoseconds")).toEqual({
      text: "−999 ms −3.3%",
      way: "down",
    });
    expect(changeOf("share", after, before, "nanoseconds")).toEqual({
      text: "+2.35 pp",
      way: "up",
    });
    expect(changeOf("time", [0, 1], [0, 1], "nanoseconds")).toEqual({ text: "±0", way: "" });
    expect(changeOf("time", [1, 1], [0, 0], "nanoseconds").way).toBe("up");
    expect(changeOf("time", [0, 1], [1, 1], "nanoseconds").way).toBe("down");
  });

  it("is colored by cost, scaled to the larger side", () => {
    expect(hueOf(changeFill("time", after, before, false))).toBe(215);
    expect(hueOf(changeFill("share", after, before, false))).toBe(0);
    expect(hueOf(changeFill("time", [s, 1], [0, 1], false))).toBe(0);
    expect(hueOf(changeFill("time", [0, 1], [s, 1], true))).toBe(215);
    // Twice the time is as strong a change at any scale, and less than all new.
    expect(changeFill("time", [2, 9], [1, 9], false)).toBe(
      changeFill("time", [2 * s, 9], [s, 9], false),
    );
    expect(lightness(changeFill("time", [s, 1], [0, 1], false))).toBeLessThan(
      lightness(changeFill("time", [2 * s, 1], [s, 1], false)),
    );
    expect(changeFill("time", [0, 1], [0, 1], false)).toBe(diffFill(1, 1, false));
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
    for (const decoded of trees) {
      let roots = 0;
      for (let node = 0; node < decoded.count; node += 1) {
        let callees = 0;
        for (
          let child = node + 1;
          child < (decoded.end[node] ?? 0);
          child = decoded.end[child] ?? 0
        ) {
          callees += decoded.total[child] ?? 0;
        }
        expect(decoded.total[node]).toBe((decoded.own[node] ?? 0) + callees);
        roots += decoded.parent[node] === -1 ? (decoded.total[node] ?? 0) : 0;
      }
      expect(roots).toBe(decoded.weight);
      for (const rect of layout(decoded, null, 0)) {
        const parent = decoded.parent[rect.node] ?? -1;
        expect(rect.x + rect.width).toBeLessThanOrEqual(1 + 1e-9);
        const outer =
          parent >= 0
            ? (layout(decoded, null, 0).find((each) => each.node === parent)?.x ?? 0)
            : Number.NEGATIVE_INFINITY;
        expect(rect.x).toBeGreaterThanOrEqual(outer);
      }
    }
  });
});

describe("a zones answer", () => {
  it("decodes its columns, and finds the block at a time in each thread's row", () => {
    // [start, end, covered, blocked, preempted, thread, depth, name, count, flags]
    const blocks: number[][] = [
      [0, 100, 100, 60, 10, 4, 0, 0, 1, 0],
      [10, 40, 30, 0, 0, 4, 1, 1, 1, 2],
      [50, 52, 1.5, 0, 0.5, 4, 1, 2, 7, 0],
      [5, 30, 25, 0, 0, 9, 0, 0, 1, 0],
    ];
    const count = blocks.length;
    const bytes = new Uint8Array(count * 60);
    const view = new DataView(bytes.buffer);
    for (const [at, block] of blocks.entries()) {
      for (const [column, value] of block.entries()) {
        if (column < 5) {
          view.setFloat64((column * count + at) * 8, value, true);
        } else {
          view.setUint32(count * 40 + ((column - 5) * count + at) * 4, value, true);
        }
      }
    }
    const zones = decodeZones(
      {
        range: [0, 100],
        blocks: count,
        names: ["outer", "inner", "zones"],
        marks: [],
        plots: [],
        offCpu: true,
      },
      bytes,
    );
    expect(zones.offCpu).toBe(true);
    expect([...zones.start]).toEqual([0, 10, 50, 5]);
    expect([...zones.covered]).toEqual([100, 30, 1.5, 25]);
    expect([...zones.blocked]).toEqual([60, 0, 0, 0]);
    expect([...zones.preempted]).toEqual([10, 0, 0.5, 0]);
    expect([...zones.thread]).toEqual([4, 4, 4, 9]);
    expect([...zones.zones]).toEqual([1, 1, 7, 1]);
    expect([...zones.flags]).toEqual([0, 2, 0, 0]);
    const index = indexBlocks(zones);
    expect([...index.depths]).toEqual([
      [4, 2],
      [9, 1],
    ]);
    expect(blockAt(zones, index, 4, 1, 20, 0)).toBe(1);
    expect(blockAt(zones, index, 4, 1, 45, 0)).toBeNull();
    // Slack reaches the nearer of two blocks, never the farther.
    expect(blockAt(zones, index, 4, 1, 45, 6)).toBe(2);
    expect(blockAt(zones, index, 4, 1, 44, 4)).toBe(1);
    // Another thread's block at the same time and depth is not this one's.
    expect(blockAt(zones, index, 9, 0, 20, 0)).toBe(3);
    expect(blockAt(zones, index, 9, 1, 20, 100)).toBeNull();
    expect(blockAt(zones, index, 7, 0, 20, 100)).toBeNull();
    // What holds a zone, outermost first; a fold names nothing.
    expect(nesting(zones, index, 4, 1, 20)).toEqual(["outer"]);
    expect(nesting(zones, index, 4, 2, 51)).toEqual(["outer"]);
    expect(() =>
      decodeZones(
        { range: [0, 1], blocks: 2, names: [], marks: [], plots: [], offCpu: false },
        bytes,
      ),
    ).toThrow("2 zone blocks need 120 bytes, not 240");
  });

  it("finds the block at a time by bisection, whatever order the blocks came in", () => {
    // One row of a thousand blocks, each 10 long with a gap of 10, sent
    // in reverse; a linear oracle agrees at every time and slack.
    const count = 1000;
    const bytes = new Uint8Array(count * 60);
    const view = new DataView(bytes.buffer);
    for (let at = 0; at < count; at += 1) {
      const start = (count - 1 - at) * 20;
      view.setFloat64(at * 8, start, true);
      view.setFloat64((count + at) * 8, start + 10, true);
      view.setUint32(count * 40 + (3 * count + at) * 4, 1, true);
    }
    const zones = decodeZones(
      { range: [0, 20_000], blocks: count, names: ["a"], marks: [], plots: [], offCpu: false },
      bytes,
    );
    const index = indexBlocks(zones);
    for (const slack of [0, 3, 7]) {
      for (let time = -10; time < 20_010; time += 7) {
        let expected: number | null = null;
        let distance = Number.POSITIVE_INFINITY;
        for (let at = 0; at < count; at += 1) {
          const start = zones.start[at] ?? 0;
          const end = zones.end[at] ?? 0;
          const away = time < start ? start - time : time > end ? time - end : 0;
          if (away <= slack && away < distance) {
            [expected, distance] = [at, away];
          }
        }
        const found = blockAt(zones, index, 0, 0, time, slack);
        const foundAway =
          found === null
            ? null
            : Math.max((zones.start[found] ?? 0) - time, time - (zones.end[found] ?? 0), 0);
        expect(foundAway).toBe(expected === null ? null : distance);
      }
    }
  });

  it("lays lanes one under another, and finds the one at a height", () => {
    const { tops, height } = layLanes([0, 3, 1], 18, 16);
    expect([...tops]).toEqual([0, 18, 84]);
    expect(height).toBe(118);
    expect(laneAt(tops, -1)).toBe(-1);
    expect(laneAt(tops, 0)).toBe(0);
    expect(laneAt(tops, 17.9)).toBe(0);
    expect(laneAt(tops, 18)).toBe(1);
    expect(laneAt(tops, 83)).toBe(1);
    expect(laneAt(tops, 117)).toBe(2);
  });

  it("picks a band of lengths from histogram buckets, either way across", () => {
    const spans = bucketSpans(
      [
        [100, 4],
        [200, 0],
        [400, 1],
      ],
      900,
    );
    expect(spans).toEqual([
      [100, 199],
      [200, 399],
      [400, 900],
    ]);
    expect(bandOf(spans, 1, 1)).toEqual([200, 399]);
    expect(bandOf(spans, 2, 0)).toEqual([100, 900]);
    // One zone alone: its bucket holds its length.
    expect(bucketSpans([[50, 1]], 50)).toEqual([[50, 50]]);
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

describe("the lanes' time", () => {
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
    const values = Array.from({ length: 300 * 2048 }, () => 1);
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

  it("is an operation alone when it takes no name", () => {
    expect(parseTransform("collapseCycles")).toEqual({ op: "collapseCycles", name: "" });
    expect(parseTransform("collapseCycles:main")).toBeNull();
    expect(parseTransform("collapse")).toBeNull();
    const look = transformed({ tf: ["merge:a"] }, { op: "collapseCycles", name: "" });
    expect(look.tf).toEqual(["merge:a", "collapseCycles"]);
    expect(validateProfileLook({ tf: look.tf })).toEqual({ tf: ["merge:a", "collapseCycles"] });
    expect(queryOf("p", look).transforms).toEqual([
      { op: "merge", name: "a" },
      { op: "collapseCycles", name: "" },
    ]);
  });
});

describe("zones in the address", () => {
  it("keeps a band and a scope only with a site, and a scope of the band only with a band", () => {
    expect(validateProfileLook({ zb: "5-9", zf: "all", bz: "rest" })).toEqual({});
    expect(validateProfileLook({ zs: "drain", zf: "band" })).toEqual({ zs: "drain" });
    expect(validateProfileLook({ zs: "drain", zb: "9-5", bz: "rest" })).toEqual({ zs: "drain" });
    const look: ProfileLook = { zs: "drain", zb: "5-9", zf: "band", bz: "rest", zi: "3:10-20" };
    expect(roundTrip(look)).toEqual(look);
    expect(validateProfileLook({ zi: "x:10-20" })).toEqual({});
    expect(validateProfileLook({ zi: "3:20-10" })).toEqual({});
  });

  it("reads a band as inclusive, and one length as a band", () => {
    expect(parseBand("7-7")).toEqual([7, 7]);
    expect(parseBand("8-7")).toBeNull();
    expect(parseInstance("12:5-9")).toEqual({ thread: 12, span: [5, 9] });
    expect(parseInstance(":5-9")).toBeNull();
  });

  it("asks for the samples inside the site's zones, or its band's, but never a wait's", () => {
    expect(scopeOf({ zs: "drain" })).toBeNull();
    expect(scopeOf({ zs: "drain", zf: "all", zb: "5-9" })).toEqual({
      name: "drain",
      band: null,
      outside: false,
    });
    expect(queryOf("p", { zs: "drain", zb: "5-9", zf: "band" }).zones).toEqual({
      name: "drain",
      band: [5, 9],
      outside: false,
    });
    expect(queryOf("p", { zs: "drain", zf: "all", src: "waits" }).zones).toBeUndefined();
  });

  it("compares the band with the rest of its site over the same range", () => {
    const query = queryOf("p", { zs: "drain", zb: "5-9", bz: "rest", range: "100-200" });
    expect(query.zones).toEqual({ name: "drain", band: [5, 9], outside: false });
    expect(query.base).toEqual({
      profile: "p",
      range: [100, 200],
      zones: { name: "drain", band: [5, 9], outside: true },
    });
    // The rest takes the place of another profile.
    expect(queryOf("p", { zs: "drain", zb: "5-9", bz: "rest", bp: "q" }).base?.profile).toBe("p");
  });

  it("reads one zone's span instead of the range and the site's zones, threads untouched", () => {
    const look = withInstance(
      { range: "0-1000", th: ["1"], zs: "drain", zf: "all" },
      { thread: 4, span: [10, 20] },
    );
    expect(look.th).toEqual(["1"]);
    expect(spanOf(look)).toEqual([10, 20]);
    const query = queryOf("p", look);
    expect(query.range).toEqual([10, 20]);
    expect(query.threads).toEqual([1]);
    expect(query.zones).toBeUndefined();
  });

  it("lets go of the zone, then the band, then the site", () => {
    const look: ProfileLook = {
      range: "0-9",
      zs: "drain",
      zb: "5-9",
      zf: "band",
      bz: "rest",
      zi: "3:1-2",
    };
    const once = cleared(look);
    expect(once).toEqual({ range: "0-9", zs: "drain", zb: "5-9", zf: "band", bz: "rest" });
    const twice = cleared(once);
    expect(twice).toEqual({ range: "0-9", zs: "drain" });
    expect(cleared({ zs: "drain", zb: "5-9", zf: "all" })).toEqual({ zs: "drain", zf: "all" });
    const thrice = cleared(twice);
    expect(thrice).toEqual({ range: "0-9" });
    // Nothing left to clear is the same look, so Escape does nothing.
    expect(cleared(thrice)).toBe(thrice);
  });

  it("maps the old Zones view to the flame graph", () => {
    expect(validateProfileLook({ view: "zones", zs: "drain" })).toEqual({ zs: "drain" });
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
  it("is named by its row's thread and its id, so workers of one name differ", () => {
    const threads = [
      {
        row: 0,
        pid: 40,
        tid: 40,
        name: "main",
        samples: 0,
        weight: 0,
        zones: 0,
        blocked: 0,
        preempted: 0,
      },
      {
        row: 1,
        pid: 40,
        tid: 47,
        name: "WorkerThread",
        samples: 0,
        weight: 0,
        zones: 0,
        blocked: 0,
        preempted: 0,
      },
      {
        row: 2,
        pid: 40,
        tid: 48,
        name: "WorkerThread",
        samples: 0,
        weight: 0,
        zones: 0,
        blocked: 0,
        preempted: 0,
      },
      {
        row: 3,
        pid: 40,
        tid: 52,
        name: null,
        samples: 0,
        weight: 0,
        zones: 0,
        blocked: 0,
        preempted: 0,
      },
    ];
    expect(threadName(threads, 0)).toBe("main 40");
    expect(threadName(threads, 1)).toBe("WorkerThread 47");
    expect(threadName(threads, 2)).toBe("WorkerThread 48");
    expect(threadName(threads, 3)).toBe("thread 52");
    expect(threadName(threads, 5)).toBe("thread 5");
  });
});

/** The nanoseconds of a breakdown's parts, together. */
function partsSum(parts: [string, number][]): number {
  return parts.reduce((total, [, each]) => total + each, 0);
}

describe("inspect", () => {
  it("quotes only the arguments a shell would split or expand", () => {
    expect(shellQuote("-uu")).toBe("-uu");
    expect(shellQuote("/a/b.c=d:e@f%g+h,i")).toBe("/a/b.c=d:e@f%g+h,i");
    expect(shellQuote("two words")).toBe("'two words'");
    expect(shellQuote("")).toBe("''");
    expect(shellQuote(String.raw`\b\w+_\w+\s*\(`)).toBe(String.raw`'\b\w+_\w+\s*\('`);
    expect(shellQuote("it's")).toBe(String.raw`'it'\''s'`);
    expect(shellQuote("$HOME")).toBe("'$HOME'");
  });

  it("joins kept arguments as a shell reads them, and keeps a joined line as it is", () => {
    expect(commandLine({ arguments: ["rg", "-e", "a b", ""], text: "rg -e a b " })).toBe(
      "rg -e 'a b' ''",
    );
    expect(commandLine({ arguments: null, text: "rg -e a b" })).toBe("rg -e a b");
  });

  it("finds the depth half the samples reach", () => {
    expect(medianDepth([])).toBeNull();
    expect(medianDepth([0, 0])).toBeNull();
    expect(medianDepth([0, 3, 0, 0, 1])).toBe(1);
    expect(medianDepth([0, 1, 0, 0, 3])).toBe(4);
    expect(medianDepth([0, 1, 1, 1, 1])).toBe(2);
  });

  it("splits microscope's CPU into parts that add up to it", () => {
    const split = recorderBreakdown(100, 60, 10, 50);
    expect(split).toEqual([
      ["Setup", 10],
      ["While recording", 30],
      ["Debug info after exit", 50],
      ["Other debug info", 10],
    ]);
    expect(partsSum(split)).toBe(100);

    // Reading after exit is part of reading at all, never more than it.
    const over = recorderBreakdown(100, 20, 10, 50);
    expect(over[2]).toEqual(["Debug info after exit", 20]);
    expect(over[3]).toEqual(["Other debug info", 0]);
    expect(partsSum(over)).toBe(100);
  });

  it("lists the settings that say how it was recorded, not what Inspect shows elsewhere", () => {
    const names = recordingSettings(
      ["source", "program", "arguments", "command 0", "arguments 12", "start", "off-CPU"].map(
        (name) => ({ name, value: "x" }),
      ),
    ).map(({ name }) => name);
    expect(names).toEqual(["source", "off-CPU"]);
  });

  it("marks round times under a chart and its end, leaving out one too near the end", () => {
    // 539 ms: a 100 ms step, and 500 ms is past 90% of the way, so only the end shows there.
    expect(timeTicks([0, 539e6], 600)).toEqual([
      { at: 0, label: "0" },
      { at: 100 / 539, label: "100 ms" },
      { at: 200 / 539, label: "200 ms" },
      { at: 300 / 539, label: "300 ms" },
      { at: 400 / 539, label: "400 ms" },
      { at: 1, label: "539 ms" },
    ]);
    // 2.6 s: a half-second step, and 2.5 s is too near the end.
    expect(timeTicks([0, 2.6e9], 600).map(({ label }) => label)).toEqual([
      "0",
      "0.5 s",
      "1.0 s",
      "1.5 s",
      "2.0 s",
      "2.60 s",
    ]);
    expect(timeTicks([0, 0], 600)).toEqual([]);
    // A view into the profile ends on its own time, not on its length.
    expect(
      timeTicks([1e9, 1.5e9], 600)
        .map(({ label }) => label)
        .at(-1),
    ).toBe("1.5 s");
    // Twice as wide, twice as many: a 50 ms step over the same 539 ms.
    expect(timeTicks([0, 539e6], 1200).map(({ label }) => label)).toEqual([
      "0",
      ...[50, 100, 150, 200, 250, 300, 350, 400, 450].map((ms) => `${ms} ms`),
      "539 ms",
    ]);
    // Rounded to the step, a view's end can read as the round time before
    // it: that one is left out, so no two marks read alike.
    expect(timeTicks([3_827_129_872, 4_244_501_091], 344).map(({ label }) => label)).toEqual([
      "4.0 s",
      "4.2 s",
    ]);
    for (const view of [
      [4_365_562_973, 6_231_633_582],
      [9_386_206_571, 9_386_234_158],
      [105_497_791, 105_544_710],
    ] as const) {
      const labels = timeTicks(view, 400).map(({ label }) => label);
      expect(new Set(labels).size).toBe(labels.length);
    }
  });
});
