import { assert } from "./assert";
import type { Hello, Notice, Person, ProfileInfo, ServerMessage, State, Stream } from "./protocol";

export type Link =
  | "connecting"
  | "open"
  | "reconnecting"
  | "unreachable"
  | "unauthorized"
  | "incompatible";

export interface OutputPiece {
  seq: number;
  stream: Stream;
  text: string;
}

export interface Note extends Notice {
  at: number;
}

export interface Model {
  link: Link;
  hello: Hello | null;
  state: State | null;
  people: Person[];
  output: OutputPiece[];

  outputBytes: number;

  outputSeq: number;
  notices: Note[];

  profiles: ProfileInfo[];
}

const OUTPUT_LIMIT = 1024 * 1024;

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
    assert(event.link !== "open", "the link opened without a hello");
    return { ...model, link: event.link };
  }
  const message = event.message;
  switch (message.type) {
    case "hello":
      return { ...model, link: "open", hello: message, people: [], output: [], outputBytes: 0 };
    case "state": {
      const { type: _, ...state } = message;

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
      return model;
    default:
      return unknownMessage(message, model);
  }
}

function unknownMessage(_message: never, model: Model): Model {
  return model;
}

function appendOutput(model: Model, stream: Stream, text: string): Model {
  const piece = { seq: model.outputSeq, stream, text };
  const output = [...model.output, piece];
  let bytes = model.outputBytes + piece.text.length;
  let first = 0;
  while (bytes > OUTPUT_LIMIT && first < output.length - 1) {
    const dropped = output[first];
    assert(dropped !== undefined, `output piece ${first} of ${output.length} is missing`);
    bytes -= dropped.text.length;
    first += 1;
  }
  assert(bytes >= piece.text.length, `output holds ${bytes} bytes, less than its last piece`);
  assert(
    output[first]?.seq === piece.seq - (output.length - 1 - first),
    `output pieces from ${first} are not numbered in order up to ${piece.seq}`,
  );
  return {
    ...model,
    output: first === 0 ? output : output.slice(first),
    outputBytes: bytes,
    outputSeq: model.outputSeq + 1,
  };
}

export function controls(model: Model): boolean {
  return model.hello?.role === "control" && model.link === "open";
}

export function targetName(state: State | null): string | null {
  const program = state?.target?.program;
  if (!program) {
    return null;
  }
  return program.slice(program.lastIndexOf("/") + 1);
}

export function stopKind(kind: string): string {
  return kind.replaceAll(/[A-Z]/gu, (letter) => ` ${letter.toLowerCase()}`);
}

export function commandName(state: State | null): string | null {
  const program = targetName(state);
  if (!program) {
    return null;
  }

  const shortened = (state?.target?.arguments ?? []).map((argument) =>
    argument.includes("/") && !argument.startsWith("-")
      ? argument.slice(argument.lastIndexOf("/") + 1)
      : argument,
  );
  return [program, ...shortened].join(" ");
}
