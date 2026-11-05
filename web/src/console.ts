// The console's scrollback and line history, which belong to this tab, and
// what a line asks for. History lasts across reloads, in this browser.
//
// Run control is the page's own: `next`, `continue`, and the rest run the
// actions the toolbar runs, so they follow the same rules, and the entry says
// where the program stopped. Everything else goes to microscope, which reads
// it as an expression or one of its commands.

import { createStore } from "zustand";

import type { ActionName } from "./actions";
import { assert } from "./assert";
import type { ConsoleResult, Inferior } from "./protocol";
import { read, write } from "./storage";

export interface Entry {
  id: number;
  line: string;
  /** The stop it ran at, when it ran at one. */
  stop: number | null;
  /** The answer, once it arrives. */
  result?: ConsoleResult;
  error?: string;
  /** What a run-control line did: where the program stopped, or that it
   * still runs. */
  said?: string;
}

export interface ConsoleState {
  entries: Entry[];
  /** Lines run, oldest first. */
  history: string[];
}

/** The most entries kept on screen and lines kept in history. */
const ENTRIES = 200;
const HISTORY = 100;
const KEY = "microscope-console-history";

const isLines = (value: unknown): value is string[] =>
  Array.isArray(value) && value.every((line) => typeof line === "string");

export const consoleStore = createStore<ConsoleState>(() => ({
  entries: [],
  history: read(KEY, [], isLines),
}));

let next = 1;

/** Adds a line being run, returning its entry's id. */
export function started(line: string, stop: number | null): number {
  const id = next++;
  consoleStore.setState((state) => {
    const history = [...state.history.filter((previous) => previous !== line), line].slice(
      -HISTORY,
    );
    write(KEY, history);
    return { entries: [...state.entries, { id, line, stop }].slice(-ENTRIES), history };
  });
  return id;
}

export function finished(
  id: number,
  answer: { result?: ConsoleResult; error?: string; said?: string },
): void {
  assert(Number.isInteger(id) && id >= 1 && id < next, `console entry ${id} was never started`);
  consoleStore.setState((state) => ({
    entries: state.entries.map((entry) => (entry.id === id ? { ...entry, ...answer } : entry)),
  }));
}

export function clearConsole(): void {
  consoleStore.setState({ entries: [] });
}

/** The longest text every candidate starts with. */
export function commonPrefix(labels: readonly string[]): string {
  if (labels.length === 0) {
    return "";
  }
  let prefix = labels[0];
  assert(prefix !== undefined, `${labels.length} candidates have no first`);
  for (const label of labels) {
    while (!label.startsWith(prefix)) {
      prefix = prefix.slice(0, -1);
    }
  }
  return prefix;
}

/** What a console line asks for. */
export type Asked =
  /** A run-control action, as the toolbar runs it. */
  | { kind: "action"; action: ActionName }
  /** `run`: start the program, or start it again. */
  | { kind: "start" }
  /** Run the thread shown until it reaches a location. */
  | { kind: "advance"; location: string }
  /** Show code in the Disassembly view: the frame's own, or elsewhere. */
  | { kind: "disassemble"; target: { address: string } | { name: string } | null }
  /** Step into one call of the line: a function's name or `*0xADDRESS`. */
  | { kind: "into"; call: string }
  | { kind: "clear" }
  /** A run-control word used wrong, and how to use it. */
  | { kind: "usage"; message: string }
  /** An expression or another command, which microscope reads. */
  | { kind: "server" };

/** The run-control words, which take no arguments. The terminal's names
 * and aliases, so its habits carry over. */
const ACTIONS: Readonly<Record<string, ActionName>> = {
  continue: "continue",
  c: "continue",
  next: "over",
  n: "over",
  step: "into",
  s: "into",
  finish: "out",
  fin: "out",
  f: "out",
  stepi: "instruction",
  si: "instruction",
  nexti: "overInstruction",
  ni: "overInstruction",
  pause: "pause",
  interrupt: "pause",
  kill: "kill",
  restart: "restart",
};

/** Actions an empty line runs again, as a terminal debugger repeats them. */
const REPEATED: ReadonlySet<ActionName> = new Set([
  "continue",
  "over",
  "into",
  "out",
  "instruction",
  "overInstruction",
]);

