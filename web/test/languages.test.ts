// The languages without a CodeMirror package of their own are tokenized by
// small stream languages: each with its own comments, strings, and case.

import {
  javascriptLanguage,
  jsxLanguage,
  tsxLanguage,
  typescriptLanguage,
} from "@codemirror/lang-javascript";
import { Language } from "@codemirror/language";
import { classHighlighter, highlightTree } from "@lezer/highlight";
import { describe, expect, it } from "vitest";
import { language, tokenStyle } from "../src/ui/source/languages";

/** The highlighted tokens of `text`, as [text, class] pairs. */
function tokens(path: string, text: string): [string, string][] {
  const syntax = language(path);
  if (!(syntax instanceof Language)) {
    throw new Error(`no language for ${path}`);
  }
  const found: [string, string][] = [];
  highlightTree(syntax.parser.parse(text), classHighlighter, (from, to, classes) => {
    found.push([text.slice(from, to), classes]);
  });
  return found;
}

/** The text of the tokens of `kind`. */
function of(found: [string, string][], kind: string): string[] {
  return found.filter(([, classes]) => classes.includes(`tok-${kind}`)).map(([text]) => text);
}

describe("stream languages", () => {
  it("nest Odin's block comments and span lines with its raw strings", () => {
    const found = tokens("a.odin", "/* a /* b */ c */ x := `r\ns` // done\nproc");
    expect(of(found, "comment")).toEqual(["/* a /* b */ c */", "// done"]);
    expect(of(found, "string")).toEqual(["`r", "s`"]);
    expect(of(found, "keyword")).toEqual(["proc"]);
  });

  it("read Fortran's keywords in any case and its doubled quotes", () => {
    const found = tokens("a.f90", "INTEGER :: n = 3 ! three\nprint *, 'it''s'");
    expect(of(found, "keyword")).toEqual(["INTEGER", "print"]);
    expect(of(found, "comment")).toEqual(["! three"]);
    expect(of(found, "string")).toEqual(["'it''s'"]);
    expect(of(found, "number")).toEqual(["3"]);
  });

  it("nest D's plus comments", () => {
    const found = tokens("a.d", '/+ a /+ b +/ c +/ auto s = r"x\\";');
    expect(of(found, "comment")).toEqual(["/+ a /+ b +/ c +/"]);
    expect(of(found, "keyword")).toEqual(["auto"]);
    expect(of(found, "string")).toEqual(['r"x\\"']);
  });

  it("read Nim's comments and its triple-quoted strings across lines", () => {
    const found = tokens("a.nim", '#[ a #[ b ]# ]# let s = """one\n"two"""" # done');
    expect(of(found, "comment")).toEqual(["#[ a #[ b ]# ]#", "# done"]);
    expect(of(found, "string")).toEqual(['"""one', '"two""""']);
  });

  it("span Python's triple quotes across lines and take its prefixes", () => {
    const found = tokens(
      "a.py",
      [
        "def f(): return 'it' # note",
        String.raw`t = rb'\d' + f"{x}" + "a#b"`,
        "n = 0x1f + 1.5e-3j",
        `s = """one`,
        `it's "two"`,
        `three""" + '''a`,
        "b''' if x else 0",
        "@cache",
      ].join("\n"),
    );
    expect(of(found, "keyword")).toEqual(["def", "return", "if", "else"]);
    expect(of(found, "comment")).toEqual(["# note"]);
    expect(of(found, "string")).toEqual([
      "'it'",
      String.raw`rb'\d'`,
      'f"{x}"',
      '"a#b"',
      '"""one',
      `it's "two"`,
      'three"""',
      "'''a",
      "b'''",
    ]);
    expect(of(found, "number")).toEqual(["0x1f", "1.5e-3j", "0"]);
  });

  it("keep Python's f-strings whole around strings in their fields", () => {
    const found = tokens(
      "a.py",
      [
        `a = f"{d["key"]}" + 1`,
        "b = f'{x!r:>{width}} {{y}}' + 2",
        String.raw`c = rf"{d['a'] + "}"}\d" + 3`,
        `d = f"""{d["""`,
        `x"""]}""" + 4`,
        `e = "{d["k"]}" + 5`,
        `g = f"{x:'>10}" + f'{x:{"w"}}' + 6`,
        `h = f"{`,
        "  x",
        `}" + 7`,
      ].join("\n"),
    );
    expect(of(found, "string")).toEqual([
      `f"{d["key"]}"`,
      "f'{x!r:>{width}} {{y}}'",
      String.raw`rf"{d['a'] + "}"}\d"`,
      `f"""{d["""`,
      `x"""]}"""`,
      `"{d["`,
      `"]}"`,
      `f"{x:'>10}"`,
      `f'{x:{"w"}}'`,
      `f"{`,
      "  x",
      `}"`,
    ]);
    expect(of(found, "number")).toEqual(["1", "2", "3", "4", "5", "6", "7"]);
  });

  it("read Python's soft keywords only where they begin statements", () => {
    const found = tokens(
      "a.py",
      [
        "match command:",
        "    case [x, *rest]:",
        "        type Point = tuple[int, int]",
        "match = re.match(pattern, text)",
        "kind = type(value)",
        "self.type = 1",
        "case(1)",
        "match x:  # a comment: here",
        "type == 1",
        "type Pair[T] = tuple[T, T]",
      ].join("\n"),
    );
    expect(of(found, "keyword")).toEqual(["match", "case", "type", "match", "type"]);
  });

  it("tell Ada's characters from its attributes", () => {
    const found = tokens("a.adb", 'Last := Items\'Last; -- end\nC := \'q\'; S := "a""b"; BEGIN');
    expect(of(found, "comment")).toEqual(["-- end"]);
    expect(of(found, "string")).toEqual(["'q'", '"a""b"']);
    expect(of(found, "keyword")).toEqual(["BEGIN"]);
  });
});

