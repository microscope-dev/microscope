// Renderers run in a worker that has locked itself down, behind a
// sandboxed frame; their pictures come back as data for the page to check
// and draw. These run the real scripts in Chromium. The served frame, its
// policy, and Firefox are covered by e2e/sandbox.spec.ts.

import { afterEach, describe, expect, it } from "vitest";
import { validate } from "../src/visualize/picture";
import { type DrawContext, type Outcome, palette as pagePalette } from "../src/visualize/sandbox";
import { buildSvg } from "../src/visualize/svg";
import workerSource from "../src/visualize/worker.js?raw";
import { palette } from "./palette";
import { renderer, sandbox } from "./sandboxes";

const context: DrawContext = { previous: null, paths: {}, width: 400, theme: "light", palette };

/** A renderer that draws one text: the JSON of what `body` returns. */
const reporting = (body: string) => `
microscope.draw((input, context) => {
  const report = (() => { ${body} })();
  return microscope.picture({ width: 1, height: 1, shapes: [
    microscope.text({ x: 0, y: 0, text: JSON.stringify(report, (_, v) => typeof v === "bigint" ? v + "n" : v) }),
  ] });
});`;

/** The worker's messages, one at a time. */
class Probe {
  readonly worker: Worker;
  private readonly messages: unknown[] = [];
  private wake: (() => void) | null = null;

  constructor(source: string, name = "probe") {
    const url = URL.createObjectURL(new Blob([workerSource], { type: "text/javascript" }));
    this.worker = new Worker(url);
    this.worker.addEventListener("message", (event) => {
      this.messages.push(event.data);
      this.wake?.();
    });
    this.worker.postMessage({ type: "load", source, name });
    probes.push(this);
  }

  async next(): Promise<Record<string, unknown>> {
    while (this.messages.length === 0) {
      await new Promise<void>((resolve) => {
        this.wake = resolve;
      });
    }
    return this.messages.shift() as Record<string, unknown>;
  }

  async draw(input: unknown, given: DrawContext = context): Promise<Record<string, unknown>> {
    this.worker.postMessage({ type: "draw", id: 7, input, context: given });
    return this.next();
  }

  /** What a reporting renderer reported. */
  async report(input: unknown = {}): Promise<unknown> {
    const answer = await this.draw(input);
    expect(answer.type, JSON.stringify(answer)).toBe("picture");
    const picture = validate(answer.picture);
    const text = picture.shapes[0];
    if (text?.type !== "text") {
      throw new Error("no report");
    }
    return JSON.parse(text.text);
  }
}

const probes: Probe[] = [];
afterEach(() => {
  for (const probe of probes.splice(0)) {
    probe.worker.terminate();
  }
});

// Ways out of a worker, and ways to reach the page or the debugger.
const DANGEROUS = [
  "fetch",
  "XMLHttpRequest",
  "WebSocket",
  "WebSocketStream",
  "WebTransport",
  "EventSource",
  "FontFace",
  "fonts",
  "importScripts",
  "Worker",
  "SharedWorker",
  "postMessage",
  "onmessage",
  "addEventListener",
  "dispatchEvent",
  "close",
  "caches",
  "indexedDB",
  "navigator",
  "location",
  "origin",
  "BroadcastChannel",
  "MessageChannel",
  "MessagePort",
  "Request",
  "Response",
  "Headers",
  "Blob",
  "URL",
  "FileReader",
  "crypto",
  "performance",
  "WebAssembly",
  "RTCPeerConnection",
  "Notification",
  "SharedArrayBuffer",
  "Atomics",
  "self",
  "scheduler",
  "reportError",
];

