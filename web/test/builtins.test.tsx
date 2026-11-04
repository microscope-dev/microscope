// Each built-in renderer is loaded the way a user's is, from its file in
// views/visualizers, run in the locked-down worker on known inputs, and
// its picture, checked by the page's validator, held to known answers.

import { describe, expect, it } from "vitest";
import { commands } from "vitest/browser";

import type { Picture, Shape } from "../src/visualize/picture";
import { palette } from "./palette";
import { draw, shapesOf, texts, titles } from "./renderer";

const source = (name: string) => commands.readFile(`../views/visualizers/${name}.js`);

/** The picture's shapes of one type. */
const ofType = <T extends Shape["type"]>(picture: Picture, type: T) =>
  shapesOf(picture).filter((shape): shape is Shape & { type: T } => shape.type === type) as Extract<
    Shape,
    { type: T | (T extends "polyline" | "polygon" ? "polyline" | "polygon" : never) }
  >[];

const bars = (picture: Picture) =>
  ofType(picture, "rect").filter(
    (rect) => rect.title !== undefined && !rect.title.includes("stop before"),
  );

describe("bar-chart", async () => {
  const chart = await source("bar-chart");

  it("sorts a map by value and folds past 40 into Other, with its count and total", async () => {
    // A Go map's entries arrive in any order.
    const entries = Array.from({ length: 45 }, (_, i) => [
      `/api/${String(i).padStart(2, "0")}`,
      BigInt(((i * 37) % 45) + 1),
    ]);
    const picture = await draw(chart, { entries }, { paths: { entries: "hits" } });
    const drawn = bars(picture);
    expect(drawn).toHaveLength(41);
    const values = drawn.slice(0, 40).map((rect) => Number(rect.title?.split(": ")[1]));
    expect(values).toEqual(values.toSorted((a, b) => b - a));
    expect(values[0]).toBe(45);
    // 1 to 5 are left: 15 in all.
    expect(drawn[40]?.title).toBe("Other: 5 more entries, total 15");
    expect(drawn[40]?.fill).toBe(palette.ink3);
    expect(picture.caption).toBe(
      "45 entries · total 1035 · max 45 at /api/17 · sorted by value (a map's order may be random) · top 40 + Other (5 more, total 15)",
    );
    // A map's entries are not its members, so bars select nothing.
    expect(drawn.some((rect) => rect.select !== undefined)).toBe(false);
  });

  it("extends negative values the other way from zero and marks the stop before", async () => {
    const picture = await draw(
      chart,
      { values: new Int32Array([5, -3, 2]), labels: ["up", "down", "flat"] },
      {
        previous: { values: new Int32Array([4, -3, 2]), labels: ["up", "down", "flat"] },
        paths: { values: "deltas" },
      },
    );
    const [up, down, flat] = bars(picture);
    expect(up?.title).toBe("up: 5");
    expect((down?.x ?? 0) + (down?.width ?? 0)).toBeCloseTo(up?.x ?? -1);
    expect(flat?.select).toBe("deltas[2]");
    const ticks = ofType(picture, "line").filter((line) => line.title?.includes("stop before"));
    expect(ticks.map((tick) => tick.title)).toEqual(["up at the stop before: 4"]);
    expect(picture.caption).toBe(
      "3 entries · total 4 · max 5 at up · 1 negative · in the program's order · | = the stop before",
    );
  });

  it("draws columns in the program's order when vertical", async () => {
    const hours = new Int32Array(24).map((_, h) => 400 + h * 10);
    const picture = await draw(
      chart,
      { values: hours, orientation: "vertical" },
      { paths: { values: "by_hour" } },
    );
    const drawn = bars(picture);
    expect(drawn.map((rect) => rect.title)).toEqual(Array.from(hours, (v, h) => `[${h}]: ${v}`));
    expect(drawn.every((rect, h) => h === 0 || rect.x > (drawn[h - 1]?.x ?? 0))).toBe(true);
    expect(drawn[23]?.select).toBe("by_hour[23]");
    expect(texts(picture)).toContain("630");
  });

  it("says what it cannot draw", async () => {
    await expect(draw(chart, { values: ["a"] })).rejects.toThrow(
      "bar-chart draws numbers, and `values[0]` is a string",
    );
    await expect(draw(chart, { values: [1], orientation: "diagonal" })).rejects.toThrow(
      "orientation",
    );
  });
});