describe("JavaScript and TypeScript", () => {
  /** The text of each `tk-` theme role in `text`, as the source view colors it. */
  function roles(path: string, text: string): Record<string, string[]> {
    const syntax = language(path);
    if (!(syntax instanceof Language)) {
      throw new Error(`no language for ${path}`);
    }
    const found: Record<string, string[]> = {};
    highlightTree(syntax.parser.parse(text), tokenStyle, (from, to, classes) => {
      found[classes] = [...(found[classes] ?? []), text.slice(from, to)];
    });
    return found;
  }

  it("choose the language by extension", () => {
    const chosen = ["a.js", "a.mjs", "a.cjs", "a.jsx", "a.ts", "a.mts", "a.cts", "a.TSX"].map(
      language,
    );
    expect(chosen).toEqual([
      javascriptLanguage,
      javascriptLanguage,
      javascriptLanguage,
      jsxLanguage,
      typescriptLanguage,
      typescriptLanguage,
      typescriptLanguage,
      tsxLanguage,
    ]);
    expect(language("a.json")).toEqual([]);
    expect(language("a.ts.map")).toEqual([]);
  });

  it("read JSX only in .jsx and .tsx", () => {
    const text = 'let e = <a href="x">y</a>;';
    expect(roles("a.jsx", text)["tk-s"]).toEqual(['"x"']);
    expect(roles("a.tsx", text)["tk-s"]).toEqual(['"x"']);
    expect(roles("a.ts", text)["tk-s"]).not.toEqual(['"x"']);
  });

  it("color JavaScript's keywords, strings, templates, and comments", () => {
    const found = roles(
      "a.cjs",
      `// note\nconst re = /a+/g; /* b */ async function f() { return \`x\${await g('s')}\`; }`,
    );
    expect(found["tk-c"]).toEqual(["// note", "/* b */"]);
    expect(found["tk-k"]).toEqual(["const", "async", "function", "return", "await"]);
    expect(found["tk-s"]).toEqual(["/a+/g", "`x", "'s'", "`"]);
    expect(found["tk-f"]).toEqual(["f", "g"]);
  });

  it("color TypeScript's type annotations in .mts but not in .mjs", () => {
    const text = "interface P { x: number }\nlet p: P = { x: 1 };";
    const typescript = roles("a.mts", text);
    expect(typescript["tk-k"]).toEqual(["interface", "let"]);
    expect(typescript["tk-t"]).toEqual(["P", "number", "P"]);
    expect(typescript["tk-n"]).toEqual(["1"]);
    expect(roles("a.mjs", text)["tk-t"]).not.toContain("number");
  });
});
