// What a renderer returns, checked before the page shows any of it. A
// picture is refused whole, naming the first shape at fault and why, and
// never cleaned: unknown properties, numbers that are not finite, colors
// that are not colors, path data outside SVG's grammar, and pictures over
// the limits are all refused.

/** The most shapes a picture holds, groups included. */
export const MOST_SHAPES = 100_000;
/** How deeply groups nest. */
const MOST_DEPTH = 32;
/** The widest or tallest a picture is, in its own units. */
const MOST_SIDE = 32_768;
/** The longest text, title, or caption, in characters. */
export const MOST_TEXT = 4_096;
/** The most pixels a picture's images hold, together. */
export const MOST_PIXELS = 16 * 1024 * 1024;
/** The most coordinates polylines and polygons hold, together. */
const MOST_POINTS = 4_000_000;
/** The longest path data, in characters. */
const MOST_PATH = 4 * 1024 * 1024;

export interface Style {
  fill?: string;
  stroke?: string;
  strokeWidth?: number;
  opacity?: number;
  dash?: number[];
  title?: string;
  select?: string;
}

export type Shape =
  | (Style & { type: "rect"; x: number; y: number; width: number; height: number; radius?: number })
  | (Style & { type: "circle"; x: number; y: number; r: number })
  | (Style & { type: "line"; x1: number; y1: number; x2: number; y2: number })
  | (Style & { type: "polyline" | "polygon"; points: ArrayLike<number> })
  | (Style & { type: "path"; d: string })
  | (Style & {
      type: "text";
      x: number;
      y: number;
      text: string;
      size?: number;
      weight?: "normal" | "bold" | number;
      family?: "sans" | "mono";
      anchor?: "start" | "middle" | "end";
      baseline?: Baseline;
    })
  | (Style & {
      type: "group";
      shapes: Shape[];
      x?: number;
      y?: number;
      scale?: number;
      rotate?: number;
    })
  | (Style & { type: "image"; x: number; y: number; width: number; height: number } & (
        | {
            pixels: Uint8Array | Uint8ClampedArray;
            columns: number;
            rows: number;
            smooth?: boolean;
          }
        | { rendered: ImageBitmap; smooth?: boolean }
      ));

type Baseline = "auto" | "alphabetic" | "middle" | "central" | "hanging" | "ideographic";

export interface Picture {
  type: "picture";
  width: number;
  height: number;
  shapes: Shape[];
  caption?: string;
}

/** Why a picture was refused, naming where. */
export class PictureError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "PictureError";
  }
}

const STYLE = ["fill", "stroke", "strokeWidth", "opacity", "dash", "title", "select"];

const PROPERTIES: Record<Shape["type"], { required: string[]; optional: string[] }> = {
  rect: { required: ["x", "y", "width", "height"], optional: ["radius"] },
  circle: { required: ["x", "y", "r"], optional: [] },
  line: { required: ["x1", "y1", "x2", "y2"], optional: [] },
  polyline: { required: ["points"], optional: [] },
  polygon: { required: ["points"], optional: [] },
  path: { required: ["d"], optional: [] },
  text: {
    required: ["x", "y", "text"],
    optional: ["size", "weight", "family", "anchor", "baseline"],
  },
  group: { required: ["shapes"], optional: ["x", "y", "scale", "rotate"] },
  image: {
    required: ["x", "y", "width", "height"],
    optional: ["pixels", "columns", "rows", "rendered", "smooth"],
  },
};

const BASELINES = new Set(["auto", "alphabetic", "middle", "central", "hanging", "ideographic"]);

