import { assert } from "../assert";

export interface RendererCode {
  name: string;
  digest: string;
  source: string;
}

export interface RendererFailure {
  message: string;
  file: string | null;
  line: number | null;
  column: number | null;
}

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

export interface DrawContext {
  previous: unknown;

  paths: Record<string, string | null>;
  width: number;
  theme: "light" | "dark";
  palette: Palette;
}

export interface LiveContext extends DrawContext {
  height: number;
}

export type Outcome =
  | { kind: "picture"; picture: unknown }
  | { kind: "live" }
  | { kind: "failed"; failure: RendererFailure }
  | { kind: "timeout" };

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

interface LiveKey {
  key: string;
  shift: boolean;
  ctrl: boolean;
  alt: boolean;
}

export type LiveFailure =
  | { kind: "failed"; failure: RendererFailure }
  | { kind: "unresponsive" }
  | { kind: "lost" };

export function failureText(failure: RendererFailure): string {
  const column = failure.column === null ? "" : `:${failure.column}`;
  const line = failure.line === null ? "" : `:${failure.line}${column}`;
  const where = failure.file ? `${failure.file}${line}: ` : "";
  return `${where}${failure.message}`;
}

export interface LiveListener {
  waiting(): void;

  started(): void;

  frame(bitmap: ImageBitmap | null): void;
  redraw(): void;
  animate(on: boolean): void;
  caption(text: string): void;
  hint(text: string | null): void;
  select(path: string): void;

  failed(failure: LiveFailure): void;
}

export interface LiveSession {
  frame(time: number): void;
  update(input: unknown, context: LiveContext): void;

  resize(width: number, height: number): void;
  pointer(event: LivePointer): void;
  key(event: LiveKey): void;

  stop(): void;
}

export const DRAW_TIMEOUT = 2_000;

export const READY_TIMEOUT = 5_000;

const PARALLEL = 4;

const LIVE_PARALLEL = 4;

const PING_EVERY = 1_000;

export const UNRESPONSIVE = 2_000;

const MOST_EARLY = 256;

const WATCH_EVERY = 100;

const MOST_TOLD = 1_000;

type FrameState =
  | { kind: "loading" }
  | { kind: "ready"; port: MessagePort }
  | { kind: "failed"; failure: RendererFailure }
  | { kind: "closed" };

const problem = (message: string): RendererFailure => ({
  message,
  file: null,
  line: null,
  column: null,
});

const NOT_STARTED = "the renderer frame did not start";
const LOST = "the renderer frame was lost";
const CLOSED = "the page closed the sandbox";

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

interface Live {
  listener: LiveListener;
  pong(): void;

  told(): void;
  end(failure: LiveFailure): void;
}

export class Sandbox {
  private readonly frame: HTMLIFrameElement;
  private state: FrameState = { kind: "loading" };

  private readonly loaded: Promise<void>;
  private settle: () => void = () => {};
  private readonly readyTimer: ReturnType<typeof setTimeout>;
  private readonly pending = new Map<number, Pending>();

  private readonly waiting: ((go: boolean) => void)[] = [];
  private running = 0;
  private next = 1;

  private readonly liveDigests = new Set<string>();

  private readonly lives = new Map<string, Live>();

  private readonly liveWaiting: ((go: boolean) => void)[] = [];
  private liveRunning = 0;
  private nextLive = 1;

  constructor(url: string) {
    this.frame = document.createElement("iframe");
    this.frame.setAttribute("sandbox", "allow-scripts");
    this.frame.hidden = true;
    this.frame.title = "renderers";
    this.loaded = new Promise((resolve) => {
      this.settle = resolve;
    });

    this.readyTimer = setTimeout(() => this.lose(NOT_STARTED), READY_TIMEOUT);
    let greeted = false;
    this.frame.addEventListener("load", () => {
      if (greeted) {
        this.lose(LOST);
        return;
      }
      greeted = true;
      const channel = new MessageChannel();
      channel.port1.addEventListener("message", (event) => {
        const message = event.data as FrameMessage;
        if (message.type !== "ready") {
          this.receive(message);
        } else if (this.state.kind === "loading") {
          clearTimeout(this.readyTimer);
          this.state = { kind: "ready", port: channel.port1 };
          this.settle();
        }
      });
      channel.port1.start();

      this.frame.contentWindow?.postMessage({ type: "hello" }, "*", [channel.port2]);
    });
    this.frame.src = url;
    document.body.append(this.frame);
  }

  get spent(): boolean {
    return this.state.kind === "failed" || this.state.kind === "closed";
  }

  async draw(
    card: string,
    renderer: RendererCode,
    input: unknown,
    context: DrawContext,
  ): Promise<Outcome> {
    if (this.liveDigests.has(renderer.digest)) {
      return { kind: "live" };
    }
    if (!(await this.turn())) {
      return { kind: "failed", failure: problem(CLOSED) };
    }
    assert(
      this.running >= 1 && this.running <= PARALLEL,
      `${this.running} draws run, not 1 to ${PARALLEL}`,
    );
    const state = await this.opened();
    if (state.kind !== "ready") {
      this.leave();
      return { kind: "failed", failure: this.failure(state) };
    }
    const { port } = state;
    const id = this.next++;
    assert(!this.pending.has(id), `draw ${id} is already pending`);
    return new Promise<Outcome>((resolve) => {
      const pending: Pending = {
        card,
        digest: renderer.digest,
        resolve,
        timer: setTimeout(() => {
          if (!this.frame.isConnected) {
            this.lose(LOST);
            return;
          }
          this.finish(id, { kind: "timeout" });
          this.release(card);
        }, DRAW_TIMEOUT),
      };
      this.pending.set(id, pending);
      port.postMessage({ type: "draw", card, id, renderer, input, context });
    });
  }

