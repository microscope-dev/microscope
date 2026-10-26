// The built-in renderers, each drawing a real program's values through a
// views file. Every test reads the card's inputs exactly, through Copy CSV,
// holds the drawing to them, then steps once and checks what the drawing
// marks against the stop before.

import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import * as path from "node:path";
import type { Locator, Page } from "@playwright/test";
import { card, copyCsv, drawAt, showTab, stubClipboard, titles } from "./drawings";
import { expect, fixture, join, type Microscope, root, test } from "./server";

/** Starts microscope on a fixture with its views file, and joins it. */
async function open(
  page: Page,
  microscope: Microscope,
  program: string,
  views: string,
): Promise<void> {
  await stubClipboard(page);
  const server = await microscope.start([
    "--views",
    path.join(root, "tests/fixtures", views),
    fixture(program),
  ]);
  await join(page, server.link);
}

/** The numbers an input holds, in order, from a card's CSV. */
function numbers(rows: Map<string, string>, name: string): number[] {
  const found: number[] = [];
  for (let index = 0; rows.has(`${name}[${index}]`); index++) {
    found.push(Number(rows.get(`${name}[${index}]`)));
  }
  return found;
}

/** The quantile `p` of `values` by linear interpolation (type 7). */
function quantile(sorted: number[], p: number): number {
  const h = (sorted.length - 1) * p;
  const lo = Math.floor(h);
  const below = sorted[lo] as number;
  return lo + 1 >= sorted.length ? below : below + (h - lo) * ((sorted[lo + 1] as number) - below);
}

/** Steps to the next stop and waits until `board`, on screen, has drawn it. */
async function step(page: Page, board: Locator, stop: string): Promise<void> {
  await board.scrollIntoViewIfNeeded();
  await page.keyboard.press("F5");
  await expect(board.locator(".drawing-head")).toContainText(`stop ${stop}`);
}

test("Copy CSV keeps a lazy drawing in view when an earlier drawing grows", async ({ page }) => {
  await page.setContent(`
    <style>html { overflow-anchor: none; }</style>
    <div id="earlier" style="height: 900px"></div>
    <section aria-label="Drawing of bytes" style="height: 80px">
      bytes <button hidden>Copy CSV</button>
    </section>
  `);
  await page.evaluate(() => {
    const earlier = document.getElementById("earlier") as HTMLElement;
    const board = document.querySelector("section") as HTMLElement;
    const button = board.querySelector("button") as HTMLButtonElement;
    let grew = false;
    const observer = new IntersectionObserver((entries) => {
      if (!entries.some((entry) => entry.isIntersecting)) {
        return;
      }
      if (!grew) {
        // The earlier renderer finishes before this card starts its draw.
        grew = true;
        earlier.style.height = "2000px";
      } else {
        button.hidden = false;
        observer.disconnect();
      }
    });
    observer.observe(board);
    button.onclick = () => {
      (window as unknown as { copied: string }).copied = "path,value\nvalues[0],7\n";
    };
  });
  expect(await copyCsv(page, card(page, "bytes"))).toEqual(new Map([["values[0]", "7"]]));
});

