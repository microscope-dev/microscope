// Live drawings of a C++ game's meshes and skeleton: the built-in mesh
// viewer turns a torus in WebGL2 under the pointer and keeps its camera
// across stops, names what is wrong with a broken mesh, and a custom 2-D
// renderer draws the skeleton's bones. The canvas says which session,
// stop, and pointer events its frame shows, so each check waits for the
// frame it reads.

import { mkdir, writeFile } from "node:fs/promises";
import * as path from "node:path";

import type { Locator, Page } from "@playwright/test";

import { card, drawAt } from "./drawings";
import { expect, fixture, join, type Microscope, root, test } from "./server";

const views = path.join(root, "tests/fixtures/cpp/mesh/mesh.views");

async function open(page: Page, microscope: Microscope, file = views): Promise<void> {
  const server = await microscope.start(["--views", file, fixture("mesh")]);
  await join(page, server.link);
  await drawAt(page, "mesh.cpp:124");
}

const canvasOf = (board: Locator) => board.locator("canvas.live-canvas");

/** Brings a card on screen until its canvas shows stop `stop`: the cards
 * above it grow as they draw, and may push it away again. */
async function shown(board: Locator, stop = "2"): Promise<void> {
  await expect(async () => {
    await board.scrollIntoViewIfNeeded();
    await expect(canvasOf(board)).toHaveAttribute("data-stop", stop, { timeout: 1000 });
  }).toPass();
}

/** A 32 by 20 grid of the canvas's pixels, as `#rrggbb`. The page may read
 * them: it only shows the bitmaps the renderer sends. */
function sample(canvas: Locator): Promise<string[]> {
  return canvas.evaluate((source: HTMLCanvasElement) => {
    const copy = document.createElement("canvas");
    copy.width = source.width;
    copy.height = source.height;
    const draw = copy.getContext("2d") as CanvasRenderingContext2D;
    draw.drawImage(source, 0, 0);
    const colors: string[] = [];
    for (let row = 0; row < 20; row++) {
      for (let column = 0; column < 32; column++) {
        const x = Math.floor(((column + 0.5) * source.width) / 32);
        const y = Math.floor(((row + 0.5) * source.height) / 20);
        const [r, g, b] = draw.getImageData(x, y, 1, 1).data;
        colors.push(
          `#${[r, g, b].map((part) => (part ?? 0).toString(16).padStart(2, "0")).join("")}`,
        );
      }
    }
    return colors;
  });
}

/** The share of samples that differ. */
const difference = (a: string[], b: string[]) =>
  a.filter((color, index) => color !== b[index]).length / a.length;

/** Waits until the canvas shows a frame drawn after every pointer and key
 * event sent so far. */
async function caughtUp(canvas: Locator, events?: number): Promise<void> {
  const sent = events ?? Number(await canvas.getAttribute("data-sent"));
  await expect
    .poll(async () => Number(await canvas.getAttribute("data-seen")))
    .toBeGreaterThanOrEqual(sent);
}

/** Drags from the canvas's middle by `dx`, `dy`, and waits for the frame
 * that shows the drag. */
async function drag(page: Page, canvas: Locator, dx: number, dy: number): Promise<void> {
  const [x, y] = await at(canvas, 10 * 32 + 16);
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + dx, y + dy);
  // Releasing asks for no frame; the move before it did.
  const moved = Number(await canvas.getAttribute("data-sent"));
  await page.mouse.up();
  await caughtUp(canvas, moved);
}

/** Where on the page a sample of the grid is. */
async function at(canvas: Locator, index: number): Promise<[number, number]> {
  const box = await canvas.boundingBox();
  if (box === null) {
    throw new Error("the canvas is not shown");
  }
  return [
    box.x + (((index % 32) + 0.5) * box.width) / 32,
    box.y + ((Math.floor(index / 32) + 0.5) * box.height) / 20,
  ];
}

