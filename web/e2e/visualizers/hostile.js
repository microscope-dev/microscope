// A hostile renderer: it tries every way out of the sandbox we know of and
// draws what each attempt did, one text per probe, titled by its name. The
// test writes its listeners' ports over HTTP_PORT and UDP_PORT, and "live"
// over MODE to run the probes in a live renderer with WebGL in use, which
// says what each did in its caption.

const http = "http://127.0.0.1:HTTP_PORT";
const udp = Number("UDP_PORT");

/** What an attempt did: "absent" when the means is gone, "blocked" when it
 * threw or failed, and "sent" when nothing stopped it. */
async function attempt(present, run) {
  if (!present) {
    return "absent";
  }
  try {
    const result = await run();
    return result === false ? "blocked" : "sent";
  } catch {
    return "blocked";
  }
}

const settle = (promise, ms = 300) =>
  Promise.race([
    promise,
    new Promise((resolve) => {
      setTimeout(() => resolve("pending"), ms);
    }),
  ]);

// oxlint-disable-next-line no-eval -- a way back to the global scope it tries
const global = (0, eval)("this");
const recovered = (name) =>
  [
    globalThis[name],
    Object.getPrototypeOf(globalThis)?.[name],
    globalThis.constructor?.prototype?.[name],
    new Function("return this")()[name],
    global[name],
  ].find((found) => found !== undefined);

const probes = {
  fetch: () => attempt(recovered("fetch"), () => recovered("fetch")(`${http}/fetch`)),
  xhr: () =>
    attempt(recovered("XMLHttpRequest"), () => {
      const request = new (recovered("XMLHttpRequest"))();
      request.open("GET", `${http}/xhr`);
      request.send();
    }),
  websocket: () =>
    attempt(
      recovered("WebSocket"),
      () => new (recovered("WebSocket"))(`ws://127.0.0.1:HTTP_PORT/ws`),
    ),
  eventsource: () =>
    attempt(recovered("EventSource"), () => new (recovered("EventSource"))(`${http}/events`)),
  importScripts: () =>
    attempt(recovered("importScripts"), () => recovered("importScripts")(`${http}/import-scripts`)),
  dynamicImport: () => attempt(true, () => settle(import(`${http}/import.js`))),
  worker: () => attempt(recovered("Worker"), () => new (recovered("Worker"))(`${http}/worker.js`)),
  font: () =>
    attempt(recovered("FontFace"), () =>
      settle(new (recovered("FontFace"))("x", `url(${http}/font)`).load()),
    ),
  beacon: () =>
    attempt(recovered("navigator")?.sendBeacon, () =>
      recovered("navigator").sendBeacon(`${http}/beacon`, "x"),
    ),
  indexedDB: () => attempt(recovered("indexedDB"), () => recovered("indexedDB").open("x")),
  caches: () => attempt(recovered("caches"), () => settle(recovered("caches").open("x"))),
  broadcast: () =>
    attempt(recovered("BroadcastChannel"), () =>
      // oxlint-disable-next-line unicorn/require-post-message-target-origin -- a BroadcastChannel takes no target origin
      new (recovered("BroadcastChannel"))("microscope").postMessage("x"),
    ),
  webrtc: () =>
    attempt(recovered("RTCPeerConnection"), () => {
      const peer = new (recovered("RTCPeerConnection"))({
        iceServers: [{ urls: `stun:127.0.0.1:${udp}` }],
      });
      peer.createDataChannel("x");
      return peer.createOffer().then((offer) => peer.setLocalDescription(offer));
    }),
  webtransport: () =>
    attempt(
      recovered("WebTransport"),
      () => new (recovered("WebTransport"))(`https://127.0.0.1:${udp}/`),
    ),
  postMessage: () => attempt(recovered("postMessage"), () => recovered("postMessage")("x")),
  listen: () =>
    attempt(recovered("addEventListener"), () =>
      recovered("addEventListener")("message", () => {}),
    ),
  close: () => attempt(recovered("close"), () => recovered("close")()),
  location: () => attempt(recovered("location"), () => String(recovered("location"))),
};

async function outcomes() {
  const found = [];
  for (const [name, probe] of Object.entries(probes)) {
    found.push(`${name}: ${await probe()}`);
  }
  return found;
}

const mode = String("MODE");
if (mode === "live") {
  microscope.live(async (canvas) => {
    // A context drawing with the GPU while the probes run.
    const gl = canvas.getContext("webgl2");
    const kind = gl === null ? "2d" : "webgl2";
    if (gl === null) {
      canvas.getContext("2d")?.fillRect(0, 0, 1, 1);
    } else {
      gl.clearColor(0, 0.5, 0, 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
      const texture = gl.createTexture();
      gl.bindTexture(gl.TEXTURE_2D, texture);
      gl.texImage2D(
        gl.TEXTURE_2D,
        0,
        gl.RGBA,
        1,
        1,
        0,
        gl.RGBA,
        gl.UNSIGNED_BYTE,
        new Uint8Array(4),
      );
    }
    microscope.caption([kind, ...(await outcomes()), "probes done"].join(" · "));
    return { frame() {} };
  });
} else {
  microscope.draw(async () => {
    const shapes = [];
    let y = 12;
    for (const outcome of await outcomes()) {
      const name = outcome.split(":")[0];
      shapes.push(microscope.text({ x: 4, y, text: outcome, title: name, family: "mono" }));
      y += 14;
    }
    return microscope.picture({ width: 260, height: y, shapes, caption: "probes done" });
  });
}