test.describe("a C program's metrics", () => {
  const metrics = (page: Page, microscope: Microscope) =>
    open(page, microscope, "metrics", "c/metrics/metrics.views");

  test("line-plot keeps a one-sample spike and gaps for NaN, against a ghost of the stop before", async ({
    page,
    microscope,
  }) => {
    await metrics(page, microscope);
    await drawAt(page, "metrics.c:119");
    const board = card(page, "m");
    await showTab(board, "line-plot");
    const values = numbers(await copyCsv(page, board), "values");
    expect(values).toHaveLength(600);
    // The spike is at 3/5 of the way, 250 plus the tick, and NaN at 1/10.
    expect(values[361]).toBe(251);
    expect(values.filter(Number.isNaN)).toHaveLength(1);
    const finite = values.filter(Number.isFinite);
    const least = values.indexOf(Math.min(...finite));
    const caption = board.locator(".drawing-caption");
    await expect(caption).toContainText("n=600");
    await expect(caption).toContainText(new RegExp(`min \\S+ at \\[${least}\\]`));
    await expect(caption).toContainText("max 251 at [361]");
    await expect(caption).toContainText("1 NaN, left as gaps");
    // More values than columns, yet the spike's column names it exactly.
    await expect(caption).toContainText("values a column: min/max band");
    expect((await titles(board)).some((title) => title.includes("max 251 at [361]"))).toBe(true);
    await expect(board.locator("svg text", { hasText: "stop before" })).toHaveCount(0);

    await step(page, board, "#3");
    await expect(caption).toContainText("max 252 at [361]");
    await expect(board.locator("svg text", { hasText: "stop before" })).toHaveCount(1);
  });

  test("histogram bins every sample and marks exact percentiles", async ({ page, microscope }) => {
    await metrics(page, microscope);
    await drawAt(page, "metrics.c:119");
    const board = card(page, "m");
    await showTab(board, "histogram");
    const latency = numbers(await copyCsv(page, board), "values");
    expect(latency).toHaveLength(5000);
    const sorted = [...latency].sort((a, b) => a - b);
    await expect(board.locator(".drawing-caption")).toContainText("n=5000");
    const drawn = await titles(board);
    for (const [name, p] of [
      ["p50", 0.5],
      ["p90", 0.9],
      ["p99", 0.99],
    ] as const) {
      expect(drawn).toContain(`${name} ${quantile(sorted, p)}`);
    }
    const bins = drawn
      .flatMap((title) => /^\[.*[)\]]: (\d+) · /.exec(title)?.[1] ?? [])
      .map(Number);
    expect(bins.reduce((a, b) => a + b, 0)).toBe(5000);
    expect(drawn.some((title) => title.endsWith("· 100% up to here"))).toBe(true);
    expect(drawn).not.toContain("the stop before");

    await step(page, board, "#3");
    expect(await titles(board)).toContain("the stop before");

    // The chosen drawing stays chosen at the next stop, and in another view.
    const histogram = board.getByRole("button", { name: "histogram", exact: true });
    await expect(histogram).toHaveAttribute("aria-pressed", "true");
    await page.getByRole("button", { name: "Source", exact: true }).click();
    await page.keyboard.press("Alt+v");
    await expect(histogram).toHaveAttribute("aria-pressed", "true");
  });

  test("box-plot gives each endpoint's exact quartiles and error bars, and opens its samples", async ({
    page,
    microscope,
  }) => {
    await metrics(page, microscope);
    await drawAt(page, "metrics.c:119");
    const board = card(page, "m");
    await showTab(board, "box-plot");
    const rows = await copyCsv(page, board);
    const names = ["search", "items", "login", "cart"];
    expect(names.map((_, index) => rows.get(`labels[${index}]`))).toEqual(names);
    const search = numbers(rows, "groups[0]").sort((a, b) => a - b);
    expect(search).toHaveLength(300);
    const box = board.getByRole("button", { name: /^search: n=300/ });
    const summary = await box.locator("title").textContent();
    expect(summary?.split("\n")).toEqual(
      expect.arrayContaining([
        `min ${search[0]}`,
        `q1 ${quantile(search, 0.25)}`,
        `median ${quantile(search, 0.5)}`,
        `q3 ${quantile(search, 0.75)}`,
        `max ${search.at(-1)}`,
      ]),
    );
    const drawn = await titles(board);
    for (const name of names) {
      expect(
        drawn.some((title) => title.startsWith(`${name}: mean `) && title.includes("sd")),
      ).toBe(true);
    }
    // Every 50th sample is 40 more: six outliers an endpoint.
    await expect(board.locator(".drawing-caption")).toContainText("24 outliers");

    await box.click();
    await expect(page.getByRole("dialog", { name: "Part m.endpoints[0]" })).toBeVisible();
    await page
      .getByRole("dialog", { name: "Part m.endpoints[0]" })
      .getByRole("button", { name: "Close" })
      .click();

    await step(page, board, "#3");
    expect(
      (await titles(board)).some(
        (title) => title.startsWith("search: median") && title.endsWith("at the stop before"),
      ),
    ).toBe(true);
  });

  test("scatter-plot names each point exactly and trails those that moved", async ({
    page,
    microscope,
  }) => {
    await metrics(page, microscope);
    await drawAt(page, "metrics.c:119");
    const board = card(page, "m");
    await showTab(board, "scatter-plot");
    const rows = await copyCsv(page, board);
    const x = numbers(rows, "x");
    const y = numbers(rows, "y");
    expect([x.length, y.length]).toEqual([700, 700]);
    await expect(board.locator(".drawing-caption")).toContainText("n=700");
    const drawn = await titles(board);
    expect(drawn).toContain(`[0]: x ${x[0]}, y ${y[0]}`);
    expect(drawn).toContain(`[699]: x ${x[699]}, y ${y[699]}`);
    expect(drawn.filter((title) => /^\[\d+\]: x /.test(title))).toHaveLength(700);
    const lines = await board.locator(".drawing-body svg line").count();

    // Every point's time grows by a tenth a tick.
    await step(page, board, "#3");
    await expect(board.locator(".drawing-body svg line")).toHaveCount(lines + 700);
  });

  test("heatmap diverges around zero, hatches NaN, and opens a cell", async ({
    page,
    microscope,
  }) => {
    await metrics(page, microscope);
    await drawAt(page, "metrics.c:119");
    const board = card(page, "m");
    await showTab(board, "heatmap");
    const rows = await copyCsv(page, board);
    expect(rows.get("values[3][8]")).toBe("NaN");
    expect(rows.get("row_labels[3]")).toBe("cpu3");
    const misses = Array.from({ length: 8 }, (_, row) => numbers(rows, `values[${row}]`));
    const all = misses.flat().filter(Number.isFinite);
    await expect(board.locator(".drawing-caption")).toHaveText(
      `8 × 24 cells · diverging around 0 · min ${Math.min(...all)} · max ${Math.max(...all)} · 1 NaN, hatched`,
    );
    const drawn = await titles(board);
    expect(drawn).toContain("cpu3[8]: NaN");
    expect(drawn).toContain(`cpu1[2]: ${misses[1]?.[2]}`);
    await board.getByRole("button", { name: `cpu1[2]: ${misses[1]?.[2]}` }).click();
    await expect(page.getByRole("dialog", { name: "Part m.misses[1][2]" })).toBeVisible();
    await page
      .getByRole("dialog", { name: "Part m.misses[1][2]" })
      .getByRole("button", { name: "Close" })
      .click();

    await step(page, board, "#3");
    await expect(board.locator(".drawing-caption")).toContainText("changed since the stop before");
  });

  test("bitmap shows each byte as a pixel, names it on hover, and outlines what changed", async ({
    page,
    microscope,
  }) => {
    await metrics(page, microscope);
    await drawAt(page, "metrics.c:119");
    const board = card(page, "m");
    await showTab(board, "bitmap");
    const image = board.locator('canvas[aria-label="pixels"]');
    // Each byte is ((row + column + tick) % 16) * 16.
    const gray = (column: number, row: number) =>
      image.evaluate(
        (canvas: HTMLCanvasElement, [c, r]: number[]) =>
          canvas.getContext("2d")?.getImageData(c as number, r as number, 1, 1).data[0],
        [column, row],
      );
    expect(await gray(0, 0)).toBe(16);
    expect(await gray(5, 3)).toBe(144);
    await expect(board.locator(".drawing-caption")).toHaveText(/^64 × 40 gray8 · at \d+×$/);
    const point = async (column: number, row: number) => {
      const box = await image.boundingBox();
      if (box === null) {
        throw new Error("the image is not on screen");
      }
      await page.mouse.move(
        box.x + ((column + 0.5) * box.width) / 64,
        box.y + ((row + 0.5) * box.height) / 40,
      );
    };
    await point(5, 3);
    await expect(board.getByRole("tooltip")).toHaveText("pixels: column 5, row 3");

    // Every pixel changes, and the outline leaves them visible.
    await step(page, board, "#3");
    await expect(board.locator(".drawing-caption")).toContainText(
      "2560 pixels changed since the stop before, outlined",
    );
    expect(await gray(0, 0)).toBe(32);
    await page.mouse.move(0, 0);
    await point(6, 3);
    await expect(board.getByRole("tooltip")).toHaveText("pixels: column 6, row 3");
  });

  test("bits draws a 64-bit word's bits and outlines those that flipped", async ({
    page,
    microscope,
  }) => {
    await metrics(page, microscope);
    await drawAt(page, "metrics.c:119");
    const board = card(page, "m");
    await showTab(board, "bits");
    // The tick's two flags and the tick itself, shifted up a byte.
    expect((await copyCsv(page, board)).get("values")).toBe("258");
    await expect(board.locator(".drawing-caption")).toHaveText(
      "1 × 64-bit · origin top-left · 8 bits a row · 0x102",
    );
    const drawn = await titles(board);
    expect(drawn.filter((title) => title.endsWith(": 1"))).toEqual([
      "value bit 1: 1",
      "value bit 8: 1",
    ]);
    await expect(board.locator("svg text", { hasText: "0x0000000000000102" })).toBeVisible();

    await step(page, board, "#3");
    await expect(board.locator(".drawing-caption")).toHaveText(
      "1 × 64-bit · origin top-left · 8 bits a row · 0x204 · 4 bits changed since the stop before, outlined",
    );
    // A bit opens the word it is part of.
    await board
      .locator("svg rect", { has: page.locator("title", { hasText: /^value bit 2: 1$/ }) })
      .click();
    await expect(page.getByRole("dialog", { name: "Part m.flags" })).toBeVisible();
  });
});

