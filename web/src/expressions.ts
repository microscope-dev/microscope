// Expressions in source text: what a hover evaluates, and the names and
// member chains whose values show beside the lines just run. Python source,
// which its path names, is read by Python's words, comments, and strings.

import { isPythonPath, PYTHON_KEYWORDS, pythonKeywordHolds } from "./python";

const NAME = /[A-Za-z_][A-Za-z0-9_]*/y;

const KEYWORDS = new Set(
  (
    "if else while for do return switch case break continue goto sizeof struct enum union " +
    "typedef static const volatile unsigned signed int long short char float double void bool " +
    "true false nullptr extern inline register auto restrict default let mut fn pub use mod impl " +
    "self Self match loop in as ref where func var type package import range go defer chan map " +
    "interface select fallthrough nil and or not new delete this class public private " +
    "protected virtual template typename namespace using try catch throw orelse comptime"
  ).split(" "),
);

/** Words that name a type, not a value, when they come next. */
const TAGS = new Set(["struct", "enum", "union", "class"]);

export interface Found {
  text: string;
  /** Where the expression starts and ends in the line. */
  from: number;
  to: number;
}

const isNameCharacter = (character: string | undefined) =>
  character !== undefined && /[A-Za-z0-9_]/.test(character);

/** Whether `word`, at `from` to `to` in `line` of the source at `path`, is
 * a keyword there. */
function isKeyword(word: string, line: string, from: number, to: number, path?: string): boolean {
  if (!isPythonPath(path)) {
    return KEYWORDS.has(word);
  }
  return PYTHON_KEYWORDS.has(word) && pythonKeywordHolds(word, line.slice(0, from), line.slice(to));
}

/**
 * The expression a hover at `column` names: the name under it, with the
 * members and pointers before it, such as `req->value`. Null over anything
 * that is not a value's name.
 */
export function expressionAt(line: string, column: number, path?: string): Found | null {
  if (!isNameCharacter(line[column])) {
    return null;
  }
  let from = column;
  while (from > 0 && isNameCharacter(line[from - 1])) {
    from -= 1;
  }
  let to = column;
  while (isNameCharacter(line[to])) {
    to += 1;
  }
  const word = line.slice(from, to);
  if (/^\d/.test(word) || isKeyword(word, line, from, to, path)) {
    return null;
  }
  // The members before it: `a.b->` then the name.
  let start = from;
  for (;;) {
    const before = line.slice(0, start);
    const link = before.endsWith("->") ? 2 : before.endsWith(".") ? 1 : 0;
    if (link === 0) {
      break;
    }
    let base = start - link;
    if (!isNameCharacter(line[base - 1])) {
      break;
    }
    while (base > 0 && isNameCharacter(line[base - 1])) {
      base -= 1;
    }
    start = base;
  }
  return { text: line.slice(start, to), from: start, to };
}

/** The line without comments, strings, and characters, each string or
 * character a space; in Python, without `#` comments, nor the prefixes, as
 * the `f` of an f-string, that strings begin with. */
function code(line: string, python: boolean): string {
  let result = "";
  let index = 0;
  while (index < line.length) {
    const character = line[index] as string;
    if (python ? character === "#" : line.startsWith("//", index)) {
      break;
    }
    if (!python && line.startsWith("/*", index)) {
      const end = line.indexOf("*/", index + 2);
      index = end < 0 ? line.length : end + 2;
      result += " ";
      continue;
    }
    if (character === '"' || character === "'") {
      if (python) {
        result = result.replace(/(^|[^A-Za-z0-9_])(?:rb|br|fr|rf|tr|rt|[rubft])$/i, "$1");
      }
      let end = index + 1;
      while (end < line.length && line[end] !== character) {
        end += line[end] === "\\" ? 2 : 1;
      }
      index = end + 1;
      result += " ";
      continue;
    }
    result += character;
    index += 1;
  }
  return result;
}

/**
 * The values a line uses, in order and once each: names and member chains,
 * leaving out calls, keywords, types after `struct`, and constants written
 * in capitals.
 */
export function expressionsIn(line: string, path?: string): string[] {
  const python = isPythonPath(path);
  const text = code(line, python);
  const found: string[] = [];
  let index = 0;
  let previous = "";
  while (index < text.length) {
    NAME.lastIndex = index;
    const match = NAME.exec(text);
    if (!match || isNameCharacter(text[index - 1])) {
      index += 1;
      continue;
    }
    let chain = match[0];
    let end = index + chain.length;
    for (;;) {
      const link = text.startsWith("->", end) ? "->" : text.startsWith(".", end) ? "." : null;
      if (!link) {
        break;
      }
      NAME.lastIndex = end + link.length;
      const member = NAME.exec(text);
      if (!member) {
        break;
      }
      chain += link + member[0];
      end = NAME.lastIndex;
    }
    const call = /^\s*\(/.test(text.slice(end));
    const word = match[0];
    const constant = word === chain && /^[A-Z][A-Z0-9_]*$/.test(word);
    if (
      !call &&
      !constant &&
      !isKeyword(word, text, index, index + word.length, path) &&
      !TAGS.has(previous) &&
      !found.includes(chain)
    ) {
      found.push(chain);
    }
    previous = word;
    index = end;
  }
  return found;
}
