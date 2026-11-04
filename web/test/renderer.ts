// Runs a renderer the way the page does: its source in the real worker,
// which locks itself down first, and its picture through the validator.

import { afterEach } from "vitest";

import { type Picture, type Shape, validate } from "../src/visualize/picture";
import type { DrawContext } from "../src/visualize/sandbox";
import workerSource from "../src/visualize/worker.js?raw";
import { palette } from "./palette";

const workers: Worker[] = [];
afterEach(() => {
  for (const worker of workers.splice(0)) {
    worker.terminate();
  }
});

/** The worker's answer to drawing `input` with `source`. */
export async function run(
  source: string,
  input: unknown,
  context: Partial<DrawContext> = {},
): Promise<Record<string, unknown>> {
  const url = URL.createObjectURL(new Blob([workerSource], { type: "text/javascript" }));
  const worker = new Worker(url);
  workers.push(worker);
  const answers: Record<string, unknown>[] = [];
  let wake = () => {};
  worker.addEventListener("message", (event) => {
    answers.push(event.data);
    wake();
  });
  const next = async () => {
    while (answers.length === 0) {
      await new Promise<void>((resolve) => {
        wake = resolve;
      });
    }
    return answers.shift() as Record<string, unknown>;
  };
  worker.postMessage({ type: "load", source, name: "renderer" });
  const loaded = await next();
  if (loaded.error !== null) {
    throw new Error(`the renderer did not load: ${JSON.stringify(loaded.error)}`);
  }
  const full: DrawContext = {
    previous: null,
    paths: {},
    width: 800,
    theme: "light",
    palette,
    ...context,
  };
  worker.postMessage({ type: "draw", id: 1, input, context: full });
  return next();
}

/** The picture `source` draws for `input`, checked as the page checks it. */
export async function draw(
  source: string,
  input: unknown,
  context: Partial<DrawContext> = {},
): Promise<Picture> {
  const answer = await run(source, input, context);
  if (answer.type !== "picture") {
    throw new Error(`the renderer failed: ${JSON.stringify(answer.error)}`);
  }
  return validate(answer.picture);
}

/** Every shape of a picture, groups' shapes included, in drawing order. */
export function shapesOf(picture: Picture): Shape[] {
  const all: Shape[] = [];
  const walk = (shapes: Shape[]) => {
    for (const shape of shapes) {
      all.push(shape);
      if (shape.type === "group") {
        walk(shape.shapes);
      }
    }
  };
  walk(picture.shapes);
  return all;
}

/** The text of every text shape. */
export function texts(picture: Picture): string[] {
  return shapesOf(picture).flatMap((shape) => (shape.type === "text" ? [shape.text] : []));
}

/** The title of every titled shape. */
export function titles(picture: Picture): string[] {
  return shapesOf(picture).flatMap((shape) => (shape.title !== undefined ? [shape.title] : []));
}
