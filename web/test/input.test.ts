// What people type: debugger keys, program arguments, and environments.

import * as fc from "fast-check";
import { describe, expect, it } from "vitest";

import { COMMANDS } from "../src/ui/commands";
import { joinWords, parseEnvironment, splitWords } from "../src/words";
import { type Command, READY, registry } from "../src/workbench/commands";
import type { Chord, KeyLike } from "../src/workbench/keys";
import { altLetterOrDigit, keyPress } from "./arbitraries/keys";

const commandFor = COMMANDS.match;
const describeKey = COMMANDS.describe;

const key = (name: string, modifiers: Partial<KeyLike> = {}): KeyLike => ({
  key: name,
  shiftKey: false,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  ...modifiers,
});

const pressCode = (typed: string, code: string, modifiers: Partial<KeyLike>) =>
  commandFor({ ...key(typed, modifiers), code }, false);

describe("debugger keys", () => {
  it("maps VS Code's run keys whether or not a text box has focus", () => {
    for (const inTextBox of [false, true]) {
      expect(commandFor(key("F5"), inTextBox)).toBe("continue");
      expect(commandFor(key("F6"), inTextBox)).toBe("pause");
      expect(commandFor(key("F5", { shiftKey: true }), inTextBox)).toBe("kill");
      expect(commandFor(key("F5", { shiftKey: true, ctrlKey: true }), inTextBox)).toBe("restart");
    }
  });

  it("leaves letters to text boxes and other modifiers to the browser", () => {
    expect(commandFor(key("c"), false)).toBe("continue");
    expect(commandFor(key("c"), true)).toBeNull();
    expect(commandFor(key("c", { ctrlKey: true }), false)).toBeNull();
    expect(commandFor(key("F5", { ctrlKey: true }), false)).toBeNull();
    expect(commandFor(key("F5", { metaKey: true }), false)).toBeNull();
    expect(commandFor(key("F5", { altKey: true }), false)).toBeNull();
  });

  it("steps as VS Code does, and by letter", () => {
    expect(commandFor(key("F10"), true)).toBe("over");
    expect(commandFor(key("F11"), true)).toBe("into");
    expect(commandFor(key("F11", { shiftKey: true }), true)).toBe("out");
    expect(commandFor(key("n"), false)).toBe("over");
    expect(commandFor(key("N", { shiftKey: true }), false)).toBe("overInstruction");
    expect(commandFor(key("S", { shiftKey: true }), false)).toBe("instruction");
    expect(commandFor(key("?", { shiftKey: true }), false)).toBe("help");
  });

  it("reads Alt chords by the key pressed, whatever Alt makes it type", () => {
    expect(commandFor(key("¡", { altKey: true, code: "Digit1" }), false)).toBe("threads");
    expect(commandFor(key("∂", { altKey: true, code: "KeyD" }), true)).toBe("viewDisassembly");
  });

  it("never runs a command for a character typed into a text box", () => {
    fc.assert(
      fc.property(keyPress, (press) => {
        const typed = press.key.length === 1 && !press.ctrlKey && !press.altKey;
        fc.pre(typed);
        expect(commandFor(press, true)).toBeNull();
      }),
    );
  });

  it("leaves every press with Cmd or the Windows key to the browser", () => {
    fc.assert(
      fc.property(keyPress, fc.boolean(), (press, inTextBox) => {
        expect(commandFor({ ...press, metaKey: true }, inTextBox)).toBeNull();
      }),
    );
  });

  it("reads an Alt chord by its physical key, whatever Alt makes it type", () => {
    fc.assert(
      fc.property(altLetterOrDigit, fc.boolean(), ({ letter, typed, shiftKey }, inTextBox) => {
        const code = /\d/u.test(letter) ? `Digit${letter}` : `Key${letter.toUpperCase()}`;
        const press = { key: typed, code, shiftKey, ctrlKey: false, altKey: true, metaKey: false };
        expect(commandFor(press, inTextBox)).toBe(commandFor({ ...press, key: letter }, inTextBox));
      }),
    );
  });

  it("writes keys as tooltips show them", () => {
    expect(describeKey("continue")).toBe("F5");
    expect(describeKey("kill")).toBe("⇧F5");
    expect(describeKey("restart")).toBe("Ctrl+⇧F5");
    expect(describeKey("out")).toBe("⇧F11");
    expect(describeKey("palette")).toBe("Ctrl+K");
    expect(describeKey("over", true)).toBe("n");
    expect(describeKey("frameUp")).toBeNull();
    expect(describeKey("moveToGroupLeft")).toBe("Alt+⇧←");
  });

  it("lists keys for assistive technology in its own syntax", () => {
    expect(COMMANDS.aria("kill")).toBe("Shift+F5");
    expect(COMMANDS.aria("continue")).toBe("F5 c");
    expect(COMMANDS.aria("moveToGroupLeft")).toBe("Alt+Shift+ArrowLeft");
    expect(COMMANDS.aria("frameUp")).toBe("u");
  });

  it("reads bracket and backslash chords by the key pressed, whatever it types", () => {
    // Alt+Shift+] types ’ on a Mac, and Shift makes \ a |.
    expect(pressCode("’", "BracketRight", { altKey: true, shiftKey: true })).toBe("moveTabRight");
    expect(pressCode("‘", "BracketLeft", { altKey: true })).toBe("previousTab");
    expect(pressCode("\\", "Backslash", { ctrlKey: true })).toBe("splitRight");
    expect(pressCode("|", "Backslash", { ctrlKey: true, shiftKey: true })).toBe("splitDown");
    // Without Alt or Ctrl, a bracket is typed, not a chord.
    expect(pressCode("}", "BracketRight", { shiftKey: true })).toBeNull();
  });
});

