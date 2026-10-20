// Draws a checked picture on a canvas, for pictures with more shapes than
// SVG draws quickly. It paints what the SVG builder would: groups move and
// style their shapes, lines are drawn by their stroke, and shapes with no
// fill are in the page's ink. The page hit-tests it itself, so titles are
// still hover text and selects still open their parts, though only for the
// pointer: the canvas has no element per shape for the keyboard to reach.

import type { OnSelect, Tip } from "./draw";
import { pixelIndex, pixelName } from "./draw";
import { isLine, type Picture, type Shape } from "./picture";

/** What a shape inherits from the groups around it. */
interface Inherited {
  fill: string;
  stroke: string | undefined;
  strokeWidth: number;
  dash: number[] | undefined;
  alpha: number;
  title: string | undefined;
  select: string | undefined;
}

/** The page's ink and fonts. */
interface Page {
  ink: string;
  sans: string;
  mono: string;
}

/** A shape the pointer can find: its outline where it was drawn. */
interface Hit {
  path: Path2D;
  matrix: DOMMatrix;
  fill: boolean;
  /** The stroke's width, or 0 when it has none. */
  stroke: number;
  /** Its bounds on the canvas, in device pixels, or null when unknown. */
  box: [number, number, number, number] | null;
  title: string | undefined;
  select: string | undefined;
  image?: { x: number; y: number; width: number; height: number; columns: number; rows: number };
}

/** The most device pixels a picture's canvas holds. */
const MOST_PIXELS = 16 * 1024 * 1024;

const BASELINES: Record<string, CanvasTextBaseline> = {
  auto: "alphabetic",
  alphabetic: "alphabetic",
  middle: "middle",
  central: "middle",
  hanging: "hanging",
  ideographic: "ideographic",
};

const ALIGN: Record<string, CanvasTextAlign> = { start: "left", middle: "center", end: "right" };

/** The picture as a canvas, scaled to fit its container like the SVG. */
export function buildCanvas(picture: Picture, tip: Tip, onSelect?: OnSelect): HTMLCanvasElement {
  const canvas = document.createElement("canvas");
  canvas.classList.add("picture");
  canvas.setAttribute("role", "img");
  if (picture.caption !== undefined) {
    canvas.setAttribute("aria-label", picture.caption);
  }
  const area = Math.max(1, picture.width * picture.height);
  const ratio = Math.min(Math.max(1, window.devicePixelRatio || 1), Math.sqrt(MOST_PIXELS / area));
  canvas.width = Math.max(1, Math.round(picture.width * ratio));
  canvas.height = Math.max(1, Math.round(picture.height * ratio));
  canvas.style.width = `${picture.width}px`;
  canvas.style.maxWidth = "100%";
  canvas.style.height = "auto";
  canvas.style.aspectRatio = `${picture.width} / ${picture.height}`;
  canvas.style.display = "block";
  const pen = canvas.getContext("2d");
  if (pen === null) {
    return canvas;
  }
  const style = getComputedStyle(document.documentElement);
  const token = (name: string, fallback: string) => style.getPropertyValue(name).trim() || fallback;
  const page: Page = {
    ink: token("--ink", "#000"),
    sans: token("--sans", "sans-serif"),
    mono: token("--mono", "monospace"),
  };
  const hits: Hit[] = [];
  pen.scale(canvas.width / picture.width, canvas.height / picture.height);
  const root: Inherited = {
    fill: page.ink,
    stroke: undefined,
    strokeWidth: 1,
    dash: undefined,
    alpha: 1,
    title: undefined,
    select: undefined,
  };
  for (const shape of picture.shapes) {
    paint(pen, shape, root, page, hits);
  }

  /** The topmost shape under a pointer event, and where on the canvas. */
  const find = (event: MouseEvent): { hit: Hit; x: number; y: number } | null => {
    const bounds = canvas.getBoundingClientRect();
    if (bounds.width === 0 || bounds.height === 0) {
      return null;
    }
    const x = ((event.clientX - bounds.left) * canvas.width) / bounds.width;
    const y = ((event.clientY - bounds.top) * canvas.height) / bounds.height;
    try {
      for (let index = hits.length - 1; index >= 0; index--) {
        const hit = hits[index] as Hit;
        if (!near(hit, x, y)) {
          continue;
        }
        pen.setTransform(hit.matrix);
        if (hit.fill && pen.isPointInPath(hit.path, x, y)) {
          return { hit, x, y };
        }
        if (hit.stroke > 0) {
          pen.lineWidth = Math.max(hit.stroke, 6 / Math.abs(hit.matrix.a || 1));
          if (pen.isPointInStroke(hit.path, x, y)) {
            return { hit, x, y };
          }
        }
      }
    } finally {
      pen.resetTransform();
    }
    return null;
  };

  canvas.addEventListener("mousemove", (event) => {
    const found = find(event);
    canvas.style.cursor = found?.hit.select !== undefined && onSelect ? "pointer" : "";
    if (found === null) {
      tip.hide();
      return;
    }
    const { hit } = found;
    if (hit.image) {
      const local = hit.matrix.inverse().transformPoint(new DOMPoint(found.x, found.y));
      const { x, y, width, height, columns, rows } = hit.image;
      const column = pixelIndex(local.x - x, width, columns);
      const row = pixelIndex(local.y - y, height, rows);
      tip.show(pixelName(hit.title, column, row), event);
    } else if (hit.title !== undefined) {
      tip.show(hit.title, event);
    } else {
      tip.hide();
    }
  });
  canvas.addEventListener("mouseleave", () => {
    canvas.style.cursor = "";
    tip.hide();
  });
  canvas.addEventListener("click", (event) => {
    const select = find(event)?.hit.select;
    if (select !== undefined && onSelect) {
      onSelect(select);
    }
  });
  return canvas;
}

