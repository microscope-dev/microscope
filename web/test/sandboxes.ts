// Sandboxes whose frame is the real frame script, from a blob, with the
// same sandbox the server's page gets; each test's end disposes of them.

import { afterEach, expect } from "vitest";
import frameSource from "../src/visualize/frame.js?raw";
import {
  type LiveContext,
  type LiveFailure,
  type LiveListener,
  type LiveSession,
  Sandbox,
} from "../src/visualize/sandbox";
import workerSource from "../src/visualize/worker.js?raw";
import { palette } from "./palette";

const sandboxes: Sandbox[] = [];
afterEach(() => {
  for (const each of sandboxes.splice(0)) {
    each.dispose();
  }
});

/** A sandbox whose workers run `worker`, the real worker script unless a
 * test stands in one of its own. */
export function sandbox(worker: string = workerSource): Sandbox {
  const page = `<!doctype html><script>const WORKER_SOURCE = ${JSON.stringify(worker).replaceAll("<", "\\u003c")};\n${frameSource}</script>`;
  const url = URL.createObjectURL(new Blob([page], { type: "text/html" }));
  const created = new Sandbox(url);
  sandboxes.push(created);
  return created;
}

export const renderer = (name: string, source: string) => ({ name, digest: name, source });

export const context: LiveContext = {
  previous: null,
  paths: {},
  width: 40,
  height: 20,
  theme: "light",
  palette,
};
export const size = { width: 80, height: 40 };

type Heard =
  | { type: "waiting" | "started" | "redraw" }
  | { type: "frame"; bitmap: ImageBitmap | null }
  | { type: "animate"; on: boolean }
  | { type: "caption"; text: string }
  | { type: "hint"; text: string | null }
  | { type: "select"; path: string }
  | { type: "failed"; failure: LiveFailure };

/** A live session and what it said, one message at a time. */
export class Watched {
  readonly session: LiveSession;
  private readonly heard: Heard[] = [];
  private wake: (() => void) | null = null;

  constructor(
    box: Sandbox,
    card: string,
    source: string,
    input: unknown = {},
    name = "live",
    shape = size,
  ) {
    const hear = (message: Heard) => {
      this.heard.push(message);
      this.wake?.();
    };
    const listener: LiveListener = {
      waiting: () => hear({ type: "waiting" }),
      started: () => hear({ type: "started" }),
      frame: (bitmap) => hear({ type: "frame", bitmap }),
      redraw: () => hear({ type: "redraw" }),
      animate: (on) => hear({ type: "animate", on }),
      caption: (text) => hear({ type: "caption", text }),
      hint: (text) => hear({ type: "hint", text }),
      select: (path) => hear({ type: "select", path }),
      failed: (failure) => hear({ type: "failed", failure }),
    };
    this.session = box.live(card, renderer(name, source), input, context, shape, listener);
  }

  async next(): Promise<Heard> {
    while (this.heard.length === 0) {
      await new Promise<void>((resolve) => {
        this.wake = resolve;
      });
    }
    return this.heard.shift() as Heard;
  }

  /** The next message of `type`, failing on any other. */
  async expect<T extends Heard["type"]>(type: T): Promise<Extract<Heard, { type: T }>> {
    const message = await this.next();
    expect(message.type, JSON.stringify(message)).toBe(type);
    return message as Extract<Heard, { type: T }>;
  }

  /** The caption after the next frame. */
  async caption(): Promise<string> {
    return (await this.expect("caption")).text;
  }
}

/** The color of a frame's pixel, as `#rrggbb`. */
export function pixel(bitmap: ImageBitmap, x: number, y: number): string {
  const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
  const context = canvas.getContext("2d") as OffscreenCanvasRenderingContext2D;
  context.drawImage(bitmap, 0, 0);
  const [r, g, b] = context.getImageData(x, y, 1, 1).data;
  return `#${[r, g, b].map((part) => (part ?? 0).toString(16).padStart(2, "0")).join("")}`;
}