describe("scatter-plot", async () => {
  const chart = await source("scatter-plot");

  it("titles each dot exactly, gives three groups hues, and counts what it cannot place", async () => {
    const x = new Float64Array([1, 2, 3, 4, 5, Number.NaN]);
    const y = new BigInt64Array([10n, 20n, 30n, 40n, 9007199254740993n, 1n]);
    const group = ["hit", "miss", "hit", "stale", "evicted", "hit"];
    const picture = await draw(chart, { x, y, group, labels: ["a", "b", "c", "d", "e", "f"] });
    const dots = ofType(picture, "circle").filter((dot) => dot.title !== undefined);
    expect(dots).toHaveLength(5);
    expect(dots[4]?.title).toBe("[4] e: x 5, y 9007199254740993 (evicted)");
    // The three most common groups, in name order; the rest are other.
    expect(dots.map((dot) => dot.fill)).toEqual([
      palette.series[1],
      palette.series[2],
      palette.series[1],
      palette.ink3,
      palette.series[0],
    ]);
    expect(picture.caption).toBe(
      "n=6 · groups: evicted 1, hit 3, miss 1, other 1 · x 1 to 5 · y 10 to 9007199254740993 · 1 with NaN or ±∞, not drawn",
    );
    // Two inputs are two parts, so a dot opens neither.
    expect(dots.some((dot) => dot.select !== undefined)).toBe(false);
  });

  it("opens a point drawn from one sequence of pairs, and trails the ones that moved", async () => {
    const values = [
      [1, 1],
      [2, 2],
      [3, 3],
    ];
    const picture = await draw(
      chart,
      { values },
      {
        previous: {
          values: [
            [1, 1],
            [2, 5],
            [3, 3],
          ],
        },
        paths: { values: "entries" },
      },
    );
    expect(ofType(picture, "circle").map((dot) => dot.select)).toEqual([
      "entries[0]",
      "entries[1]",
      "entries[2]",
    ]);
    const trails = ofType(picture, "line").filter((line) => line.opacity === 0.7);
    expect(trails).toHaveLength(1);
  });

  it("draws more than 10,000 points as one image", async () => {
    const n = 20_000;
    const x = new Float64Array(n).map((_, i) => i % 200);
    const y = new Float64Array(n).map((_, i) => Math.floor(i / 200));
    const picture = await draw(chart, { x, y });
    expect(ofType(picture, "circle")).toHaveLength(0);
    expect(ofType(picture, "image")).toHaveLength(1);
    expect(picture.caption).toContain("one image");
  });
});

const quantile = (sorted: number[], p: number) => {
  const h = (sorted.length - 1) * p;
  const lo = Math.floor(h);
  const a = sorted[lo] as number;
  const b = sorted[Math.min(lo + 1, sorted.length - 1)] as number;
  return a + (h - lo) * (b - a);
};

