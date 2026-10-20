// Builds a checked picture as SVG, element by element: every attribute is
// one the builder names, and text goes in as text, never as markup. Images
// are canvases the page paints, which name the pixel under the pointer.

import { type OnSelect, pixelIndex, pixelName, type Tip } from "./draw";
import { isLine, type Picture, type Shape, type Style } from "./picture";

const SVG = "http://www.w3.org/2000/svg";
const HTML = "http://www.w3.org/1999/xhtml";

/** The picture as an `<svg>`, scaled to fit its container and never cropped. */
export function buildSvg(picture: Picture, onSelect?: OnSelect, tip?: Tip): SVGSVGElement {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", `0 0 ${picture.width} ${picture.height}`);
  svg.setAttribute("width", String(picture.width));
  svg.setAttribute("height", String(picture.height));
  svg.setAttribute("role", "img");
  if (picture.caption !== undefined) {
    svg.setAttribute("aria-label", picture.caption);
  }
  svg.classList.add("picture");
  // Shapes inherit the page's ink unless they say otherwise.
  svg.style.fill = "var(--ink)";
  svg.style.stroke = "none";
  svg.style.fontSize = "12px";
  for (const shape of picture.shapes) {
    svg.append(element(shape, { onSelect, tip, owned: false }));
  }
  return svg;
}

/** What building a shape's element needs: whether a group around it has a
 * title or select, which the shape answers the pointer for. */
interface Building {
  onSelect: OnSelect | undefined;
  tip: Tip | undefined;
  owned: boolean;
}

function element(shape: Shape, building: Building): SVGElement {
  const { onSelect } = building;
  const owned = building.owned || shape.title !== undefined || shape.select !== undefined;
  let node: SVGElement;
  switch (shape.type) {
    case "rect":
      node = make("rect", {
        x: shape.x,
        y: shape.y,
        width: shape.width,
        height: shape.height,
        rx: shape.radius,
        ry: shape.radius,
      });
      break;
    case "circle":
      node = make("circle", { cx: shape.x, cy: shape.y, r: shape.r });
      break;
    case "line":
      node = make("line", { x1: shape.x1, y1: shape.y1, x2: shape.x2, y2: shape.y2 });
      break;
    case "polyline":
    case "polygon":
      node = make(shape.type, { points: Array.prototype.join.call(shape.points, " ") });
      break;
    case "path":
      node = make("path", { d: shape.d });
      break;
    case "text":
      node = make("text", {
        x: shape.x,
        y: shape.y,
        "font-size": shape.size,
        "font-weight": shape.weight,
        "text-anchor": shape.anchor,
        "dominant-baseline": shape.baseline,
      });
      node.style.fontFamily = shape.family === "mono" ? "var(--mono)" : "var(--sans)";
      node.textContent = shape.text;
      break;
    case "group": {
      const transform = [
        shape.x !== undefined || shape.y !== undefined
          ? `translate(${shape.x ?? 0} ${shape.y ?? 0})`
          : "",
        shape.rotate !== undefined ? `rotate(${shape.rotate})` : "",
        shape.scale !== undefined ? `scale(${shape.scale})` : "",
      ]
        .filter(Boolean)
        .join(" ");
      node = make("g", { transform: transform || undefined });
      for (const child of shape.shapes) {
        node.append(element(child, { ...building, owned }));
      }
      break;
    }
    case "image":
      node = image(shape, building.tip);
      break;
  }
  if (isLine(shape)) {
    outline(node, shape);
  }
  styled(node, shape);
  // An image's title goes with the pixel the tip names.
  if (shape.title !== undefined && shape.type !== "image") {
    const title = document.createElementNS(SVG, "title");
    title.textContent = shape.title;
    node.prepend(title);
  }
  // A shape that says nothing, such as a piece on its square or marks
  // over an image, lets the pointer through to the shapes beneath it.
  if (shape.type !== "group" && !owned) {
    node.style.pointerEvents = "none";
  }
  if (shape.select !== undefined && onSelect !== undefined) {
    const path = shape.select;
    node.setAttribute("tabindex", "0");
    node.setAttribute("role", "button");
    node.classList.add("selectable");
    node.addEventListener("click", () => onSelect(path));
    node.addEventListener("keydown", (event) => {
      const { key } = event as KeyboardEvent;
      if (key === "Enter" || key === " ") {
        // Space would scroll the page, as it does past a real button.
        event.preventDefault();
        onSelect(path);
      }
    });
  }
  return node;
}

function make(name: string, attributes: Record<string, string | number | undefined>): SVGElement {
  const node = document.createElementNS(SVG, name);
  for (const [attribute, value] of Object.entries(attributes)) {
    if (value !== undefined) {
      node.setAttribute(attribute, String(value));
    }
  }
  return node;
}

function outline(node: SVGElement, shape: Style): void {
  node.setAttribute("fill", "none");
  if (shape.stroke === undefined) {
    node.style.stroke = "var(--ink)";
  }
}

function styled(node: SVGElement, shape: Style): void {
  const attributes: Record<string, string | number | undefined> = {
    fill: shape.fill,
    stroke: shape.stroke,
    "stroke-width": shape.strokeWidth,
    opacity: shape.opacity,
    "stroke-dasharray": shape.dash?.join(" "),
  };
  for (const [attribute, value] of Object.entries(attributes)) {
    if (value !== undefined) {
      node.setAttribute(attribute, String(value));
    }
  }
}

function image(shape: Extract<Shape, { type: "image" }>, tip: Tip | undefined): SVGElement {
  const holder = make("foreignObject", {
    x: shape.x,
    y: shape.y,
    width: shape.width,
    height: shape.height,
  });
  const canvas = document.createElementNS(HTML, "canvas") as HTMLCanvasElement;
  canvas.style.display = "block";
  canvas.style.width = "100%";
  canvas.style.height = "100%";
  canvas.style.imageRendering = shape.smooth === true ? "auto" : "pixelated";
  if ("rendered" in shape) {
    canvas.width = shape.rendered.width;
    canvas.height = shape.rendered.height;
    canvas.getContext("2d")?.drawImage(shape.rendered, 0, 0);
  } else {
    canvas.width = shape.columns;
    canvas.height = shape.rows;
    const pixels = new Uint8ClampedArray(shape.pixels);
    canvas.getContext("2d")?.putImageData(new ImageData(pixels, shape.columns, shape.rows), 0, 0);
  }
  if (shape.title !== undefined) {
    canvas.setAttribute("aria-label", shape.title);
  }
  if (tip) {
    canvas.addEventListener("mousemove", (event) => {
      const bounds = canvas.getBoundingClientRect();
      if (bounds.width === 0 || bounds.height === 0) {
        return;
      }
      const column = pixelIndex(event.clientX - bounds.left, bounds.width, canvas.width);
      const row = pixelIndex(event.clientY - bounds.top, bounds.height, canvas.height);
      tip.show(pixelName(shape.title, column, row), event);
    });
    canvas.addEventListener("mouseleave", () => tip.hide());
  }
  holder.append(canvas);
  return holder;
}
