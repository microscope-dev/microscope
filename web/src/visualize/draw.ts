import { assert } from "../assert";
import { buildCanvas } from "./canvas";
import { MOST_SHAPES, type Picture, type Shape } from "./picture";
import { buildSvg } from "./svg";

export type OnSelect = (path: string) => void;

const MOST_SVG_SHAPES = 2_000;

function countShapes(shapes: Shape[]): number {
  let count = 0;
  for (const shape of shapes) {
    count += 1;
    if (shape.type === "group") {
      count += countShapes(shape.shapes);
    }
  }
  return count;
}

/**
 * Renders a validated `Picture` as DOM SVG for small scenes (`<= MOST_SVG_SHAPES`)
 * and falls back to `<canvas>` with manual hit-testing for large scenes to avoid DOM bloat.
 */
export function buildPicture(picture: Picture, onSelect?: OnSelect): HTMLElement {
  const holder = document.createElement("div");
  holder.classList.add("picture-holder");
  holder.style.position = "relative";
  const tip = new Tip(holder);
  const count = countShapes(picture.shapes);
  assert(count <= MOST_SHAPES, `picture holds ${count} shapes, past ${MOST_SHAPES}`);
  holder.append(
    count > MOST_SVG_SHAPES
      ? buildCanvas(picture, tip, onSelect)
      : buildSvg(picture, onSelect, tip),
    tip.element,
  );
  return holder;
}

export class Tip {
  readonly element: HTMLDivElement;
  private readonly holder: HTMLElement;

  constructor(holder: HTMLElement) {
    this.holder = holder;
    this.element = document.createElement("div");
    this.element.classList.add("drawing-tip");
    this.element.setAttribute("role", "tooltip");
    this.element.hidden = true;
  }

  show(text: string, event: MouseEvent): void {
    const bounds = this.holder.getBoundingClientRect();
    this.element.textContent = text;
    this.element.style.left = `${event.clientX - bounds.left + 12}px`;
    this.element.style.top = `${event.clientY - bounds.top + 12}px`;
    this.element.hidden = false;
  }

  hide(): void {
    this.element.hidden = true;
  }
}

export function pixelIndex(offset: number, size: number, count: number): number {
  return Math.min(count - 1, Math.max(0, Math.floor((offset / size) * count)));
}

export function pixelName(title: string | undefined, column: number, row: number): string {
  const place = `column ${column}, row ${row}`;
  return title === undefined ? place : `${title}: ${place}`;
}
