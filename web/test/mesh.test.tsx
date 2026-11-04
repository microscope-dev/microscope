// The built-in mesh viewer, loaded from views/visualizers/mesh.js and run
// live in the real sandbox with WebGL2, on meshes laid out as a C++ engine
// keeps them: a position, a normal, and RGBA in 28 bytes a vertex.

import { describe, expect, it } from "vitest";
import { commands } from "vitest/browser";

import type { LivePointer } from "../src/visualize/sandbox";
import { palette } from "./palette";
import { context, pixel, sandbox, Watched } from "./sandboxes";

const source = await commands.readFile("../views/visualizers/mesh.js");
const shape = { width: 240, height: 160 };

/** Vertices as an engine lays them out, and 32-bit indices. */
function mesh(
  positions: number[][],
  indices: number[] | null,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  const vertices = new DataView(new ArrayBuffer(positions.length * 28));
  positions.forEach((position, index) => {
    for (const [k, value] of position.entries()) {
      vertices.setFloat32(index * 28 + k * 4, value, true);
    }
    vertices.setFloat32(index * 28 + 20, 1, true);
    for (let k = 0; k < 4; k++) {
      vertices.setUint8(index * 28 + 24 + k, 200);
    }
  });
  const input: Record<string, unknown> = {
    vertices: new Uint8Array(vertices.buffer),
    stride: 28,
    position: 0,
    normal: 12,
    ...extra,
  };
  if (indices !== null) {
    input.indices = new Uint8Array(new Uint32Array(indices).buffer);
    input.index_size ??= 4;
  }
  return input;
}

const square = [
  [-1, -1, 0],
  [1, -1, 0],
  [1, 1, 0],
  [-1, 1, 0],
];
const squareIndices = [0, 1, 2, 0, 2, 3];

/** A session of the viewer on `input`, once it has drawn its first frame. */
async function viewer(input: unknown, size = shape) {
  const live = new Watched(sandbox(), "card", source, input, "mesh", size);
  await live.expect("started");
  const caption = await live.caption();
  live.session.frame(0);
  const bitmap = (await live.expect("frame")).bitmap as ImageBitmap;
  return { live, caption, bitmap };
}

/** The next frame, after whatever asked for it. */
async function nextFrame(live: Watched): Promise<ImageBitmap> {
  live.session.frame(16);
  for (;;) {
    const message = await live.next();
    if (message.type === "frame") {
      return message.bitmap as ImageBitmap;
    }
  }
}

/** Every pixel of a frame, to compare frames. */
function pixels(bitmap: ImageBitmap): Uint8ClampedArray {
  const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
  const draw = canvas.getContext("2d") as OffscreenCanvasRenderingContext2D;
  draw.drawImage(bitmap, 0, 0);
  return draw.getImageData(0, 0, bitmap.width, bitmap.height).data;
}

const pointer = (type: LivePointer["type"], more: Partial<LivePointer> = {}): LivePointer => ({
  type,
  x: 0,
  y: 0,
  dx: 0,
  dy: 0,
  buttons: 0,
  wheel: 0,
  shift: false,
  ctrl: false,
  alt: false,
  ...more,
});