describe("histogram", async () => {
  const chart = await source("histogram");

  it("cuts Freedman–Diaconis bins and titles each with its count and running share", async () => {
    const values = new Float64Array(1000).map((_, i) => i);
    const picture = await draw(chart, { values });
    // IQR 499.5 over 1000 samples is bins 99.9 wide, ten of them.
    expect(picture.caption).toBe(
      "n=1000 · 10 bins (Freedman–Diaconis) of 99.9 · min 0 · max 999 · mean 499.5 · quantiles: linear (type 7)",
    );
    const bins = ofType(picture, "rect").filter((rect) => rect.title?.startsWith("["));
    expect(bins).toHaveLength(10);
    expect(bins[0]?.title).toBe("[0, 99.9): 100 · 10% up to here");
    expect(bins[9]?.title).toBe("[899.1, 999]: 100 · 100% up to here");
  });

  it("finds its quantiles by selection exactly as sorting does", async () => {
    let seed = 3;
    const random = () => {
      seed = (seed * 1103515245 + 12345) & 0x7fffffff;
      return seed / 0x7fffffff;
    };
    const values = new Float64Array(10_001).map(() => Math.exp(random() * 4));
    values[17] = Number.NaN;
    const sorted = Array.from(values)
      .filter((value) => Number.isFinite(value))
      .toSorted((a, b) => a - b);
    const picture = await draw(chart, { values });
    for (const [name, p] of [
      ["p50", 0.5],
      ["p90", 0.9],
      ["p99", 0.99],
    ] as const) {
      expect(titles(picture)).toContain(`${name} ${quantile(sorted, p)}`);
    }
    expect(picture.caption).toContain("1 NaN or ±∞, not counted");
  });

  it("draws counts already binned, and the stop before as an outline", async () => {
    const picture = await draw(
      chart,
      { counts: new BigUint64Array([1n, 3n, 6n]), edges: new Float64Array([0, 1, 2, 4]) },
      {
        previous: {
          counts: new BigUint64Array([2n, 3n, 5n]),
          edges: new Float64Array([0, 1, 2, 4]),
        },
      },
    );
    expect(picture.caption).toBe(
      "n=10 · 3 bins as given · quantiles interpolate within bins · outline = the stop before",
    );
    expect(titles(picture)).toContain("[2, 4]: 6 · 100% up to here");
    // Half the samples are by 2 + (5 - 4) / 6 of the last bin's width.
    expect(titles(picture)).toContain(`p50 ${2 + (1 / 6) * 2}`);
    expect(ofType(picture, "polyline").map((line) => line.title)).toEqual(["the stop before"]);
  });
});

describe("box-plot", async () => {
  const chart = await source("box-plot");

  it("draws type-7 quartiles, Tukey whiskers, and the outliers past them", async () => {
    const samples = new Float64Array([...Array.from({ length: 20 }, (_, i) => i + 1), 100]);
    const picture = await draw(chart, {
      groups: [
        ["search", samples],
        ["items", new Float64Array([1, 2, 3, 4])],
      ],
    });
    const [search, items] = ofType(picture, "rect").filter((rect) => rect.title !== undefined);
    expect(search?.title).toBe(
      "search: n=21\nmin 1\nq1 6\nmedian 11\nq3 16\nmax 100\nwhiskers 1 to 20",
    );
    expect(items?.title).toBe(
      "items: n=4\nmin 1\nq1 1.75\nmedian 2.5\nq3 3.25\nmax 4\nwhiskers 1 to 4",
    );
    expect(titles(picture)).toContain("search: outlier 100");
    expect(picture.caption).toBe(
      "2 groups · quartiles: linear (type 7) · whiskers: 1.5 × IQR · 1 outlier",
    );
    expect(texts(picture)).toEqual(expect.arrayContaining(["search", "n=21", "items", "n=4"]));
  });

  it("draws error bars of the mean, summaries as given, and the median at the stop before", async () => {
    const groups = { a: new Float64Array([1, 2, 3, 4, 5]), b: new Float64Array([2, 4, 6]) };
    const picture = await draw(
      chart,
      { groups, error: "sd" },
      {
        previous: {
          groups: { a: new Float64Array([1, 2, 2, 4, 5]), b: new Float64Array([2, 4, 6]) },
        },
        paths: { groups: "latency" },
      },
    );
    expect(titles(picture)).toContain("a: mean 3 ±1 sd (1.419 to 4.581)");
    expect(titles(picture)).toContain("a: median 2 at the stop before");
    expect(titles(picture).filter((title) => title.includes("stop before"))).toHaveLength(1);
    expect(
      ofType(picture, "rect")
        .filter((rect) => rect.select !== undefined)
        .map((rect) => rect.select),
    ).toEqual(["latency.a", "latency.b"]);

    const given = await draw(chart, {
      groups: [{ min: 1, q1: 2, median: 3, q3: 4, max: 5 }],
      mean: [3.5],
      error: [0.5],
      error_label: "95% CI",
    });
    expect(given.caption).toBe("1 group · 0 outliers");
    expect(titles(given)).toContain("[0]\nmin 1\nq1 2\nmedian 3\nq3 4\nmax 5");
    expect(titles(given)).toContain("[0]: mean 3.5 95% CI (3 to 4)");
  });
});

