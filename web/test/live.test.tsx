// Live renderers keep a worker of their own, draw the frames the page asks
// for into their own canvas, and hand each back as only its pixels. These
// run the real frame and worker scripts in Chromium.

import { describe, expect, it } from "vitest";
import { context, pixel, renderer, sandbox, Watched } from "./sandboxes";

// Fills its canvas with the input's color and says, after each frame, what
// it has heard since the last.
const reporting = `
microscope.live((canvas, input, context) => {
  const draw = canvas.getContext("2d");
  let color = input.color;
  let heard = ["start " + context.width + "x" + context.height + " " + canvas.width + "x" + canvas.height];
  return {
    frame() {
      draw.fillStyle = color;
      draw.fillRect(0, 0, canvas.width, canvas.height);
      microscope.caption(heard.join("; "));
      heard = [];
    },
    update(next, given) {
      color = next.color;
      heard.push("update " + next.color + " previous " + JSON.stringify(given.previous));
    },
    resize(width, height) {
      heard.push("resize " + width + "x" + height + " " + canvas.width + "x" + canvas.height);
    },
    pointer(event) {
      heard.push(event.type + " " + event.x + "," + event.y + " " + event.buttons + " " + Object.isFrozen(event));
      if (event.type === "down") {
        microscope.hint("vertex 3");
        microscope.select("cells[2]");
        microscope.redraw();
      }
    },
    key(event) {
      heard.push("key " + event.key + (event.shift ? " shift" : ""));
      microscope.animate(event.key === "a");
    },
  };
});`;

