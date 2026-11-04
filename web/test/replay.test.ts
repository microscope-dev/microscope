// The page's state logic, fed what real servers sent.

import { describe, expect, it, vi } from "vitest";

import { action } from "../src/actions";
import { Connection, type TransportHandlers } from "../src/connection";
import {
  commandName,
  initialModel,
  type Model,
  type ModelEvent,
  reduce,
  stopKind,
} from "../src/model";
import type { ServerMessage, State, Target } from "../src/protocol";
import { shown } from "../src/ui/Status";
import { POLL } from "./fake-server";
import { binaries, received, transcript, transcriptNames } from "./transcripts";

function replay(messages: ServerMessage[]): Model[] {
  const models: Model[] = [];
  let model = initialModel;
  for (const [index, message] of messages.entries()) {
    model = reduce(model, { type: "message", message, at: index });
    models.push(model);
  }
  return models;
}

const output = (model: Model) => model.output.map((piece) => piece.text).join("");

describe("every recorded transcript", () => {
  it.each(transcriptNames())("%s replays into a consistent model", (name) => {
    const messages = received(name);
    expect(messages[0]?.type).toBe("hello");
    const models = replay(messages);
    for (const model of models) {
      // Every state names its session exactly when something is debugged.
      expect(model.state?.session === null).toBe(model.state?.target === null);
      // Each model is something the status and actions can describe.
      expect(shown(model).label).not.toBe("");
      for (const command of ["continue", "pause", "kill", "restart"] as const) {
        action(command, model);
      }
    }
    // Revisions within one session never go backward.
    let last: { session: string | null; revision: number } | null = null;
    for (const message of messages.filter((each) => each.type === "state")) {
      const floor = last?.session === message.session ? last.revision : Number.NEGATIVE_INFINITY;
      expect(message.revision).toBeGreaterThanOrEqual(floor);
      last = { session: message.session, revision: message.revision };
    }
  });
});

describe("a program run to its exit", () => {
  const models = replay(received("run-to-exit"));
  const final = models.at(-1) as Model;

  it("collects both streams in order", () => {
    expect(output(final)).toContain("out 1\n");
    expect(final.output.some((piece) => piece.stream === "stderr")).toBe(true);
  });

  it("offers to run it again once it exits", () => {
    expect(final.state?.inferior.state).toBe("exited");
    expect(shown(final).label).toBe("Exited");
    expect(action("continue", final).enabled).toBe(true);
    expect(action("pause", final).enabled).toBe(false);
  });

  it("names the program and its file arguments by their file names", () => {
    const state = final.state as State;
    const target = state.target as Target;
    const python = {
      ...state,
      target: {
        ...target,
        program: "/nix/store/x-python3/bin/python3.14",
        arguments: ["-X", "dev", "/home/me/work/inventory.py", "--count=3"],
      },
    };
    expect(commandName(python)).toBe("python3.14 -X dev inventory.py --count=3");
    const relative = {
      ...python,
      target: { ...python.target, arguments: ["./tests/views.py", "--out=/tmp/x", "a/b"] },
    } as State;
    expect(commandName(relative)).toBe("python3.14 views.py --out=/tmp/x b");
    expect(commandName(null)).toBeNull();
  });

  it("names what kind of stop each is in words", () => {
    expect(stopKind("breakpoint")).toBe("breakpoint");
    expect(stopKind("languageException")).toBe("language exception");
    expect(stopKind("watchpointArmFailed")).toBe("watchpoint arm failed");
  });

  it("waited for continue before it started", () => {
    const firstState = models.find((model) => model.state)?.state;
    expect(firstState?.inferior.state).toBe("notStarted");
  });
});

describe("two tabs racing a stop", () => {
  const models = replay(received("two-tabs"));

  it("continues the very stop it showed", async () => {
    const stopped = models.find((model) => model.state?.inferior.state === "stopped") as Model;
    const inferior = stopped.state?.inferior;
    const continuing = action("continue", stopped);
    if (!continuing.enabled || inferior?.state !== "stopped") {
      throw new Error("the transcript never offered continue at a stop");
    }
    const { connection, sent } = await openConnection();
    void continuing.run(connection);
    expect(JSON.parse(sent[0] ?? "{}")).toMatchObject({
      method: "continue",
      params: { stop: inferior.stop },
    });
  });

  it("hears who started the program", () => {
    const notices = (models.at(-1) as Model).notices;
    expect(notices.map((notice) => notice.text)).toContain("started the program");
  });
});

describe("a Start pane replacing a session", () => {
  const models = replay(received("picker"));

  it("drops the old program's output when another starts", () => {
    const sessions = new Set(models.map((model) => model.state?.session).filter(Boolean));
    expect(sessions.size).toBeGreaterThanOrEqual(2);
    const starts = models.filter((model, index) => {
      const previous = models[index - 1];
      return Boolean(previous?.state?.session) && model.state?.session !== previous?.state?.session;
    });
    expect(starts.map((model) => model.output)).toEqual(starts.map(() => []));
  });

  it("ends with nothing debugged after a failed launch", () => {
    const final = models.at(-1) as Model;
    expect(final.state?.session).toBeNull();
    expect(shown(final).label).toBe("Nothing loaded");
    expect(action("continue", final)).toMatchObject({ enabled: false });
  });
});