const command = (keys: Chord[]): Command<null> => ({
  label: "x",
  menu: null,
  keys,
  available: () => READY,
  run: () => {},
});

describe("the command registry", () => {
  it("refuses a press bound to two commands", () => {
    expect(() => registry({ a: command([{ key: "F5" }]), b: command([{ key: "F5" }]) })).toThrow(
      "F5 is bound to a and to b",
    );
    // A letter is one press, with or without Shift.
    expect(() =>
      registry({ a: command([{ key: "?", shift: true }]), b: command([{ key: "?" }]) }),
    ).toThrow("? is bound to a and to b");
    expect(() =>
      registry({ a: command([{ key: "F5", shift: true }]), b: command([{ key: "F5" }]) }),
    ).not.toThrow();
  });

  it("offers every command in a menu, under a name of its own", () => {
    const labels = COMMANDS.ids.map((id) => COMMANDS.get(id).label);
    expect(new Set(labels).size).toBe(labels.length);
    expect(COMMANDS.ids.filter((id) => COMMANDS.get(id).menu === null)).toEqual([]);
  });
});

describe("program arguments", () => {
  it.each([
    ["", []],
    ["  a   b ", ["a", "b"]],
    [`'one two' "three four"`, ["one two", "three four"]],
    [`a\\ b`, ["a b"]],
    [`"say \\"hi\\" \\n"`, [`say "hi" \\n`]],
    [`'it''s'`, ["its"]],
    [`""`, [""]],
    [`--name=x"y z"`, ["--name=xy z"]],
    [`'$HOME' *.c`, ["$HOME", "*.c"]],
  ])("splits %j", (line, words) => {
    expect(splitWords(line)).toEqual({ ok: true, words });
  });

  it.each([
    [`"open`, `a " quote is not closed`],
    [`'open`, `a ' quote is not closed`],
    [`end\\`, "a backslash ends the line"],
  ])("refuses %j", (line, error) => {
    expect(splitWords(line)).toEqual({ ok: false, error });
  });

  it("joins words into a line that splits back into them", () => {
    const cases = [
      ["plain", "--flag=1", "./path/x"],
      ["two words", "", "it's", `"q"`, "a\\b", "$x"],
    ];
    for (const words of cases) {
      expect(splitWords(joinWords(words))).toEqual({ ok: true, words });
    }
    expect(joinWords(["plain", "two words"])).toBe("plain 'two words'");
  });
});

describe("environments", () => {
  it("reads NAME=VALUE lines, keeping later equals signs", () => {
    expect(parseEnvironment("A=1\n\n  B=x=y  \nEMPTY=\n")).toEqual({
      ok: true,
      pairs: [
        ["A", "1"],
        ["B", "x=y"],
        ["EMPTY", ""],
      ],
    });
  });

  it("names the line it cannot read", () => {
    expect(parseEnvironment("A=1\nnonsense")).toEqual({
      ok: false,
      error: "line 2 is not NAME=VALUE",
    });
    expect(parseEnvironment("=value")).toMatchObject({ ok: false });
  });
});