/** A function, `FILE:LINE`, `FILE:FUNCTION`, or `0xADDRESS`. */
const LOCATION = /^[\w.$/-]+(?::[\w.$]+)?$/u;
/** A function's name, such as `main`, `ns::fn`, or `std::vec::Vec<T>::push`. */
const SYMBOL = /^[a-z_][\w:.$<>~]*$/iu;
/** An address the link can hold. */
const ADDRESS = /^0x[0-9a-f]{1,16}$/iu;
/** A function the line calls, or the call's address. */
const CALL = /^(?:\*0x[0-9a-f]+|[a-z_][\w:]*)$/iu;

/**
 * What `line` asks for. A run-control word is a command only when the rest
 * of the line fits it, so `n + 1` and `c = 2` stay expressions.
 */
export function asked(line: string): Asked {
  const words = line.trim().split(/\s+/u);
  const [word = "", ...rest] = words;
  const action = Object.hasOwn(ACTIONS, word) ? ACTIONS[word] : undefined;
  if (action !== undefined) {
    if (rest.length === 0) {
      return { kind: "action", action };
    }
    if (action === "into" && rest.length === 1) {
      const [call = ""] = rest;
      if (call === "task") {
        return {
          kind: "usage",
          message: "the page cannot step into a new task yet; step over the line that starts it",
        };
      }
      if (CALL.test(call)) {
        return { kind: "into", call };
      }
    }
    return { kind: "server" };
  }
  switch (word) {
    case "run":
    case "r":
      return rest.length === 0 ? { kind: "start" } : { kind: "server" };
    case "advance":
    case "adv":
    case "until":
      if (rest.length === 0) {
        return {
          kind: "usage",
          message: `${word} runs to a location, such as ${word} kvstore.c:112 or ${word} main`,
        };
      }
      return rest.length === 1 && LOCATION.test(rest[0] ?? "")
        ? { kind: "advance", location: rest[0] ?? "" }
        : { kind: "server" };
    case "disassemble":
    case "disas": {
      // With an instruction count it stays microscope's listing.
      if (rest.length > 1) {
        return { kind: "server" };
      }
      const [target] = rest;
      if (target === undefined) {
        return { kind: "disassemble", target: null };
      }
      if (/^0x/iu.test(target)) {
        return ADDRESS.test(target)
          ? { kind: "disassemble", target: { address: target.toLowerCase() } }
          : { kind: "usage", message: `${target} is not an address such as 0x401136` };
      }
      return SYMBOL.test(target)
        ? { kind: "disassemble", target: { name: target } }
        : { kind: "server" };
    }
    case "clear":
    case "cls":
      return rest.length === 0 ? { kind: "clear" } : { kind: "server" };
    case "quit":
    case "q":
      return rest.length === 0
        ? { kind: "usage", message: "the page does not quit; Kill ends the program" }
        : { kind: "server" };
    default:
      return { kind: "server" };
  }
}

/** Whether an empty line runs `line` again. */
export function repeats(line: string): boolean {
  const wanted = asked(line);
  return wanted.kind === "action" && REPEATED.has(wanted.action);
}

/**
 * What the program did after a run-control line, or null while there is
 * nothing new to say: it still runs, or it is where it was.
 */
export function outcome(before: Inferior | undefined, after: Inferior | undefined): string | null {
  if (!after || after.state === "running") {
    return null;
  }
  if (before && sameStop(before, after)) {
    return null;
  }
  switch (after.state) {
    case "stopped": {
      const place = after.place;
      const where = place
        ? place.path && place.line
          ? `${place.path.slice(place.path.lastIndexOf("/") + 1)}:${place.line}`
          : (place.function ?? place.address)
        : "an unknown place";
      return `stop #${after.stop} · ${after.reason.description} · ${where}`;
    }
    case "exited":
      return `exited · ${after.description}`;
    case "notStarted":
      return "not started";
    case "detached":
      return `detached · process ${after.pid} runs on`;
  }
}

function sameStop(before: Inferior, after: Inferior): boolean {
  if (before.state === "stopped" && after.state === "stopped") {
    return before.stop === after.stop;
  }
  return before.state === after.state && before.state !== "stopped";
}

/** The run-control lines, which `help` adds to microscope's own list. */
export const RUN_HELP = `
Run control, which the page runs as its toolbar does:
  continue, c              Continue, or start a program not running
  next, n                  Step over the line
  step, s [function]       Step into the line, or into one of its calls
  finish, fin, f           Run until the selected frame returns
  stepi, si / nexti, ni    Step one instruction, into or over calls
  advance, until LOCATION  Run until the thread reaches a location
  run, r / restart         Start the program, or start it again
  disassemble, disas [FN]  Show code in the Disassembly view
  pause / kill             Stop the running program, or end it
  clear, cls, Ctrl+L       Clear the console
An empty line repeats the last step or continue.`;
