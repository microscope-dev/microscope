(() => {
  const url = URL.createObjectURL(new Blob([WORKER_SOURCE], { type: "text/javascript" }));

  const cards = new Map();
  let port = null;

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
    // oxlint-disable-next-line unicorn/require-post-message-target-origin -- a worker's postMessage takes no target origin
    worker.postMessage({ type: "load", source: renderer.source, name: renderer.name });
    cards.set(card, held);
    return held;
  }

  function workerFor(card, renderer) {
    const held = cards.get(card);
    if (held !== undefined && held.digest === renderer.digest) {
      return held;
    }
    release(card);
    return start(card, renderer);
  }

  const LIVE = new Set(["update", "resize", "pointer", "key", "frame", "ping"]);

  function receive(event) {
    const message = event.data;
    if (message?.type === "draw") {
      const { worker } = workerFor(message.card, message.renderer);
      const { id, input, context } = message;
      // oxlint-disable-next-line unicorn/require-post-message-target-origin -- a worker's postMessage takes no target origin
      worker.postMessage({ type: "draw", id, input, context });
    } else if (message?.type === "live-start") {
      const { worker } = workerFor(message.card, message.renderer);
      const { input, context, size } = message;
      // oxlint-disable-next-line unicorn/require-post-message-target-origin -- a worker's postMessage takes no target origin
      worker.postMessage({ type: "live-start", input, context, size });
    } else if (LIVE.has(message?.type)) {
      const { card, ...rest } = message;
      // oxlint-disable-next-line unicorn/require-post-message-target-origin -- a worker's postMessage takes no target origin
      cards.get(card)?.worker.postMessage(rest);
    } else if (message?.type === "release") {
      release(message.card);
    }
  }

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
