// The page's side of the sandbox: one hidden, sandboxed frame per tab,
// served by microscope at /visualizer-frame, which keeps a worker per card.
// The page talks to it over a port it hands the frame, never through the
// window, runs at most four draws at once, and ends a draw that takes
// longer than two seconds by ending its worker. A live renderer keeps a
// worker of its own while its card is on screen, at most four at once, and
// draws the frames the page asks for; one that stops answering, or floods
// the page with messages, is ended.

/** A renderer: its name, its JavaScript, and the digest that names both. */
export interface RendererCode {
  name: string;
  digest: string;
  source: string;
}

/** Where a renderer failed: its file and line when the browser says. */
export interface RendererFailure {
  message: string;
  file: string | null;
  line: number | null;
  column: number | null;
}

/** The colors `microscope.theme` holds, in the page's current theme. */
export interface Palette {
  ink: string;
  ink2: string;
  ink3: string;
  paper: string;
  surface: string;
  line: string;
  accent: string;
  changed: string;
  good: string;
  bad: string;
  series: string[];
}

/** What a renderer's function gets beside its inputs. */
export interface DrawContext {
  previous: unknown;
  /** The part of the drawn value each input is, or null. */
  paths: Record<string, string | null>;
  width: number;
  theme: "light" | "dark";
  palette: Palette;
}

/** What a live renderer's functions get beside its inputs; `width` and
 * `height` are the canvas's size in CSS pixels. */
export interface LiveContext extends DrawContext {
  height: number;
}

export type Outcome =
  | { kind: "picture"; picture: unknown }
  /** The renderer is live: it draws in a session of its own. */
  | { kind: "live" }
  | { kind: "failed"; failure: RendererFailure }
  | { kind: "timeout" };

/** The pointer as a live renderer sees it, in the canvas's pixels. */
export interface LivePointer {
  type: "down" | "move" | "up" | "wheel" | "leave";
  x: number;
  y: number;
  dx: number;
  dy: number;
  buttons: number;
  wheel: number;
  shift: boolean;
  ctrl: boolean;
  alt: boolean;
}

/** A key pressed while a live renderer's canvas has focus. */
interface LiveKey {
  key: string;
  shift: boolean;
  ctrl: boolean;
  alt: boolean;
}

/** Why a live session ended on its own. */
export type LiveFailure =
  | { kind: "failed"; failure: RendererFailure }
  | { kind: "unresponsive" }
  | { kind: "lost" };

/** Where a renderer failed and why, as the page shows it. */
export function failureText(failure: RendererFailure): string {
  const where = failure.file
    ? `${failure.file}${failure.line !== null ? `:${failure.line}${failure.column !== null ? `:${failure.column}` : ""}` : ""}: `
    : "";
  return `${where}${failure.message}`;
}

/** What a live session tells the page. */
export interface LiveListener {
  /** The session waits for one of the four that run to end. */
  waiting(): void;
  /** The session has its worker; it waits while four others run. */
  started(): void;
  /** A frame, or null when the renderer has drawn nothing yet. */
  frame(bitmap: ImageBitmap | null): void;
  redraw(): void;
  animate(on: boolean): void;
  caption(text: string): void;
  hint(text: string | null): void;
  select(path: string): void;
  /** The session ended, its worker with it. */
  failed(failure: LiveFailure): void;
}

/** A live renderer running for a card. */
export interface LiveSession {
  /** Asks for one frame; the listener's `frame` answers it. */
  frame(time: number): void;
  update(input: unknown, context: LiveContext): void;
  /** The canvas's new size, in device pixels. */
  resize(width: number, height: number): void;
  pointer(event: LivePointer): void;
  key(event: LiveKey): void;
  /** Ends the session and its worker. */
  stop(): void;
}

/** How long a draw may take before its worker is ended. */
export const DRAW_TIMEOUT = 2_000;
/** How many draws run at once. */
const PARALLEL = 4;
/** How many live renderers run at once. */
const LIVE_PARALLEL = 4;
/** How often a live worker is asked whether it still answers. */
const PING_EVERY = 1_000;
/** How long a live worker may take to answer before it is ended. */
export const UNRESPONSIVE = 2_000;
/** How many pointer and key events a session keeps until it starts. */
const MOST_EARLY = 256;
/** How often the watchdog looks. */
const WATCH_EVERY = 100;
/** How many messages besides frames a live worker may send between two of
 * the watchdog's looks. Its worker sends a few per message from the page. */
const MOST_TOLD = 1_000;