const slices = (picture: Picture) =>
  ofType(picture, "path").filter((path) => path.title !== undefined);

describe("donut-chart", async () => {
  const chart = await source("donut-chart");

  it("draws seven slices largest first, folds the rest into Other, and adds up to the whole", async () => {
    const entries = Array.from({ length: 10 }, (_, i) => [`type${i}`, BigInt(100 - i * 9)]);
    const picture = await draw(chart, { entries: entries.toReversed() });
    const drawn = slices(picture);
    expect(drawn).toHaveLength(8);
    expect(drawn[0]?.title).toBe("type0: 100 · 16.8%");
    expect(drawn[7]?.title).toBe("Other (3): 84 · 14.1%");
    expect(drawn[7]?.fill).toBe(palette.ink3);
    const shares = drawn.map((slice) => Number(/· ([\d.]+)%/u.exec(slice.title ?? "")?.[1]));
    expect(Math.abs(shares.reduce((a, b) => a + b, 0) - 100)).toBeLessThan(0.5);
    expect(picture.caption).toBe("10 slices · total 595 · 7 largest + Other (3, 14.1%)");
    expect(texts(picture)).toContain("595");
  });

  it("keeps each label's hue, gives each share's change, and refuses negative values", async () => {
    const one = await draw(
      chart,
      { values: [3, 1], labels: ["b", "a"] },
      { previous: { values: [1, 1], labels: ["b", "a"] } },
    );
    const two = await draw(chart, { values: [1, 3], labels: ["a", "b"] });
    const fill = (picture: Picture, label: string) =>
      slices(picture).find((slice) => slice.title?.startsWith(`${label}:`))?.fill;
    expect(fill(one, "a")).toBe(fill(two, "a"));
    expect(fill(one, "b")).toBe(fill(two, "b"));
    expect(slices(one).map((slice) => slice.title)).toEqual([
      "b: 3 · 75%\n+25 points since the stop before",
      "a: 1 · 25%\n-25 points since the stop before",
    ]);
    await expect(draw(chart, { values: [3, -1] })).rejects.toThrow(
      "a donut shows shares, and [1] is -1",
    );
  });
});

const cells = (picture: Picture) =>
  ofType(picture, "rect").filter((rect) => rect.title !== undefined);

describe("heatmap", async () => {
  const chart = await source("heatmap");

  it("diverges around zero, hatches NaN, and opens each cell", async () => {
    const values = [new Float64Array([-4, 0, 4]), new Float64Array([2, Number.NaN, -2])];
    const picture = await draw(chart, { values }, { paths: { values: "misses" } });
    const drawn = cells(picture);
    expect(drawn.map((cell) => cell.title)).toEqual([
      "[0][0]: -4",
      "[0][1]: 0",
      "[0][2]: 4",
      "[1][0]: 2",
      "[1][1]: NaN",
      "[1][2]: -2",
    ]);
    expect(drawn[1]?.fill).toBe("#ececec");
    expect(drawn[0]?.fill).toBe("#2266c4");
    expect(drawn[2]?.fill).toBe("#d23f3e");
    expect(drawn[4]?.fill).toBe(palette.line);
    expect(drawn[5]?.select).toBe("misses[1][2]");
    expect(picture.caption).toBe(
      "2 × 3 cells · diverging around 0 · min -4 · max 4 · 1 NaN, hatched",
    );
    expect(texts(picture)).toEqual(expect.arrayContaining(["-4", "0", "4", "NaN"]));
  });

  it("is sequential for one sign, and outlines what changed", async () => {
    const picture = await draw(
      chart,
      { values: new Uint8Array([1, 2, 3, 4]), columns: 2, row_labels: ["cpu0", "cpu1"] },
      { previous: { values: new Uint8Array([1, 2, 3, 9]), columns: 2 }, paths: { values: "load" } },
    );
    expect(cells(picture).map((cell) => cell.title)).toEqual([
      "cpu0[0]: 1",
      "cpu0[1]: 2",
      "cpu1[0]: 3",
      "cpu1[1]: 4",
    ]);
    expect(cells(picture)[3]?.select).toBe("load[3]");
    expect(picture.caption).toBe(
      "2 × 2 cells · sequential · min 1 · max 4 · 1 changed since the stop before",
    );
    expect(
      ofType(picture, "rect").filter((rect) => rect.fill === "none" && rect.stroke === palette.ink),
    ).toHaveLength(2);
  });

  it("draws more than 10,000 cells as one image, each pixel the largest of its cells", async () => {
    const values = new Float32Array(1000 * 1000).map((_, i) => (i === 500_500 ? 99 : (i % 7) - 3));
    const before = values.slice();
    before[17] = 50;
    const picture = await draw(
      chart,
      { values, columns: 1000 },
      { previous: { values: before, columns: 1000 } },
    );
    const images = ofType(picture, "image");
    // The grid and the changed overlay; the legend's ramp.
    expect(images).toHaveLength(3);
    expect(cells(picture)).toHaveLength(0);
    expect(picture.caption).toContain("each pixel the largest of its cells");
    expect(picture.caption).toContain("1 changed since the stop before");
    expect(picture.caption).toContain("max 99");
  });
});

