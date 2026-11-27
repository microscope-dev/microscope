import { cpp } from "@codemirror/lang-cpp";
import { go } from "@codemirror/lang-go";
import {
  javascriptLanguage,
  jsxLanguage,
  tsxLanguage,
  typescriptLanguage,
} from "@codemirror/lang-javascript";
import { rust } from "@codemirror/lang-rust";
import {
  HighlightStyle,
  Language,
  LanguageSupport,
  StreamLanguage,
  type StringStream,
  syntaxHighlighting,
} from "@codemirror/language";
import type { Extension } from "@codemirror/state";
import { highlightTree, tags } from "@lezer/highlight";

import { assert } from "../../assert";
import { PYTHON_KEYWORDS, pythonKeywordHolds } from "../../python";

const ZIG_KEYWORDS = new Set(
  (
    "addrspace align allowzero and anyframe anytype asm async await break callconv catch comptime " +
    "const continue defer else enum errdefer error export extern fn for if inline linksection " +
    "noalias noinline nosuspend opaque or orelse packed pub resume return struct suspend switch " +
    "test threadlocal try union unreachable usingnamespace var volatile while"
  ).split(" "),
);
const ZIG_TYPES =
  /^(?:[iu]\d+|f16|f32|f64|f80|f128|usize|isize|bool|void|noreturn|type|anyerror|comptime_int|comptime_float|c_\w+)$/u;

const zig = StreamLanguage.define<{ inString: false }>({
  name: "zig",
  startState: () => ({ inString: false }),
  token(stream) {
    if (stream.eatSpace()) {
      return null;
    }
    if (stream.match("//")) {
      stream.skipToEnd();
      return "comment";
    }
    if (stream.match("\\\\")) {
      stream.skipToEnd();
      return "string";
    }
    const quote = stream.peek();
    if (quote === '"' || quote === "'") {
      stream.next();
      let escaped = false;
      for (let char = stream.next(); char !== undefined; char = stream.next()) {
        if (char === quote && !escaped) {
          break;
        }
        escaped = !escaped && char === "\\";
      }
      return "string";
    }
    if (
      // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
      stream.match(/^0x[0-9a-fA-F_]+|^0b[01_]+|^0o[0-7_]+|^\d[\d_]*(\.\d[\d_]*)?([eE][-+]?\d+)?/u)
    ) {
      return "number";
    }
    // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
    if (stream.match(/^@\w+/u)) {
      return "builtin";
    }
    const word = stream.match(/^[A-Za-z_]\w*/u);
    if (word) {
      const text = stream.current();
      if (ZIG_KEYWORDS.has(text)) {
        return "keyword";
      }
      if (ZIG_TYPES.test(text)) {
        return "typeName";
      }
      if (text === "true" || text === "false" || text === "null" || text === "undefined") {
        return "atom";
      }
      return stream.peek() === "(" ? "variableName.function" : "variableName";
    }
    stream.next();
    return null;
  },
  tokenTable: {
    "variableName.function": tags.function(tags.variableName),
    builtin: tags.standard(tags.name),
  },
});

interface Syntax {
  name: string;
  keywords: string;

  ignoreCase?: boolean;
  lineComment: string;

  blockComments?: [string, string, boolean][];

  doubledQuotes?: boolean;

  singleQuoted?: boolean;

  rawStrings?: [string, string][];

  attributes?: boolean;
}

interface SimpleState {
  comment: [string, string, boolean] | null;
  depth: number;

  closing: string | null;
}

/**
 * Lightweight StreamLanguage factory for languages without Lezer packages (Odin, Fortran, D,
 * Nim, Ada), handling nested block comments, raw strings, doubled quotes, and Ada `'` attributes.
 */