test.describe("a card's header", () => {
  test("keeps its tabs whole, inside the card, and in place in a narrow page", async ({
    page,
    microscope,
  }) => {
    await page.setViewportSize({ width: 1024, height: 700 });
    await open(page, microscope, "metrics", "c/metrics/metrics.views");
    await drawAt(page, "metrics.c:119");
    const board = card(page, "m");
    const tabs = board.getByRole("group", { name: "Drawings of m" }).getByRole("button");
    const boxes = () =>
      tabs.evaluateAll((buttons) =>
        buttons.map((button) => {
          const box = button.getBoundingClientRect();
          const card = (button.closest("section") as HTMLElement).getBoundingClientRect();
          return {
            x: Math.round(box.x),
            y: Math.round(box.y),
            height: Math.round(box.height),
            inside: box.left >= card.left && box.right <= card.right,
          };
        }),
      );
    const before = await boxes();
    expect(before).toHaveLength(7);
    for (const box of before) {
      expect(box.height).toBeLessThan(24);
      expect(box.inside).toBe(true);
    }
    // Drawing fills in the rest of the header, which moves no tab.
    await expect(board.locator(".drawing-caption")).toContainText("n=600");
    await expect(board.getByRole("button", { name: "Copy CSV" })).toBeVisible();
    expect(await boxes()).toEqual(before);
  });
});