// CSS's named colors.
const NAMED = new Set(
  (
    "aliceblue antiquewhite aqua aquamarine azure beige bisque black blanchedalmond blue " +
    "blueviolet brown burlywood cadetblue chartreuse chocolate coral cornflowerblue cornsilk " +
    "crimson cyan darkblue darkcyan darkgoldenrod darkgray darkgreen darkgrey darkkhaki " +
    "darkmagenta darkolivegreen darkorange darkorchid darkred darksalmon darkseagreen " +
    "darkslateblue darkslategray darkslategrey darkturquoise darkviolet deeppink deepskyblue " +
    "dimgray dimgrey dodgerblue firebrick floralwhite forestgreen fuchsia gainsboro ghostwhite " +
    "gold goldenrod gray green greenyellow grey honeydew hotpink indianred indigo ivory khaki " +
    "lavender lavenderblush lawngreen lemonchiffon lightblue lightcoral lightcyan " +
    "lightgoldenrodyellow lightgray lightgreen lightgrey lightpink lightsalmon lightseagreen " +
    "lightskyblue lightslategray lightslategrey lightsteelblue lightyellow lime limegreen linen " +
    "magenta maroon mediumaquamarine mediumblue mediumorchid mediumpurple mediumseagreen " +
    "mediumslateblue mediumspringgreen mediumturquoise mediumvioletred midnightblue mintcream " +
    "mistyrose moccasin navajowhite navy oldlace olive olivedrab orange orangered orchid " +
    "palegoldenrod palegreen paleturquoise palevioletred papayawhip peachpuff peru pink plum " +
    "powderblue purple rebeccapurple red rosybrown royalblue saddlebrown salmon sandybrown " +
    "seagreen seashell sienna silver skyblue slateblue slategray slategrey snow springgreen " +
    "steelblue tan teal thistle tomato turquoise violet wheat white whitesmoke yellow " +
    "yellowgreen none transparent"
  ).split(" "),
);

const NUMBER = String.raw`[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?%?`;
const FUNCTION = new RegExp(
  String.raw`^(?:rgba?|hsla?)\(\s*${NUMBER}(?:deg)?(?:\s*[,\s]\s*${NUMBER}){2}(?:\s*[,/]\s*${NUMBER})?\s*\)$`,
  "i",
);

/** Whether `value` is a color the page shows: hex, rgb(), hsl(), or a name. */
function isColor(value: unknown): value is string {
  if (typeof value !== "string" || value.length > 64) {
    return false;
  }
  return (
    /^#(?:[0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(value) ||
    FUNCTION.test(value) ||
    NAMED.has(value.toLowerCase())
  );
}

/** A path into the drawn value: member names, `.`, and integer indices. */
export function isSelect(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length <= 1024 &&
    /^(?:[A-Za-z_][A-Za-z0-9_]*|\[\d+\])(?:\.[A-Za-z_][A-Za-z0-9_]*|\[\d+\])*$/.test(value)
  );
}

// Each command and how many numbers each of its repetitions takes.
const ARGUMENTS: Record<string, number> = {
  m: 2,
  l: 2,
  h: 1,
  v: 1,
  c: 6,
  s: 4,
  q: 4,
  t: 2,
  a: 7,
  z: 0,
};

/** Whether `d` is SVG path data: commands, each with whole repetitions of
 * its numbers, starting with a move. */