function simple(syntax: Syntax) {
  const keywords = new Set(syntax.keywords.split(" "));
  const blocks = syntax.blockComments ?? [];
  const raws = syntax.rawStrings ?? [];
  assert(syntax.lineComment !== "", `${syntax.name} has an empty line comment`);
  assert(
    blocks.every(([open, close]) => open !== "" && close !== ""),
    `${syntax.name} has an empty block comment delimiter`,
  );
  assert(
    raws.every(([open, close]) => open !== "" && close !== ""),
    `${syntax.name} has an empty raw string delimiter`,
  );

  const inComment = (stream: StringStream, state: SimpleState) => {
    assert(state.comment !== null, "reading a block comment with none open");
    assert(state.depth >= 1, "reading a block comment at depth below 1");
    const [open, close, nests] = state.comment;
    while (!stream.eol()) {
      // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
      if (nests && stream.match(open)) {
        state.depth += 1;
        // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
      } else if (stream.match(close)) {
        state.depth -= 1;
        if (state.depth === 0) {
          state.comment = null;
          break;
        }
      } else {
        stream.next();
      }
    }
    return "comment";
  };

  const inRaw = (stream: StringStream, state: SimpleState) => {
    const closing = state.closing;
    assert(closing !== null, "reading a raw string with no closing");
    while (!stream.eol()) {
      // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
      if (stream.match(closing)) {
        while (closing.startsWith('"') && stream.eat('"')) {}
        state.closing = null;
        break;
      }
      stream.next();
    }
    return "string";
  };

  return StreamLanguage.define<SimpleState>({
    name: syntax.name,
    startState: () => ({ comment: null, depth: 0, closing: null }),
    copyState: (state) => ({ ...state }),
    token(stream, state) {
      if (state.comment !== null) {
        return inComment(stream, state);
      }
      if (state.closing !== null) {
        return inRaw(stream, state);
      }
      if (stream.eatSpace()) {
        return null;
      }
      const block = blocks.find(([open]) => stream.match(open));
      if (block) {
        state.comment = block;
        state.depth = 1;
        return inComment(stream, state);
      }
      // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
      if (stream.match(syntax.lineComment)) {
        stream.skipToEnd();
        return "comment";
      }
      const raw = raws.find(([open]) => stream.match(open));
      if (raw) {
        state.closing = raw[1];
        return inRaw(stream, state);
      }
      const before = stream.string[stream.pos - 1] ?? "";
      const quote = stream.peek();
      if (quote === '"' || (quote === "'" && syntax.singleQuoted)) {
        stream.next();
        for (let char = stream.next(); char !== undefined; char = stream.next()) {
          if (char === quote) {
            if (!(syntax.doubledQuotes && stream.eat(quote))) {
              break;
            }
          } else if (char === "\\" && !syntax.doubledQuotes) {
            stream.next();
          }
        }
        return "string";
      }
      if (quote === "'") {
        if (syntax.attributes) {
          // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
          if (/[\w)]/u.test(before) || !stream.match(/^'.'/u)) {
            stream.next();
            return null;
          }
          return "string";
        }
        stream.match(/^'(?:\\.|[^'\\])*'?/u);
        return "string";
      }
      if (
        !/\w/u.test(before) &&
        // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
        stream.match(/^0x[0-9a-fA-F_]+|^\d[\d_]*(\.\d[\d_]*)?([eEdD][-+]?\d+)?/u)
      ) {
        return "number";
      }
      // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
      if (stream.match(/^[A-Za-z_]\w*/u)) {
        const text = stream.current();
        if (keywords.has(syntax.ignoreCase ? text.toLowerCase() : text)) {
          return "keyword";
        }
        if (text === "true" || text === "false" || text === "nil" || text === "null") {
          return "atom";
        }
        return stream.peek() === "(" ? "variableName.function" : "variableName";
      }
      stream.next();
      return null;
    },
    tokenTable: {
      "variableName.function": tags.function(tags.variableName),
    },
  });
}

const odin = simple({
  name: "odin",
  keywords:
    "asm auto_cast bit_set break case cast context continue defer distinct do dynamic else enum " +
    "fallthrough for foreign if import in map matrix not_in or_break or_continue or_else " +
    "or_return package proc return struct switch transmute typeid union using when where",
  lineComment: "//",
  blockComments: [["/*", "*/", true]],
  rawStrings: [["`", "`"]],
});

const fortran = simple({
  name: "fortran",
  keywords:
    "allocatable allocate associate block call case character class close complex contains " +
    "contiguous cycle data deallocate default dimension do else elemental elseif end enddo endif " +
    "exit external function goto if implicit in inout integer intent interface intrinsic logical " +
    "module none nullify only open optional out parameter pointer print private procedure " +
    "program public pure read real recursive result return save select stop subroutine target " +
    "then type use value where while write",
  ignoreCase: true,
  lineComment: "!",
  doubledQuotes: true,
  singleQuoted: true,
});

const d = simple({
  name: "d",
  keywords:
    "abstract alias align asm assert auto bool break byte case cast catch char class const " +
    "continue dchar debug default delegate delete deprecated do double else enum export extern " +
    "final finally float for foreach foreach_reverse function goto if immutable import in inout " +
    "int interface invariant is lazy long mixin module new nothrow out override package pragma " +
    "private protected public pure real ref return scope shared short static struct super switch " +
    "synchronized template this throw try typeid typeof ubyte uint ulong union unittest ushort " +
    "version void wchar while with",
  lineComment: "//",
  blockComments: [
    ["/*", "*/", false],
    ["/+", "+/", true],
  ],
  rawStrings: [
    ["`", "`"],
    ['r"', '"'],
  ],
});

