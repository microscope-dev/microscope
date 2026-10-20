// The page refuses any picture it cannot show exactly as described, and
// names where it went wrong.

import { describe, expect, it } from "vitest";
import { isPathData, MOST_SHAPES, type Picture, validate } from "../src/visualize/picture";

const picture = (shapes: unknown[], extra: object = {}) => ({
  type: "picture",
  width: 100,
  height: 50,
  shapes,
  ...extra,
});

describe("validate", () => {
  it("accepts every kind of shape with every property", () => {
    const shapes = [
      { type: "rect", x: 0, y: 0, width: 10, height: 10, radius: 2, fill: "#abc", title: "a" },
      { type: "circle", x: 5, y: 5, r: 3, stroke: "rgb(1 2 3 / 50%)", strokeWidth: 2 },
      { type: "line", x1: 0, y1: 0, x2: 1, y2: 1, dash: [2, 2], opacity: 0.5 },
      { type: "polyline", points: new Float64Array([0, 0, 1, 1]) },
      { type: "polygon", points: [0, 0, 1, 0, 1, 1], fill: "hsl(120deg, 50%, 50%)" },
      { type: "path", d: "M0 0L10,10 h-5 a5 5 0 0 1 5 5z", select: "mailbox[21].color" },
      {
        type: "text",
        x: 0,
        y: 0,
        text: "e4",
        size: 12,
        weight: 600,
        family: "mono",
        anchor: "middle",
        baseline: "central",
      },
      {
        type: "group",
        x: 1,
        y: 2,
        scale: 2,
        rotate: 45,
        shapes: [{ type: "circle", x: 0, y: 0, r: 1, fill: "rebeccapurple" }],
      },
      {
        type: "image",
        x: 0,
        y: 0,
        width: 4,
        height: 2,
        pixels: new Uint8ClampedArray(2 * 1 * 4),
        columns: 2,
        rows: 1,
        smooth: true,
      },
    ];
    const checked: Picture = validate(picture(shapes, { caption: "Black to move" }));
    expect(checked.shapes).toHaveLength(shapes.length);
  });

  // Each is refused, with where and why.
  const refused: [string, unknown, RegExp][] = [
    ["not a picture", { type: "rect" }, /did not return microscope\.picture/],
    ["an unknown property", picture([], { onload: "x" }), /`onload` is not one of its properties/],
    [
      "a url( color",
      picture([{ type: "rect", x: 0, y: 0, width: 1, height: 1, fill: "url(#x)" }]),
      /shape 0 \(rect\): `fill` is not a color/,
    ],
    [
      "a color with a function inside",
      picture([{ type: "circle", x: 0, y: 0, r: 1, fill: "rgb(var(--x), 0, 0)" }]),
      /`fill` is not a color/,
    ],
    [
      "an unknown shape",
      picture([{ type: "foreignObject", x: 0, y: 0 }]),
      /shape 0: "foreignObject" is not a shape/,
    ],
    [
      "a script in an unknown property",
      picture([{ type: "text", x: 0, y: 0, text: "a", onclick: "alert(1)" }]),
      /`onclick` is not one of its properties/,
    ],
    [
      "NaN",
      picture([{ type: "circle", x: Number.NaN, y: 0, r: 1 }]),
      /shape 0 \(circle\): `x` must be a finite number, not NaN/,
    ],
    [
      "Infinity in points",
      picture([{ type: "polyline", points: [0, 0, 1, Number.POSITIVE_INFINITY] }]),
      /`points\[3\]` must be a finite number/,
    ],
    [
      "an odd number of coordinates",
      picture([{ type: "polygon", points: [0, 0, 1] }]),
      /x, y pairs/,
    ],
    [
      "a negative size",
      picture([{ type: "rect", x: 0, y: 0, width: -1, height: 1 }]),
      /`width` must not be negative/,
    ],
    [
      "path data with junk",
      picture([{ type: "path", d: "M0 0 L10 10 url(#x)" }]),
      /`d` is not SVG path data/,
    ],
    [
      "a missing property",
      picture([{ type: "line", x1: 0, y1: 0, x2: 1 }]),
      /shape 0 \(line\): `y2` is missing/,
    ],
    [
      "a __proto__ key",
      picture([JSON.parse('{"type":"circle","x":0,"y":0,"r":1,"__proto__":{"fill":"red"}}')]),
      /`__proto__` is not one of its properties/,
    ],
    [
      "a select that is an expression",
      picture([{ type: "rect", x: 0, y: 0, width: 1, height: 1, select: "f(x)" }]),
      /`select` must be member names and indices/,
    ],
    [
      "a select that assigns",
      picture([{ type: "rect", x: 0, y: 0, width: 1, height: 1, select: "a = 1" }]),
      /`select` must be member names and indices/,
    ],
    [
      "too much text",
      picture([{ type: "text", x: 0, y: 0, text: "x".repeat(4097) }]),
      /longer than 4096 characters/,
    ],
    [
      "too many shapes",
      picture(
        Array.from({ length: MOST_SHAPES + 1 }, () => ({ type: "circle", x: 0, y: 0, r: 1 })),
      ),
      /more than 100000 shapes/,
    ],
    [
      "groups nested too deep",
      picture([
        Array.from({ length: 40 }).reduce<object>((inner) => ({ type: "group", shapes: [inner] }), {
          type: "circle",
          x: 0,
          y: 0,
          r: 1,
        }),
      ]),
      /nest more than 32 deep/,
    ],
    [
      "pixels of the wrong size",
      picture([
        {
          type: "image",
          x: 0,
          y: 0,
          width: 1,
          height: 1,
          pixels: new Uint8ClampedArray(3),
          columns: 1,
          rows: 1,
        },
      ]),
      /holds 3 bytes, not 4 for 1×1 RGBA pixels/,
    ],
    [
      "pixels as an ordinary array",
      picture([
        {
          type: "image",
          x: 0,
          y: 0,
          width: 1,
          height: 1,
          pixels: [0, 0, 0, 0],
          columns: 1,
          rows: 1,
        },
      ]),
      /`pixels` must be a Uint8ClampedArray/,
    ],
    ["a shape that is not a plain object", picture([new Date()]), /shape 0 is not a plain object/],
    [
      "a nested shape's fault, named by its place",
      picture([
        { type: "circle", x: 0, y: 0, r: 1 },
        { type: "group", shapes: [{ type: "text", x: 0, y: 0, text: 5 }] },
      ]),
      /shape 1 \(group\) › shape 0 \(text\): `text` must be a string, not 5/,
    ],
    ["a width of 0", picture([], { width: 0 }), /the picture's width must be more than 0/],
    [
      "a height too great to draw",
      picture([], { height: 40_000 }),
      /the picture's height must be at most 32768/,
    ],
    [
      "an opacity over one",
      picture([{ type: "circle", x: 0, y: 0, r: 1, opacity: 2 }]),
      /`opacity` must be from 0 to 1/,
    ],
  ];
  for (const [name, value, message] of refused) {
    it(`refuses ${name}`, () => {
      expect(() => validate(value)).toThrow(message);
    });
  }

  it("keeps text as text, which the builder never parses", () => {
    const text = "<img src=x onerror=alert(1)>";
    const checked = validate(picture([{ type: "text", x: 0, y: 0, text }]));
    expect(checked.shapes[0]).toMatchObject({ text });
  });
});

describe("path data", () => {
  it("accepts commands with whole repetitions of their numbers", () => {
    for (const d of ["M0 0", "m1,2 3,4", "M0 0 C1 1 2 2 3 3 S4 4 5 5", "M0 0z", "M.5-1e3 1.5.5"]) {
      expect(isPathData(d), d).toBe(true);
    }
  });

  it("refuses data that does not start with a move or breaks a command", () => {
    for (const d of ["L0 0", "M0", "M0 0 L1", "M0 0 Z 1", "M0 0 X1 1", "", "M0 0;"]) {
      expect(isPathData(d), d).toBe(false);
    }
  });
});
