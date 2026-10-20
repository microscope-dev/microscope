// The tab's one connection to `microscope web`: requests with typed answers,
// server messages turned into model events, and reconnecting after a drop.
// The transport is injected so tests drive it without a socket.

import { base } from "./base";
import type { ModelEvent } from "./model";
import {
  type Method,
  type ParamsOf,
  PROTOCOL_VERSION,
  RequestError,
  type Results,
  type ServerMessage,
} from "./protocol";

export interface TransportHandlers {
  open(): void;
  message(text: string): void;
  /** A binary frame: an answer's bytes, sent just before the answer. */
  binary?(data: ArrayBuffer): void;
  close(): void;
}

export interface Transport {
  send(text: string): void;
  close(): void;
}

/** Opens a transport; each call is one attempt. */
export type Connect = (handlers: TransportHandlers) => Transport;

/** Whether the browser holds a token this server accepts. */
export type Authorized = () => Promise<boolean>;

interface Pending {
  resolve(value: unknown): void;
  reject(error: RequestError): void;
}

export interface ConnectionOptions {
  connect: Connect;
  authorized: Authorized;
  emit(event: ModelEvent): void;
  /** Delays between reconnection attempts, the last repeating. */
  backoff?: readonly number[];
  now?: () => number;
}

export class Connection {
  readonly #options: ConnectionOptions;
  #transport: Transport | null = null;
  #open = false;
  #stopped = true;
  /** Bumped by each attempt and by stop, so a late event from an older
   * socket is ignored. */
  #generation = 0;
  #attempt = 0;
  #nextId = 1;
  #pending = new Map<number, Pending>();
  /** Bytes sent ahead of the answers they belong to, by request. */
  #binary = new Map<number, Uint8Array>();
  #timer: ReturnType<typeof setTimeout> | null = null;

  constructor(options: ConnectionOptions) {
    this.#options = options;
  }

  /** Connects, and keeps reconnecting until stopped. */
  start(): void {
    if (!this.#stopped) {
      return;
    }
    this.#stopped = false;
    this.#options.emit({ type: "link", link: "connecting" });
    void this.#attemptConnection();
  }

  /** Whether it was started and not stopped since. */
  get started(): boolean {
    return !this.#stopped;
  }

  stop(): void {
    this.#stopped = true;
    this.#generation += 1;
    if (this.#timer !== null) {
      clearTimeout(this.#timer);
      this.#timer = null;
    }
    this.#transport?.close();
    this.#transport = null;
    this.#open = false;
    this.#failPending();
  }

  /** Sends a request, resolving with its answer. */
  request<M extends Method>(
    method: M,
    ...params: ParamsOf<M> extends undefined ? [] : [ParamsOf<M>]
  ): Promise<Results[M]> {
    if (!this.#open || this.#transport === null) {
      return Promise.reject(new RequestError("disconnected", "not connected to microscope"));
    }
    const id = this.#nextId++;
    const message = params.length === 0 ? { id, method } : { id, method, params: params[0] };
    return new Promise<Results[M]>((resolve, reject) => {
      this.#pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
      this.#transport?.send(JSON.stringify(message));
    });
  }

  async #attemptConnection(): Promise<void> {
    this.#generation += 1;
    const generation = this.#generation;
    const current = () => generation === this.#generation;
    let authorized: boolean;
    try {
      authorized = await this.#options.authorized();
    } catch {
      if (current()) {
        this.#retry();
      }
      return;
    }
    if (this.#stopped || !current()) {
      return;
    }
    if (!authorized) {
      this.#stopped = true;
      this.#options.emit({ type: "link", link: "unauthorized" });
      return;
    }
    this.#transport = this.#options.connect({
      open: () => {
        if (current()) {
          this.#open = true;
        }
      },
      message: (text) => {
        if (current()) {
          this.#receive(text);
        }
      },
      binary: (data) => {
        if (current()) {
          this.#receiveBinary(data);
        }
      },
      close: () => {
        if (current()) {
          this.#closed();
        }
      },
    });
  }

  /** Keeps a frame of bytes, an 8-byte little-endian request id and the
   * bytes, for the answer that follows it. */
  #receiveBinary(data: ArrayBuffer): void {
    if (data.byteLength < 8) {
      return;
    }
    const id = Number(new DataView(data).getBigUint64(0, true));
    if (this.#pending.has(id)) {
      this.#binary.set(id, new Uint8Array(data, 8));
    }
  }

  #receive(text: string): void {
    let message: ServerMessage;
    try {
      message = JSON.parse(text) as ServerMessage;
    } catch {
      return;
    }
    if (message.type === "hello") {
      this.#attempt = 0;
      if (message.version !== PROTOCOL_VERSION) {
        this.#options.emit({ type: "link", link: "incompatible" });
        this.stop();
        return;
      }
    }
    if (message.type === "result" || message.type === "error") {
      const pending = this.#pending.get(message.id);
      const payload = this.#binary.get(message.id);
      this.#binary.delete(message.id);
      if (pending) {
        this.#pending.delete(message.id);
        if (message.type === "result") {
          pending.resolve(
            payload && typeof message.result === "object" && message.result !== null
              ? { ...message.result, payload }
              : message.result,
          );
        } else {
          pending.reject(new RequestError(message.error.kind, message.error.message));
        }
      }
      return;
    }
    this.#options.emit({ type: "message", message, at: (this.#options.now ?? Date.now)() });
  }

  #closed(): void {
    this.#open = false;
    this.#transport = null;
    this.#failPending();
    if (!this.#stopped) {
      this.#options.emit({ type: "link", link: "reconnecting" });
      this.#retry();
    }
  }

  #retry(): void {
    const backoff = this.#options.backoff ?? [100, 250, 500, 1000, 2000];
    const delay = backoff[Math.min(this.#attempt, backoff.length - 1)] ?? 1000;
    this.#attempt += 1;
    this.#timer = setTimeout(() => {
      this.#timer = null;
      if (!this.#stopped) {
        void this.#attemptConnection();
      }
    }, delay);
  }

  #failPending(): void {
    for (const pending of this.#pending.values()) {
      pending.reject(new RequestError("disconnected", "the connection to microscope closed"));
    }
    this.#pending.clear();
    this.#binary.clear();
  }
}