test.describe("a Go program's counters", () => {
  test("bar-chart sorts a map by value and folds the rest into Other; a vertical one opens a bar", async ({
    page,
    microscope,
  }) => {
    await open(page, microscope, "charts", "go/charts/charts.views");
    await drawAt(page, "main.go:44");
    const hits = card(page, "hits");
    const rows = await copyCsv(page, hits);
    const entries: [string, number][] = [];
    for (let index = 0; rows.has(`entries[${index}][0]`); index++) {
      entries.push([
        rows.get(`entries[${index}][0]`) as string,
        Number(rows.get(`entries[${index}][1]`)),
      ]);
    }
    expect(entries).toHaveLength(56);
    expect(new Map(entries).get("/api/search")).toBe(600);
    const sorted = [...entries].sort(([a, x], [b, y]) => y - x || (a < b ? -1 : a > b ? 1 : 0));
    const bars = (await titles(hits)).filter(
      (title) => !title.includes("stop before") && title.includes(": "),
    );
    const shown = sorted.slice(0, 40).map(([label, value]) => `${label}: ${value}`);
    const rest = sorted.slice(40).reduce((total, [, value]) => total + value, 0);
    expect(bars.filter((title) => shown.includes(title))).toEqual(shown);
    expect(bars).toContain(`Other: 16 more entries, total ${rest}`);
    await expect(hits.locator(".drawing-caption")).toContainText("top 40 + Other (16 more");

    await step(page, hits, "#3");
    expect(await titles(hits)).toContain("/api/search at the stop before: 600");

    // Hours by index, in the program's order; a bar opens its element.
    const hours = card(page, "hours");
    await hours.scrollIntoViewIfNeeded();
    await expect(hours.locator(".drawing-head")).toContainText("stop #3");
    const counts = numbers(await copyCsv(page, hours), "values");
    expect(counts).toHaveLength(24);
    const drawn = await titles(hours);
    expect(drawn).toEqual(
      expect.arrayContaining(counts.map((count, hour) => `[${hour}]: ${count}`)),
    );
    await hours.getByRole("button", { name: `[7]: ${counts[7]}` }).click();
    await expect(page.getByRole("dialog", { name: "Part hours[7]" })).toContainText(
      String(counts[7]),
    );
  });

  test("donut-chart shows the seven largest shares and Other, summing to the whole", async ({
    page,
    microscope,
  }) => {
    await open(page, microscope, "charts", "go/charts/charts.views");
    await drawAt(page, "main.go:44");
    const bytes = card(page, "bytes");
    const rows = await copyCsv(page, bytes);
    const values: number[] = [];
    for (let index = 0; rows.has(`entries[${index}][1]`); index++) {
      values.push(Number(rows.get(`entries[${index}][1]`)));
    }
    expect(values).toHaveLength(10);
    const total = values.reduce((a, b) => a + b, 0);
    await expect(bytes.locator("svg text", { hasText: String(total) })).toBeVisible();
    const slices = (await titles(bytes)).filter((title) => / · [\d.]+%/.test(title));
    expect(slices).toHaveLength(8);
    expect(slices[0]).toBe(
      `text/html: 100000 · ${Number(((100000 / total) * 100).toPrecision(3))}%`,
    );
    expect(slices.at(-1)).toMatch(/^Other \(3\): /);
    const shares = slices.map((title) => Number(/ · ([\d.]+)%/.exec(title)?.[1]));
    expect(Math.abs(shares.reduce((a, b) => a + b, 0) - 100)).toBeLessThan(0.05 * slices.length);

    await step(page, bytes, "#3");
    expect((await titles(bytes)).some((title) => title.startsWith("text/html: 200000 · "))).toBe(
      true,
    );
  });
});

