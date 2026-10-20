// How the connection behaves when the server goes away, refuses the
// browser, or speaks another version.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  Connection,
  leaveWhileCached,
  login,
  type TransportHandlers,
  tradeOnce,
} from "../src/connection";
import type { Link, ModelEvent } from "../src/model";
import { PROTOCOL_VERSION } from "../src/protocol";

const hello = (version = PROTOCOL_VERSION) =>
  JSON.stringify({
    type: "hello",
    version,
    connection: 1,
    role: "control",
    name: "tester",
    cwd: "/",
  });

/** A connection whose every attempt is recorded and driven by the test. */
function harness(authorized: () => Promise<boolean> = async () => true) {
  const attempts: TransportHandlers[] = [];
  const sent: string[] = [];
  const events: ModelEvent[] = [];
  const connection = new Connection({
    connect: (handlers) => {
      attempts.push(handlers);
      return { send: (text) => sent.push(text), close: () => handlers.close() };
    },
    authorized,
    emit: (event) => events.push(event),
    backoff: [100, 1000],
  });
  const links = () =>
    events.flatMap((event): Link[] => (event.type === "link" ? [event.link] : []));
  return { connection, attempts, sent, events, links };
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("a dropped connection", () => {
  it("fails what was asked and reconnects with backoff", async () => {
    const { connection, attempts, links } = harness();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    attempts[0]?.open();
    attempts[0]?.message(hello());
    const asked = connection.request("pause");

    attempts[0]?.close();
    await expect(asked).rejects.toMatchObject({ kind: "disconnected" });
    expect(links().at(-1)).toBe("reconnecting");
    await expect(connection.request("pause")).rejects.toMatchObject({ kind: "disconnected" });

    // The first retry waits the first delay, and the next the longer one.
    await vi.advanceTimersByTimeAsync(99);
    expect(attempts).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(attempts).toHaveLength(2);
    attempts[1]?.close();
    await vi.advanceTimersByTimeAsync(999);
    expect(attempts).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1);
    expect(attempts).toHaveLength(3);

    // A hello starts the delays over.
    attempts[2]?.open();
    attempts[2]?.message(hello());
    attempts[2]?.close();
    await vi.advanceTimersByTimeAsync(100);
    expect(attempts).toHaveLength(4);
    connection.stop();
  });

  it("retries when the server cannot even be asked", async () => {
    let up = false;
    const { connection, attempts } = harness(async () => {
      if (!up) {
        throw new Error("connection refused");
      }
      return true;
    });
    connection.start();
    await vi.advanceTimersByTimeAsync(100);
    expect(attempts).toHaveLength(0);
    up = true;
    await vi.advanceTimersByTimeAsync(1000);
    expect(attempts).toHaveLength(1);
    connection.stop();
  });

  it("stays down once stopped", async () => {
    const { connection, attempts } = harness();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    connection.stop();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(attempts).toHaveLength(1);
  });
});

describe("a refused browser", () => {
  it("stops trying when it holds no token", async () => {
    const { connection, attempts, links } = harness(async () => false);
    connection.start();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(attempts).toHaveLength(0);
    expect(links()).toEqual(["connecting", "unauthorized"]);
  });

  it("stops at a server of another protocol version", async () => {
    const { connection, attempts, events, links } = harness();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    attempts[0]?.open();
    attempts[0]?.message(hello(PROTOCOL_VERSION + 1));
    await vi.advanceTimersByTimeAsync(10_000);
    expect(attempts).toHaveLength(1);
    expect(links()).toEqual(["connecting", "incompatible"]);
    // The model never sees the foreign server's hello.
    expect(events.some((event) => event.type === "message")).toBe(false);
  });
});

describe("answers", () => {
  it("ignores unparseable frames and answers nobody asked for", async () => {
    const { connection, attempts, sent, events } = harness();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    attempts[0]?.open();
    const asked = connection.request("processes");
    attempts[0]?.message("{not json");
    attempts[0]?.message(JSON.stringify({ type: "result", id: 99, result: null }));
    const { id } = JSON.parse(sent[0] ?? "{}") as { id: number };
    attempts[0]?.message(
      JSON.stringify({ type: "error", id, error: { kind: "forbidden", message: "view only" } }),
    );
    await expect(asked).rejects.toMatchObject({ kind: "forbidden", message: "view only" });
    expect(events.filter((event) => event.type === "message")).toEqual([]);
    connection.stop();
  });
});