interface Pending {
  card: string;
  digest: string;
  resolve: (outcome: Outcome) => void;
  timer: ReturnType<typeof setTimeout> | null;
}

type FrameMessage =
  | { type: "ready" }
  | { type: "loaded"; card: string; error: RendererFailure | null }
  | { type: "picture"; card: string; id: number; picture: unknown }
  | { type: "live"; card: string; id: number }
  | { type: "error"; card: string; id: number | null; error: RendererFailure }
  | { type: "fatal"; card: string; error: RendererFailure }
  | { type: "frame"; card: string; bitmap: ImageBitmap | null }
  | { type: "redraw"; card: string }
  | { type: "animate"; card: string; on: boolean }
  | { type: "caption"; card: string; text: string }
  | { type: "hint"; card: string; text: string | null }
  | { type: "select"; card: string; path: string }
  | { type: "lost"; card: string }
  | { type: "pong"; card: string };

/** What a live session's worker says, by its card. */
interface Live {
  listener: LiveListener;
  pong(): void;
  /** Counts a message besides a frame. */
  told(): void;
  end(failure: LiveFailure): void;
}

export class Sandbox {
  private readonly frame: HTMLIFrameElement;
  /** The frame's port once it is ready, or null once the page closes it. */
  private readonly port: Promise<MessagePort | null>;
  private closePort: () => void = () => {};
  private readonly pending = new Map<number, Pending>();
  /** Draws waiting for a turn, told whether they may go ahead. */
  private readonly waiting: ((go: boolean) => void)[] = [];
  private running = 0;
  private disposed = false;
  private next = 1;
  /** Renderers known to be live, by digest: drawing one starts no worker. */
  private readonly liveDigests = new Set<string>();
  /** Running live sessions, by the card their worker is kept for. */
  private readonly lives = new Map<string, Live>();
  /** Live sessions waiting for one of the four to end. */
  private readonly liveWaiting: ((go: boolean) => void)[] = [];
  private liveRunning = 0;
  private nextLive = 1;

  /** Starts the frame at `url`, the server's /visualizer-frame. */
  constructor(url: string) {
    this.frame = document.createElement("iframe");
    this.frame.setAttribute("sandbox", "allow-scripts");
    this.frame.hidden = true;
    this.frame.title = "renderers";
    this.port = new Promise((resolve) => {
      this.closePort = () => resolve(null);
      this.frame.addEventListener(
        "load",
        () => {
          const channel = new MessageChannel();
          channel.port1.addEventListener("message", (event) => {
            const message = event.data as FrameMessage;
            if (message.type === "ready") {
              resolve(channel.port1);
            } else {
              this.receive(message);
            }
          });
          channel.port1.start();
          // The frame's origin is opaque, so it has no name to post to.
          this.frame.contentWindow?.postMessage({ type: "hello" }, "*", [channel.port2]);
        },
        { once: true },
      );
    });
    this.frame.src = url;
    document.body.append(this.frame);
  }

  /** Draws `input` with `renderer` in the worker of `card`, starting one
   * when the card has none or had another renderer. */
  async draw(
    card: string,
    renderer: RendererCode,
    input: unknown,
    context: DrawContext,
  ): Promise<Outcome> {
    const closed: Outcome = {
      kind: "failed",
      failure: { message: "the page closed the sandbox", file: null, line: null, column: null },
    };
    if (this.liveDigests.has(renderer.digest)) {
      return { kind: "live" };
    }
    if (!(await this.turn())) {
      return closed;
    }
    const port = await this.port;
    if (port === null || this.disposed) {
      this.leave();
      return closed;
    }
    const id = this.next++;
    return new Promise<Outcome>((resolve) => {
      const pending: Pending = { card, digest: renderer.digest, resolve, timer: null };
      pending.timer = setTimeout(() => {
        this.finish(id, { kind: "timeout" });
        this.release(card);
      }, DRAW_TIMEOUT);
      this.pending.set(id, pending);
      port.postMessage({ type: "draw", card, id, renderer, input, context });
    });
  }

