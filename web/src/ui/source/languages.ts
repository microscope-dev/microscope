// Syntax for the languages microscope debugs, chosen by file extension.

import { cpp } from "@codemirror/lang-cpp";
import { go } from "@codemirror/lang-go";
import { rust } from "@codemirror/lang-rust";
import {
  HighlightStyle,
  StreamLanguage,
  type StringStream,
  syntaxHighlighting,
} from "@codemirror/language";
import type { Extension } from "@codemirror/state";
import { tags } from "@lezer/highlight";

const ZIG_KEYWORDS = new Set(
  (
    "addrspace align allowzero and anyframe anytype asm async await break callconv catch comptime " +
    "const continue defer else enum errdefer error export extern fn for if inline linksection " +
    "noalias noinline nosuspend opaque or orelse packed pub resume return struct suspend switch " +
    "test threadlocal try union unreachable usingnamespace var volatile while"
  ).split(" "),
);
const ZIG_TYPES =
  /^(?:[iu]\d+|f16|f32|f64|f80|f128|usize|isize|bool|void|noreturn|type|anyerror|comptime_int|comptime_float|c_\w+)$/;

/** A small Zig tokenizer: keywords, types, strings, numbers, comments. */
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
      stream.match(/^0x[0-9a-fA-F_]+|^0b[01_]+|^0o[0-7_]+|^\d[\d_]*(\.\d[\d_]*)?([eE][-+]?\d+)?/)
    ) {
      return "number";
    }
    if (stream.match(/^@\w+/)) {
      return "builtin";
    }
    const word = stream.match(/^[A-Za-z_]\w*/);
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

/** How a language writes its comments and strings, for {@link simple}. */
interface Syntax {
  name: string;
  keywords: string;
  /** Whether keywords are keywords in any case. */
  ignoreCase?: boolean;
  lineComment: string;
  /** What opens and closes each block comment, and whether it nests. */
  blockComments?: [string, string, boolean][];
  /** Whether a quote within a string is written twice, not escaped. */
  doubledQuotes?: boolean;
  /** Whether single quotes also quote strings, as Fortran's do. */
  singleQuoted?: boolean;
  /** Raw strings that may span lines, by their opening and closing. */
  rawStrings?: [string, string][];
  /** Whether a quote after a name begins an attribute, as Ada's does. */
  attributes?: boolean;
}

interface SimpleState {
  /** The block comment open, and how deeply it nests. */
  comment: [string, string, boolean] | null;
  depth: number;
  /** What closes the raw string open, if one is. */
  closing: string | null;
}

/** A small tokenizer for a language that names its keywords, comments,
 * and strings. */