describe("a page the browser keeps to come back to", () => {
  const transition = (type: string, persisted: boolean) =>
    Object.assign(new Event(type), { persisted });

  it("leaves the session while kept, and rejoins when shown again", async () => {
    const { connection, attempts } = harness();
    const page = new EventTarget();
    leaveWhileCached(connection, page);
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    attempts[0]?.open();
    attempts[0]?.message(hello());

    // Leaving for good closes the socket anyway; only a kept page pauses.
    page.dispatchEvent(transition("pagehide", false));
    const asked = connection.request("pause");
    page.dispatchEvent(transition("pagehide", true));
    await expect(asked).rejects.toThrow("the connection to microscope closed");
    await vi.advanceTimersByTimeAsync(5000);
    expect(attempts).toHaveLength(1);

    page.dispatchEvent(transition("pageshow", true));
    await vi.advanceTimersByTimeAsync(0);
    expect(attempts).toHaveLength(2);
    connection.stop();
  });

  it("does not connect a page that had not", async () => {
    const { connection, attempts } = harness();
    const page = new EventTarget();
    leaveWhileCached(connection, page);
    page.dispatchEvent(transition("pagehide", true));
    page.dispatchEvent(transition("pageshow", true));
    await vi.advanceTimersByTimeAsync(0);
    expect(attempts).toHaveLength(0);
  });
});

describe("a join link", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  const answering = (status: number) =>
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(null, { status })),
    );

  it("is accepted or refused by the server", async () => {
    answering(204);
    await expect(login("good")).resolves.toBe(true);
    answering(403);
    await expect(login("other server's")).resolves.toBe(false);
  });

  it("is not called refused when the server could not answer", async () => {
    answering(502);
    await expect(login("good")).rejects.toThrow();
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("Failed to fetch");
      }),
    );
    await expect(login("good")).rejects.toThrow();
  });

  it("is traded again after a failure, but once after an answer", async () => {
    let up = false;
    const trade = vi.fn(async (token: string) => {
      if (!up) {
        throw new TypeError("Failed to fetch");
      }
      return token === "good";
    });
    const join = tradeOnce(trade);
    await expect(join("good")).rejects.toThrow();
    up = true;
    await expect(join("good")).resolves.toBe(true);
    await expect(join("good")).resolves.toBe(true);
    expect(trade).toHaveBeenCalledTimes(2);
  });
});

describe("a socket that closes late", () => {
  const transition = (type: string, persisted: boolean) =>
    Object.assign(new Event(type), { persisted });

  /** Like a browser socket: closing only asks, the close event comes
   * later, and sending while not open throws. */
  function lateHarness() {
    const sockets: { handlers: TransportHandlers; open: boolean; sent: number }[] = [];
    const events: ModelEvent[] = [];
    const connection = new Connection({
      connect: (handlers) => {
        const socket = { handlers, open: false, sent: 0 };
        sockets.push(socket);
        return {
          send: () => {
            if (!socket.open) {
              throw new DOMException("not open", "InvalidStateError");
            }
            socket.sent += 1;
          },
          close: () => {},
        };
      },
      authorized: async () => true,
      emit: (event) => events.push(event),
      backoff: [100, 1000],
    });
    const opened = (index: number) => {
      const socket = sockets[index];
      if (socket) {
        socket.open = true;
        socket.handlers.open();
        socket.handlers.message(hello());
      }
    };
    const closed = (index: number) => {
      const socket = sockets[index];
      if (socket) {
        socket.open = false;
        socket.handlers.close();
      }
    };
    const links = () =>
      events.flatMap((event): Link[] => (event.type === "link" ? [event.link] : []));
    const page = new EventTarget();
    leaveWhileCached(connection, page);
    const bounce = () => {
      page.dispatchEvent(transition("pagehide", true));
      page.dispatchEvent(transition("pageshow", true));
    };
    return { connection, sockets, opened, closed, links, bounce };
  }

  it("does not reconnect when the old socket closes during authorization", async () => {
    const { connection, sockets, opened, closed, links, bounce } = lateHarness();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    opened(0);
    bounce();
    closed(0);
    await vi.advanceTimersByTimeAsync(5000);
    expect(sockets).toHaveLength(2);
    expect(links()).not.toContain("reconnecting");
    connection.stop();
  });

  it("keeps the new socket when the old one closes after it", async () => {
    const { connection, sockets, opened, closed, links, bounce } = lateHarness();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    opened(0);
    bounce();
    await vi.advanceTimersByTimeAsync(0);
    expect(sockets).toHaveLength(2);

    // Asking before the new socket opens is refused, not thrown.
    await expect(connection.request("pause")).rejects.toMatchObject({ kind: "disconnected" });

    opened(1);
    closed(0);
    await vi.advanceTimersByTimeAsync(5000);
    expect(sockets).toHaveLength(2);
    expect(links()).not.toContain("reconnecting");
    const asked = connection.request("pause");
    expect(sockets[1]?.sent).toBe(1);
    connection.stop();
    await expect(asked).rejects.toMatchObject({ kind: "disconnected" });
  });
});