test.describe("a Rust program's profile", () => {
  test("flame-graph sizes frames by total, the same from nodes and from folded stacks", async ({
    page,
    microscope,
  }) => {
    await open(page, microscope, "profile", "rust/profile/profile.views");
    await drawAt(page, "main.rs:71");
    const profile = card(page, "profile");
    const rows = await copyCsv(page, profile);
    const nodes: { name: string; value: number; parent: number }[] = [];
    for (let index = 0; rows.has(`nodes[${index}].name`); index++) {
      nodes.push({
        name: rows.get(`nodes[${index}].name`) as string,
        value: Number(rows.get(`nodes[${index}].value`)),
        parent: Number(rows.get(`nodes[${index}].parent`)),
      });
    }
    expect(nodes).toHaveLength(12);
    const all = nodes.reduce((sum, node) => sum + node.value, 0);
    const total = (index: number): number =>
      (nodes[index]?.value ?? 0) +
      nodes.reduce(
        (sum, node, child) => (child !== index && node.parent === index ? sum + total(child) : sum),
        0,
      );
    const caption = `12 frames · ${all} in all · width = total`;
    await expect(profile.locator(".drawing-caption")).toHaveText(caption);
    const score = nodes.findIndex((node) => node.name === "main.score");
    const query = nodes.findIndex((node) => node.name === "main.(*Index).Query");
    const titleOf = (index: number) =>
      `${nodes[index]?.name}\nself ${nodes[index]?.value} · total ${total(index)} · ${Number(((total(index) / all) * 100).toPrecision(3))}% of all`;
    const drawn = await titles(profile);
    expect(drawn).toContain(titleOf(score));
    expect(drawn).toContain(titleOf(query));
    // The same profile as folded stacks draws the same frames.
    const folded = card(page, "folded");
    await expect(folded.locator(".drawing-caption")).toHaveText(caption);
    expect((await titles(folded)).sort()).toEqual([...drawn].sort());

    await profile.getByRole("button", { name: titleOf(score) }).click();
    await expect(page.getByRole("dialog", { name: `Part profile.nodes[${score}]` })).toContainText(
      "main.score",
    );
    await page
      .getByRole("dialog", { name: `Part profile.nodes[${score}]` })
      .getByRole("button", { name: "Close" })
      .click();

    await step(page, profile, "#3");
    await expect(profile.locator(".drawing-caption")).toContainText(
      "outlined: total changed since the stop before",
    );
    expect(
      (await titles(profile)).some(
        (title) => title.startsWith("main.score\n") && title.includes("at the stop before"),
      ),
    ).toBe(true);
  });
});

