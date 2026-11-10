// The built-in renderers, each drawing a real program's values through a
// views file. Every test reads the card's inputs exactly, through Copy CSV,
// holds the drawing to them, then steps once and checks what the drawing
// marks against the stop before.

import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import * as path from "node:path";

import type { Locator, Page } from "@playwright/test";

import { card, copyCsv, drawAt, showTab, stubClipboard, titles } from "./drawings";
import { expect, fixture, join, type Microscope, menu, root, test } from "./server";

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

/** Steps to the next stop and waits until `board`, on screen, has drawn it. */
async function step(page: Page, board: Locator, stop: string): Promise<void> {
  await board.scrollIntoViewIfNeeded();
  await page.keyboard.press("F5");
  await expect(board.locator(".drawing-head")).toContainText(`stop ${stop}`);
}

const metrics = (page: Page, microscope: Microscope) =>
  open(page, microscope, "metrics", "c/metrics/metrics.views");

/** The number of values a card's CSV holds under `name`, as `name[i]` or
 * `name[i][j]`. */
function csvCount(rows: Map<string, string>, name: string): number {
  return [...rows.keys()].filter((key) => key.startsWith(`${name}[`)).length;
}

/** The renderers' own math is held to known answers in
 * test/builtins.test.tsx; here each draws a real program's values once,
 * opens a part, and marks the stop before after one step. */
test("a C program's metrics draw in every built-in, open their parts, and mark the stop before", async ({
  page,
  microscope,
}) => {
  await metrics(page, microscope);
  await drawAt(page, "metrics.c:119");
  const board = card(page, "m");
  const caption = board.locator(".drawing-caption");
  const part = async (name: string) => {
    const dialog = page.getByRole("dialog", { name: `Part ${name}` });
    await expect(dialog).toBeVisible();
    await dialog.getByRole("button", { name: "Close" }).click();
  };

  // Every value the card was given is drawn: the CSV's count is the
  // caption's.
  for (const [tab, input, drawn] of [
    ["line-plot", "values", "n=600"],
    ["histogram", "values", "n=5000"],
    ["scatter-plot", "x", "n=700"],
  ] as const) {
    await showTab(board, tab);
    const rows = await copyCsv(page, board);
    await expect(caption).toContainText(drawn);
    expect(`n=${csvCount(rows, input)}`).toBe(drawn);
  }

  await showTab(board, "box-plot");
  expect(csvCount(await copyCsv(page, board), "groups[0]")).toBe(300);
  await board.getByRole("button", { name: /^search: n=300/u }).click();
  await part("m.endpoints[0]");

  await showTab(board, "heatmap");
  const rows = await copyCsv(page, board);
  expect(csvCount(rows, "values")).toBe(8 * 24);
  await expect(caption).toContainText("8 × 24 cells");
  await board.getByRole("button", { name: `cpu1[2]: ${rows.get("values[1][2]")}` }).click();
  await part("m.misses[1][2]");

  await showTab(board, "bitmap");
  await expect(caption).toContainText("64 × 40 gray8");

  await showTab(board, "bits");
  expect((await copyCsv(page, board)).get("values")).toBe("258");
  await expect(caption).toContainText("0x102");

  // One step, and each drawing shown before it marks what changed since.
  await board.scrollIntoViewIfNeeded();
  await page.keyboard.press("F5");
  await expect(board.locator(".drawing-head")).toContainText("stop #3");
  await expect(caption).toContainText("4 bits changed since the stop before, outlined");
  await board
    .locator("svg rect", { has: page.locator("title", { hasText: /^value bit 2: 1$/u }) })
    .click();
  await part("m.flags");
  await showTab(board, "bitmap");
  await expect(caption).toContainText("2560 pixels changed since the stop before, outlined");
  await showTab(board, "heatmap");
  await expect(caption).toContainText("changed since the stop before");
  await showTab(board, "line-plot");
  await expect(board.locator("svg text", { hasText: "stop before" })).toHaveCount(1);
  await showTab(board, "histogram");
  await expect.poll(() => titles(board)).toContain("the stop before");

  // The chosen drawing stays chosen in another view.
  const histogram = board.getByRole("button", { name: "histogram", exact: true });
  await menu(page, "Go", "Show source");
  await page.keyboard.press("Alt+v");
  await expect(histogram).toHaveAttribute("aria-pressed", "true");
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
          const section = (button.closest("section") as HTMLElement).getBoundingClientRect();
          return {
            x: Math.round(box.x),
            y: Math.round(box.y),
            height: Math.round(box.height),
            inside: box.left >= section.left && box.right <= section.right,
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
  test("bar-chart sorts a map by value and folds the rest into Other, a donut sums its shares, and a vertical bar opens", async ({
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
    const sorted = entries.toSorted(([a, x], [b, y]) => y - x || (a < b ? -1 : a > b ? 1 : 0));
    const bars = (await titles(hits)).filter(
      (title) => !title.includes("stop before") && title.includes(": "),
    );
    const shown = sorted.slice(0, 40).map(([label, value]) => `${label}: ${value}`);
    const rest = sorted.slice(40).reduce((total, [, value]) => total + value, 0);
    expect(bars.filter((title) => shown.includes(title))).toEqual(shown);
    expect(bars).toContain(`Other: 16 more entries, total ${rest}`);
    await expect(hits.locator(".drawing-caption")).toContainText("top 40 + Other (16 more");

    // The seven largest shares and Other, summing to the whole.
    const bytes = card(page, "bytes");
    const sizes = await copyCsv(page, bytes);
    const values: number[] = [];
    for (let index = 0; sizes.has(`entries[${index}][1]`); index++) {
      values.push(Number(sizes.get(`entries[${index}][1]`)));
    }
    expect(values).toHaveLength(10);
    const total = values.reduce((a, b) => a + b, 0);
    await expect(bytes.locator("svg text", { hasText: String(total) })).toBeVisible();
    const slices = (await titles(bytes)).filter((title) => / · [\d.]+%/u.test(title));
    expect(slices).toHaveLength(8);
    expect(slices[0]).toBe(
      `text/html: 100000 · ${Number(((100000 / total) * 100).toPrecision(3))}%`,
    );
    expect(slices.at(-1)).toMatch(/^Other \(3\): /u);
    const shares = slices.map((title) => Number(/ · ([\d.]+)%/u.exec(title)?.[1]));
    expect(Math.abs(shares.reduce((a, b) => a + b, 0) - 100)).toBeLessThan(0.05 * slices.length);

    await step(page, hits, "#3");
    expect(await titles(hits)).toContain("/api/search at the stop before: 600");
    await bytes.scrollIntoViewIfNeeded();
    await expect(bytes.locator(".drawing-head")).toContainText("stop #3");
    expect((await titles(bytes)).some((title) => title.startsWith("text/html: 200000 · "))).toBe(
      true,
    );

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
    expect((await titles(folded)).toSorted()).toEqual(drawn.toSorted());

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
    await page.getByRole("menuitem", { name: /^line-plot/u }).click();
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
    await page.getByRole("menuitem", { name: /^histogram/u }).click();
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
        .filter((node) => /^\d+: node/u.test(node.key))
        .toSorted((a, b) => a.x - b.x)
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
    await page.getByRole("menuitem", { name: /^bar-chart/u }).click();
    const counts = page.getByRole("region", { name: "Drawing of counts as bar-chart" });
    // "tree", "map", "node" so far.
    await expect
      .poll(async () =>
        (await titles(counts)).filter((title) => /^\w+: \d+$/u.test(title)).toSorted(),
      )
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