const frames = (picture: Picture) =>
  ofType(picture, "rect").filter((rect) => rect.title !== undefined);

describe("flame-graph", async () => {
  const chart = await source("flame-graph");

  it("sums totals from parent indices, and the same from folded stacks", async () => {
    const nodes = [
      { name: "main", value: 0n, parent: 0n },
      { name: "serve", value: 2n, parent: 0n },
      { name: "json", value: 14n, parent: 1n },
      { name: "malloc", value: 9n, parent: 2n },
      { name: "gc", value: 5n, parent: 0n },
    ];
    const tree = await draw(chart, { nodes }, { paths: { nodes: "profile.nodes" } });
    expect(frames(tree).map((frame) => frame.title)).toEqual([
      "main\nself 0 · total 30 · 100% of all",
      "serve\nself 2 · total 25 · 83.3% of all",
      "json\nself 14 · total 23 · 76.7% of all",
      "malloc\nself 9 · total 9 · 30% of all",
      "gc\nself 5 · total 5 · 16.7% of all",
    ]);
    expect(frames(tree).map((frame) => frame.select)).toEqual([
      "profile.nodes[0]",
      "profile.nodes[1]",
      "profile.nodes[2]",
      "profile.nodes[3]",
      "profile.nodes[4]",
    ]);
    expect(tree.caption).toBe("5 frames · 30 in all · width = total");

    const stacks = [
      ["main;serve", 2n],
      ["main;serve;json", 14n],
      ["main;serve;json;malloc", 9n],
      ["main;gc", 5n],
    ];
    const folded = await draw(chart, { stacks });
    expect(frames(folded).map((frame) => frame.title)).toEqual(
      frames(tree).map((frame) => frame.title),
    );
  });

  it("outlines frames whose total changed, and refuses a cycle", async () => {
    const now = [
      ["a;b", 3],
      ["a;c", 1],
    ];
    const picture = await draw(
      chart,
      { stacks: now },
      {
        previous: {
          stacks: [
            ["a;b", 2],
            ["a;c", 1],
          ],
        },
      },
    );
    const outlined = frames(picture).filter((frame) => frame.stroke !== undefined);
    expect(outlined.map((frame) => frame.title?.split("\n")[0])).toEqual(["a", "b"]);
    expect(picture.caption).toBe(
      "3 frames · 4 in all · width = total · 2 outlined: total changed since the stop before",
    );
    await expect(
      draw(chart, {
        nodes: [
          { name: "x", value: 1, parent: 1 },
          { name: "y", value: 1, parent: 0 },
        ],
      }),
    ).rejects.toThrow("cycle");
  });

  it("counts frames under a pixel instead of drawing them", async () => {
    const stacks = [
      ["root;big", 1_000_000],
      ...Array.from({ length: 50 }, (_, i) => [`root;tiny${i}`, 1]),
    ];
    const picture = await draw(chart, { stacks });
    expect(frames(picture)).toHaveLength(2);
    expect(picture.caption).toContain("50 frames under 1 px, counted not drawn");
  });
});

const image = (picture: Picture) =>
  ofType(picture, "image")[0] as unknown as { pixels: Uint8Array; columns: number };

