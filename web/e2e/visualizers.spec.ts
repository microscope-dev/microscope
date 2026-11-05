// Drawing values in the page: a program's own renderer, a session's views
// file, what changed since the stop before, a drawing's parts, the link,
// failures, and reloading views.

import { copyFile, readFile, writeFile } from "node:fs/promises";
import * as path from "node:path";

import type { Page } from "@playwright/test";

import { card, drawAt } from "./drawings";
import { expect, fixture, join, type Microscope, menu, root, test } from "./server";

/** The squares a board's drawing outlines as changed since the stop before. */
function outlined(page: Page): Promise<string[]> {
  return card(page, "self")
    .locator("rect[stroke]")
    .evaluateAll((rects) =>
      rects.map((rect) => rect.querySelector("title")?.textContent?.split(":")[0] ?? ""),
    );
}

test.describe("a program's own renderer", () => {
  test.use({ program: [fixture("chess")] });

  test("draws a board, marks what moved, and opens a square's part", async ({
    page,
    microscope,
  }) => {
    await join(page, microscope.link);
    await drawAt(page, "main.rs:108");
    const board = card(page, "self");
    await expect(page.getByRole("tab", { name: /Drawings\s*1/u })).toBeVisible();
    // Pieces are named by their squares' titles.
    await expect(board.getByRole("button", { name: "e1: White King" })).toBeVisible();
    await expect(board.getByRole("button", { name: "d8: Black Queen" })).toBeVisible();
    await expect(board.getByRole("button", { name: "e4", exact: true })).toBeVisible();
    await expect(board.locator(".drawing-caption")).toHaveText("White to move");
    await expect(board).toContainText("chess.views[1]");
    expect(await outlined(page)).toEqual([]);

    // The next move: e2e4, which the board marks against the stop before.
    await page.keyboard.press("F5");
    await expect(board).toContainText("stop #3");
    await expect(board.getByRole("button", { name: "e4: White Pawn" })).toBeVisible();
    await expect(board.locator(".drawing-caption")).toHaveText("Black to move");
    await expect.poll(() => outlined(page)).toEqual(["e2", "e4"]);

    // A square opens its part of the value, which can be watched.
    await board.getByRole("button", { name: "e4: White Pawn" }).click();
    const part = page.getByRole("dialog", { name: "Part self.mailbox[28]" });
    await expect(part).toContainText("Some");
    await part.getByRole("button", { name: "Watch", exact: true }).click();
    await expect(page).toHaveURL(/[?&]w=self.mailbox%5B28%5D/u);
    await part.getByRole("button", { name: "Close" }).click();
    await expect(part).toBeHidden();

    // The link shows the same drawing.
    await page.reload();
    await expect(card(page, "self").getByRole("button", { name: "e4: White Pawn" })).toBeVisible();
  });

  test("Draw as… pins any value to any renderer, which says why it cannot draw it", async ({
    page,
    microscope,
  }) => {
    await join(page, microscope.link);
    await drawAt(page, "main.rs:108");
    const add = page.getByRole("textbox", { name: "Add a watch", exact: true });
    await add.fill("self.fullmove");
    await add.press("Enter");
    await page.getByRole("button", { name: "Draw self.fullmove as…" }).first().click();
    await page.getByRole("menuitem", { name: /^chess-board/u }).click();
    await expect(page).toHaveURL(/[?&]d=self.fullmove~chess-board/u);
    // The board's renderer reads squares the number does not have.
    const pinned = page.getByRole("region", { name: "Drawing of self.fullmove as chess-board" });
    await expect(pinned.getByRole("alert")).toContainText(/chess-board\.js:\d+:\d+: TypeError/u);
    // Removing it unpins it.
    await pinned.getByRole("button", { name: "Stop drawing self.fullmove" }).click();
    await expect(pinned).toBeHidden();
    await expect(page).not.toHaveURL(/[?&]d=/u);
  });
});

/** Starts microscope on life with `views`, and joins it. */
async function life(page: Page, microscope: Microscope, views: string): Promise<Microscope> {
  const server = await microscope.start(["--views", views, fixture("life")]);
  await join(page, server.link);
  return server;
}

/** How many of a life board's cells are alive, and how many were just born,
 * from the pixels the page shows. */
function cells(page: Page): Promise<{ alive: number; born: number }> {
  return card(page, "life")
    .locator("canvas")
    .evaluate((canvas: HTMLCanvasElement) => {
      const context = canvas.getContext("2d") as CanvasRenderingContext2D;
      const { data } = context.getImageData(0, 0, canvas.width, canvas.height);
      let alive = 0;
      let born = 0;
      for (let at = 0; at < data.length; at += 4) {
        if (data[at] === 26) {
          alive += 1;
        } else if (data[at] === 176) {
          alive += 1;
          born += 1;
        }
      }
      return { alive, born };
    });
}

