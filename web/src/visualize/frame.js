// The sandboxed frame's script: the page served at /visualizer-frame runs
// it with WORKER_SOURCE, the worker's own script, defined before it. The
// frame has an opaque origin and a policy that allows no request at all;
// it keeps one worker per card and passes messages between them and the
// page, over the one port the page hands it, frames as ImageBitmaps. It
// never runs a renderer.

/* global WORKER_SOURCE */

(() => {
  const url = URL.createObjectURL(new Blob([WORKER_SOURCE], { type: "text/javascript" }));
  /** Each card's worker, and the digest of the renderer it loaded. */
  const cards = new Map();
  let port = null;

  /** Fails when the frame's own bookkeeping is wrong: a bug in microscope. */
  function assert(condition, message) {
    if (!condition) {
      throw new Error(`assertion failed: ${message}`);
    }
  }

  function release(card) {
    const held = cards.get(card);
    if (held !== undefined) {
      held.worker.terminate();
      cards.delete(card);
    }
  }

  function start(card, renderer) {
    assert(!cards.has(card), "a card's worker starts once it has none");
    const worker = new Worker(url);
    const held = { worker, digest: renderer.digest };
    worker.addEventListener("message", (event) => {
      if (cards.get(card) === held) {
        // A live renderer's frame moves on as it came, without a copy.
        const bitmap = event.data?.bitmap;
        port.postMessage({ ...event.data, card }, bitmap instanceof ImageBitmap ? [bitmap] : []);
      }
    });
    worker.addEventListener("error", (event) => {
      event.preventDefault();
      if (cards.get(card) === held) {
        port.postMessage({
          type: "error",
          card,
          id: null,
          error: {
            message: event.message || "the worker failed",
            file: null,
            line: null,
            column: null,
          },
        });
      }
    });
    worker.postMessage({ type: "load", source: renderer.source, name: renderer.name });
    cards.set(card, held);
    return held;
  }

  /** The card's worker for `renderer`, started afresh for another. */
  function workerFor(card, renderer) {
    const held = cards.get(card);
    if (held !== undefined && held.digest === renderer.digest) {
      return held;
    }
    release(card);
    return start(card, renderer);
  }

  // What the page may send a live renderer's worker, as it is.
  const LIVE = new Set(["update", "resize", "pointer", "key", "frame", "ping"]);

  function receive(event) {
    const message = event.data;
    if (message?.type === "draw") {
      workerFor(message.card, message.renderer).worker.postMessage({
        type: "draw",
        id: message.id,
        input: message.input,
        context: message.context,
      });
    } else if (message?.type === "live-start") {
      workerFor(message.card, message.renderer).worker.postMessage({
        type: "live-start",
        input: message.input,
        context: message.context,
        size: message.size,
      });
    } else if (LIVE.has(message?.type)) {
      const { card, ...rest } = message;
      cards.get(card)?.worker.postMessage(rest);
    } else if (message?.type === "release") {
      release(message.card);
    }
  }

  // The page's first message hands over the port; nothing else is heard.
  window.addEventListener("message", function hello(event) {
    if (
      event.source !== window.parent ||
      port !== null ||
      !(event.ports[0] instanceof MessagePort)
    ) {
      return;
    }
    window.removeEventListener("message", hello);
    port = event.ports[0];
    port.addEventListener("message", receive);
    port.start();
    port.postMessage({ type: "ready" });
  });
})();