describe("bitmap", async () => {
  const chart = await source("bitmap");

  it("draws gray, RGBA, RGB565, and one-bit pixels at a whole zoom", async () => {
    const gray = await draw(chart, {
      pixels: new Uint8Array([0, 128, 255, 64, 1, 2, 3, 4]),
      columns: 4,
    });
    expect(Array.from(image(gray).pixels.slice(4, 8))).toEqual([128, 128, 128, 255]);
    expect(gray.caption).toBe("4 × 2 gray8 · at 200×");
    expect(gray.width).toBe(800);
    const red = await draw(chart, {
      pixels: new Uint8Array([0x00, 0xf8]),
      columns: 1,
      format: "rgb565",
    });
    expect([...image(red).pixels]).toEqual([255, 0, 0, 255]);
    const bits = await draw(chart, {
      pixels: new Uint8Array([0b1000_0001]),
      columns: 8,
      format: "bits",
    });
    expect(Array.from({ length: 8 }, (_, c) => image(bits).pixels[c * 4])).toEqual([
      0, 255, 255, 255, 255, 255, 255, 0,
    ]);
    const rows = await draw(chart, {
      values: [new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8])],
      format: "rgba8",
    });
    expect(image(rows).columns).toBe(2);
  });

  it("marks what changed and says when bytes are not whole rows", async () => {
    const picture = await draw(
      chart,
      { pixels: new Uint8Array([0, 0, 0, 9]), columns: 2 },
      { previous: { pixels: new Uint8Array([0, 0, 0, 0]), columns: 2 } },
    );
    expect(ofType(picture, "image")).toHaveLength(2);
    expect(picture.caption).toContain("1 pixel changed since the stop before, outlined");
    await expect(draw(chart, { pixels: new Uint8Array(7), columns: 2 })).rejects.toThrow(
      "7 bytes are not whole rows of 2 gray8 pixels",
    );
  });
});

const overlay = (picture: Picture) =>
  ofType(picture, "image")[1] as unknown as {
    pixels: Uint8ClampedArray;
    columns: number;
    rows: number;
  };

const alpha = (overlaid: ReturnType<typeof overlay>, column: number, row: number) =>
  overlaid.pixels[(row * overlaid.columns + column) * 4 + 3];

describe("bitmap changes", async () => {
  const chart = await source("bitmap");

  it("outlines changed pixels, so an image that changed everywhere still shows", async () => {
    const before = new Uint8Array(16);
    const everywhere = await draw(
      chart,
      { pixels: new Uint8Array(16).fill(9), columns: 4 },
      { previous: { pixels: before, columns: 4 } },
    );
    const ring = overlay(everywhere);
    // Four marks a pixel each way, at this zoom.
    expect([ring.columns, ring.rows]).toEqual([16, 16]);
    expect(alpha(ring, 0, 7)).toBeGreaterThan(0);
    expect(alpha(ring, 15, 7)).toBeGreaterThan(0);
    expect(alpha(ring, 7, 7)).toBe(0);
    expect(alpha(ring, 4, 4)).toBe(0);
    expect(everywhere.caption).toContain("16 pixels changed since the stop before, outlined");

    // One pixel, at column 1 and row 1: a ring of its own, which its
    // unchanged neighbours do not share.
    const one = new Uint8Array(16);
    one[5] = 9;
    const single = overlay(
      await draw(chart, { pixels: one, columns: 4 }, { previous: { pixels: before, columns: 4 } }),
    );
    expect(alpha(single, 4, 5)).toBeGreaterThan(0);
    expect(alpha(single, 7, 5)).toBeGreaterThan(0);
    expect(alpha(single, 5, 5)).toBe(0);
    expect(alpha(single, 3, 5)).toBe(0);
    expect(alpha(single, 8, 5)).toBe(0);
  });

  it("tints changed pixels when they are too small to outline", async () => {
    const wide = await draw(
      chart,
      { pixels: new Uint8Array(1200).fill(9), columns: 1200 },
      { previous: { pixels: new Uint8Array(1200), columns: 1200 } },
    );
    const tint = overlay(wide);
    expect(tint.columns).toBe(1200);
    expect(alpha(tint, 600, 0)).toBeGreaterThan(0);
    expect(alpha(tint, 600, 0)).toBeLessThan(128);
    expect(wide.caption).toContain("1200 pixels changed since the stop before, tinted");
  });
});

