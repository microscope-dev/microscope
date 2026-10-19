// What Python's grammar says of a word: whether it is a keyword where it
// stands. The highlighter and the expressions a line shows both ask.

/** Python's keywords, hard and soft. */
export const PYTHON_KEYWORDS = new Set(
  (
    "False None True and as assert async await break case class continue def del elif else " +
    "except finally for from global if import in is lambda match nonlocal not or pass raise " +
    "return try type while with yield"
  ).split(" "),
);

/** Whether Python's keyword `word`, which `before` precedes on its line and
 * `rest` follows, is one where it stands. `match`, `case`, and `type` are
 * soft: keywords only where they begin the statement they name, a `match`
 * or `case` ending in a colon or a `type` alias. Elsewhere they are names,
 * as `type(x)` calls one. */
export function pythonKeywordHolds(word: string, before: string, rest: string): boolean {
  if (word !== "match" && word !== "case" && word !== "type") {
    return true;
  }
  if (before.trim() !== "") {
    return false;
  }
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

/** Whether `path` names Python source. */
export function isPythonPath(path: string | undefined): boolean {
  return /\.pyi?$/i.test(path ?? "");
}