describe("mesh", () => {
  it("draws a mesh over the page's surface and counts it", async () => {
    const { caption, bitmap } = await viewer(mesh(square, squareIndices));
    expect(caption).toBe("4 vertices · 2 triangles");
    expect([bitmap.width, bitmap.height]).toEqual([240, 160]);
    expect(pixel(bitmap, 1, 1)).toBe(palette.surface);
    expect(pixel(bitmap, 120, 80)).not.toBe(palette.surface);
  });

  it("turns on a drag, zooms on the wheel, shows wireframe on W, and keeps its camera", async () => {
    const { live, bitmap } = await viewer(mesh(square, squareIndices));
    const first = pixels(bitmap);
    // Pressing hides the hover text.
    live.session.pointer(pointer("down", { buttons: 1 }));
    expect(await live.next()).toEqual({ type: "hint", text: null });
    live.session.pointer(pointer("move", { buttons: 1, dx: 40, dy: 10 }));
    await live.expect("redraw");
    const turned = pixels(await nextFrame(live));
    expect(turned).not.toEqual(first);

    // A new stop's mesh is seen from where the camera was.
    live.session.pointer(pointer("up"));
    live.session.update(mesh(square, squareIndices), context);
    expect(await live.caption()).toBe("4 vertices · 2 triangles");
    expect(pixels(await nextFrame(live))).toEqual(turned);

    live.session.pointer(pointer("wheel", { wheel: 500 }));
    await live.expect("redraw");
    const zoomed = pixels(await nextFrame(live));
    expect(zoomed).not.toEqual(turned);

    live.session.key({ key: "w", shift: false, ctrl: false, alt: false });
    await live.expect("redraw");
    const wire = await nextFrame(live);
    expect(pixels(wire)).not.toEqual(zoomed);
    // Only the edges are drawn: the square's middle is the surface.
    expect(pixel(wire, 120, 80)).toBe(palette.surface);
  });

  it("fits a mesh whole into a tall, narrow canvas", async () => {
    const { bitmap } = await viewer(mesh(square, squareIndices), { width: 60, height: 240 });
    // The square's corners stay inside: its edge columns are the surface.
    for (let y = 0; y < 240; y += 8) {
      expect(pixel(bitmap, 0, y)).toBe(palette.surface);
      expect(pixel(bitmap, 59, y)).toBe(palette.surface);
    }
    expect(pixel(bitmap, 30, 120)).not.toBe(palette.surface);
  });

  it("names the vertex nearest the pointer", async () => {
    // The camera looks at the middle of the mesh, where vertex 0 is.
    const star = [
      [0, 0, 0],
      [1, 0, 0],
      [-1, 0, 0],
      [0, 1, 0],
      [0, -1, 0],
    ];
    const { live, caption } = await viewer(mesh(star, null, { primitive: "points" }));
    expect(caption).toBe("5 points");
    live.session.pointer(pointer("move", { x: 120, y: 80 }));
    await live.expect("redraw");
    live.session.frame(16);
    expect(await live.next()).toEqual({ type: "hint", text: "vertex 0: (0, 0, 0)" });
  });

  it("names what is wrong with a mesh and draws none of it", async () => {
    const flown = square.map((position) => [...position]);
    (flown[2] as number[])[0] = Number.NaN;
    const nan = await viewer(mesh(flown, squareIndices));
    expect(nan.caption).toBe("4 vertices · 2 triangles · not drawn: vertex 2 is at (NaN, 1, 0)");
    expect(pixel(nan.bitmap, 120, 80)).toBe(palette.surface);

    const stray = await viewer(mesh(square, [0, 1, 2, 0, 7, 3]));
    expect(stray.caption).toBe(
      "4 vertices · 2 triangles · not drawn: index 4 is 7, past the last vertex, 3",
    );
    expect(pixel(stray.bitmap, 120, 80)).toBe(palette.surface);

    // A finite position the transform carries past the largest number.
    const huge = [1e300, 0, 0, 0, 0, 1e300, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1];
    const overflown = await viewer(mesh(square, squareIndices, { transform: huge }));
    expect(overflown.caption).toBe(
      "4 vertices · 2 triangles · not drawn: vertex 0 is at (-1, -1, 0), which its transform carries to (-1e+300, -1e+300, 0), past a float's range",
    );
    expect(pixel(overflown.bitmap, 120, 80)).toBe(palette.surface);

    const torn = await viewer(mesh(square, [0, 1, 2, 0]));
    expect(torn.caption).toBe(
      "4 vertices · 1 triangles · not drawn: 4 indices are not whole triangles",
    );
  });

  it("reads 16-bit indices, lines, vertices in order, and a z-up transform", async () => {
    const shorts = mesh(square, null, {
      indices: new Uint8Array(new Uint16Array(squareIndices).buffer),
      index_size: 2,
    });
    expect((await viewer(shorts)).caption).toBe("4 vertices · 2 triangles");

    const outline = await viewer(mesh(square, [0, 1, 1, 2, 2, 3, 3, 0], { primitive: "lines" }));
    expect(outline.caption).toBe("4 vertices · 4 lines");
    expect(pixel(outline.bitmap, 120, 80)).toBe(palette.surface);

    // Without indices, the vertices are the triangles, in order.
    const shifted = [1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 5, 5, 5, 1];
    const placed = await viewer(
      mesh(square.slice(0, 3), null, { up: "z", transform: new Float32Array(shifted) }),
    );
    expect(placed.caption).toBe("3 vertices · 1 triangles");
    expect(pixel(placed.bitmap, 1, 1)).toBe(palette.surface);
  });

  it("fails on inputs that are not a mesh, saying which", async () => {
    const live = new Watched(sandbox(), "card", source, mesh(square, null, { stride: 8 }), "mesh");
    await live.expect("started");
    const failed = await live.expect("failed");
    expect(failed.failure).toMatchObject({
      kind: "failed",
      failure: { message: "RangeError: `position` at 0 does not fit in a 8-byte vertex" },
    });
  });
});