export function isPathData(d: unknown): d is string {
  if (typeof d !== "string" || d.length > MOST_PATH) {
    return false;
  }
  const token = /\s*,?\s*(?:([MmLlHhVvCcSsQqTtAaZz])|([+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?))/y;
  let command: string | null = null;
  let count = 0;
  let at = 0;
  const whole = () =>
    command === null ||
    ARGUMENTS[command] === 0 ||
    (count > 0 && count % (ARGUMENTS[command] ?? 1) === 0);
  while (at < d.length) {
    token.lastIndex = at;
    const found = token.exec(d);
    if (found === null) {
      return /^\s*$/.test(d.slice(at));
    }
    at = token.lastIndex;
    if (found[1] !== undefined) {
      if (!whole() || (command === null && found[1].toLowerCase() !== "m")) {
        return false;
      }
      command = found[1].toLowerCase();
      count = 0;
    } else if (command === null || command === "z") {
      return false;
    } else {
      count += 1;
    }
  }
  return command !== null && whole();
}

interface Totals {
  shapes: number;
  pixels: number;
  points: number;
}

/** Checks what a renderer returned, and returns it as a picture. */
export function validate(value: unknown): Picture {
  const where = "the picture";
  const picture = object(value, where);
  if (picture.type !== "picture") {
    throw new PictureError("the renderer did not return microscope.picture(…)");
  }
  only(picture, ["type", "width", "height", "shapes", "caption"], where);
  side(picture.width, `${where}'s width`);
  side(picture.height, `${where}'s height`);
  if (picture.caption !== undefined) {
    text(picture.caption, `${where}'s caption`);
  }
  const totals: Totals = { shapes: 0, pixels: 0, points: 0 };
  shapes(picture.shapes, "", 1, totals);
  return picture as unknown as Picture;
}

function shapes(value: unknown, within: string, depth: number, totals: Totals): void {
  if (!Array.isArray(value)) {
    throw new PictureError(`${within ? `${within}: ` : ""}shapes must be an array`);
  }
  if (depth > MOST_DEPTH) {
    throw new PictureError(`${within}: groups nest more than ${MOST_DEPTH} deep`);
  }
  value.forEach((item, index) => {
    totals.shapes += 1;
    if (totals.shapes > MOST_SHAPES) {
      throw new PictureError(`the picture has more than ${MOST_SHAPES} shapes`);
    }
    shape(item, `${within ? `${within} › ` : ""}shape ${index}`, depth, totals);
  });
}

function shape(value: unknown, at: string, depth: number, totals: Totals): void {
  const item = object(value, at);
  const type = item.type;
  if (typeof type !== "string" || !Object.hasOwn(PROPERTIES, type)) {
    throw new PictureError(`${at}: ${JSON.stringify(String(type))} is not a shape`);
  }
  const where = `${at} (${type})`;
  const { required, optional } = PROPERTIES[type as Shape["type"]];
  only(item, ["type", ...required, ...optional, ...STYLE], where);
  for (const name of required) {
    if (item[name] === undefined) {
      throw new PictureError(`${where}: \`${name}\` is missing`);
    }
  }
  style(item, where);
  switch (type) {
    case "rect":
      numbers(item, ["x", "y"], where);
      sizes(item, ["width", "height", "radius"], where);
      break;
    case "circle":
      numbers(item, ["x", "y"], where);
      sizes(item, ["r"], where);
      break;
    case "line":
      numbers(item, ["x1", "y1", "x2", "y2"], where);
      break;
    case "polyline":
    case "polygon":
      points(item.points, where, totals);
      break;
    case "path":
      if (!isPathData(item.d)) {
        throw new PictureError(`${where}: \`d\` is not SVG path data`);
      }
      break;
    case "text":
      numbers(item, ["x", "y"], where);
      text(item.text, `${where}: \`text\``);
      typography(item, where);
      break;
    case "group":
      numbers(item, ["x", "y", "rotate"], where);
      sizes(item, ["scale"], where);
      shapes(item.shapes, where, depth + 1, totals);
      break;
    case "image":
      numbers(item, ["x", "y"], where);
      sizes(item, ["width", "height"], where);
      image(item, where, totals);
      break;
  }
}

function object(value: unknown, where: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new PictureError(`${where} is not an object`);
  }
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== Object.prototype && prototype !== null) {
    throw new PictureError(`${where} is not a plain object`);
  }
  return value as Record<string, unknown>;
}

function only(item: Record<string, unknown>, allowed: string[], where: string): void {
  for (const name of Object.keys(item)) {
    if (!allowed.includes(name)) {
      throw new PictureError(`${where}: \`${name}\` is not one of its properties`);
    }
  }
}

function finite(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new PictureError(`${what} must be a finite number, not ${describe(value)}`);
  }
  return value;
}

function size(value: unknown, what: string): void {
  if (finite(value, what) < 0) {
    throw new PictureError(`${what} must not be negative`);
  }
}