  /** Runs the live `renderer` for `card` in a worker of its own, once
   * fewer than four others run, until the page stops it or it fails. */
  live(
    card: string,
    renderer: RendererCode,
    input: unknown,
    context: LiveContext,
    size: { width: number; height: number },
    listener: LiveListener,
  ): LiveSession {
    // A worker of its own: messages from an earlier session never reach it.
    const key = `${card}~live${this.nextLive++}`;
    let current = { input, context, size };
    let port: MessagePort | null = null;
    let ended = false;
    let asked: number | null = null;
    let pinged = 0;
    let told = 0;
    let watchdog: ReturnType<typeof setInterval> | null = null;
    // The pointer and keys sent while the session waits for its worker.
    const early: Record<string, unknown>[] = [];
    const send = (message: Record<string, unknown>, transfer: Transferable[] = []) => {
      port?.postMessage({ ...message, card: key }, transfer);
    };
    const given = (message: Record<string, unknown>) => {
      if (port !== null) {
        send(message);
      } else if (!ended && early.length < MOST_EARLY) {
        early.push(message);
      }
    };
    const finish = () => {
      if (ended) {
        return false;
      }
      ended = true;
      if (watchdog !== null) {
        clearInterval(watchdog);
      }
      if (port !== null) {
        this.lives.delete(key);
        send({ type: "release" });
        this.liveRunning -= 1;
        this.liveWaiting.shift()?.(true);
      }
      return true;
    };
    const live: Live = {
      listener,
      pong: () => {
        asked = null;
      },
      told: () => {
        told += 1;
      },
      end: (failure) => {
        if (finish()) {
          listener.failed(failure);
        }
      },
    };
    if (this.liveRunning >= LIVE_PARALLEL) {
      listener.waiting();
    }
    void (async () => {
      if (!(await this.liveTurn())) {
        return;
      }
      const opened = await this.port;
      if (opened === null || this.disposed || ended) {
        this.liveRunning -= 1;
        this.liveWaiting.shift()?.(true);
        return;
      }
      port = opened;
      this.lives.set(key, live);
      send({ type: "live-start", renderer, ...current });
      for (const message of early.splice(0)) {
        send(message);
      }
      pinged = performance.now();
      watchdog = setInterval(() => {
        const now = performance.now();
        if (told > MOST_TOLD) {
          live.end({
            kind: "failed",
            failure: {
              message: `the renderer sent more than ${MOST_TOLD} messages in ${WATCH_EVERY} ms`,
              file: null,
              line: null,
              column: null,
            },
          });
          return;
        }
        told = 0;
        if (asked !== null && now - asked >= UNRESPONSIVE) {
          live.end({ kind: "unresponsive" });
        } else if (asked === null && now - pinged >= PING_EVERY) {
          asked = now;
          pinged = now;
          send({ type: "ping" });
        }
      }, WATCH_EVERY);
      listener.started();
    })();
    return {
      frame: (time) => send({ type: "frame", time }),
      update: (next, nextContext) => {
        current = { ...current, input: next, context: nextContext };
        send({ type: "update", input: next, context: nextContext });
      },
      resize: (width, height) => {
        current = { ...current, size: { width, height } };
        send({ type: "resize", width, height });
      },
      pointer: (event) => given({ type: "pointer", event }),
      key: (event) => given({ type: "key", event }),
      stop: () => {
        finish();
      },
    };
  }

  /** Ends the card's worker, and with it any draw it has not answered. */
  release(card: string): void {
    this.fail(card, {
      message: "the drawing was cancelled",
      file: null,
      line: null,
      column: null,
    });
    void this.port.then((port) => port?.postMessage({ type: "release", card }));
  }

  /** Removes the frame, ending every worker. */
  dispose(): void {
    this.disposed = true;
    this.closePort();
    for (const waiting of [...this.waiting.splice(0), ...this.liveWaiting.splice(0)]) {
      waiting(false);
    }
    for (const live of [...this.lives.values()]) {
      live.end({
        kind: "failed",
        failure: { message: "the page closed the sandbox", file: null, line: null, column: null },
      });
    }
    for (const id of [...this.pending.keys()]) {
      this.finish(id, {
        kind: "failed",
        failure: { message: "the page closed the sandbox", file: null, line: null, column: null },
      });
    }
    this.frame.remove();
  }

  private receive(message: FrameMessage): void {
    if (message.type !== "ready" && this.lives.has(message.card)) {
      this.receiveLive(this.lives.get(message.card) as Live, message);
      return;
    }
    switch (message.type) {
      case "picture":
        this.finish(message.id, { kind: "picture", picture: message.picture });
        break;
      case "live": {
        // The card's worker found a live renderer; a session of its own
        // draws it, so this one ends.
        const digest = this.pending.get(message.id)?.digest;
        if (digest !== undefined) {
          this.liveDigests.add(digest);
        }
        this.finish(message.id, { kind: "live" });
        this.release(message.card);
        break;
      }
      case "error":
        if (message.id !== null) {
          this.finish(message.id, { kind: "failed", failure: message.error });
        } else {
          this.fail(message.card, message.error);
        }
        break;
      case "loaded":
        if (message.error !== null) {
          this.fail(message.card, message.error);
          this.release(message.card);
        }
        break;
      case "fatal":
        this.fail(message.card, message.error);
        this.release(message.card);
        break;
      default:
        break;
    }
  }

