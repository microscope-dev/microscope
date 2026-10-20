// Draws a checked picture into the page: as SVG, where every shape's title
// and select are accessible, or past 2,000 shapes on a canvas the page
// hit-tests itself, which draws ten thousand dots in a few milliseconds.
// The canvas's titles and selects answer only the pointer. Either way an
// image says which of its pixels is under the pointer.

import { buildCanvas } from "./canvas";
import type { Picture, Shape } from "./picture";
import { buildSvg } from "./svg";

/** What clicking, or pressing Enter or Space on, a shape with `select` does. */
export type OnSelect = (path: string) => void;

/** The most shapes the page draws as SVG. */
const MOST_SVG_SHAPES = 2_000;

/** How many shapes a picture holds, groups and what they hold included. */
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

/** The picture in a holder, with the tip its hover text shows in. */
export function buildPicture(picture: Picture, onSelect?: OnSelect): HTMLElement {
  const holder = document.createElement("div");
  holder.classList.add("picture-holder");
  holder.style.position = "relative";
  const tip = new Tip(holder);
  holder.append(
    countShapes(picture.shapes) > MOST_SVG_SHAPES
      ? buildCanvas(picture, tip, onSelect)
      : buildSvg(picture, onSelect, tip),
    tip.element,
  );
  return holder;
}

/** Hover text the page shows itself, beside the pointer. */
export class Tip {
  readonly element: HTMLDivElement;

  constructor(private readonly holder: HTMLElement) {
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

/** Which of `count` pixels across `size` the point `offset` into them is on. */
export function pixelIndex(offset: number, size: number, count: number): number {
  return Math.min(count - 1, Math.max(0, Math.floor((offset / size) * count)));
}

/** What hovering an image's pixel says. */
export function pixelName(title: string | undefined, column: number, row: number): string {
  const place = `column ${column}, row ${row}`;
  return title === undefined ? place : `${title}: ${place}`;
}
