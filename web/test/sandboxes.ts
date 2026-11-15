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

export const pageUrl = (page: string) =>
  URL.createObjectURL(new Blob([page], { type: "text/html" }));

export const framePage = (worker: string = workerSource) =>
  `<!doctype html><script>const WORKER_SOURCE = ${JSON.stringify(worker).replaceAll("<", "\\u003c")};\n${frameSource}</script>`;

export function framed(page: string = framePage()): Sandbox {
  const created = new Sandbox(pageUrl(page));
  sandboxes.push(created);
  return created;
}

export function sandbox(worker: string = workerSource): Sandbox {
  return framed(framePage(worker));
}

export function lastFrame(): HTMLIFrameElement {
  const frames = document.querySelectorAll<HTMLIFrameElement>('iframe[title="renderers"]');
  const frame = frames.item(frames.length - 1);
  if (frame === null) {
    throw new Error("no sandbox frame");
  }
  return frame;
}

export function settled<T>(promise: Promise<T>): { value: T | undefined } {
  const state = { value: undefined as T | undefined };
  void (async () => {
    state.value = await promise;
  })();
  return state;
}

export const SILENT_PAGE = "<!doctype html><title>silent</title>";

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

  async expect<T extends Heard["type"]>(type: T): Promise<Extract<Heard, { type: T }>> {
    const message = await this.next();
    // oxlint-disable-next-line vitest/no-standalone-expect -- a helper that tests call
    expect(message).toMatchObject({ type });
    return message as Extract<Heard, { type: T }>;
  }

  async caption(): Promise<string> {
    return (await this.expect("caption")).text;
  }
}

export function pixel(bitmap: ImageBitmap, x: number, y: number): string {
  const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
  const canvasContext = canvas.getContext("2d") as OffscreenCanvasRenderingContext2D;
  canvasContext.drawImage(bitmap, 0, 0);
  const [r, g, b] = canvasContext.getImageData(x, y, 1, 1).data;
  return `#${[r, g, b].map((part) => (part ?? 0).toString(16).padStart(2, "0")).join("")}`;
}