test.describe("Draw as…", () => {
  test("draws a Zig slice of f32 as a line and as a histogram", async ({ page, microscope }) => {
    await stubClipboard(page);
    const server = await microscope.start([fixture("samples")]);
    await join(page, server.link);
    await drawAt(page, "samples.zig:22");
    for (const stop of ["#3", "#4", "#5", "#6"]) {
      await page.keyboard.press("F5");
      await expect(page.getByTestId("stops")).toContainText(stop);
    }
    await page.getByRole("button", { name: "Draw window as…" }).first().click();
    await page.getByRole("menuitem", { name: /^line-plot/ }).click();
    const line = page.getByRole("region", { name: "Drawing of window as line-plot" });
    const rows = await copyCsv(page, line);
    const values = numbers(rows, "values");
    // Five f32 samples, each written as the shortest text that is that f32,
    // as the CLI prints it: the first, sin 0 + 0, is 0.
    expect(values).toHaveLength(5);
    expect(values[0]).toBe(0);
    expect(rows.get("values[1]")).toBe("2.1088262");
    await expect(line.locator(".drawing-caption")).toContainText("n=5");
    // Few enough values that each is a mark of its own, which opens it.
    await line.getByRole("button", { name: `[2] values: ${values[2]}` }).click();
    await expect(page.getByRole("dialog", { name: "Part window[2]" })).toBeVisible();
    await page
      .getByRole("dialog", { name: "Part window[2]" })
      .getByRole("button", { name: "Close" })
      .click();

    await page.getByRole("button", { name: "Draw window as…" }).first().click();
    await page.getByRole("menuitem", { name: /^histogram/ }).click();
    const histogram = page.getByRole("region", { name: "Drawing of window as histogram" });
    await expect(histogram.locator(".drawing-caption")).toContainText("n=5");
  });
});