/** A picture's width or height: none would scale it without end, and past
 * MOST_SIDE its canvas would be too large to draw. */
function side(value: unknown, what: string): void {
  const length = finite(value, what);
  if (length <= 0) {
    throw new PictureError(`${what} must be more than 0`);
  }
  if (length > MOST_SIDE) {
    throw new PictureError(`${what} must be at most ${MOST_SIDE}`);
  }
}

function numbers(item: Record<string, unknown>, names: string[], where: string): void {
  for (const name of names) {
    if (item[name] !== undefined) {
      finite(item[name], `${where}: \`${name}\``);
    }
  }
}

function sizes(item: Record<string, unknown>, names: string[], where: string): void {
  for (const name of names) {
    if (item[name] !== undefined) {
      size(item[name], `${where}: \`${name}\``);
    }
  }
}

function text(value: unknown, what: string): void {
  if (typeof value !== "string") {
    throw new PictureError(`${what} must be a string, not ${describe(value)}`);
  }
  if (value.length > MOST_TEXT) {
    throw new PictureError(`${what} is longer than ${MOST_TEXT} characters`);
  }
}

function style(item: Record<string, unknown>, where: string): void {
  for (const name of ["fill", "stroke"]) {
    if (item[name] !== undefined && !isColor(item[name])) {
      throw new PictureError(`${where}: \`${name}\` is not a color: ${describe(item[name])}`);
    }
  }
  sizes(item, ["strokeWidth"], where);
  if (item.opacity !== undefined) {
    const opacity = finite(item.opacity, `${where}: \`opacity\``);
    if (opacity > 1) {
      throw new PictureError(`${where}: \`opacity\` must be from 0 to 1`);
    }
    size(opacity, `${where}: \`opacity\``);
  }
  if (item.dash !== undefined) {
    if (!Array.isArray(item.dash) || item.dash.length > 32) {
      throw new PictureError(`${where}: \`dash\` must be an array of at most 32 lengths`);
    }
    item.dash.forEach((length, index) => {
      size(length, `${where}: \`dash[${index}]\``);
    });
  }
  if (item.title !== undefined) {
    text(item.title, `${where}: \`title\``);
  }
  if (item.select !== undefined && !isSelect(item.select)) {
    throw new PictureError(
      `${where}: \`select\` must be member names and indices, such as \`a.b[2]\`, not ${describe(item.select)}`,
    );
  }
}

function typography(item: Record<string, unknown>, where: string): void {
  if (item.size !== undefined && finite(item.size, `${where}: \`size\``) <= 0) {
    throw new PictureError(`${where}: \`size\` must be positive`);
  }
  const weight = item.weight;
  if (
    weight !== undefined &&
    weight !== "normal" &&
    weight !== "bold" &&
    !(typeof weight === "number" && Number.isInteger(weight) && weight >= 100 && weight <= 900)
  ) {
    throw new PictureError(`${where}: \`weight\` must be "normal", "bold", or 100 to 900`);
  }
  if (item.family !== undefined && item.family !== "sans" && item.family !== "mono") {
    throw new PictureError(`${where}: \`family\` must be "sans" or "mono"`);
  }
  if (
    item.anchor !== undefined &&
    item.anchor !== "start" &&
    item.anchor !== "middle" &&
    item.anchor !== "end"
  ) {
    throw new PictureError(`${where}: \`anchor\` must be "start", "middle", or "end"`);
  }
  if (item.baseline !== undefined && !BASELINES.has(item.baseline as string)) {
    throw new PictureError(`${where}: \`baseline\` must be one of ${[...BASELINES].join(", ")}`);
  }
}

