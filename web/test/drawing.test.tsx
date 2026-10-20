// The page draws a checked picture as SVG, or past 2,000 shapes on a
// canvas that it hit-tests itself, so a renderer never has to choose:
// either way a shape's title is its hover text and its select opens it,
// and an image says which of its pixels is under the pointer.

import { cleanup, render } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { page, userEvent } from "vitest/browser";
import { cache } from "../src/data";
import type { Look } from "../src/focus";
import { SessionContext } from "../src/store";
import { DrawAs, Drawings, Selected } from "../src/ui/Drawings";
import type { Focus } from "../src/ui/Workspace";
import { buildPicture } from "../src/visualize/draw";
import { closeImages, type Picture, validate } from "../src/visualize/picture";
import { type FakeServer, fakeServer } from "./fake-server";

// Cards read the tab's focus, and change its look, through these.
const focus = vi.hoisted(() => ({ current: null as unknown }));
const looked = vi.hoisted(() => [] as unknown[]);
vi.mock("../src/ui/Workspace", () => ({ useFocus: () => focus.current }));
vi.mock("../src/ui/navigation", async (original) => ({
  ...(await original<typeof import("../src/ui/navigation")>()),
  useLook: () => (change: unknown) => looked.push(change),
}));

const shown: HTMLElement[] = [];

afterEach(() => {
  for (const element of shown.splice(0)) {
    element.remove();
  }
});

function show(picture: unknown, selected: string[] = []): HTMLElement {
  const holder = buildPicture(validate(picture) as Picture, (path) => selected.push(path));
  holder.style.width = "fit-content";
  document.body.append(holder);
  shown.push(holder);
  return holder;
}

/** Moves the pointer to picture units `x`, `y` of `target`, which shows a
 * picture `width` units wide. */
function hover(target: Element, width: number, x: number, y: number, type = "mousemove"): void {
  const box = target.getBoundingClientRect();
  const scale = box.width / width;
  target.dispatchEvent(
    new MouseEvent(type, {
      bubbles: true,
      clientX: box.left + x * scale,
      clientY: box.top + y * scale,
    }),
  );
}

const tip = (holder: HTMLElement) => holder.querySelector<HTMLElement>("[role=tooltip]");

/** A 50 × 50 grid of titled, selectable cells: 2,500 shapes. */
function grid(extra: unknown[] = []) {
  const shapes: unknown[] = [];
  for (let index = 0; index < 2_500; index++) {
    shapes.push({
      type: "rect",
      x: (index % 50) * 4,
      y: Math.floor(index / 50) * 4,
      width: 4,
      height: 4,
      fill: index % 2 === 0 ? "#ff0000" : "#0000ff",
      title: `cell ${index}`,
      select: `[${index}]`,
    });
  }
  return { type: "picture", width: 300, height: 200, shapes: [...shapes, ...extra] };
}

function pixel(canvas: HTMLCanvasElement, x: number, y: number): number[] {
  const scale = canvas.width / 300;
  const data = canvas
    .getContext("2d")
    ?.getImageData(Math.floor(x * scale), Math.floor(y * scale), 1, 1).data;
  return [...(data ?? [])];
}