const messageEvent = (body: unknown, at = 0): ModelEvent => ({
  type: "message",
  message: body as ServerMessage,
  at,
});

describe("what a tab keeps", () => {
  it("keeps the latest 50 notices", () => {
    let model = initialModel;
    for (let at = 0; at < 60; at += 1) {
      model = reduce(
        model,
        messageEvent({ type: "notice", connection: 1, name: "ada", text: `${at}` }, at),
      );
    }
    expect(model.notices).toHaveLength(50);
    expect(model.notices[0]?.text).toBe("10");
    expect(model.notices.at(-1)?.text).toBe("59");
  });

  it("keeps the latest mebibyte of output, and always the newest piece", () => {
    let model = initialModel;
    const chunk = "x".repeat(256 * 1024);
    for (let index = 0; index < 6; index += 1) {
      model = reduce(model, messageEvent({ type: "output", stream: "stdout", text: chunk }));
    }
    expect(model.outputBytes).toBe(1024 * 1024);
    expect(model.output.map((piece) => piece.seq)).toEqual([2, 3, 4, 5]);
    const huge = "y".repeat(2 * 1024 * 1024);
    model = reduce(model, messageEvent({ type: "output", stream: "stderr", text: huge }));
    expect(model.output.map((piece) => piece.seq)).toEqual([6]);
    expect(model.outputBytes).toBe(huge.length);
  });

  it("ignores a message type it does not know", () => {
    const model = replay(received("steps")).at(-1) as Model;
    expect(reduce(model, messageEvent({ type: "fromTheFuture", detail: 1 }))).toBe(model);
  });
});

describe("the connection, driven by recorded traffic", () => {
  it.each(transcriptNames())("%s: each request gets its own answer", async (name) => {
    const lines = transcript(name);
    const frames = binaries(name);
    const events: ModelEvent[] = [];
    const { connection, sent, transport } = await openConnection((event) => events.push(event));
    const ask = connection.request.bind(connection) as (
      method: string,
      params?: unknown,
    ) => Promise<unknown>;
    // The recorded client numbered its requests its own way.
    const local = new Map<number, number>();
    const answers = new Map<number, Promise<unknown>>();
    for (const line of lines) {
      if (line.to === "server") {
        const { id, ...request } = line.message;
        const promise =
          "params" in request ? ask(request.method, request.params) : ask(request.method);
        local.set(id, (JSON.parse(sent.at(-1) ?? "{}") as { id: number }).id);
        answers.set(
          id,
          promise.then(
            (result) => ({ ok: result }),
            (error: Error) => ({ error: error.message }),
          ),
        );
      } else if ("binary" in line) {
        const bytes = frames.get(line.binary.id) ?? new Uint8Array();
        const frame = new Uint8Array(8 + bytes.length);
        new DataView(frame.buffer).setBigUint64(0, BigInt(local.get(line.binary.id) ?? 0), true);
        frame.set(bytes, 8);
        transport.binary?.(frame.buffer);
      } else if (line.message.type === "result" || line.message.type === "error") {
        transport.message(JSON.stringify({ ...line.message, id: local.get(line.message.id) }));
      } else {
        transport.message(JSON.stringify(line.message));
      }
    }
    for (const line of lines) {
      if (line.to !== "page" || !("message" in line)) {
        continue;
      }
      const { message } = line;
      if (message.type !== "result" && message.type !== "error") {
        continue;
      }
      // An answer's bytes come with it.
      const payload = frames.get(message.id);
      const wanted =
        message.type === "error"
          ? { error: message.error.message }
          : { ok: payload ? { ...(message.result as object), payload } : message.result };
      expect(await answers.get(message.id)).toEqual(wanted);
    }
    // Answers never reach the model; everything else does, in order.
    const forwarded = events.flatMap((event) => (event.type === "message" ? [event.message] : []));
    const expected = received(name).filter(
      (message) => message.type !== "result" && message.type !== "error",
    );
    expect(forwarded).toEqual(expected);
  });
});

/** A connection whose transport has opened, recording what it sends. */
async function openConnection(emit: (event: ModelEvent) => void = () => {}) {
  let handlers: TransportHandlers | null = null;
  const sent: string[] = [];
  const connection = new Connection({
    connect: (given) => {
      handlers = given;
      return { send: (text) => sent.push(text), close: () => {} };
    },
    authorized: () => Promise.resolve(true),
    emit,
  });
  connection.start();
  const transport = await vi.waitFor(() => {
    if (!handlers) {
      throw new Error("the connection never connected");
    }
    return handlers as TransportHandlers;
  }, POLL);
  transport.open();
  return { connection, sent, transport };
}

describe("a program whose children join the session", () => {
  const states = replay(received("group")).flatMap((model) => (model.state ? [model.state] : []));

  it("shows the child that stopped, and then the process selected", () => {
    const stopped = states.find(
      (state) => state.group?.stopped === 2 && state.inferior.state === "stopped",
    );
    expect(stopped?.group?.selected).toBe(2);
    expect(stopped?.group?.members.map((member) => member.joined)).toEqual(["launched", "forked"]);
    expect(states.some((state) => state.group?.selected === 1 && state.group.stopped === 2)).toBe(
      true,
    );
    expect(states.at(-1)?.group?.members.every((member) => member.state === "exited")).toBe(true);
  });
});
