// A session whose server is the test: it sees each request and answers
// when it chooses, so tests can hold an answer back.

import { vi } from "vitest";
import type { TransportHandlers } from "../src/connection";
import { PROTOCOL_VERSION, type ServerMessage } from "../src/protocol";
import { createSession, type Session } from "../src/store";

export interface Asked {
  id: number;
  method: string;
  params?: unknown;
}

export interface FakeServer {
  session: Session;
  /** Every request so far, in order. */
  requests: Asked[];
  /** Waits for the next request `matches` accepts that is not yet answered. */
  next(method: string): Promise<Asked>;
  answer(request: Asked, result: unknown): void;
  send(message: ServerMessage): void;
}

export async function fakeServer(): Promise<FakeServer> {
  let transport: TransportHandlers | null = null;
  const requests: Asked[] = [];
  const answered = new Set<number>();
  const session = createSession({
    connect: (handlers) => {
      transport = handlers;
      return {
        send: (text) => requests.push(JSON.parse(text) as Asked),
        close: () => undefined,
      };
    },
    authorized: async () => true,
  });
  session.connection.start();
  const handlers = await vi.waitFor(() => {
    if (!transport) {
      throw new Error("the page never connected");
    }
    return transport as TransportHandlers;
  });
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
      }),
    answer: (request, result) => send({ type: "result", id: request.id, result }),
    send,
  };
}