describe("bits", async () => {
  const chart = await source("bits");

  it("draws a u64 bitboard from the bottom left, marking the bits that changed", async () => {
    const pawns = 0x000000001000ef00n;
    const picture = await draw(
      chart,
      { values: new BigUint64Array([pawns]), origin: "bottom-left", labels: ["white pawns"] },
      { previous: { values: new BigUint64Array([0xff00n]) }, paths: { values: "board.pieces" } },
    );
    const bits = ofType(picture, "rect").filter((rect) => rect.title !== undefined);
    expect(bits).toHaveLength(64);
    // a2 is bit 8: the second row from the bottom, first column.
    const a2 = bits.find((rect) => rect.title === "white pawns bit 8: 1");
    expect(a2?.y).toBe(6 * 14);
    expect(a2?.x).toBe(0);
    expect(bits.find((rect) => rect.title === "white pawns bit 28: 1")?.fill).toBe(
      palette.series[0],
    );
    expect(picture.caption).toBe(
      "1 × 64-bit · origin bottom-left · 8 bits a row · 0x1000ef00 · 2 bits changed since the stop before, outlined",
    );
    expect(ofType(picture, "group")[0]?.select).toBe("board.pieces[0]");
    expect(texts(picture)).toContain("0x000000001000ef00");
  });

  it("takes a plain integer's width from the view", async () => {
    const picture = await draw(chart, { values: 5, width: 4, columns: 4 });
    expect(ofType(picture, "rect").map((rect) => rect.title)).toEqual([
      "value bit 0: 1",
      "value bit 1: 0",
      "value bit 2: 1",
      "value bit 3: 0",
    ]);
  });

  it("takes one label for one integer, and says when nothing changed", async () => {
    const picture = await draw(
      chart,
      { values: 0xffffn, labels: "white", origin: "bottom-left" },
      { previous: { values: 0xffffn, labels: "white", origin: "bottom-left" } },
    );
    expect(texts(picture)).toContain("white");
    expect(picture.caption).toBe(
      "1 × 64-bit · origin bottom-left · 8 bits a row · 0xffff · unchanged since the stop before",
    );
  });
});

describe("unchanged since the stop before", () => {
  it("is what the bitmap and the heatmap say when nothing changed", async () => {
    const bitmap = await draw(
      await source("bitmap"),
      { pixels: new Uint8Array(4), columns: 2 },
      { previous: { pixels: new Uint8Array(4), columns: 2 } },
    );
    expect(bitmap.caption).toBe("2 × 2 gray8 · at 320× · unchanged since the stop before");
    const grid = { values: new Float64Array([1, 2, 3, 4]), columns: 2 };
    const heatmap = await draw(await source("heatmap"), grid, { previous: grid });
    expect(heatmap.caption).toMatch(/ · unchanged since the stop before$/u);
  });
});