/** Leaves the session while the browser keeps the page to come back to, with
 * its socket maybe still open, and rejoins when the page is shown again. */
export function leaveWhileCached(connection: Connection, page: EventTarget): void {
  let paused = false;
  page.addEventListener("pagehide", (event) => {
    if ((event as PageTransitionEvent).persisted && connection.started) {
      paused = true;
      connection.stop();
    }
  });
  page.addEventListener("pageshow", (event) => {
    if ((event as PageTransitionEvent).persisted && paused) {
      paused = false;
      connection.start();
    }
  });
}

/** Connects a browser WebSocket to this page's server. */
export const browserConnect: Connect = (handlers) => {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(`${scheme}//${location.host}${base}api/ws`);
  socket.binaryType = "arraybuffer";
  socket.addEventListener("open", () => handlers.open());
  socket.addEventListener("message", (event) => {
    if (typeof event.data === "string") {
      handlers.message(event.data);
    } else if (event.data instanceof ArrayBuffer) {
      handlers.binary?.(event.data);
    }
  });
  socket.addEventListener("close", () => handlers.close());
  return {
    send: (text) => socket.send(text),
    close: () => socket.close(),
  };
};

/** Asks the server whether this browser's cookie is good. */
export const browserAuthorized: Authorized = async () => {
  const response = await fetch(`${base}api/check`, { method: "POST", credentials: "same-origin" });
  if (response.status === 204) {
    return true;
  }
  if (response.status === 403) {
    return false;
  }
  throw new Error(`unexpected status ${response.status}`);
};

/** Trades a join link's token for this server's cookie: false when the server
 * refuses the token, and throws when it could not answer. */
export async function login(token: string): Promise<boolean> {
  const response = await fetch(`${base}api/login`, {
    method: "POST",
    credentials: "same-origin",
    body: token,
  });
  if (response.status === 204) {
    return true;
  }
  if (response.status === 400 || response.status === 403) {
    return false;
  }
  throw new Error(`unexpected status ${response.status}`);
}

/** Trades a token once its answer is known; a trade that could not reach the
 * server is forgotten, so trying again asks again. */
export function tradeOnce(trade: (token: string) => Promise<boolean> = login) {
  let trading: Promise<boolean> | null = null;
  return (token: string): Promise<boolean> => {
    if (!trading) {
      const asked = trade(token);
      trading = asked;
      asked.catch(() => {
        trading = null;
      });
    }
    return trading;
  };
}
