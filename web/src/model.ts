// The page's view of the session, built only from what the server says.
// `reduce` is pure, so every behavior here is tested by replaying real
// server traffic (test/replay.test.ts).

import type { Hello, Notice, Person, ProfileInfo, ServerMessage, State, Stream } from "./protocol";

/** How the page is connected. */
export type Link =
  | "connecting"
  | "open"
  | "reconnecting"
  /** The browser has no token for this server: it needs a join link. */
  | "unauthorized"
  /** The server speaks another protocol version. */
  | "incompatible";

export interface OutputPiece {
  /** Counts pieces from the start, so trimming keeps every key. */
  seq: number;
  stream: Stream;
  text: string;
}

export interface Note extends Notice {
  /** Milliseconds since the epoch when it arrived. */
  at: number;
}

export interface Model {
  link: Link;
  hello: Hello | null;
  state: State | null;
  people: Person[];
  output: OutputPiece[];
  /** How many bytes `output` holds. */
  outputBytes: number;
  /** The next piece's sequence number. */
  outputSeq: number;
  notices: Note[];
  /** The profiles the server serves, whatever it debugs. */
  profiles: ProfileInfo[];
}

/** The most output a tab keeps; older pieces are dropped first. */
const OUTPUT_LIMIT = 1024 * 1024;

/** The most notices a tab keeps. */
const NOTICE_LIMIT = 50;

export const initialModel: Model = {
  link: "connecting",
  hello: null,
  state: null,
  people: [],
  output: [],
  outputBytes: 0,
  outputSeq: 0,
  notices: [],
  profiles: [],
};

export type ModelEvent =
  | { type: "link"; link: Link }
  | { type: "message"; message: ServerMessage; at: number };

export function reduce(model: Model, event: ModelEvent): Model {
  if (event.type === "link") {
    return { ...model, link: event.link };
  }
  const message = event.message;
  switch (message.type) {
    case "hello":
      // A new connection: the server resends its state and recent output.
      return { ...model, link: "open", hello: message, people: [], output: [], outputBytes: 0 };
    case "state": {
      const { type: _, ...state } = message;
      // Another program's output does not belong to this one.
      if (model.state && model.state.session !== state.session) {
        return { ...model, state, output: [], outputBytes: 0 };
      }
      return { ...model, state };
    }
    case "output":
      return appendOutput(model, message.stream, message.text);
    case "presence":
      return { ...model, people: message.people };
    case "notice": {
      const { type: _, ...notice } = message;
      const notices = [...model.notices, { ...notice, at: event.at }].slice(-NOTICE_LIMIT);
      return { ...model, notices };
    }
    case "profiles":
      return { ...model, profiles: message.profiles };
    case "result":
    case "error":
      // Answers go to the request that asked; the connection routes them.
      return model;
    default:
      return unknownMessage(message, model);
  }
}

/** Fails to compile when a known message type is missed, and ignores the
 * types a newer server may add. */
function unknownMessage(_message: never, model: Model): Model {
  return model;
}

function appendOutput(model: Model, stream: Stream, text: string): Model {
  const piece = { seq: model.outputSeq, stream, text };
  const output = [...model.output, piece];
  let bytes = model.outputBytes + piece.text.length;
  let first = 0;
  while (bytes > OUTPUT_LIMIT && first < output.length - 1) {
    bytes -= output[first]?.text.length ?? 0;
    first += 1;
  }
  return {
    ...model,
    output: first === 0 ? output : output.slice(first),
    outputBytes: bytes,
    outputSeq: model.outputSeq + 1,
  };
}

/** Whether this tab may run and stop the program. */
export function controls(model: Model): boolean {
  return model.hello?.role === "control" && model.link === "open";
}

/** The short name of what is being debugged, such as `kvstore`. */
export function targetName(state: State | null): string | null {
  const program = state?.target?.program;
  if (!program) {
    return null;
  }
  return program.slice(program.lastIndexOf("/") + 1);
}

/** A stop's kind, which the protocol spells as one word, in words. */
export function stopKind(kind: string): string {
  return kind.replace(/[A-Z]/g, (letter) => ` ${letter.toLowerCase()}`);
}

/**
 * The program and its arguments, short: each path by its file name, as an
 * interpreter's script is best known. The whole command is the target's.
 */
export function commandName(state: State | null): string | null {
  const program = targetName(state);
  if (!program) {
    return null;
  }
  // An option's value, as in `--out=/tmp/x`, stays whole.
  const shortened = (state?.target?.arguments ?? []).map((argument) =>
    argument.includes("/") && !argument.startsWith("-")
      ? argument.slice(argument.lastIndexOf("/") + 1)
      : argument,
  );
  return [program, ...shortened].join(" ");
}