describe("a live renderer", () => {
  it("is drawn by a session of its own, which the page drives", async () => {
    const box = sandbox();
    const code = renderer("reporting", reporting);
    // Drawing it says it is live, and then costs no worker at all.
    expect(await box.draw("card", code, {}, { ...context, width: 40 })).toEqual({ kind: "live" });
    expect(await box.draw("card", code, {}, { ...context, width: 40 })).toEqual({ kind: "live" });

    const live = new Watched(box, "card", reporting, { color: "#ff0000" }, "reporting");
    await live.expect("started");
    live.session.frame(0);
    expect(await live.caption()).toBe("start 40x20 80x40");
    const first = await live.expect("frame");
    expect(first.bitmap).toBeInstanceOf(ImageBitmap);
    expect([first.bitmap?.width, first.bitmap?.height]).toEqual([80, 40]);
    expect(pixel(first.bitmap as ImageBitmap, 40, 20)).toBe("#ff0000");

    // A new stop, a new size, the pointer, and keys arrive in order.
    live.session.update({ color: "#0000ff" }, { ...context, previous: { color: "#ff0000" } });
    live.session.resize(60, 30);
    live.session.pointer({
      type: "down",
      x: 5,
      y: 6,
      dx: 0,
      dy: 0,
      buttons: 1,
      wheel: 0,
      shift: false,
      ctrl: false,
      alt: false,
    });
    expect(await live.next()).toEqual({ type: "hint", text: "vertex 3" });
    expect(await live.next()).toEqual({ type: "select", path: "cells[2]" });
    await live.expect("redraw");
    live.session.key({ key: "a", shift: true, ctrl: false, alt: false });
    expect(await live.next()).toEqual({ type: "animate", on: true });
    live.session.frame(16);
    expect(await live.caption()).toBe(
      'update #0000ff previous {"color":"#ff0000"}; resize 60x30 60x30; down 5,6 1 true; key a shift',
    );
    const second = await live.expect("frame");
    expect([second.bitmap?.width, second.bitmap?.height]).toEqual([60, 30]);
    expect(pixel(second.bitmap as ImageBitmap, 30, 15)).toBe("#0000ff");
    live.session.stop();
  });

  it("is ended when it stops answering, and starts afresh", async () => {
    const box = sandbox();
    const stuck = "microscope.live(() => ({ frame() { for (;;) {} } }));";
    const live = new Watched(box, "card", stuck);
    await live.expect("started");
    live.session.frame(0);
    const started = performance.now();
    expect(await live.next()).toEqual({ type: "failed", failure: { kind: "unresponsive" } });
    expect(performance.now() - started).toBeGreaterThanOrEqual(2_000);

    const again = new Watched(box, "card", "microscope.live(() => ({ frame() {} }));", {}, "calm");
    await again.expect("started");
    again.session.frame(0);
    // A canvas no context drew on sends no pixels.
    expect(await again.next()).toEqual({ type: "frame", bitmap: null });
  });

  it("is ended when a function it returned never settles", async () => {
    const box = sandbox();
    const pending = "microscope.live(() => ({ frame: () => new Promise(() => {}) }));";
    const live = new Watched(box, "card", pending);
    await live.expect("started");
    live.session.frame(0);
    // The worker itself still runs; the renderer will never draw again.
    expect(await live.next()).toEqual({ type: "failed", failure: { kind: "unresponsive" } });
  });

  it("fails where its code threw, and once the page stops it says nothing", async () => {
    const box = sandbox();
    const throwing = `microscope.live(() => ({
  key() {
    throw new RangeError("no such key");
  },
}));`;
    const live = new Watched(box, "card", throwing, {}, "keys");
    await live.expect("started");
    live.session.key({ key: "q", shift: false, ctrl: false, alt: false });
    expect(await live.next()).toEqual({
      type: "failed",
      failure: {
        kind: "failed",
        failure: { message: "RangeError: no such key", file: "keys.js", line: 3, column: 11 },
      },
    });

    const broken = new Watched(box, "other", "microscope.live(", {}, "broken");
    await broken.expect("started");
    const failed = await broken.expect("failed");
    expect(failed.failure).toMatchObject({
      kind: "failed",
      failure: { message: expect.stringMatching(/^SyntaxError/), file: "broken.js" },
    });
  });

  it("calls microscope.draw or microscope.live, once", async () => {
    const box = sandbox();
    const outcome = await box.draw(
      "card",
      renderer("both", "microscope.live(() => ({}));\nmicroscope.draw(() => null);"),
      {},
      { ...context, width: 40 },
    );
    expect(outcome).toMatchObject({
      kind: "failed",
      failure: {
        message: "Error: a renderer calls microscope.draw or microscope.live once",
        line: 2,
      },
    });
  });

  it("runs four sessions at once; the fifth waits for one to stop", async () => {
    const box = sandbox();
    const calm = "microscope.live(() => ({ frame() { microscope.caption('drawn'); } }));";
    const running = ["1", "2", "3", "4"].map((card) => new Watched(box, card, calm));
    for (const live of running) {
      await live.expect("started");
    }
    const fifth = new Watched(box, "5", calm);
    await fifth.expect("waiting");
    running[0]?.session.stop();
    await fifth.expect("started");
    fifth.session.frame(0);
    expect(await fifth.caption()).toBe("drawn");
  });

  it("gets the pointer and keys sent before it started, in order", async () => {
    const box = sandbox();
    const calm = "microscope.live(() => ({}));";
    const running = ["1", "2", "3", "4"].map((card) => new Watched(box, card, calm));
    for (const live of running) {
      await live.expect("started");
    }
    const echo = `microscope.live(() => ({
  pointer(event) { microscope.hint(event.type + " " + event.x); },
  key(event) { microscope.hint(event.key); },
}));`;
    const late = new Watched(box, "5", echo);
    await late.expect("waiting");
    const at = (x: number) => ({
      type: "move" as const,
      x,
      y: 0,
      dx: 0,
      dy: 0,
      buttons: 0,
      wheel: 0,
      shift: false,
      ctrl: false,
      alt: false,
    });
    late.session.pointer(at(4));
    late.session.key({ key: "q", shift: false, ctrl: false, alt: false });
    late.session.pointer(at(9));
    running[0]?.session.stop();
    await late.expect("started");
    expect(await late.next()).toEqual({ type: "hint", text: "move 4" });
    expect(await late.next()).toEqual({ type: "hint", text: "q" });
    expect(await late.next()).toEqual({ type: "hint", text: "move 9" });
  });

  it("sends only the last caption, hint, and select it set, however many it set", async () => {
    const box = sandbox();
    const flooding = `microscope.live(() => {
  setTimeout(() => {
    for (let i = 0; i < 100000; i++) microscope.hint("late " + i);
  });
  return {
    frame() {
      for (let i = 0; i < 1000000; i++) {
        microscope.caption("caption " + i);
        microscope.select("cells[" + i + "]");
        microscope.redraw();
      }
    },
  };
});`;
    const live = new Watched(box, "card", flooding);
    await live.expect("started");
    expect(await live.next()).toEqual({ type: "hint", text: "late 99999" });
    live.session.frame(0);
    expect(await live.caption()).toBe("caption 999999");
    expect(await live.next()).toEqual({ type: "select", path: "cells[999999]" });
    await live.expect("redraw");
    await live.expect("frame");
  });

  it("is ended when its worker floods the page", async () => {
    // A worker that skips worker.js's own limits, as a broken one might.
    const hostile = `onmessage = (event) => {
  if (event.data.type === "live-start") {
    for (let i = 0; i < 20000; i++) postMessage({ type: "caption", text: "flood " + i });
  }
};`;
    const live = new Watched(sandbox(hostile), "card", "");
    await live.expect("started");
    let heard = await live.next();
    while (heard.type === "caption") {
      heard = await live.next();
    }
    expect(heard).toEqual({
      type: "failed",
      failure: {
        kind: "failed",
        failure: {
          message: "the renderer sent more than 1000 messages in 100 ms",
          file: null,
          line: null,
          column: null,
        },
      },
    });
  });

  it("ends with the sandbox", async () => {
    const box = sandbox();
    const live = new Watched(box, "card", "microscope.live(() => ({}));");
    await live.expect("started");
    box.dispose();
    expect(await live.next()).toMatchObject({
      type: "failed",
      failure: { kind: "failed", failure: { message: "the page closed the sandbox" } },
    });
  });
});
