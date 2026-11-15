import { assert } from "./assert";

export const PYTHON_KEYWORDS = new Set(
  (
    "False None True and as assert async await break case class continue def del elif else " +
    "except finally for from global if import in is lambda match nonlocal not or pass raise " +
    "return try type while with yield"
  ).split(" "),
);

export function pythonKeywordHolds(word: string, before: string, rest: string): boolean {
  assert(PYTHON_KEYWORDS.has(word), "asking whether a word that is no Python keyword holds as one");
  if (word !== "match" && word !== "case" && word !== "type") {
    return true;
  }
  if (before.trim() !== "") {
    return false;
  }
  if (word === "type") {
    return /^[ \t]+[A-Za-z_]\w*\s*(?:\[|=(?!=))/u.test(rest);
  }
  if (/^\s*[=:.,)]/u.test(rest)) {
    return false;
  }

  const code = rest.match(/^(?:[^#'"]|'[^']*'|"[^"]*")*/u)?.[0] ?? rest;
  return code.trimEnd().endsWith(":");
}

export function isPythonPath(path: string | undefined): boolean {
  return /\.pyi?$/iu.test(path ?? "");
}