function points(value: unknown, where: string, totals: Totals): void {
  const list =
    Array.isArray(value) || value instanceof Float32Array || value instanceof Float64Array
      ? (value as ArrayLike<unknown>)
      : null;
  if (list === null || list.length % 2 !== 0) {
    throw new PictureError(`${where}: \`points\` must be an array of x, y pairs`);
  }
  totals.points += list.length;
  if (totals.points > MOST_POINTS) {
    throw new PictureError(`the picture has more than ${MOST_POINTS / 2} points`);
  }
  for (let index = 0; index < list.length; index++) {
    const coordinate = list[index];
    if (typeof coordinate !== "number" || !Number.isFinite(coordinate)) {
      throw new PictureError(
        `${where}: \`points[${index}]\` must be a finite number, not ${describe(coordinate)}`,
      );
    }
  }
}

function image(item: Record<string, unknown>, where: string, totals: Totals): void {
  if (item.smooth !== undefined && typeof item.smooth !== "boolean") {
    throw new PictureError(`${where}: \`smooth\` must be true or false`);
  }
  if (item.rendered !== undefined) {
    if (item.pixels !== undefined || item.columns !== undefined || item.rows !== undefined) {
      throw new PictureError(`${where}: an image has either a canvas or pixels`);
    }
    if (typeof ImageBitmap === "undefined" || !(item.rendered instanceof ImageBitmap)) {
      throw new PictureError(`${where}: \`rendered\` is not an image`);
    }
    totals.pixels += item.rendered.width * item.rendered.height;
  } else {
    const { pixels, columns, rows } = item;
    for (const [name, count] of [
      ["columns", columns],
      ["rows", rows],
    ] as const) {
      if (typeof count !== "number" || !Number.isInteger(count) || count < 1) {
        throw new PictureError(`${where}: \`${name}\` must be a positive whole number`);
      }
    }
    if (!(pixels instanceof Uint8ClampedArray || pixels instanceof Uint8Array)) {
      throw new PictureError(`${where}: \`pixels\` must be a Uint8ClampedArray of RGBA bytes`);
    }
    const count = (columns as number) * (rows as number);
    if (pixels.length !== count * 4) {
      throw new PictureError(
        `${where}: \`pixels\` holds ${pixels.length} bytes, not ${count * 4} for ${columns}×${rows} RGBA pixels`,
      );
    }
    totals.pixels += count;
  }
  if (totals.pixels > MOST_PIXELS) {
    throw new PictureError(`the picture's images hold more than ${MOST_PIXELS} pixels`);
  }
}

/** Whether a shape is a line, which is drawn by its stroke, never filled,
 * and in ink unless it says otherwise. */
export function isLine(shape: Shape): boolean {
  return shape.type === "line" || shape.type === "polyline";
}

/** Frees the images a renderer drew on canvases of its own, in a picture
 * that will not be drawn again. It may be one the page refused, so it is
 * walked warily: cycles and all, it visits at most MOST_SHAPES shapes. */
export function closeImages(picture: unknown): void {
  if (typeof ImageBitmap === "undefined") {
    return;
  }
  let budget = MOST_SHAPES;
  const walk = (shapes: unknown, depth: number) => {
    if (!Array.isArray(shapes) || depth > MOST_DEPTH) {
      return;
    }
    for (const shape of shapes) {
      if (budget-- <= 0) {
        return;
      }
      if (typeof shape === "object" && shape !== null) {
        const { rendered, shapes: inner } = shape as Record<string, unknown>;
        if (rendered instanceof ImageBitmap) {
          rendered.close();
        }
        walk(inner, depth + 1);
      }
    }
  };
  if (typeof picture === "object" && picture !== null) {
    walk((picture as Record<string, unknown>).shapes, 1);
  }
}

function describe(value: unknown): string {
  if (typeof value === "string") {
    return JSON.stringify(value.length > 40 ? `${value.slice(0, 40)}…` : value);
  }
  if (typeof value === "bigint") {
    return `${value}n`;
  }
  if (typeof value === "number" || typeof value === "boolean" || value == null) {
    return String(value);
  }
  return Array.isArray(value) ? "an array" : `a ${typeof value}`;
}