/** Whether a point is in or near a shape's bounds. */
function near(hit: Hit, x: number, y: number): boolean {
  if (hit.box === null) {
    return true;
  }
  const slack = hit.stroke > 0 ? 6 + hit.stroke * Math.abs(hit.matrix.a) : 0;
  const [left, top, right, bottom] = hit.box;
  return x >= left - slack && x <= right + slack && y >= top - slack && y <= bottom + slack;
}

/** The device-pixel bounds of a local box under the pen's transform. */
function bounds(matrix: DOMMatrix, left: number, top: number, right: number, bottom: number) {
  const corners = [
    matrix.transformPoint(new DOMPoint(left, top)),
    matrix.transformPoint(new DOMPoint(right, top)),
    matrix.transformPoint(new DOMPoint(left, bottom)),
    matrix.transformPoint(new DOMPoint(right, bottom)),
  ];
  const xs = corners.map((corner) => corner.x);
  const ys = corners.map((corner) => corner.y);
  return [Math.min(...xs), Math.min(...ys), Math.max(...xs), Math.max(...ys)] as [
    number,
    number,
    number,
    number,
  ];
}

function paint(
  pen: CanvasRenderingContext2D,
  shape: Shape,
  inherited: Inherited,
  page: Page,
  hits: Hit[],
): void {
  const own: Inherited = {
    fill: shape.fill ?? inherited.fill,
    stroke: shape.stroke ?? inherited.stroke,
    strokeWidth: shape.strokeWidth ?? inherited.strokeWidth,
    dash: shape.dash ?? inherited.dash,
    alpha: inherited.alpha * (shape.opacity ?? 1),
    title: shape.title ?? inherited.title,
    select: shape.select ?? inherited.select,
  };
  if (shape.type === "group") {
    pen.save();
    pen.translate(shape.x ?? 0, shape.y ?? 0);
    pen.rotate(((shape.rotate ?? 0) * Math.PI) / 180);
    pen.scale(shape.scale ?? 1, shape.scale ?? 1);
    for (const child of shape.shapes) {
      paint(pen, child, own, page, hits);
    }
    pen.restore();
    return;
  }
  pen.globalAlpha = own.alpha;
  // Before any shape strokes, text included, so none keeps the last one's.
  pen.setLineDash(own.dash ?? []);
  const matrix = pen.getTransform();
  const lined = isLine(shape);
  const fill = lined || own.fill === "none" ? null : own.fill;
  const stroke = lined ? (shape.stroke ?? page.ink) : own.stroke;
  const stroked = stroke !== undefined && stroke !== "none" ? stroke : null;
  const hittable = own.title !== undefined || own.select !== undefined;

  let path: Path2D | null = null;
  let box: Hit["box"] = null;
  switch (shape.type) {
    case "rect":
      path = new Path2D();
      if (shape.radius) {
        path.roundRect(shape.x, shape.y, shape.width, shape.height, shape.radius);
      } else {
        path.rect(shape.x, shape.y, shape.width, shape.height);
      }
      box = bounds(matrix, shape.x, shape.y, shape.x + shape.width, shape.y + shape.height);
      break;
    case "circle":
      path = new Path2D();
      path.arc(shape.x, shape.y, shape.r, 0, Math.PI * 2);
      box = bounds(
        matrix,
        shape.x - shape.r,
        shape.y - shape.r,
        shape.x + shape.r,
        shape.y + shape.r,
      );
      break;
    case "line":
      path = new Path2D();
      path.moveTo(shape.x1, shape.y1);
      path.lineTo(shape.x2, shape.y2);
      box = bounds(
        matrix,
        Math.min(shape.x1, shape.x2),
        Math.min(shape.y1, shape.y2),
        Math.max(shape.x1, shape.x2),
        Math.max(shape.y1, shape.y2),
      );
      break;
    case "polyline":
    case "polygon": {
      path = new Path2D();
      const points = shape.points;
      let left = Number.POSITIVE_INFINITY;
      let top = Number.POSITIVE_INFINITY;
      let right = Number.NEGATIVE_INFINITY;
      let bottom = Number.NEGATIVE_INFINITY;
      for (let at = 0; at + 1 < points.length; at += 2) {
        const x = points[at] as number;
        const y = points[at + 1] as number;
        if (at === 0) {
          path.moveTo(x, y);
        } else {
          path.lineTo(x, y);
        }
        left = Math.min(left, x);
        right = Math.max(right, x);
        top = Math.min(top, y);
        bottom = Math.max(bottom, y);
      }
      if (shape.type === "polygon") {
        path.closePath();
      }
      box = points.length > 0 ? bounds(matrix, left, top, right, bottom) : null;
      break;
    }
    case "path":
      path = new Path2D(shape.d);
      break;
    case "text": {
      const size = shape.size ?? 12;
      const weight = shape.weight ?? "normal";
      pen.font = `${weight} ${size}px ${shape.family === "mono" ? page.mono : page.sans}`;
      pen.textAlign = ALIGN[shape.anchor ?? "start"] ?? "left";
      pen.textBaseline = BASELINES[shape.baseline ?? "auto"] ?? "alphabetic";
      if (fill !== null) {
        pen.fillStyle = fill;
        pen.fillText(shape.text, shape.x, shape.y);
      }
      if (stroked !== null) {
        pen.strokeStyle = stroked;
        pen.lineWidth = own.strokeWidth;
        pen.strokeText(shape.text, shape.x, shape.y);
      }
      if (hittable) {
        const metrics = pen.measureText(shape.text);
        const left = shape.x - metrics.actualBoundingBoxLeft;
        const right = shape.x + metrics.actualBoundingBoxRight;
        const top = shape.y - metrics.actualBoundingBoxAscent;
        const bottom = shape.y + metrics.actualBoundingBoxDescent;
        const outline = new Path2D();
        outline.rect(left, top, right - left, bottom - top);
        hits.push({
          path: outline,
          matrix,
          fill: true,
          stroke: 0,
          box: bounds(matrix, left, top, right, bottom),
          title: own.title,
          select: own.select,
        });
      }
      return;
    }
    case "image": {
      const source = imageSource(shape);
      pen.imageSmoothingEnabled = shape.smooth === true;
      if (source !== null) {
        pen.drawImage(source.image, shape.x, shape.y, shape.width, shape.height);
      }
      if (hittable) {
        const outline = new Path2D();
        outline.rect(shape.x, shape.y, shape.width, shape.height);
        hits.push({
          path: outline,
          matrix,
          fill: true,
          stroke: 0,
          box: bounds(matrix, shape.x, shape.y, shape.x + shape.width, shape.y + shape.height),
          title: own.title,
          select: own.select,
          image: {
            x: shape.x,
            y: shape.y,
            width: shape.width,
            height: shape.height,
            columns: source?.columns ?? 1,
            rows: source?.rows ?? 1,
          },
        });
      }
      return;
    }
  }
  if (fill !== null) {
    pen.fillStyle = fill;
    pen.fill(path);
  }
  if (stroked !== null) {
    pen.strokeStyle = stroked;
    pen.lineWidth = own.strokeWidth;
    pen.stroke(path);
  }
  if (hittable && (fill !== null || stroked !== null)) {
    hits.push({
      path,
      matrix,
      fill: fill !== null,
      stroke: stroked !== null ? own.strokeWidth : 0,
      box,
      title: own.title,
      select: own.select,
    });
  }
}

/** What an image shape paints, and its size in pixels. */
function imageSource(
  shape: Extract<Shape, { type: "image" }>,
): { image: CanvasImageSource; columns: number; rows: number } | null {
  if ("rendered" in shape) {
    return { image: shape.rendered, columns: shape.rendered.width, rows: shape.rendered.height };
  }
  const canvas = document.createElement("canvas");
  canvas.width = shape.columns;
  canvas.height = shape.rows;
  const pen = canvas.getContext("2d");
  if (pen === null) {
    return null;
  }
  pen.putImageData(
    new ImageData(new Uint8ClampedArray(shape.pixels), shape.columns, shape.rows),
    0,
    0,
  );
  return { image: canvas, columns: shape.columns, rows: shape.rows };
}