describe("buildPicture", () => {
  it("draws up to 2,000 shapes as SVG and more on a canvas", () => {
    const small = show({
      type: "picture",
      width: 10,
      height: 10,
      shapes: Array.from({ length: 2_000 }, () => ({
        type: "rect",
        x: 0,
        y: 0,
        width: 1,
        height: 1,
      })),
    });
    expect(small.querySelector("svg")).not.toBeNull();
    expect(small.querySelector("canvas.picture")).toBeNull();
    const large = show(grid());
    expect(large.querySelector("svg")).toBeNull();
    expect(large.querySelector("canvas.picture")).not.toBeNull();
  });

  it("paints a canvas as the SVG would, groups and all", () => {
    const holder = show(
      grid([
        // A group's fill is its shapes' fill, and it moves and scales them.
        {
          type: "group",
          x: 220,
          y: 20,
          scale: 2,
          fill: "#00ff00",
          shapes: [{ type: "rect", x: 0, y: 0, width: 10, height: 10 }],
        },
        { type: "circle", x: 260, y: 150, r: 10, fill: "#ff00ff", opacity: 0.5 },
        { type: "line", x1: 220, y1: 100, x2: 290, y2: 100, stroke: "#000000", strokeWidth: 4 },
      ]),
    );
    const canvas = holder.querySelector("canvas.picture") as HTMLCanvasElement;
    expect(pixel(canvas, 2, 2)).toEqual([255, 0, 0, 255]);
    expect(pixel(canvas, 6, 2)).toEqual([0, 0, 255, 255]);
    expect(pixel(canvas, 235, 35)).toEqual([0, 255, 0, 255]);
    expect(pixel(canvas, 245, 45)).toEqual([0, 0, 0, 0]);
    const [r, g, b, a] = pixel(canvas, 260, 150);
    expect([r, g, b]).toEqual([255, 0, 255]);
    expect(a).toBeGreaterThan(120);
    expect(a).toBeLessThan(135);
    expect(pixel(canvas, 250, 100)).toEqual([0, 0, 0, 255]);
  });

  it("frees the images of a picture it refused, however it is built", async () => {
    const rendered = await createImageBitmap(new ImageData(1, 1));
    const image = { type: "image", x: 0, y: 0, width: 1, height: 1, rendered };
    const looping: unknown[] = [{ type: "group", shapes: [] }, image];
    (looping[0] as { shapes: unknown[] }).shapes = looping;
    const refused = { type: "picture", width: 1, height: 1, shapes: looping };
    expect(() => validate(refused)).toThrow();
    closeImages(refused);
    // A closed bitmap has no pixels left.
    expect(rendered.width).toBe(0);
  });

  it("strokes a canvas's text with its own dash, not the shape's before it", () => {
    const dashes: number[][] = [];
    const strokeText = vi
      .spyOn(CanvasRenderingContext2D.prototype, "strokeText")
      .mockImplementation(function (this: CanvasRenderingContext2D) {
        dashes.push(this.getLineDash());
      });
    try {
      show(
        grid([
          { type: "line", x1: 0, y1: 0, x2: 10, y2: 0, dash: [4, 2] },
          { type: "text", x: 0, y: 20, text: "plain", stroke: "#000000" },
          { type: "text", x: 0, y: 40, text: "dotted", stroke: "#000000", dash: [1, 1] },
        ]),
      );
    } finally {
      strokeText.mockRestore();
    }
    expect(dashes).toEqual([[], [1, 1]]);
  });

  it("gives a canvas's shapes their titles on hover and opens them on click", () => {
    const selected: string[] = [];
    const holder = show(grid(), selected);
    const canvas = holder.querySelector("canvas.picture") as HTMLCanvasElement;
    // Cell 51 is the second cell of the second row.
    hover(canvas, 300, 6, 6);
    expect(tip(holder)?.hidden).toBe(false);
    expect(tip(holder)?.textContent).toBe("cell 51");
    expect(canvas.style.cursor).toBe("pointer");
    hover(canvas, 300, 6, 6, "click");
    expect(selected).toEqual(["[51]"]);
    // Nothing is under the pointer beside the grid.
    hover(canvas, 300, 250, 190);
    expect(tip(holder)?.hidden).toBe(true);
    expect(canvas.style.cursor).toBe("");
    hover(canvas, 300, 250, 190, "click");
    expect(selected).toEqual(["[51]"]);
    canvas.dispatchEvent(new MouseEvent("mouseleave"));
    expect(tip(holder)?.hidden).toBe(true);
  });

  it("names the pixel under the pointer, drawn either way, through an untitled image over it", () => {
    const image = {
      type: "image",
      x: 0,
      y: 0,
      width: 40,
      height: 20,
      title: "board",
      pixels: new Uint8ClampedArray(4 * 2 * 4).fill(255),
      columns: 4,
      rows: 2,
    };
    // Marks four times finer than the board's pixels, which say nothing.
    const overlay = {
      ...image,
      title: undefined,
      pixels: new Uint8ClampedArray(16 * 8 * 4),
      columns: 16,
      rows: 8,
    };
    const small = show({ type: "picture", width: 300, height: 200, shapes: [image, overlay] });
    const box = (small.querySelector("svg") as SVGSVGElement).getBoundingClientRect();
    const scale = box.width / 300;
    const point = { clientX: box.left + 25 * scale, clientY: box.top + 15 * scale };
    const under = document.elementFromPoint(point.clientX, point.clientY);
    under?.dispatchEvent(new MouseEvent("mousemove", { bubbles: true, ...point }));
    expect(tip(small)?.textContent).toBe("board: column 2, row 1");

    const large = show(
      grid([
        { ...image, x: 220 },
        { ...overlay, x: 220 },
      ]),
    );
    const canvas = large.querySelector("canvas.picture") as HTMLCanvasElement;
    hover(canvas, 300, 221, 1);
    expect(tip(large)?.textContent).toBe("board: column 0, row 0");
    hover(canvas, 300, 259, 19);
    expect(tip(large)?.textContent).toBe("board: column 3, row 1");
  });
});

const at = { stop: 3, thread: 1, frame: 0 };

