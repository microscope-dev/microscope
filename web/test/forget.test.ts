// What a tab forgets of its cached answers as the session, the stop, the
// program's values, its settings, or the connection change.

import { describe, expect, it } from "vitest";
import type { Connection } from "../src/connection";
import { Cache } from "../src/data";
import { forget, type Marks } from "../src/forget";
import { type Method, RequestError } from "../src/protocol";
import { Recall } from "../src/tree";

/** A cache whose every request is counted by method. */
function counted() {
  const asked = new Map<string, number>();
  const connection = {
    request: (method: string) => {
      asked.set(method, (asked.get(method) ?? 0) + 1);
      return Promise.resolve({});
    },
  } as unknown as Connection;
  const answers = new Cache();
  const ask = (method: Method) => answers.get(connection, method, { stop: 1 });
  /** The methods whose answers were forgotten, by asking each again. */
  const forgotten = (methods: readonly Method[]) =>
    methods.filter((method) => {
      const before = asked.get(method) ?? 0;
      ask(method);
      return (asked.get(method) ?? 0) > before;
    });
  return { answers, ask, forgotten };
}

const METHODS: readonly Method[] = [
  "sources",
  "modules",
  "functions",
  "scopes",
  "children",
  "evaluate",
  "readMemory",
  "registers",
  "tasks",
  "draw",
  "disassemble",
  "backtrace",
  "stepTargets",
  "signals",
  "exceptions",
  "renderers",
];

const base: Marks = {
  session: "a",
  latest: 1,
  writes: 0,
  settings: 0,
  connection: 1,
  open: true,
  member: 1,
};

function forgets(after: Partial<Marks>, before: Partial<Marks> = {}) {
  const { answers, ask, forgotten } = counted();
  for (const method of METHODS) {
    ask(method);
  }
  const values = new Recall();
  values.note(1, "x", "1");
  const cleared = forget({ ...base, ...before }, { ...base, ...after }, answers, values);
  return { cleared, forgot: forgotten(METHODS), values };
}

describe("forgetting cached answers", () => {
  it("forgets everything when the session changes", () => {
    const { cleared, forgot, values } = forgets({ session: "b" });
    expect(cleared).toBe(true);
    expect(forgot).toEqual(METHODS);
    expect(values.stops()).toBe(0);
  });

  it("forgets the program's files and functions at a new stop", () => {
    const { cleared, forgot } = forgets({ latest: 2 });
    expect(cleared).toBe(false);
    expect(forgot).toEqual(["sources", "modules", "functions"]);
  });

  it("forgets what a stop holds when a value or memory is written", () => {
    expect(forgets({ writes: 1 }).forgot).toEqual([
      "scopes",
      "children",
      "evaluate",
      "readMemory",
      "registers",
      "tasks",
      "draw",
      "disassemble",
      "backtrace",
      "stepTargets",
    ]);
  });

  it("forgets presentation and stop policies when settings change", () => {
    expect(forgets({ settings: 1 }).forgot).toEqual([
      "scopes",
      "children",
      "evaluate",
      "draw",
      "signals",
      "exceptions",
      "renderers",
    ]);
  });

  it("forgets value handles on a new connection", () => {
    expect(forgets({ connection: 2 }).forgot).toEqual([
      "scopes",
      "children",
      "evaluate",
      "readMemory",
      "registers",
      "tasks",
      "draw",
    ]);
  });

  it("forgets every answer when another process is selected, keeping values", () => {
    // Each process has its own stops, so its answers may share another's keys.
    const { cleared, forgot, values } = forgets({ member: 2 });
    expect(cleared).toBe(false);
    expect(forgot).toEqual(METHODS);
    expect(values.stops()).toBe(1);
  });

  it("keeps everything when nothing changed", () => {
    expect(forgets({}).forgot).toEqual([]);
  });
});

describe("the answer cache", () => {
  it("keeps only the most recently used answers", () => {
    let asked = 0;
    const connection = {
      request: () => {
        asked += 1;
        return Promise.resolve({});
      },
    } as unknown as Connection;
    const answers = new Cache();
    for (let stop = 0; stop <= 400; stop += 1) {
      answers.get(connection, "scopes", { stop });
    }
    expect(asked).toBe(401);
    // The newest is kept and the oldest went first.
    answers.get(connection, "scopes", { stop: 400 });
    expect(asked).toBe(401);
    answers.get(connection, "scopes", { stop: 0 });
    expect(asked).toBe(402);
  });

  it("forgets answers by prefix and tells its readers", () => {
    const { answers, ask, forgotten } = counted();
    ask("scopes");
    ask("sources");
    const version = answers.version();
    answers.forget("scopes ");
    expect(answers.version()).toBe(version + 1);
    expect(forgotten(["scopes", "sources"])).toEqual(["scopes"]);
    // Forgetting nothing changes nothing.
    answers.forget("nothing ");
    expect(answers.version()).toBe(version + 1);
  });

  it("asks again after a reconnect for what a dropped connection lost", async () => {
    let up = false;
    let asked = 0;
    const connection = {
      request: () => {
        asked += 1;
        return up
          ? Promise.resolve({})
          : Promise.reject(new RequestError("disconnected", "the connection closed"));
      },
    } as unknown as Connection;
    const answers = new Cache();
    const failed = answers.get(connection, "functions", { query: "main" });
    expect((await failed.promise).ok).toBe(false);
    const version = answers.version();

    // The drop itself does not tell readers to ask again, or they would
    // ask in a loop while the server is away.
    expect(answers.version()).toBe(version);

    up = true;
    forget({ ...base, open: false }, base, answers, new Recall());
    expect(answers.version()).toBeGreaterThan(version);
    const again = answers.get(connection, "functions", { query: "main" });
    expect((await again.promise).ok).toBe(true);
    expect(asked).toBe(2);
  });
});