test.describe("a session's views file", () => {
  test("draws memory read at once, as pixels, at each stop", async ({ page, microscope }) => {
    await life(page, microscope, path.join(root, "tests/fixtures/c/life/life.views"));
    await drawAt(page, "life.c:59");
    const board = card(page, "life");
    await expect(board.locator(".drawing-caption")).toHaveText("generation 1");
    await expect(board).toContainText("life.js");
    // A glider and a blinker: eight cells, none born before the first stop.
    expect(await cells(page)).toEqual({ alive: 8, born: 0 });

    await page.keyboard.press("F5");
    await expect(board.locator(".drawing-caption")).toHaveText("generation 2");
    const second = await cells(page);
    expect(second.alive).toBe(8);
    expect(second.born).toBeGreaterThan(0);

    // Drawing a value the frame already draws pins that card, which can
    // then stop being pinned.
    await page.getByRole("button", { name: "Draw life", exact: true }).first().click();
    await expect(page).toHaveURL(/[?&]d=life(&|$)/u);
    await board.getByRole("button", { name: "Stop drawing life" }).click();
    await expect(page).not.toHaveURL(/[?&]d=/u);
    await expect(board).toBeVisible();
  });

  test("a card lists its inputs, copies them, and names the pixel under the pointer", async ({
    page,
    microscope,
  }) => {
    await page.addInitScript(() => {
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: {
          writeText: (text: string) => {
            (window as unknown as { copied: string }).copied = text;
            return Promise.resolve();
          },
        },
      });
    });
    await life(page, microscope, path.join(root, "tests/fixtures/c/life/life.views"));
    await drawAt(page, "life.c:59");
    const board = card(page, "life");
    await expect(board.locator(".drawing-caption")).toHaveText("generation 1");

    // Each of the 3,072 cells' bytes, then the other inputs, a row each.
    await board.getByRole("button", { name: "Table" }).click();
    const table = board.getByRole("table", { name: "Inputs of life" });
    await expect(table).toHaveAttribute("aria-rowcount", "3075");
    await expect(table.getByRole("row").nth(1)).toHaveText("cells[0]0");
    await table.evaluate((element) => {
      const scroller = element.parentElement as HTMLElement;
      scroller.scrollTop = scroller.scrollHeight;
    });
    await expect(table.getByRole("row").last()).toHaveText("generation1");
    await expect(table.getByRole("row", { name: "columns 64" })).toBeVisible();

    await board.getByRole("button", { name: "Copy CSV" }).click();
    await expect(page.getByText("Copied 3074 values as CSV")).toBeVisible();
    const copied = await page.evaluate(() => (window as unknown as { copied: string }).copied);
    const lines = copied.trimEnd().split("\n");
    expect(lines).toHaveLength(3075);
    expect(lines.slice(0, 2)).toEqual(["path,value", "cells[0],0"]);
    expect(lines.slice(-2)).toEqual(["columns,64", "generation,1"]);
    // The blinker's middle cell, alive at every generation.
    expect(lines[1 + 20 * 64 + 31]).toBe("cells[1311],1");

    // Each cell is 6 units square.
    const image = board.locator("canvas");
    const box = await image.boundingBox();
    if (box === null) {
      throw new Error("the board is not on screen");
    }
    const scale = box.width / 384;
    await page.mouse.move(box.x + (31 * 6 + 3) * scale, box.y + (20 * 6 + 3) * scale);
    await expect(board.getByRole("tooltip")).toHaveText("cells: column 31, row 20");
  });

  test("a picture of thousands of shapes is drawn on a canvas that still names and opens them", async ({
    page,
    microscope,
  }) => {
    const directory = test.info().outputPath("views");
    const { mkdir } = await import("node:fs/promises");
    await mkdir(directory, { recursive: true });
    const views = path.join(directory, "cells.views");
    await writeFile(
      views,
      `microscope-views 1
extend c life {
    visualize "cells" {
        cells = bytes(&cells[0][0], sizeof(cells))
        columns = 64
    }
}
`,
    );
    // A rect a cell: 3,072 shapes, each titled and selectable.
    await writeFile(
      path.join(directory, "cells.js"),
      `microscope.draw(({ cells, columns }) => microscope.picture({
  width: columns * 6, height: (cells.length / columns) * 6,
  shapes: Array.from(cells, (alive, index) => {
    const column = index % columns, row = Math.floor(index / columns);
    return microscope.rect({
      x: column * 6, y: row * 6, width: 6, height: 6,
      fill: alive ? microscope.theme.ink : microscope.theme.surface,
      title: \`\${column},\${row}: \${alive ? "alive" : "dead"}\`,
      select: \`cells[\${row}][\${column}]\`,
    });
  }),
}));
`,
    );
    await life(page, microscope, views);
    await drawAt(page, "life.c:59");
    const board = card(page, "life");
    const canvas = board.locator("canvas.picture");
    await expect(canvas).toBeVisible();
    await expect(board.locator(".drawing-body svg")).toHaveCount(0);
    const box = await canvas.boundingBox();
    if (box === null) {
      throw new Error("the board is not on screen");
    }
    const scale = box.width / 384;
    const cell = (column: number, row: number) =>
      [box.x + (column * 6 + 3) * scale, box.y + (row * 6 + 3) * scale] as const;
    await page.mouse.move(...cell(31, 20));
    await expect(board.getByRole("tooltip")).toHaveText("31,20: alive");
    await page.mouse.move(...cell(5, 40));
    await expect(board.getByRole("tooltip")).toHaveText("5,40: dead");
    await page.mouse.click(...cell(31, 20));
    const part = page.getByRole("dialog", { name: "Part life.cells[20][31]" });
    await expect(part).toContainText("1");
  });

  test("a new session's first drawing has nothing before it", async ({ page, microscope }) => {
    await life(page, microscope, path.join(root, "tests/fixtures/c/life/life.views"));
    await drawAt(page, "life.c:59");
    await page.keyboard.press("F5");
    await expect(card(page, "life").locator(".drawing-caption")).toHaveText("generation 2");

    // The same program again, whose first board differs from the last one
    // drawn before it, at an earlier stop.
    await menu(page, "File", "Launch a program…");
    const program = page.getByRole("combobox", { name: "Program" });
    await program.fill(fixture("life"));
    await page.keyboard.press("Escape");
    await page.getByRole("button", { name: "Load", exact: true }).click();
    await page.getByRole("button", { name: /End it and continue/u }).click();
    await expect(page.getByTestId("status")).toContainText("Not started");
    const adder = page.getByRole("textbox", { name: "Add a breakpoint" });
    await adder.fill("life.c:59");
    await adder.press("Enter");
    await adder.press("Escape");
    await page.keyboard.press("F5");
    await expect(page.getByTestId("stops")).toContainText("#4 breakpoint");
    await page.keyboard.press("Alt+v");
    await expect(card(page, "life").locator(".drawing-caption")).toHaveText("generation 1");
    expect(await cells(page)).toEqual({ alive: 8, born: 0 });
  });

  test("a renderer is told the width its card offers, from its first drawing and when it changes", async ({
    page,
    microscope,
  }) => {
    const directory = test.info().outputPath("views");
    const { mkdir } = await import("node:fs/promises");
    await mkdir(directory, { recursive: true });
    const views = path.join(directory, "width.views");
    await writeFile(
      views,
      'microscope-views 1\nextend c life {\n    visualize "width" { generation = generation }\n}\n',
    );
    await writeFile(
      path.join(directory, "width.js"),
      "microscope.draw((_, { width }) => microscope.picture({ width: 10, height: 10, shapes: [], caption: String(width) }));\n",
    );
    await life(page, microscope, views);
    await drawAt(page, "life.c:59");
    const board = card(page, "life");
    const caption = board.locator(".drawing-caption");
    await expect(caption).toHaveText(/^\d+$/u);
    const offered = () =>
      board.locator(".drawing-body").evaluate((body: HTMLElement) => {
        const style = getComputedStyle(body);
        const padding =
          // oxlint-disable-next-line unicorn/prefer-number-coercion -- the values end in "px", which Number() turns into NaN
          Number.parseFloat(style.paddingLeft) + Number.parseFloat(style.paddingRight);
        return Math.floor(body.clientWidth - padding);
      });
    expect(Number(await caption.textContent())).toBe(await offered());

    // A narrower page draws again at the card's new width.
    const wide = await offered();
    await page.setViewportSize({ width: 1024, height: 720 });
    await expect.poll(offered).toBeLessThan(wide);
    await expect(caption).toHaveText(String(await offered()));
  });

  test("a card draws again in the colors of the system's new scheme", async ({
    page,
    microscope,
  }) => {
    const directory = test.info().outputPath("views");
    const { mkdir } = await import("node:fs/promises");
    await mkdir(directory, { recursive: true });
    const views = path.join(directory, "scheme.views");
    await writeFile(
      views,
      'microscope-views 1\nextend c life {\n    visualize "scheme" { generation = generation }\n}\n',
    );
    await writeFile(
      path.join(directory, "scheme.js"),
      "microscope.draw((_, { theme }) => microscope.picture({ width: 10, height: 10, shapes: [], caption: theme + ' ' + microscope.theme.ink }));\n",
    );
    await page.emulateMedia({ colorScheme: "light" });
    await life(page, microscope, views);
    await drawAt(page, "life.c:59");
    const caption = card(page, "life").locator(".drawing-caption");
    const ink = () =>
      page.evaluate(() =>
        getComputedStyle(document.documentElement).getPropertyValue("--ink").trim(),
      );
    await expect(caption).toHaveText(`light ${await ink()}`);
    await page.emulateMedia({ colorScheme: "dark" });
    await expect(caption).toHaveText(`dark ${await ink()}`);
  });

  test("a pointer draws as what it points to", async ({ page, microscope }) => {
    await life(page, microscope, path.join(root, "tests/fixtures/c/life/life.views"));
    await drawAt(page, "step");
    await expect(card(page, "life").locator(".drawing-caption")).toHaveText("generation 0");
  });

  test("reloading views draws with the renderer as it is now", async ({ page, microscope }) => {
    const directory = test.info().outputPath("views");
    const { mkdir } = await import("node:fs/promises");
    await mkdir(directory, { recursive: true });
    const views = path.join(directory, "life.views");
    const renderer = path.join(directory, "life.js");
    await copyFile(path.join(root, "tests/fixtures/c/life/life.views"), views);
    await copyFile(path.join(root, "tests/fixtures/c/life/life.js"), renderer);
    await life(page, microscope, views);
    await drawAt(page, "life.c:59");
    const board = card(page, "life");
    await expect(board.locator(".drawing-caption")).toHaveText("generation 1");

    const source = await readFile(renderer, "utf8");
    await writeFile(renderer, source.replace("caption: `generation", "caption: `gen"));
    await page.keyboard.press("Control+k");
    await page.keyboard.type("Reload views");
    await page.keyboard.press("Enter");
    await expect(board.locator(".drawing-caption")).toHaveText("gen 1");
  });

  test("a card says why it cannot draw, and the page stays responsive", async ({
    page,
    microscope,
  }) => {
    const directory = test.info().outputPath("views");
    const { mkdir } = await import("node:fs/promises");
    await mkdir(directory, { recursive: true });
    const views = path.join(directory, "failing.views");
    await writeFile(
      views,
      `microscope-views 1
extend c life {
    visualize "throws" { generation = generation }
    visualize "forever" { generation = generation }
    visualize "invalid" { generation = generation }
    visualize "bitmap" {
        pixels = bytes(0x10, 64)
        columns = 8
    }
}
`,
    );
    await writeFile(
      path.join(directory, "throws.js"),
      "microscope.draw(({ generation }) => {\n  throw new RangeError(`no generation ${generation}`);\n});\n",
    );
    await writeFile(
      path.join(directory, "forever.js"),
      "microscope.draw(() => { for (;;) {} });\n",
    );
    await writeFile(
      path.join(directory, "invalid.js"),
      `microscope.draw(() => microscope.picture({ width: 10, height: 10, shapes: [
  microscope.rect({ x: 0, y: 0, width: 10, height: 10, fill: "url(https://example.com/x)" }),
] }));
`,
    );
    await life(page, microscope, views);
    await drawAt(page, "life.c:59");
    const board = card(page, "life");
    const tab = (name: string) => board.getByRole("button", { name, exact: true });

    await tab("throws").click();
    await expect(board.getByRole("alert")).toHaveText("throws.js:2:9: RangeError: no generation 1");

    await tab("invalid").click();
    await expect(board.getByRole("alert")).toContainText(
      "invalid.js returned a picture the page cannot show: shape 0 (rect): `fill` is not a color",
    );

    await tab("bitmap").click();
    await expect(board.getByRole("alert")).toContainText("Cannot draw: `pixels`:");
    await expect(board.getByRole("alert")).toContainText("0x10");

    // A renderer that never returns is stopped, and the page answers meanwhile.
    await tab("forever").click();
    await expect(board.getByRole("alert")).toHaveText(
      "The renderer took longer than 2 s, so it was stopped.",
    );
    await page.keyboard.press("Alt+s");
    await expect(page).not.toHaveURL(/view=drawings/u);

    // A renderer nobody provides is named.
    const url = new URL(page.url());
    url.searchParams.set("view", "drawings");
    url.searchParams.append("d", "life~nowhere");
    await page.goto(url.href);
    const nowhere = page.getByRole("region", { name: "Drawing of life as nowhere" });
    await nowhere.scrollIntoViewIfNeeded();
    await expect(nowhere.getByRole("alert")).toContainText("nowhere");
  });
});