  live(
    card: string,
    renderer: RendererCode,
    input: unknown,
    context: LiveContext,
    size: { width: number; height: number },
    listener: LiveListener,
  ): LiveSession {
    const key = `${card}~live${this.nextLive++}`;
    let current = { input, context, size };
    let port: MessagePort | null = null;
    let ended = false;
    let asked: number | null = null;
    let pinged = 0;
    let told = 0;
    let watchdog: ReturnType<typeof setInterval> | null = null;

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
        assert(this.lives.get(key) === live, `live session ${key} is not in lives`);
        this.lives.delete(key);
        send({ type: "release" });
        this.liveRunning -= 1;
        assert(this.liveRunning >= 0, `${this.liveRunning} live sessions run`);
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
      assert(
        this.liveRunning >= 1 && this.liveRunning <= LIVE_PARALLEL,
        `${this.liveRunning} live sessions run, not 1 to ${LIVE_PARALLEL}`,
      );
      const opened = await this.opened();
      if (opened.kind !== "ready" || ended) {
        this.liveRunning -= 1;
        assert(this.liveRunning >= 0, `${this.liveRunning} live sessions run`);
        this.liveWaiting.shift()?.(true);
        if (opened.kind === "failed" && finish()) {
          listener.failed({ kind: "failed", failure: opened.failure });
        }
        return;
      }
      port = opened.port;
      assert(!this.lives.has(key), `live session ${key} is already in lives`);
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

  release(card: string): void {
    this.fail(card, {
      message: "the drawing was cancelled",
      file: null,
      line: null,
      column: null,
    });
    void (async () => {
      const state = await this.opened();
      if (state.kind === "ready") {
        const { port } = state;
        port.postMessage({ type: "release", card });
      }
    })();
  }

  dispose(): void {
    clearTimeout(this.readyTimer);
    this.state = { kind: "closed" };
    this.settle();
    for (const waiting of [...this.waiting.splice(0), ...this.liveWaiting.splice(0)]) {
      waiting(false);
    }
    for (const live of this.lives.values()) {
      live.end({
        kind: "failed",
        failure: problem(CLOSED),
      });
    }
    for (const id of this.pending.keys()) {
      this.finish(id, {
        kind: "failed",
        failure: problem(CLOSED),
      });
    }
    assert(this.lives.size === 0, `${this.lives.size} live sessions outlive the sandbox`);
    assert(this.pending.size === 0, `${this.pending.size} draws outlive the sandbox`);
    this.frame.remove();
  }

  private async opened(): Promise<FrameState> {
    await this.loaded;
    if (this.state.kind === "ready" && !this.frame.isConnected) {
      this.lose(LOST);
    }
    return this.state;
  }

  private failure(state: FrameState): RendererFailure {
    return state.kind === "failed" ? state.failure : problem(CLOSED);
  }

  private lose(message: string): void {
    if (this.state.kind === "failed" || this.state.kind === "closed") {
      return;
    }
    clearTimeout(this.readyTimer);
    const failure = problem(message);
    this.state = { kind: "failed", failure };
    this.settle();

    for (const live of this.lives.values()) {
      live.end({ kind: "failed", failure });
    }
    for (const id of this.pending.keys()) {
      this.finish(id, { kind: "failed", failure });
    }
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
        const digest = this.pending.get(message.id)?.digest;
        if (digest !== undefined) {
          this.liveDigests.add(digest);
        }
        this.finish(message.id, { kind: "live" });
        this.release(message.card);
        break;
      }
      case "error":
        if (message.id === null) {
          this.fail(message.card, message.error);
        } else {
          this.finish(message.id, { kind: "failed", failure: message.error });
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
    assert(pending.timer !== null, `draw ${id} has no timer`);
    clearTimeout(pending.timer);
    this.leave();
    pending.resolve(outcome);
  }

  private leave(): void {
    this.running -= 1;
    assert(this.running >= 0, `${this.running} draws run`);
    this.waiting.shift()?.(true);
  }

  private liveTurn(): Promise<boolean> {
    if (this.state.kind === "closed") {
      return Promise.resolve(false);
    }
    if (this.liveRunning < LIVE_PARALLEL) {
      this.liveRunning += 1;
      return Promise.resolve(true);
    }
    return new Promise((resolve) => {
      this.liveWaiting.push((go) => {
        if (go) {
          this.liveRunning += 1;
        }
        resolve(go);
      });
    });
  }

  private turn(): Promise<boolean> {
    if (this.state.kind === "closed") {
      return Promise.resolve(false);
    }
    if (this.running < PARALLEL) {
      this.running += 1;
      return Promise.resolve(true);
    }
    return new Promise((resolve) => {
      this.waiting.push((go) => {
        if (go) {
          this.running += 1;
        }
        resolve(go);
      });
    });
  }
}

const SERIES = {
  light: ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#6250d6", "#e34948"],
  dark: ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767"],
};

export function contextFor(
  previous: unknown,
  paths: Record<string, string | null>,
  width: number,
): DrawContext {
  const theme = currentTheme();
  return { previous, paths, width, theme, palette: palette(theme) };
}

export function currentTheme(): "light" | "dark" {
  const chosen = document.documentElement.dataset.theme;
  if (chosen === "light" || chosen === "dark") {
    return chosen;
  }
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

let colors: CanvasRenderingContext2D | null = null;

function sixDigits(color: string): string {
  colors ??= document.createElement("canvas").getContext("2d");
  if (colors === null) {
    return color;
  }
  colors.fillStyle = "#000000";
  colors.fillStyle = color;
  return String(colors.fillStyle);
}

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
