import { assert } from "./assert";
import { isPythonPath, PYTHON_KEYWORDS, pythonKeywordHolds } from "./python";

const NAME = /[A-Za-z_][A-Za-z0-9_]*/uy;

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

const TAGS = new Set(["struct", "enum", "union", "class"]);

export interface Found {
  text: string;

  from: number;
  to: number;
}

const isNameCharacter = (character: string | undefined) =>
  character !== undefined && /[A-Za-z0-9_]/u.test(character);

function isKeyword(word: string, line: string, from: number, to: number, path?: string): boolean {
  if (!isPythonPath(path)) {
    return KEYWORDS.has(word);
  }
  return PYTHON_KEYWORDS.has(word) && pythonKeywordHolds(word, line.slice(0, from), line.slice(to));
}

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
  if (/^\d/u.test(word) || isKeyword(word, line, from, to, path)) {
    return null;
  }

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
        result = result.replace(/(^|[^A-Za-z0-9_])(?:rb|br|fr|rf|tr|rt|[rubft])$/iu, "$1");
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
    const call = /^\s*\(/u.test(text.slice(end));
    const word = match[0];
    const constant = word === chain && /^[A-Z][A-Z0-9_]*$/u.test(word);
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
    assert(end > index, `expression scan stuck at column ${index}`);
    index = end;
  }
  return found;
}