  private receiveLive(live: Live, message: FrameMessage): void {
    const { listener } = live;
    if (message.type !== "frame") {
      live.told();
    }
    switch (message.type) {
      case "frame":
        listener.frame(message.bitmap);
        break;
      case "redraw":
        listener.redraw();
        break;
      case "animate":
        listener.animate(message.on);
        break;
      case "caption":
        listener.caption(message.text);
        break;
      case "hint":
        listener.hint(message.text);
        break;
      case "select":
        listener.select(message.path);
        break;
      case "pong":
        live.pong();
        break;
      case "lost":
        live.end({ kind: "lost" });
        break;
      case "loaded":
        if (message.error !== null) {
          live.end({ kind: "failed", failure: message.error });
        }
        break;
      case "error":
      case "fatal":
        live.end({ kind: "failed", failure: message.error });
        break;
      default:
        break;
    }
  }

  /** Fails every draw of `card`, as when its renderer did not load. */
  private fail(card: string, failure: RendererFailure): void {
    for (const [id, pending] of this.pending) {
      if (pending.card === card) {
        this.finish(id, { kind: "failed", failure });
      }
    }
  }

  private finish(id: number, outcome: Outcome): void {
    const pending = this.pending.get(id);
    if (pending === undefined) {
      return;
    }
    this.pending.delete(id);
    if (pending.timer !== null) {
      clearTimeout(pending.timer);
    }
    this.leave();
    pending.resolve(outcome);
  }

  /** Ends a draw's turn, giving it to the next that waits. */
  private leave(): void {
    this.running -= 1;
    this.waiting.shift()?.(true);
  }

  /** Waits until fewer than LIVE_PARALLEL live sessions run, and says
   * whether this one may start. */
  private liveTurn(): Promise<boolean> {
    if (this.disposed) {
      return Promise.resolve(false);
    }
    if (this.liveRunning < LIVE_PARALLEL) {
      this.liveRunning += 1;
      return Promise.resolve(true);
    }
    return new Promise((resolve) =>
      this.liveWaiting.push((go) => {
        if (go) {
          this.liveRunning += 1;
        }
        resolve(go);
      }),
    );
  }

  /** Waits until fewer than PARALLEL draws run, and says whether this
   * one may run: not once the sandbox is closed. */
  private turn(): Promise<boolean> {
    if (this.disposed) {
      return Promise.resolve(false);
    }
    if (this.running < PARALLEL) {
      this.running += 1;
      return Promise.resolve(true);
    }
    return new Promise((resolve) =>
      this.waiting.push((go) => {
        if (go) {
          this.running += 1;
        }
        resolve(go);
      }),
    );
  }
}

// The eight categorical hues, in the order validated for each theme.
const SERIES = {
  light: ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#6250d6", "#e34948"],
  dark: ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767"],
};

/** What a renderer's function gets, in the theme the page shows now. */
export function contextFor(
  previous: unknown,
  paths: Record<string, string | null>,
  width: number,
): DrawContext {
  const theme = currentTheme();
  return { previous, paths, width, theme, palette: palette(theme) };
}

/** The theme the page shows now. */
export function currentTheme(): "light" | "dark" {
  const chosen = document.documentElement.getAttribute("data-theme");
  if (chosen === "light" || chosen === "dark") {
    return chosen;
  }
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

let colors: CanvasRenderingContext2D | null = null;

/** `color` as `#rrggbb`, which a canvas writes an opaque color as; a
 * built stylesheet may shorten `#ffffff` to `#fff`. */
function sixDigits(color: string): string {
  colors ??= document.createElement("canvas").getContext("2d");
  if (colors === null) {
    return color;
  }
  colors.fillStyle = "#000000";
  colors.fillStyle = color;
  return String(colors.fillStyle);
}

/** The page's own colors, as its stylesheet gives them now. */
export function palette(theme: "light" | "dark"): Palette {
  const style = getComputedStyle(document.documentElement);
  const token = (name: string) => sixDigits(style.getPropertyValue(`--${name}`).trim());
  return {
    ink: token("ink"),
    ink2: token("ink-2"),
    ink3: token("ink-3"),
    paper: token("paper"),
    surface: token("surface"),
    line: token("line"),
    accent: token("accent"),
    changed: token("changed"),
    good: token("run"),
    bad: token("error"),
    series: SERIES[theme],
  };
}