test.describe("the built-in mesh viewer", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "headless Firefox has no WebGL");

  test("turns a torus under the pointer and keeps its camera across a step", async ({
    page,
    microscope,
  }) => {
    await open(page, microscope);
    const board = card(page, "mesh");
    const canvas = canvasOf(board);
    await expect(board.locator(".drawing-caption")).toHaveText("1,920 vertices · 3,840 triangles");
    await expect(canvas).toHaveAttribute("data-stop", "2");
    const first = await sample(canvas);
    // The torus covers much of the card's surface.
    const surface = first[0] as string;
    expect(first.filter((color) => color !== surface).length / first.length).toBeGreaterThan(0.1);

    // Hovering the torus names the vertex nearest the pointer.
    const covered = first.findIndex((color) => color !== surface);
    await page.mouse.move(...(await at(canvas, covered)));
    await expect(board.getByRole("tooltip")).toHaveText(
      /^vertex \d+: \(-?[\d.]+, -?[\d.]+, -?[\d.]+\)$/u,
    );

    // Dragging turns it.
    await drag(page, canvas, 300, 60);
    const turned = await sample(canvas);
    expect(difference(turned, first)).toBeGreaterThan(0.1);

    // The program's next stop deforms the torus, seen from where it was
    // turned to, by the same worker: debugger keys work while the canvas
    // has focus.
    await expect(canvas).toBeFocused();
    await page.keyboard.press("F5");
    await expect(canvas).toHaveAttribute("data-stop", "3");
    await expect(board.locator(".drawing-head")).toContainText("stop #3");
    const stepped = await sample(canvas);
    expect(difference(stepped, turned)).toBeLessThan(difference(stepped, first));
    await expect(canvas).toHaveAttribute("data-session", "1");

    // The wheel zooms, and W shows the wireframe without watching anything.
    let sent = Number(await canvas.getAttribute("data-sent"));
    await page.mouse.wheel(0, -600);
    await caughtUp(canvas, sent + 1);
    const zoomed = await sample(canvas);
    expect(difference(zoomed, stepped)).toBeGreaterThan(0.05);
    sent = Number(await canvas.getAttribute("data-sent"));
    await page.keyboard.press("w");
    await caughtUp(canvas, sent + 1);
    expect(difference(await sample(canvas), zoomed)).toBeGreaterThan(0.05);
    expect(new URL(page.url()).searchParams.has("w")).toBe(false);
    // Escape gives the keys back to the debugger.
    await page.keyboard.press("Escape");
    await expect(canvas).not.toBeFocused();
  });

  test("starts afresh when its card comes back on screen", async ({ page, microscope }) => {
    await open(page, microscope);
    const board = card(page, "mesh");
    const canvas = canvasOf(board);
    await expect(canvas).toHaveAttribute("data-stop", "2");
    const first = await sample(canvas);
    await drag(page, canvas, 300, 0);
    expect(difference(await sample(canvas), first)).toBeGreaterThan(0.1);

    // Away, its worker ends; back, a new one draws from the first camera.
    await card(page, "skeleton").scrollIntoViewIfNeeded();
    await expect(canvas).not.toBeInViewport();
    await board.scrollIntoViewIfNeeded();
    await expect(canvas).toHaveAttribute("data-session", "2");
    await expect(canvas).toHaveAttribute("data-stop", "2");
    expect(await sample(canvas)).toEqual(first);
  });
});

test("says WebGL is unavailable where it is", async ({ page, microscope, browserName }) => {
  test.skip(browserName !== "firefox", "only headless Firefox lacks WebGL");
  await open(page, microscope);
  const board = card(page, "mesh");
  await expect(board.getByRole("alert")).toContainText(
    /^mesh\.js:\d+:\d+: Error: WebGL is unavailable in this browser Restart$/u,
  );
  await expect(board.getByRole("button", { name: "Restart" })).toBeVisible();
});

test("a live 2-D renderer draws a skeleton, names its joints, and opens a bone", async ({
  page,
  microscope,
}) => {
  await open(page, microscope);
  const board = card(page, "skeleton");
  await shown(board);
  const canvas = canvasOf(board);
  const caption = board.locator(".drawing-caption");
  await expect(caption).toHaveText("7 bones · zoom 1.00");

  // The hips are in the middle. The canvas may be taller than its pane
  // shows, so the hover scrolls the middle into view first.
  const box = await canvas.boundingBox();
  if (box === null) {
    throw new Error("the canvas is not shown");
  }
  await canvas.hover({ position: { x: box.width / 2, y: box.height / 2 } });
  await expect(board.getByRole("tooltip")).toHaveText("hips: (0, 0)");
  await page.mouse.down();
  await page.mouse.up();
  await expect(board.getByRole("dialog", { name: "Part skeleton.bones[0]" })).toBeVisible();

  await page.mouse.wheel(0, 400);
  await expect(caption).toHaveText("7 bones · zoom 0.67");

  // A step moves the arms, in the same session.
  await caughtUp(canvas);
  const before = await sample(canvas);
  const session = await canvas.getAttribute("data-session");
  await page.keyboard.press("F5");
  await expect(canvas).toHaveAttribute("data-stop", "3");
  expect(difference(await sample(canvas), before)).toBeGreaterThan(0);
  await expect(canvas).toHaveAttribute("data-session", session as string);
});

test("a live renderer that stops answering is ended, and restarts", async ({
  page,
  microscope,
}) => {
  const directory = test.info().outputPath("views");
  await mkdir(directory, { recursive: true });
  const file = path.join(directory, "endless.views");
  await writeFile(
    file,
    'microscope-views 1\nextend c++ engine::Skeleton {\n    visualize "endless" { count = count }\n}\n',
  );
  // It draws, until a key sends it into a loop it never leaves.
  await writeFile(
    path.join(directory, "endless.js"),
    `microscope.live((canvas) => {
  const draw = canvas.getContext("2d");
  return {
    frame() {
      draw.fillStyle = "#2a78d6";
      draw.fillRect(0, 0, canvas.width, canvas.height);
    },
    key() {
      for (;;) {}
    },
  };
});
`,
  );
  await open(page, microscope, file);
  const board = card(page, "skeleton");
  await shown(board);
  const canvas = canvasOf(board);
  await canvas.focus();
  await page.keyboard.press("h");
  await expect(board.getByRole("alert")).toHaveText(
    "The renderer stopped responding for 2 s, so microscope stopped it. Restart",
  );
  await board.getByRole("button", { name: "Restart" }).click();
  await expect(board.getByRole("alert")).toHaveCount(0);
  await expect(canvas).toHaveAttribute("data-session", "2");
  await expect(canvas).toHaveAttribute("data-stop", "2");
  expect(new Set(await sample(canvas))).toEqual(new Set(["#2a78d6"]));
});