/** The Drawings view at stop 3, with `look`, its server the test. */
async function drawings(look: Look): Promise<FakeServer> {
  const server = await fakeServer();
  focus.current = { session: "s1", state: {}, at, look, trace: undefined, stale: null } as Focus;
  render(
    <SessionContext value={server.session}>
      <Drawings />
    </SessionContext>,
  );
  return server;
}

const failed = (server: FakeServer, id: number, message: string) =>
  server.send({ type: "error", id, error: { kind: "failed", message } });

describe("a card", () => {
  afterEach(() => {
    cleanup();
    cache.clear();
    looked.splice(0);
  });

  it("says why it cannot load its renderer", async () => {
    const server = await drawings({ d: ["board~chess"] });
    server.answer(await server.next("draw"), {
      renderer: { name: "chess", origin: "built-in", digest: "d1" },
      offered: true,
      inputs: [],
      problem: null,
      bytes: 0,
    });
    failed(server, (await server.next("renderer")).id, "no renderer d1");
    await expect
      .element(page.getByRole("alert"))
      .toHaveTextContent("Cannot load renderer chess: no renderer d1");
  });

  it("says which input lies past the bytes the server sent", async () => {
    const server = await drawings({ d: ["samples~plot"] });
    server.answer(await server.next("draw"), {
      renderer: { name: "plot", origin: "built-in", digest: "d2" },
      offered: true,
      inputs: [{ name: "ys", value: { t: "numbers", kind: "f64", offset: 0, count: 4 }, path: "" }],
      problem: null,
      bytes: 0,
    });
    server.answer(await server.next("renderer"), {
      name: "plot",
      origin: "built-in",
      digest: "d2",
      source: "microscope.draw(() => null);",
    });
    await expect
      .element(page.getByRole("alert"))
      .toHaveTextContent("Cannot draw: ys: 4 f64 numbers at byte 0 lie past the 0 bytes sent");
  });

  it("says why a pinned value no longer evaluates", async () => {
    const server = await drawings({ d: ["board"] });
    failed(server, (await server.next("evaluate")).id, "no variable board here");
    await expect
      .element(page.getByRole("alert"))
      .toHaveTextContent("Cannot evaluate board: no variable board here");
  });

  it("offers renderers in a menu the arrow keys move through and Escape closes", async () => {
    const server = await fakeServer();
    focus.current = { session: "s1", state: {}, at, look: {}, trace: undefined, stale: null };
    render(
      <SessionContext value={server.session}>
        <DrawAs path="board" />
      </SessionContext>,
    );
    const button = page.getByRole("button", { name: "Draw board as…" });
    await userEvent.click(button);
    server.answer(await server.next("renderers"), {
      renderers: [
        { name: "chess", origin: "built-in", digest: "a" },
        { name: "grid", origin: "views/board.js", digest: "b" },
      ],
    });
    const item = (name: RegExp) => page.getByRole("menuitem", { name });
    await expect.element(item(/chess/)).toHaveFocus();
    await userEvent.keyboard("{ArrowDown}");
    await expect.element(item(/grid/)).toHaveFocus();
    await userEvent.keyboard("{ArrowDown}");
    await expect.element(item(/chess/)).toHaveFocus();
    await userEvent.keyboard("{Escape}");
    await expect.element(page.getByRole("menu")).not.toBeInTheDocument();
    await expect.element(button).toHaveFocus();

    // A click elsewhere closes it too.
    await userEvent.click(button);
    await expect.element(item(/chess/)).toHaveFocus();
    document.body.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    await expect.element(page.getByRole("menu")).not.toBeInTheDocument();

    await userEvent.click(button);
    await userEvent.keyboard("{ArrowUp}{Enter}");
    await expect.element(page.getByRole("menu")).not.toBeInTheDocument();
    expect(looked).toHaveLength(1);
  });

  it("closes a selected part with Escape, and gives focus back", async () => {
    const server = await fakeServer();
    focus.current = { session: "s1", state: {}, at, look: {}, trace: undefined, stale: null };
    function Part() {
      const [open, setOpen] = useState(false);
      return (
        <>
          <button type="button" onClick={() => setOpen(true)}>
            square
          </button>
          {open && <Selected at={at} path="board[3]" onClose={() => setOpen(false)} />}
        </>
      );
    }
    render(
      <SessionContext value={server.session}>
        <Part />
      </SessionContext>,
    );
    const square = page.getByRole("button", { name: "square" });
    await userEvent.click(square);
    const dialog = page.getByRole("dialog", { name: "Part board[3]" });
    await expect.element(dialog).toHaveFocus();
    await userEvent.keyboard("{Escape}");
    await expect.element(dialog).not.toBeInTheDocument();
    await expect.element(square).toHaveFocus();
  });
});
