// What a console line asks for: the run-control words the page runs itself,
// and everything else, which goes to microscope as it was typed.

import * as fc from "fast-check";
import { describe, expect, it } from "vitest";

import { asked, outcome, repeats } from "../src/console";
import type { Inferior, Place } from "../src/protocol";

const WORDS = [
  "continue",
  "c",
  "next",
  "n",
  "step",
  "s",
  "finish",
  "fin",
  "f",
  "stepi",
  "si",
  "nexti",
  "ni",
  "pause",
  "interrupt",
  "kill",
  "restart",
  "run",
  "r",
  "advance",
  "adv",
  "until",
  "clear",
  "cls",
  "quit",
  "q",
  "disassemble",
  "disas",
];
describe("a console line", () => {
  it.each([
    ["c", { kind: "action", action: "continue" }],
    ["continue", { kind: "action", action: "continue" }],
    ["n", { kind: "action", action: "over" }],
    ["  next  ", { kind: "action", action: "over" }],
    ["s", { kind: "action", action: "into" }],
    ["f", { kind: "action", action: "out" }],
    ["finish", { kind: "action", action: "out" }],
    ["si", { kind: "action", action: "instruction" }],
    ["nexti", { kind: "action", action: "overInstruction" }],
    ["interrupt", { kind: "action", action: "pause" }],
    ["kill", { kind: "action", action: "kill" }],
    ["restart", { kind: "action", action: "restart" }],
    ["r", { kind: "start" }],
    ["step hash", { kind: "into", call: "hash" }],
    ["s *0x401a2f", { kind: "into", call: "*0x401a2f" }],
    ["s ns::fn", { kind: "into", call: "ns::fn" }],
    ["until kvstore.c:112", { kind: "advance", location: "kvstore.c:112" }],
    ["advance main", { kind: "advance", location: "main" }],
    ["adv 0x401000", { kind: "advance", location: "0x401000" }],
    ["adv src/kv.c:table_find", { kind: "advance", location: "src/kv.c:table_find" }],
    ["cls", { kind: "clear" }],
    ["clear", { kind: "clear" }],
    ["disassemble", { kind: "disassemble", target: null }],
    ["disas table_find", { kind: "disassemble", target: { name: "table_find" } }],
    ["disas ns::Map<int>::get", { kind: "disassemble", target: { name: "ns::Map<int>::get" } }],
    ["disassemble 0x401A2F", { kind: "disassemble", target: { address: "0x401a2f" } }],
  ])("reads %j as the page's own", (line, wanted) => {
    expect(asked(line)).toEqual(wanted);
  });

  it.each([
    // Expressions that start with a run-control word's name.
    "n + 1",
    "c = 2",
    "s->key",
    "f(3)",
    "n*2",
    "step == 1",
    "until + 1",
    "r[0]",
    // Arguments the page's commands do not take: microscope says so.
    "continue 3",
    "next 2",
    "run --flag",
    "clear 1",
    // microscope's own commands.
    "print n",
    "p key",
    "info breakpoints",
    "help",
    "x/16xb key",
    "jump 79",
    // An instruction count keeps microscope's listing.
    "disas main 20",
    "disassemble 0x401000 8",
    "disas + 1",
  ])("sends %j to microscope", (line) => {
    expect(asked(line)).toEqual({ kind: "server" });
  });

  it.each([
    ["until", /runs to a location/u],
    ["advance", /runs to a location/u],
    ["step task", /cannot step into a new task/u],
    ["q", /does not quit/u],
    ["disas 0x12345678901234567", /not an address/u],
    ["disas 0xzz", /not an address/u],
  ])("says how to use %j", (line, message) => {
    const wanted = asked(line);
    expect(wanted.kind).toBe("usage");
    expect(wanted.kind === "usage" && wanted.message).toMatch(message);
  });

  it("sends any line not led by a run-control word to microscope unchanged", () => {
    fc.assert(
      fc.property(fc.string({ unit: "grapheme-ascii", maxLength: 40 }), (line) => {
        const [first = ""] = line.trim().split(/\s+/u);
        fc.pre(!WORDS.includes(first));
        expect(asked(line)).toEqual({ kind: "server" });
      }),
    );
  });

  it("keeps a run-control word followed by an operator an expression", () => {
    const operators = ["+", "-", "*", "/", "=", "==", "<", ">", "&", "|", "%", "^"];
    fc.assert(
      fc.property(
        fc.constantFrom(...WORDS),
        fc.constantFrom(...operators),
        fc.stringMatching(/^[a-z0-9 ]{0,8}$/u),
        (word, operator, rest) => {
          expect(asked(`${word} ${operator} ${rest}`).kind).toBe("server");
        },
      ),
    );
  });

  it("reads a line the same whatever spaces surround its words", () => {
    fc.assert(
      fc.property(
        fc.constantFrom(...WORDS),
        fc.constantFrom("", "main", "kvstore.c:112", "+ 1"),
        fc.stringMatching(/^[ \t]{0,3}$/u),
        fc.stringMatching(/^[ \t]{1,3}$/u),
        (word, rest, outer, inner) => {
          const plain = rest ? `${word} ${rest}` : word;
          const spaced = rest
            ? `${outer}${word}${inner}${rest}${outer}`
            : `${outer}${word}${outer}`;
          expect(asked(spaced)).toEqual(asked(plain));
        },
      ),
    );
  });
});

describe("an empty line", () => {
  it.each(["n", "next", "s", "c", "finish", "si", "ni"])("repeats %j", (line) => {
    expect(repeats(line)).toBe(true);
  });

  it.each(["p n", "kill", "restart", "r", "pause", "clear", "until main", "step hash", "n + 1"])(
    "does not repeat %j",
    (line) => {
      expect(repeats(line)).toBe(false);
    },
  );
});

const stopped = (stop: number, place: Partial<Place> = {}): Inferior => ({
  state: "stopped",
  pid: 7,
  stop,
  thread: 7,
  frame: 0,
  reason: { kind: "step", description: "stepped over" },
  place: {
    address: "0x401000",
    function: "table_insert",
    path: "/src/kvstore.c",
    line: 78,
    ...place,
  },
});

describe("what a run-control line did", () => {
  it("waits while the program runs or is where it was", () => {
    expect(outcome(stopped(2), { state: "running", pid: 7 })).toBeNull();
    expect(outcome(stopped(2), stopped(2))).toBeNull();
    expect(outcome({ state: "notStarted" }, { state: "notStarted" })).toBeNull();
  });

  it("names the new stop, its reason, and its line", () => {
    expect(outcome(stopped(2), stopped(3))).toBe("stop #3 · stepped over · kvstore.c:78");
  });

  it("names the function or address where there is no line", () => {
    expect(outcome(stopped(2), stopped(3, { path: null, line: null }))).toBe(
      "stop #3 · stepped over · table_insert",
    );
    expect(outcome(stopped(2), stopped(3, { path: null, line: null, function: null }))).toBe(
      "stop #3 · stepped over · 0x401000",
    );
  });

  it("says when the program ends", () => {
    expect(outcome(stopped(2), { state: "exited", description: "exited with status 0" })).toBe(
      "exited · exited with status 0",
    );
  });
});