describe("line-plot", async () => {
  const plot = await source("line-plot");

  it("keeps every column's extremes, so one sample in a hundred thousand shows", async () => {
    const values = new Float64Array(100_000).map((_, i) => Math.sin(i / 1000));
    values[61_234] = 50;
    values[70_001] = -50;
    const picture = await draw(plot, { values }, { paths: { values: "samples" } });
    const columnTitles = titles(picture);
    expect(columnTitles.some((title) => title.includes("max 50 at [61234]"))).toBe(true);
    expect(columnTitles.some((title) => title.includes("min -50 at [70001]"))).toBe(true);
    expect(picture.caption).toContain("max 50 at [61234]");
    expect(picture.caption).toContain("min -50 at [70001]");
    expect(picture.caption).toMatch(/values a column: min\/max band/u);
    // At most four vertices a column: first, lowest, highest, last.
    const lines = ofType(picture, "polyline").filter((line) => line.stroke === palette.series[0]);
    const vertices = lines.reduce((total, line) => total + line.points.length / 2, 0);
    const columns = new Set(
      lines.flatMap((line) =>
        Array.from({ length: line.points.length / 2 }, (_, k) => line.points[k * 2]),
      ),
    );
    expect(vertices).toBeLessThanOrEqual(columns.size * 4);
    const ys = lines.flatMap((line) =>
      Array.from({ length: line.points.length / 2 }, (_, k) => line.points[k * 2 + 1] as number),
    );
    const top = Math.min(...ys);
    const bottom = Math.max(...ys);
    // The spike reaches the top of the axis, which ends at the next round tick.
    expect(texts(picture)).toContain("60");
    expect(top).toBeLessThan(bottom - 150);
    // A decimated column spans many values, so it selects none.
    expect(shapesOf(picture).some((shape) => shape.select !== undefined)).toBe(false);
  });

  it("gives round ticks, gaps for NaN, arrows for infinities, and exact titles", async () => {
    const picture = await draw(
      plot,
      {
        values: new Float64Array([
          0,
          88.1,
          Number.NaN,
          30,
          Number.POSITIVE_INFINITY,
          12,
          0.1 + 0.2,
        ]),
      },
      { paths: { values: "samples" } },
    );
    for (const tick of ["0", "20", "40", "60", "80", "100"]) {
      expect(texts(picture)).toContain(tick);
    }
    const lines = ofType(picture, "polyline").filter((line) => line.stroke === palette.series[0]);
    expect(lines).toHaveLength(3);
    expect(titles(picture)).toContain("[4] values: +∞");
    expect(titles(picture)).toContain("[6]\nvalues: 0.30000000000000004");
    expect(picture.caption).toBe(
      "n=7 · min 0 at [0] · max 88.1 at [1] · mean 26.08 · 1 NaN, left as gaps · 1 ±∞, arrows at the edge",
    );
    // A column of one value opens it, wherever the input came from.
    const marks = shapesOf(picture).filter((shape) => shape.select !== undefined);
    expect(marks.map((mark) => mark.select)).toEqual(
      Array.from({ length: 7 }, (_, i) => `samples[${i}]`),
    );
  });

  it("writes 64-bit integers whole and 32-bit floats shortest", async () => {
    const big = await draw(plot, { values: new BigUint64Array([18446744073709551615n, 1n]) });
    expect(titles(big)).toContain("[0]\nvalues: 18446744073709551615");
    const float = await draw(plot, { values: new Float32Array([0.1, 0.2]) });
    expect(titles(float)).toContain("[0]\nvalues: 0.1");
  });

  it("keeps each series' color by its name, with a legend and end labels", async () => {
    const a = new Float64Array([1, 2, 3]);
    const b = new Float64Array([3, 2, 1]);
    const one = await draw(plot, {
      series: [
        ["beta", b],
        ["alpha", a],
      ],
    });
    const two = await draw(plot, { series: { alpha: a, beta: b } });
    for (const picture of [one, two]) {
      const strokes = ofType(picture, "polyline").map((line) => line.stroke);
      expect(strokes).toContain(palette.series[0]);
      expect(strokes).toContain(palette.series[1]);
      const tallest = ofType(picture, "polyline").find(
        (line) =>
          line.points[1] ===
          Math.max(...ofType(picture, "polyline").map((l) => l.points[1] as number)),
      );
      expect(tallest?.stroke).toBe(palette.series[0]);
      expect(texts(picture)).toEqual(expect.arrayContaining(["alpha", "beta"]));
      expect(picture.caption?.startsWith("2 series × 3")).toBe(true);
    }
    // Several series give each value's mark every series' value, and select none.
    expect(titles(one)).toContain("[1]\nbeta: 2\nalpha: 2");
  });

  it("draws the stop before as a faint ghost", async () => {
    const picture = await draw(
      plot,
      { values: new Float64Array([1, 2, 3]) },
      { previous: { values: new Float64Array([3, 2, 1]) } },
    );
    const ghosts = ofType(picture, "polyline").filter((line) => line.stroke === palette.ink3);
    expect(ghosts).toHaveLength(1);
  });

  it("says what it cannot draw", async () => {
    await expect(draw(plot, { values: [{ a: 1 }] })).rejects.toThrow("`values[0]` is not a number");
    await expect(draw(plot, { other: 1 })).rejects.toThrow("line-plot takes `values`");
  });
});