test.describe("a C++ program's own renderer", () => {
  test("draws a search tree in key order, marks a new node, and draws a std::map as bars", async ({
    page,
    microscope,
  }) => {
    await open(page, microscope, "tree", "cpp/tree/tree.views");
    await drawAt(page, "tree.cpp:48");
    const tree = card(page, "tree");
    await expect(tree.locator(".drawing-caption")).toHaveText("1 nodes · depth 1");
    await step(page, tree, "#3");
    await step(page, tree, "#4");
    await expect(tree.locator(".drawing-caption")).toHaveText(
      "3 nodes · depth 2 · 1 added since the stop before",
    );
    // In key order, left to right.
    const keys = await tree.getByRole("button").evaluateAll((nodes) =>
      nodes
        .map((node) => ({
          key: node.querySelector("title")?.textContent ?? "",
          x: node.getBoundingClientRect().x,
        }))
        .filter((node) => /^\d+: node/.test(node.key))
        .sort((a, b) => a.x - b.x)
        .map((node) => node.key),
    );
    expect(keys).toEqual(["30: node 1, depth 1", "50: node 0, depth 0", "70: node 2, depth 1"]);
    await tree.getByRole("button", { name: "70: node 2, depth 1" }).click();
    await expect(page.getByRole("dialog", { name: "Part tree.pool[2]" })).toContainText("Node");
    await page
      .getByRole("dialog", { name: "Part tree.pool[2]" })
      .getByRole("button", { name: "Close" })
      .click();

    await page.getByRole("button", { name: "Draw counts as…" }).first().click();
    await page.getByRole("menuitem", { name: /^bar-chart/ }).click();
    const counts = page.getByRole("region", { name: "Drawing of counts as bar-chart" });
    // "tree", "map", "node" so far.
    await expect
      .poll(async () => (await titles(counts)).filter((title) => /^\w+: \d+$/.test(title)).sort())
      .toEqual(["map: 1", "node: 1", "tree: 1"]);
  });
});

test.describe("a built-in renderer", () => {
  test("draws the same picture when a views file names a copy of it", async ({
    page,
    microscope,
  }) => {
    const directory = test.info().outputPath("views");
    await mkdir(directory, { recursive: true });
    await copyFile(
      path.join(root, "views/visualizers/bar-chart.js"),
      path.join(directory, "bars.js"),
    );
    const views = path.join(directory, "hours.views");
    await writeFile(
      views,
      `microscope-views 1
extend go main.Hours {
    visualize "bar-chart" {
        values = self
        orientation = "vertical"
    }
    visualize "bars" {
        values = self
        orientation = "vertical"
    }
}
`,
    );
    expect(await readFile(path.join(directory, "bars.js"), "utf8")).toContain("microscope.draw(");
    const server = await microscope.start(["--views", views, fixture("charts")]);
    await join(page, server.link);
    await drawAt(page, "main.go:44");
    const hours = card(page, "hours");
    const picture = async (renderer: string, stop: string) => {
      await showTab(hours, renderer);
      await expect(hours.locator(".drawing-head")).toContainText(
        renderer === "bars" ? "bars.js" : "built-in",
      );
      await expect(hours.locator(".drawing-head")).toContainText(`stop ${stop}`);
      await expect(hours.locator(".drawing-body svg")).toBeVisible();
      return {
        svg: await hours.locator(".drawing-body").innerHTML(),
        caption: await hours.locator(".drawing-caption").textContent(),
      };
    };
    // Each draws once at the first stop, so each has a stop before at the next.
    await picture("bars", "#2");
    await picture("bar-chart", "#2");
    await step(page, hours, "#3");
    const builtIn = await picture("bar-chart", "#3");
    const copy = await picture("bars", "#3");
    expect(copy).toEqual(builtIn);
    expect(builtIn.caption).toContain("| = the stop before");
  });
});
