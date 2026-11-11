// The tab's one connection to `microscope web`: requests with typed answers,
// server messages turned into model events, and reconnecting after a drop.
// The transport is injected so tests drive it without a socket.

import { assert } from "./assert";
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
import { presented, remote, serverUrl, standalone } from "./target";

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

/** Where the connection is. Each attempt makes its own phase, and an event
 * from a transport or check counts only while its phase is the current one,
 * so a late close from a replaced socket changes nothing. */
type Phase =
  | { kind: "stopped" }
  /** Asking the server whether this browser may connect. */
  | { kind: "authorizing" }
  /** On a socket, open once the transport says so. `transport` is null
   * until `connect` returns. */
  | { kind: "socket"; transport: Transport | null; open: boolean }
  /** Waiting to try again. */
  | { kind: "waiting"; timer: ReturnType<typeof setTimeout> };

export class Connection {
  readonly #options: ConnectionOptions;
  #phase: Phase = { kind: "stopped" };
  /** Attempts that failed since the last hello, which pick the backoff. */
  #attempt = 0;
  /** Whether the server has said hello since `start`. Until it has, a
   * failed attempt means it cannot be reached, not that it was lost. */
  #reached = false;
  #nextId = 1;
  #pending = new Map<number, Pending>();
  /** Bytes sent ahead of the answers they belong to, by request. */
  #binary = new Map<number, Uint8Array>();

  constructor(options: ConnectionOptions) {
    this.#options = options;
  }

  /** Connects, and keeps reconnecting until stopped. */
  start(): void {
    if (this.#phase.kind !== "stopped") {
      return;
    }
    this.#reached = false;
    this.#options.emit({ type: "link", link: "connecting" });
    void this.#attemptConnection();
  }

  /** Whether it was started and not stopped since. */
  get started(): boolean {
    return this.#phase.kind !== "stopped";
  }

  stop(): void {
    const phase = this.#phase;
    this.#phase = { kind: "stopped" };
    if (phase.kind === "waiting") {
      clearTimeout(phase.timer);
    } else if (phase.kind === "socket") {
      phase.transport?.close();
    }
    this.#failPending();
  }

  /** Sends a request, resolving with its answer. */
  request<M extends Method>(
    method: M,
    ...params: ParamsOf<M> extends undefined ? [] : [ParamsOf<M>]
  ): Promise<Results[M]> {
    const phase = this.#phase;
    if (phase.kind !== "socket" || !phase.open || phase.transport === null) {
      return Promise.reject(new RequestError("disconnected", "not connected to microscope"));
    }
    const transport = phase.transport;
    const id = this.#nextId++;
    assert(!this.#pending.has(id), `request id ${id} is already pending`);
    assert(!this.#binary.has(id), `request id ${id} already has bytes`);
    const message = params.length === 0 ? { id, method } : { id, method, params: params[0] };
    return new Promise<Results[M]>((resolve, reject) => {
      this.#pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
      transport.send(JSON.stringify(message));
    });
  }

  async #attemptConnection(): Promise<void> {
    assert(
      this.#phase.kind !== "socket",
      `connection attempt ${this.#attempt} started on an open socket`,
    );
    const authorizing: Phase = { kind: "authorizing" };
    this.#phase = authorizing;
    let authorized: boolean;
    try {
      authorized = await this.#options.authorized();
    } catch {
      if (this.#phase === authorizing) {
        this.#lost();
      }
      return;
    }
    if (this.#phase !== authorizing) {
      return;
    }
    if (!authorized) {
      this.#phase = { kind: "stopped" };
      this.#options.emit({ type: "link", link: "unauthorized" });
      return;
    }
    const socket: Phase & { kind: "socket" } = { kind: "socket", transport: null, open: false };
    this.#phase = socket;
    const current = () => this.#phase === socket;
    const transport = this.#options.connect({
      open: () => {
        if (current()) {
          socket.open = true;
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
          this.#failPending();
          this.#lost();
        }
      },
    });
    if (current()) {
      socket.transport = transport;
    } else {
      // It closed, or was stopped, as it connected.
      transport.close();
    }
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
      this.#reached = true;
      if (message.version !== PROTOCOL_VERSION) {
        this.#options.emit({ type: "link", link: "incompatible" });
        this.stop();
        return;
      }
    }
    if (message.type === "result" || message.type === "error") {
      const pending = this.#pending.get(message.id);
      const payload = this.#binary.get(message.id);
      assert(
        payload === undefined || pending !== undefined,
        `bytes kept for request ${message.id}, which is not pending`,
      );
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

  /** Says the server is away, lost after it answered or never reached,
   * and waits to try again. */
  #lost(): void {
    this.#options.emit({ type: "link", link: this.#reached ? "reconnecting" : "unreachable" });
    const backoff = this.#options.backoff ?? [100, 250, 500, 1000, 2000];
    const delay = backoff[Math.min(this.#attempt, backoff.length - 1)];
    assert(delay !== undefined, `backoff has no delay for attempt ${this.#attempt}`);
    this.#attempt += 1;
    const waiting: Phase = {
      kind: "waiting",
      timer: setTimeout(() => {
        if (this.#phase === waiting) {
          void this.#attemptConnection();
        }
      }, delay),
    };
    this.#phase = waiting;
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

/** Connects a browser WebSocket to the page's server: the one that served
 * it, with its cookie, or the remote one, offering the token as a
 * subprotocol, the only header a page may give a WebSocket. */
export const browserConnect: Connect = (handlers) => {
  const url = new URL(serverUrl("api/ws"), location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  const token = presented();
  const socket = new WebSocket(url, token ? ["microscope", `microscope-token.${token}`] : []);
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

/** How long the server has to answer whether the browser may connect. A
 * host that drops packets, such as one that is off, would otherwise keep
 * the page waiting for the browser's own timeout, minutes away. */
const CHECK_TIMEOUT = 5000;

/** Asks the server whether this browser's cookie or token is good. A page
 * served on its own that has no server yet has neither. */
export const browserAuthorized: Authorized = async () => {
  if (standalone && !remote()) {
    return false;
  }
  const response = await fetch(serverUrl("api/check"), {
    method: "POST",
    credentials: "same-origin",
    body: presented(),
    signal: AbortSignal.timeout(CHECK_TIMEOUT),
  });
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