describe("the worker", () => {
  it("leaves a renderer nothing that reaches outside, through any prototype", async () => {
    const probe = new Probe(
      reporting(`
        const holders = [];
        for (let o = globalThis; o !== null && o !== Object.prototype; o = Object.getPrototypeOf(o)) {
          holders.push(o);
        }
        const names = ${JSON.stringify(DANGEROUS)};
        return {
          reachable: names.filter((name) => holders.some((o) => Object.hasOwn(o, name)) || name in globalThis),
          drawing: [typeof OffscreenCanvas, typeof Path2D, typeof ImageData, typeof DOMMatrix],
          eval: typeof eval,
        };
      `),
    );
    expect(await probe.next()).toEqual({ type: "loaded", error: null });
    expect(await probe.report()).toEqual({
      reachable: [],
      drawing: ["function", "function", "function", "function"],
      eval: "function",
    });
  });

  it("evaluates the renderer only after locking down, so hoisting changes nothing", async () => {
    // Declarations hoist to the top of a script; were the renderer part of
    // the worker's script, these would run before the lockdown.
    const probe = new Probe(`
      var Reflect = { deleteProperty: () => true, ownKeys: () => [] };
      function postMessage() {}
      ${reporting("return [typeof fetch, typeof self, typeof importScripts];")}
    `);
    await probe.next();
    expect(await probe.report()).toEqual(["undefined", "undefined", "undefined"]);
  });

  it("hands inputs over as they are, typed arrays and bigints included", async () => {
    const probe = new Probe(
      reporting(`
        return {
          samples: [input.samples.constructor.name, input.samples.length, input.samples[1]],
          big: typeof input.big,
          context: [context.width, context.theme, context.previous, microscope.theme.changed, microscope.theme.series.length],
        };
      `),
    );
    await probe.next();
    expect(
      await probe.report({ samples: new Float64Array([1, 2.5]), big: 2n ** 64n - 1n }),
    ).toEqual({
      samples: ["Float64Array", 2, 2.5],
      big: "bigint",
      context: [400, "light", null, "#b0500a", 8],
    });
  });

  it("says where a renderer failed, by its file and line", async () => {
    const throwing = new Probe("microscope.draw(() => {\n  return null.x;\n});", "board");
    await throwing.next();
    const answer = await throwing.draw({});
    expect(answer).toMatchObject({
      type: "error",
      id: 7,
      error: { file: "board.js", line: 2 },
    });
    expect((answer.error as { message: string }).message).toMatch(/^TypeError: /);

    const rejecting = new Probe(
      "microscope.draw(async () => {\n\n  throw new RangeError('no squares');\n});",
      "board",
    );
    await rejecting.next();
    expect(await rejecting.draw({})).toMatchObject({
      type: "error",
      error: { message: "RangeError: no squares", file: "board.js", line: 3 },
    });
  });

  it("fails to load a renderer that does not parse or never draws", async () => {
    const broken = new Probe("microscope.draw(() => {", "broken");
    expect(await broken.next()).toMatchObject({
      type: "loaded",
      error: { message: expect.stringMatching(/^SyntaxError: /), file: "broken.js" },
    });
    const idle = new Probe("const x = 1;", "idle");
    expect(await idle.next()).toMatchObject({
      type: "loaded",
      error: {
        message: "the renderer never calls microscope.draw or microscope.live",
        file: "idle.js",
      },
    });
  });

  it("reports an error a renderer leaves uncaught in a timer", async () => {
    const probe = new Probe(
      "microscope.draw(() => { setTimeout(() => { throw new Error('later'); }); return new Promise(() => {}); });",
      "late",
    );
    await probe.next();
    expect(await probe.draw({})).toMatchObject({
      type: "error",
      id: null,
      error: { message: "Error: later", file: "late.js" },
    });
  });

  it("sends nothing for a drawn picture's calls meant for a live one", async () => {
    const probe = new Probe(`
      microscope.draw(() => {
        for (let i = 0; i < 1000; i++) {
          microscope.caption("c");
          microscope.hint("h");
          microscope.select("a");
          microscope.redraw();
          microscope.animate(true);
        }
        return microscope.picture({ width: 1, height: 1, shapes: [] });
      });`);
    await probe.next();
    expect(await probe.draw({})).toMatchObject({ type: "picture" });
  });

  it("compares inputs deeply and blends colors", async () => {
    const probe = new Probe(
      reporting(`
        const same = microscope.same;
        return {
          same: [
            same({ a: [1, { b: 2n }] }, { a: [1, { b: 2n }] }),
            same(new Uint8Array([1, 2]), new Uint8Array([1, 2])),
            same(Number.NaN, Number.NaN),
            same([1, 2], [1, 3]),
            same(new Uint8Array([1]), new Int8Array([1])),
            same({ variant: "None" }, { variant: "Some", value: 1 }),
          ],
          scale: [
            microscope.color.scale(0, "#000", "#fff"),
            microscope.color.scale(1, "#000", "#fff"),
            microscope.color.scale(0.5, "#2a78d6", "#2a78d6"),
          ],
        };
      `),
    );
    await probe.next();
    expect(await probe.report()).toEqual({
      same: [true, true, true, false, false, false],
      scale: ["#000000", "#ffffff", "#2a78d6"],
    });
  });

  it("sends a canvas the renderer drew as only its pixels", async () => {
    const probe = new Probe(`
      microscope.draw(() => {
        const canvas = new OffscreenCanvas(2, 1);
        const context = canvas.getContext("2d");
        context.fillStyle = "#ff0000";
        context.fillRect(1, 0, 1, 1);
        return microscope.picture({ width: 2, height: 1, shapes: [
          microscope.image({ x: 0, y: 0, width: 2, height: 1, canvas }),
        ] });
      });`);
    await probe.next();
    const answer = await probe.draw({});
    const picture = validate(answer.picture);
    expect(picture.shapes[0]).toMatchObject({ type: "image" });
    const shape = picture.shapes[0] as { rendered: ImageBitmap };
    expect(shape.rendered).toBeInstanceOf(ImageBitmap);
    expect([shape.rendered.width, shape.rendered.height]).toEqual([2, 1]);
  });
});

