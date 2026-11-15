import { vi } from "vitest";

import type { TransportHandlers } from "../src/connection";
import { type ErrorKind, PROTOCOL_VERSION, type ServerMessage } from "../src/protocol";
import { createSession, type Session } from "../src/store";

export const POLL = { interval: 2 };

export interface Asked {
  id: number;
  method: string;
  params?: unknown;
}

export interface FakeServer {
  session: Session;

  requests: Asked[];

  next(method: string): Promise<Asked>;
  answer(request: Asked, result: unknown): void;

  refuse(request: Asked, kind: ErrorKind, message: string): void;
  send(message: ServerMessage): void;

  onRequest(listener: (request: Asked) => void): void;
}

export async function fakeServer(): Promise<FakeServer> {
  let transport: TransportHandlers | null = null;
  const requests: Asked[] = [];
  const answered = new Set<number>();
  const listeners: ((request: Asked) => void)[] = [];
  const session = createSession({
    connect: (handlers) => {
      transport = handlers;
      return {
        send: (text) => {
          const request = JSON.parse(text) as Asked;
          requests.push(request);
          queueMicrotask(() => {
            for (const listener of listeners) {
              listener(request);
            }
          });
        },
        close: () => {},
      };
    },
    authorized: () => Promise.resolve(true),
  });
  session.connection.start();
  const handlers = await vi.waitFor(() => {
    if (!transport) {
      throw new Error("the page never connected");
    }
    return transport as TransportHandlers;
  }, POLL);
  handlers.open();
  const send = (message: ServerMessage) => handlers.message(JSON.stringify(message));
  send({
    type: "hello",
    version: PROTOCOL_VERSION,
    connection: 1,
    role: "control",
    name: "tester",
    cwd: "/work",
  });
  return {
    session,
    requests,
    next: (method) =>
      vi.waitFor(() => {
        const found = requests.find(
          (request) => request.method === method && !answered.has(request.id),
        );
        if (!found) {
          throw new Error(`nothing asked ${method}`);
        }
        answered.add(found.id);
        return found;
      }, POLL),
    answer: (request, result) => send({ type: "result", id: request.id, result }),
    refuse: (request, kind, message) =>
      send({ type: "error", id: request.id, error: { kind, message } }),
    send,
    onRequest: (listener) => listeners.push(listener),
  };
}