function simple(syntax: Syntax) {
  const keywords = new Set(syntax.keywords.split(" "));
  const blocks = syntax.blockComments ?? [];
  const raws = syntax.rawStrings ?? [];

  /** Reads on through the block comment open until it closes or the line
   * ends. */
  const inComment = (stream: StringStream, state: SimpleState) => {
    const [open, close, nests] = state.comment ?? ["", "", false];
    while (!stream.eol()) {
      if (nests && stream.match(open)) {
        state.depth += 1;
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

  /** Reads on through the raw string open until it closes or the line
   * ends. A string closed by quotes ends at the last of them. */
  const inRaw = (stream: StringStream, state: SimpleState) => {
    const closing = state.closing ?? "";
    while (!stream.eol()) {
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
          // A quote after a name begins an attribute, as in `Items'Last`.
          if (/[\w)]/.test(before) || !stream.match(/^'.'/)) {
            stream.next();
            return null;
          }
          return "string";
        }
        stream.match(/^'(?:\\.|[^'\\])*'?/);
        return "string";
      }
      if (
        !/\w/.test(before) &&
        stream.match(/^0x[0-9a-fA-F_]+|^\d[\d_]*(\.\d[\d_]*)?([eEdD][-+]?\d+)?/)
      ) {
        return "number";
      }
      if (stream.match(/^[A-Za-z_]\w*/)) {
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

const PYTHON_KEYWORDS = new Set(
  (
    "False None True and as assert async await break case class continue def del elif else " +
    "except finally for from global if import in is lambda match nonlocal not or pass raise " +
    "return try type while with yield"
  ).split(" "),
);

/** Whether a Python keyword just read is one where it stands. `match`,
 * `case`, and `type` are soft: keywords only where they begin the statement
 * they name, a `match` or `case` ending in a colon or a `type` alias.
 * Elsewhere they are names, as `type(x)` calls one. */
function pythonKeywordHolds(stream: StringStream): boolean {
  const word = stream.current();
  if (word !== "match" && word !== "case" && word !== "type") {
    return true;
  }
  if (stream.string.slice(0, stream.start).trim() !== "") {
    return false;
  }
  const rest = stream.string.slice(stream.pos);
  if (word === "type") {
    return /^[ \t]+[A-Za-z_]\w*\s*(?:\[|=(?!=))/.test(rest);
  }
  if (/^\s*[=:.,)]/.test(rest)) {
    return false;
  }
  // The statement's colon ends its line, before any comment.
  const code = rest.match(/^(?:[^#'"]|'[^']*'|"[^"]*")*/)?.[0] ?? rest;
  return code.trimEnd().endsWith(":");
}

/** Python's string prefixes, in any case, before the quote that opens a string. */
const PYTHON_STRING_START = /^(?:rb|br|fr|rf|tr|rt|[rubft])?(?=['"])/i;

/** Hexadecimal, octal, binary, or decimal with an exponent and an imaginary suffix. */
const PYTHON_NUMBER =
  /^0[xX][\da-fA-F_]+|^0[oO][0-7_]+|^0[bB][01_]+|^\d[\d_]*(\.\d[\d_]*)?(?:[eE][-+]?\d[\d_]*)?[jJ]?/;

/** What each brace or bracket open in an f-string's field holds: the field's
 * expression, its format spec after a `:`, or a bracket in the expression. */
type PythonOpen = "field" | "spec" | "bracket";

/** The state of a string: the delimiter that closes it, if it runs on; for
 * an f-string or t-string, whether it is raw, what its fields hold open, and
 * the delimiter of a string within a field, if one runs on. */
interface PythonState {
  closing: string | null;
  formatted: boolean;
  raw: boolean;
  open: PythonOpen[];
  inner: string | null;
}

/** Reads on through a string closed by `closing`, to that delimiter or the
 * line's end. A backslash escapes the next character, even a line's end,
 * which continues the string onto the next line. */
function readPythonString(stream: StringStream, state: PythonState, closing: string) {
  const triple = closing.length === 3;
  let continued = false;
  while (!stream.eol()) {
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

/** Reads on through an f-string or t-string closed by `state.closing`. From
 * 3.12 a field, between braces, holds an expression, which may hold strings
 * in the string's own quotes and span lines, then after a `:` its format
 * spec, which is text but for fields of its own; `{{` is a brace. */
function readFormattedString(stream: StringStream, state: PythonState) {
  const closing = state.closing ?? "";
  const triple = closing.length === 3;
  let continued = false;
  while (!stream.eol()) {
    if (state.inner !== null) {
      const inner = state.inner;
      if (stream.match(inner)) {
        state.inner = null;
      } else if (!stream.eat("\\") || !stream.eol()) {
        stream.next();
      }
      continue;
    }
    const top = state.open[state.open.length - 1];
    if (top === undefined || top === "spec") {
      if (!state.raw && stream.eat("\\")) {
        if (stream.eol()) {
          continued = true;
        } else {
          stream.next();
        }
        continue;
      }
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
    // An expression, whose strings end none of this one.
    const before = stream.string.charAt(stream.pos - 1);
    if (!/\w/.test(before) && stream.match(PYTHON_STRING_START)) {
      const quote = stream.next() ?? "";
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
  // A field's expression runs on to the next line; the text around it ends
  // with its line unless it is triple-quoted or a backslash continues it.
  if (state.open.length === 0 && state.inner === null && !triple && !continued) {
    state.closing = null;
  }
}

/** A small Python tokenizer: keywords, strings with their prefixes and
 * triple quotes, numbers, and `#` comments. */
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
      const letters = (Array.isArray(prefix) ? prefix[0] : "").toLowerCase();
      const quote = stream.next() ?? "";
      const closing = stream.string.startsWith(quote.repeat(3), stream.pos - 1)
        ? quote.repeat(3)
        : quote;
      stream.pos += closing.length - 1;
      if (/[ft]/.test(letters)) {
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
    if (stream.match(PYTHON_NUMBER)) {
      return "number";
    }
    if (stream.match(/^[A-Za-z_]\w*/)) {
      if (PYTHON_KEYWORDS.has(stream.current()) && pythonKeywordHolds(stream)) {
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
    default:
      return [];
  }
}

/** Token colors, as the page's tokens define them in both themes. */
export const highlighting = syntaxHighlighting(
  HighlightStyle.define([
    {
      tag: [tags.keyword, tags.controlKeyword, tags.modifier, tags.operatorKeyword],
      class: "tk-k",
    },
    { tag: [tags.typeName, tags.className, tags.standard(tags.typeName)], class: "tk-t" },
    {
      tag: [tags.function(tags.variableName), tags.function(tags.propertyName), tags.macroName],
      class: "tk-f",
    },
    { tag: [tags.string, tags.character, tags.special(tags.string)], class: "tk-s" },
    { tag: [tags.number, tags.bool, tags.atom, tags.null], class: "tk-n" },
    { tag: [tags.comment, tags.lineComment, tags.blockComment], class: "tk-c" },
    { tag: [tags.processingInstruction, tags.standard(tags.name)], class: "tk-p" },
  ]),
);