const counting = renderer(
  "count",
  `let draws = 0;
   microscope.draw((input) => microscope.picture({ width: 1, height: 1, shapes: [
     microscope.text({ x: 0, y: 0, text: input.label + " " + ++draws }),
   ] }));`,
);
const textOf = (outcome: Outcome) => {
  expect(outcome.kind, JSON.stringify(outcome)).toBe("picture");
  const shape = validate((outcome as { picture: unknown }).picture).shapes[0];
  return shape?.type === "text" ? shape.text : null;
};

describe("the sandbox", () => {
  it("keeps a card's worker from draw to draw, and starts another for a new renderer", async () => {
    const box = sandbox();
    expect(textOf(await box.draw("a", counting, { label: "a" }, context))).toBe("a 1");
    expect(textOf(await box.draw("a", counting, { label: "a" }, context))).toBe("a 2");
    // Each card has its own worker.
    expect(textOf(await box.draw("b", counting, { label: "b" }, context))).toBe("b 1");
    const changed = { ...counting, digest: "count-2" };
    expect(textOf(await box.draw("a", changed, { label: "a" }, context))).toBe("a 1");
  });

  it("ends a draw that runs too long, and the card draws again afterwards", async () => {
    const box = sandbox();
    const looping = renderer(
      "loop",
      "microscope.draw((input) => { while (input.forever) {} return microscope.picture({ width: 1, height: 1, shapes: [] }); });",
    );
    const started = performance.now();
    const outcomes = await Promise.all([
      box.draw("stuck", looping, { forever: true }, context),
      box.draw("fine", counting, { label: "fine" }, context),
    ]);
    expect(outcomes[0]).toEqual({ kind: "timeout" });
    expect(performance.now() - started).toBeGreaterThanOrEqual(2_000);
    expect(textOf(outcomes[1])).toBe("fine 1");
    const again = await box.draw("stuck", looping, { forever: false }, context);
    expect(again.kind).toBe("picture");
  });

  it("runs four draws at once and queues the rest", async () => {
    const box = sandbox();
    const slow = renderer(
      "slow",
      "microscope.draw((input) => new Promise((resolve) => setTimeout(() => resolve(microscope.picture({ width: 1, height: 1, shapes: [] })), 300)));",
    );
    const started = performance.now();
    await Promise.all(["1", "2", "3", "4", "5"].map((card) => box.draw(card, slow, {}, context)));
    // The fifth waits for one of the first four.
    expect(performance.now() - started).toBeGreaterThanOrEqual(600);
  });

  it("fails draws running and waiting when the page closes it", async () => {
    const box = sandbox();
    const never = renderer("never", "microscope.draw(() => new Promise(() => {}));");
    const draws = ["1", "2", "3", "4", "5", "6"].map((card) => box.draw(card, never, {}, context));
    box.dispose();
    for (const outcome of await Promise.all(draws)) {
      expect(outcome).toMatchObject({
        kind: "failed",
        failure: { message: "the page closed the sandbox" },
      });
    }
  });

  it("fails every draw of a renderer that cannot load", async () => {
    const box = sandbox();
    const outcome = await box.draw("x", renderer("broken", "microscope.draw("), {}, context);
    expect(outcome).toMatchObject({
      kind: "failed",
      failure: { message: expect.stringMatching(/^SyntaxError/), file: "broken.js" },
    });
  });
});

