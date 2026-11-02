// A debugger the test scripts: it serves one program's source and walks
// through a list of stops, one per continue or step, answering the page's
// requests the way `microscope web` does. Its answers are typed against the
// protocol, so a protocol change breaks the script at compile time rather
// than leaving it to drift. A request it has no answer for is refused and
// kept in `unhandled`, which a test expects to be empty: a journey says
// everything its page asks.

import type {
  Frame,
  Method,
  ParamsOf,
  Place,
  Results,
  Row,
  Scope,
  SourceLine,
  State,
  StopEntry,
} from "../../src/protocol";
import type { Asked, FakeServer } from "../fake-server";

export interface ScriptedStop {
  place: Place & { path: string; line: number };
  /** Why it stopped, as the server says: `breakpoint`, `step`. */
  reason: { kind: string; description: string };
  /** What led to it, as the stop history says: `stepped over`. */
  action: string;
  frames: Frame[];
  scopes: Scope[];
}

export interface Script {
  program: string;
  /** Source files by path, as the debug information names them. */
  files: Record<string, string>;
  entry: SourceLine;
  stops: ScriptedStop[];
}

type Handlers = { [M in Method]?: (params: ParamsOf<M>) => Results[M] };

const SESSION = "k1";
const PID = 4242;
const THREAD = 4243;

export class ScriptedDebugger {
  /** Requests the script has no answer for. */
  readonly unhandled: Asked[] = [];
  readonly #server: FakeServer;
  readonly #script: Script;
  #revision = 0;
  /** The script's stop the program is at, by index; -1 before it runs. */
  #at = -1;
  #running = false;
  #breakpoints: State["breakpoints"] = [];
  #history: StopEntry[] = [];

  constructor(server: FakeServer, script: Script) {
    this.#server = server;
    this.#script = script;
    server.onRequest((request) => this.#receive(request));
    this.#publish();
  }

  /** The stop number the page sees for the script's stop at `index`. */
  static stopNumber(index: number): number {
    return index + 1;
  }

  #handlers(): Handlers {
    const files = this.#script.files;
    return {
      setFocus: () => null,
      exceptions: () => ({ filters: [] }),
      sources: () => ({ files: Object.keys(files), entry: this.#script.entry, note: null }),
      source: ({ path }) => {
        const text = files[path];
        if (text === undefined) {
          throw new Refusal("failed", `no file ${path}`);
        }
        const lines = text.split("\n").length;
        return {
          path,
          read: path,
          text,
          breakable: Array.from({ length: lines }, (_, i) => i + 1),
        };
      },
      addBreakpoint: ({ location }) => {
        const id = this.#breakpoints.length + 1;
        const [file, line] = location.split(":");
        const path = Object.keys(files).find((candidate) => candidate.endsWith(`/${file}`));
        const stop = this.#script.stops.find(
          (stop) => stop.place.path === path && stop.place.line === Number(line),
        );
        this.#breakpoints = [
          ...this.#breakpoints,
          {
            id,
            location,
            condition: null,
            hitCondition: null,
            logMessage: null,
            hits: 0,
            places: stop ? [stop.place] : [],
          },
        ];
        this.#later(() => this.#publish());
        return { id };
      },
      continue: () => this.#advance(),
      step: ({ stop }) => {
        this.#current(stop);
        return this.#advance();
      },
      backtrace: ({ stop }) => ({
        frames: this.#current(stop).frames,
        folds: [],
        incomplete: null,
      }),
      scopes: ({ stop }) => ({ scopes: this.#current(stop).scopes }),
      // The values the script has, by the expression that reads them again;
      // others fail as the debugger fails on what it cannot read.
      evaluate: ({ stop, expression }) => {
        const rows = this.#current(stop).scopes.flatMap((scope) => scope.rows);
        const row = rows.find((candidate) => candidate.path === expression);
        if (!row) {
          throw new Refusal("failed", `the script has no value for ${expression}`);
        }
        return row;
      },
      registers: ({ stop }) => {
        this.#current(stop);
        return { registers: [] };
      },
      tasks: ({ stop }) => {
        this.#current(stop);
        return { noun: null, tasks: [], more: false, gaps: [] };
      },
    };
  }

  #receive(request: Asked): void {
    const handler = this.#handlers()[request.method as Method] as
      | ((params: unknown) => unknown)
      | undefined;
    if (!handler) {
      this.unhandled.push(request);
      this.#server.refuse(request, "unsupported", `the script has no ${request.method}`);
      return;
    }
    try {
      this.#server.answer(request, handler(request.params));
    } catch (error) {
      if (!(error instanceof Refusal)) {
        throw error;
      }
      this.#server.refuse(request, error.kind, error.message);
    }
  }

  /** The stop a request names, which must be the current one. */
  #current(stop: number): ScriptedStop {
    const current = this.#script.stops[this.#at];
    if (this.#running || !current || ScriptedDebugger.stopNumber(this.#at) !== stop) {
      throw new Refusal("staleStop", "the requested stopped snapshot is no longer current");
    }
    return current;
  }