const nim = simple({
  name: "nim",
  keywords:
    "addr and as asm bind block break case cast concept const continue converter defer discard " +
    "distinct div do elif else end enum except export finally for from func if import in include " +
    "interface is isnot iterator let macro method mixin mod not notin object of or out proc ptr " +
    "raise ref return shl shr static template try tuple type using var when while xor yield",
  lineComment: "#",
  blockComments: [["#[", "]#", true]],
  rawStrings: [
    ['"""', '"""'],
    ['r"', '"'],
  ],
});

const ada = simple({
  name: "ada",
  keywords:
    "abort abs abstract accept access aliased all and array at begin body case constant declare " +
    "delay delta digits do else elsif end entry exception exit for function generic goto if in " +
    "interface is limited loop mod new not null of or others out overriding package parallel " +
    "pragma private procedure protected raise range record rem renames requeue return reverse " +
    "select separate some subtype synchronized tagged task terminate then type until use when " +
    "while with xor",
  ignoreCase: true,
  lineComment: "--",
  doubledQuotes: true,
  attributes: true,
});

const PYTHON_STRING_START = /^(?:rb|br|fr|rf|tr|rt|[rubft])?(?=['"])/iu;

const PYTHON_NUMBER =
  /^0[xX][\da-fA-F_]+|^0[oO][0-7_]+|^0[bB][01_]+|^\d[\d_]*(\.\d[\d_]*)?(?:[eE][-+]?\d[\d_]*)?[jJ]?/u;

type PythonOpen = "field" | "spec" | "bracket";

interface PythonState {
  closing: string | null;
  formatted: boolean;
  raw: boolean;
  open: PythonOpen[];
  inner: string | null;
}

function readPythonString(stream: StringStream, state: PythonState, closing: string) {
  assert(closing.length === 1 || closing.length === 3, "python string closing of a bad length");
  const triple = closing.length === 3;
  let continued = false;
  while (!stream.eol()) {
    // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
    if (stream.match(closing)) {
      state.closing = null;
      return;
    }
    if (stream.eat("\\") && stream.eol()) {
      continued = true;
    }
    stream.next();
  }
  state.closing = triple || continued ? closing : null;
}

/**
 * Scans Python f/t-strings while tracking nested `{...}` fields, `:` format specs, inner brackets,
 * and nested string literals so quotes or braces inside expressions do not close the outer string.
 */
function readFormattedString(stream: StringStream, state: PythonState) {
  assert(state.formatted, "reading a plain string as formatted");
  const closing = state.closing;
  assert(closing !== null, "reading a formatted string with no closing");
  assert(closing.length === 1 || closing.length === 3, "python string closing of a bad length");
  const triple = closing.length === 3;
  let continued = false;
  while (!stream.eol()) {
    if (state.inner !== null) {
      const inner = state.inner;
      // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
      if (stream.match(inner)) {
        state.inner = null;
      } else if (!stream.eat("\\") || !stream.eol()) {
        stream.next();
      }
      continue;
    }
    const top = state.open.at(-1);
    if (top === undefined || top === "spec") {
      if (!state.raw && stream.eat("\\")) {
        if (stream.eol()) {
          continued = true;
        } else {
          stream.next();
        }
        continue;
      }
      // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
      if (top === undefined && stream.match(closing)) {
        state.closing = null;
        return;
      }
      const next = stream.next();
      if (next === "{") {
        if (top !== undefined || !stream.eat("{")) {
          state.open.push("field");
        }
      } else if (next === "}" && top !== undefined) {
        state.open.pop();
      }
      continue;
    }

    const before = stream.string.charAt(stream.pos - 1);
    // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
    if (!/\w/u.test(before) && stream.match(PYTHON_STRING_START)) {
      const quote = stream.next();
      assert(quote === '"' || quote === "'", "python string prefix not before a quote");
      state.inner = stream.string.startsWith(quote.repeat(3), stream.pos - 1)
        ? quote.repeat(3)
        : quote;
      stream.pos += state.inner.length - 1;
      continue;
    }
    const next = stream.next();
    if (next === "(" || next === "[" || next === "{") {
      state.open.push("bracket");
    } else if (next === ")" || next === "]" || next === "}") {
      state.open.pop();
    } else if (next === ":" && top === "field") {
      state.open[state.open.length - 1] = "spec";
    }
  }

  if (state.open.length === 0 && state.inner === null && !triple && !continued) {
    state.closing = null;
  }
}

const python = StreamLanguage.define<PythonState>({
  name: "python",
  startState: () => ({ closing: null, formatted: false, raw: false, open: [], inner: null }),
  copyState: (state) => ({ ...state, open: [...state.open] }),
  token(stream, state) {
    if (state.closing !== null) {
      if (state.formatted) {
        readFormattedString(stream, state);
      } else {
        readPythonString(stream, state, state.closing);
      }
      return "string";
    }
    if (stream.eatSpace()) {
      return null;
    }
    if (stream.match("#")) {
      stream.skipToEnd();
      return "comment";
    }
    const prefix = stream.match(PYTHON_STRING_START);
    if (prefix) {
      assert(Array.isArray(prefix), "python string prefix match is not a match array");
      const letters = prefix[0].toLowerCase();
      const quote = stream.next();
      assert(quote === '"' || quote === "'", "python string prefix not before a quote");
      const closing = stream.string.startsWith(quote.repeat(3), stream.pos - 1)
        ? quote.repeat(3)
        : quote;
      stream.pos += closing.length - 1;
      if (/[ft]/u.test(letters)) {
        state.closing = closing;
        state.formatted = true;
        state.raw = letters.includes("r");
        state.open = [];
        state.inner = null;
        readFormattedString(stream, state);
      } else {
        state.formatted = false;
        readPythonString(stream, state, closing);
      }
      return "string";
    }
    // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
    if (stream.match(PYTHON_NUMBER)) {
      return "number";
    }
    // oxlint-disable-next-line unicorn/prefer-regexp-test -- StringStream.match, not String#match
    if (stream.match(/^[A-Za-z_]\w*/u)) {
      if (
        PYTHON_KEYWORDS.has(stream.current()) &&
        pythonKeywordHolds(
          stream.current(),
          stream.string.slice(0, stream.start),
          stream.string.slice(stream.pos),
        )
      ) {
        return "keyword";
      }
      return stream.peek() === "(" ? "variableName.function" : "variableName";
    }
    stream.next();
    return null;
  },
  tokenTable: {
    "variableName.function": tags.function(tags.variableName),
  },
});

export function language(path: string): Extension {
  const extension = path.slice(path.lastIndexOf(".") + 1).toLowerCase();
  switch (extension) {
    case "c":
    case "h":
    case "cc":
    case "cpp":
    case "cxx":
    case "hh":
    case "hpp":
    case "hxx":
    case "inl":
      return cpp();
    case "rs":
      return rust();
    case "go":
      return go();
    case "zig":
      return zig;
    case "odin":
      return odin;
    case "f90":
    case "f95":
    case "f03":
    case "f08":
    case "f18":
      return fortran;
    case "d":
    case "di":
      return d;
    case "nim":
    case "nims":
      return nim;
    case "adb":
    case "ads":
      return ada;
    case "py":
    case "pyi":
      return python;

    case "js":
    case "mjs":
    case "cjs":
      return javascriptLanguage;
    case "jsx":
      return jsxLanguage;
    case "ts":
    case "mts":
    case "cts":
      return typescriptLanguage;
    case "tsx":
      return tsxLanguage;
    default:
      return [];
  }
}

export const tokenStyle = HighlightStyle.define([
  {
    tag: [tags.keyword, tags.controlKeyword, tags.modifier, tags.operatorKeyword],
    class: "tk-k",
  },
  { tag: [tags.typeName, tags.className, tags.standard(tags.typeName)], class: "tk-t" },
  {
    tag: [tags.function(tags.variableName), tags.function(tags.propertyName), tags.macroName],
    class: "tk-f",
  },
  {
    tag: [tags.string, tags.character, tags.special(tags.string), tags.regexp],
    class: "tk-s",
  },
  { tag: [tags.number, tags.bool, tags.atom, tags.null], class: "tk-n" },
  { tag: [tags.comment, tags.lineComment, tags.blockComment], class: "tk-c" },
  { tag: [tags.processingInstruction, tags.standard(tags.name)], class: "tk-p" },
]);

export const highlighting = syntaxHighlighting(tokenStyle);

export type Painted = [text: string, token: string][];

/** Parses joined lines as one document so multiline comments/strings highlight accurately, then splits tokens back per line. */
export function paintLines(path: string, lines: readonly string[]): Painted[] {
  const found = language(path);
  const grammar =
    found instanceof LanguageSupport ? found.language : found instanceof Language ? found : null;
  if (!grammar) {
    return lines.map((line) => (line === "" ? [] : [[line, ""]]));
  }

  const text = lines.join("\n");
  const painted: Painted[] = lines.map(() => []);

  let row = 0;
  let start = 0;
  let at = 0;
  const paint = (to: number, token: string) => {
    while (at < to) {
      const end = start + (lines[row]?.length ?? 0);
      if (at >= end) {
        row += 1;
        start = end + 1;
        at = Math.max(at, start);
        continue;
      }
      const stop = Math.min(to, end);
      painted[row]?.push([text.slice(at, stop), token]);
      at = stop;
    }
  };
  highlightTree(grammar.parser.parse(text), tokenStyle, (from, to, classes) => {
    paint(from, "");
    paint(to, classes);
  });
  paint(text.length, "");
  return painted;
}