describe("the builder", () => {
  it("draws text as text, with titles, and selects on click, Enter, and Space", async () => {
    const selected: string[] = [];
    const picture = validate({
      type: "picture",
      width: 80,
      height: 40,
      caption: "Black to move",
      shapes: [
        {
          type: "rect",
          x: 0,
          y: 0,
          width: 40,
          height: 40,
          fill: "#f0d9b5",
          title: "e4: White Pawn",
          select: "mailbox[28]",
        },
        { type: "text", x: 20, y: 20, text: "<img src=x onerror=alert(1)>" },
        {
          type: "image",
          x: 40,
          y: 0,
          width: 40,
          height: 40,
          pixels: new Uint8ClampedArray([255, 0, 0, 255]),
          columns: 1,
          rows: 1,
        },
      ],
    });
    const svg = buildSvg(picture, (path) => selected.push(path));
    document.body.append(svg);
    try {
      expect(svg.getAttribute("aria-label")).toBe("Black to move");
      expect(svg.querySelector("img")).toBeNull();
      expect(svg.querySelector("text")?.textContent).toBe("<img src=x onerror=alert(1)>");
      const square = svg.querySelector("rect");
      expect(square?.querySelector("title")?.textContent).toBe("e4: White Pawn");
      square?.dispatchEvent(new MouseEvent("click"));
      square?.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));
      // Space too, as on any button, without scrolling the page.
      const space = new KeyboardEvent("keydown", { key: " ", cancelable: true });
      square?.dispatchEvent(space);
      expect(space.defaultPrevented).toBe(true);
      expect(selected).toEqual(["mailbox[28]", "mailbox[28]", "mailbox[28]"]);
      const canvas = svg.querySelector("canvas");
      const pixel = canvas?.getContext("2d")?.getImageData(0, 0, 1, 1).data;
      expect([...(pixel ?? [])]).toEqual([255, 0, 0, 255]);
    } finally {
      svg.remove();
    }
  });
});

describe("the theme", () => {
  it("gives renderers every color as #rrggbb, however the stylesheet writes it", () => {
    const root = document.documentElement;
    root.style.setProperty("--surface", "#fff");
    root.style.setProperty("--ink", "rgb(26, 34, 48)");
    try {
      const colors = pagePalette("light");
      expect(colors.surface).toBe("#ffffff");
      expect(colors.ink).toBe("#1a2230");
      for (const color of [...Object.values(colors).flat()]) {
        expect(color).toMatch(/^#[0-9a-f]{6}$/);
      }
    } finally {
      root.style.removeProperty("--surface");
      root.style.removeProperty("--ink");
    }
  });
});