  /** Runs to the next stop: the page sees it run, then stop. */
  #advance(): null {
    this.#running = true;
    this.#later(() => {
      this.#publish();
      this.#at += 1;
      this.#running = false;
      const stop = this.#script.stops[this.#at];
      if (stop) {
        this.#history = [
          ...this.#history,
          {
            stop: ScriptedDebugger.stopNumber(this.#at),
            thread: THREAD,
            reason: stop.reason,
            frame: 0,
            place: stop.place,
            by: "tester",
            action: stop.action,
          },
        ];
      }
      this.#publish();
    });
    return null;
  }

  /** After the answer the page is waiting on, as the server orders them. */
  #later(run: () => void): void {
    queueMicrotask(() => queueMicrotask(run));
  }

  #publish(): void {
    const stop = this.#script.stops[this.#at];
    const started = this.#at >= 0 || this.#running;
    let inferior: State["inferior"];
    if (this.#running) {
      inferior = { state: "running", pid: PID };
    } else if (stop) {
      inferior = {
        state: "stopped",
        pid: PID,
        stop: ScriptedDebugger.stopNumber(this.#at),
        thread: THREAD,
        reason: stop.reason,
        frame: 0,
        place: stop.place,
      };
    } else if (started) {
      inferior = { state: "exited", description: "exited with status 0" };
    } else {
      inferior = { state: "notStarted" };
    }
    this.#revision += 1;
    this.#server.send({
      type: "state",
      session: SESSION,
      target: { kind: "launch", program: this.#script.program, arguments: [], pid: PID },
      busy: null,
      revision: this.#revision,
      inferior,
      threads: started
        ? [{ id: THREAD, name: "kvstore", stopped: !this.#running, labels: [] }]
        : [],
      breakpoints: this.#breakpoints,
      stops: this.#history,
      writes: 0,
      watchpoints: [],
      settings: 0,
      group: null,
    });
  }
}

class Refusal extends Error {
  readonly kind: "staleStop" | "failed";

  constructor(kind: "staleStop" | "failed", message: string) {
    super(message);
    this.kind = kind;
  }
}

/** A frame of C code at `line` of `path`. */
export function cFrame(index: number, name: string, path: string, line: number): Frame {
  return {
    index,
    name,
    kind: "physical",
    address: `0x${(0x401000 + line * 16).toString(16)}`,
    module: "kvstore",
    source: { path, line, column: null, first: null },
    language: { mark: "c", name: "C" },
    unfollowed: null,
  };
}

/** A value row with no children, as C scalars and null pointers have. */
export function scalar(name: string, type: string, text: string): Row {
  return {
    name,
    text,
    detail: null,
    type,
    path: name,
    children: null,
    editable: true,
    memory: null,
    memoryBytes: null,
    truncated: false,
    drawings: [],
  };
}

const KVSTORE = "/work/tests/fixtures/c/kvstore.c";

/** kvstore stopped in handle_request at its breakpoint, then a line on. The
 * source is the real fixture's, so lines match the real program's. */
export function kvstore(source: string): Script {
  const lineOf = (text: string) => {
    const index = source.split("\n").findIndex((line) => line.includes(text));
    if (index < 0) {
      throw new Error(`kvstore.c has no line with ${text}`);
    }
    return index + 1;
  };
  const caller = cFrame(1, "worker", KVSTORE, lineOf("int status = handle_request("));
  const at = (line: number, status: string): Pick<ScriptedStop, "frames" | "scopes"> => ({
    frames: [cFrame(0, "handle_request", KVSTORE, line), caller],
    scopes: [
      {
        key: "args",
        name: "Arguments",
        rows: [
          scalar("s", "server *", "0x0000555555558060"),
          scalar("req", "const request *", "0x00007ffff7bfee20"),
        ],
        problem: null,
        note: null,
      },
      {
        key: "locals",
        name: "Locals",
        rows: [scalar("e", "entry *", "0x0000000000000000"), scalar("status", "int", status)],
        problem: null,
        note: null,
      },
    ],
  });
  const breakpoint = lineOf("int status = 0;");
  const next = lineOf("switch (req->op) {");
  const place = (line: number) => ({
    address: `0x${(0x401000 + line * 16).toString(16)}`,
    function: "handle_request",
    path: KVSTORE,
    line,
  });
  return {
    program: "/work/build/test-programs/kvstore",
    files: { [KVSTORE]: source },
    entry: { path: KVSTORE, line: lineOf("int main(void) {"), column: null, first: null },
    stops: [
      {
        place: place(breakpoint),
        reason: { kind: "breakpoint", description: "stopped at breakpoint 1 (hit 1)" },
        action: "started the program",
        ...at(breakpoint, "21845"),
      },
      {
        place: place(next),
        reason: { kind: "step", description: "stopped after source step" },
        action: "stepped over",
        ...at(next, "0"),
      },
    ],
  };
}
