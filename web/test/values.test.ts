// The value tree remembers what is open by path, and marks what changed
// since the stop before; hovers and inline values find expressions in text.

import { describe, expect, it } from "vitest";
import { expressionAt, expressionsIn } from "../src/expressions";
import type { Row } from "../src/protocol";
import { childKey, Recall, toggled } from "../src/tree";
import { inlineExpressions, shownInline } from "../src/ui/source/inline";

describe("tree paths", () => {
  it("name each row by its scope and the names above it, whatever they hold", () => {
    expect(childKey("args", "req")).toBe("args/req");
    expect(childKey("args/req", "key")).toBe("args/req/key");
    // A name holding a slash or a percent sign stays one step.
    expect(childKey("w", "a / b")).toBe("w/a %2F b");
    expect(childKey("w", "100%")).toBe("w/100%25");
  });

  it("tell apart rows with the same name, as shadowed variables have", () => {
    expect(childKey("locals", "i", 1)).toBe("locals/i");
    expect(childKey("locals", "i", 2)).toBe("locals/i~2");
    // A name that looks like a second one is still the first of its own.
    expect(childKey("locals", "i~2", 1)).toBe("locals/i%7E2");
  });

  it("open and close a row in the link's list, in the order opened", () => {
    expect(toggled(undefined, "args/req")).toEqual(["args/req"]);
    expect(toggled(["args/req", "locals/e"], "args/req")).toEqual(["locals/e"]);
    // Closing a row closes what is open beneath it.
    expect(toggled(["args/req", "args/req/key", "args/request"], "args/req")).toEqual([
      "args/request",
    ]);
    expect(toggled(["args/req"], "args/req")).toBeUndefined();
  });
});

describe("changed values", () => {
  it("compare a path's text with the stop seen before", () => {
    const recall = new Recall();
    recall.note(10, "args/n", "1");
    recall.note(10, "args/m", "5");
    expect(recall.before(10, "args/n")).toBeUndefined();
    recall.note(12, "args/n", "2");
    recall.note(12, "args/m", "5");
    expect(recall.before(12, "args/n")).toBe("1");
    expect(recall.before(12, "args/m")).toBe("5");
    // A path the earlier stop did not show has nothing to compare.
    expect(recall.before(12, "locals/new")).toBeUndefined();
    // Going back to an older stop compares with the one before it, if seen.
    expect(recall.before(10, "args/n")).toBeUndefined();
  });

  it("keep only the last few stops", () => {
    const recall = new Recall();
    for (let stop = 1; stop <= 20; stop += 1) {
      recall.note(stop, "x", String(stop));
    }
    expect(recall.before(20, "x")).toBe("19");
    expect(recall.stops()).toBeLessThanOrEqual(4);
  });
});

describe("expressions in source", () => {
  it("find the name under the pointer with the members before it", () => {
    const line = "    entry_set(e, req->value, req->len);";
    expect(expressionAt(line, line.indexOf("value") + 2)?.text).toBe("req->value");
    expect(expressionAt(line, line.indexOf("req->len") + 1)?.text).toBe("req");
    expect(expressionAt("  s.stats.gets++;", 6)?.text).toBe("s.stats");
    expect(expressionAt("  x = 1;", 4)).toBeNull();
    expect(expressionAt("  return value;", 4)).toBeNull();
  });

  it("list a line's names and member chains, but not calls or keywords", () => {
    expect(expressionsIn("    struct entry *e = table_find(&s->table, req->key);")).toEqual([
      "e",
      "s->table",
      "req->key",
    ]);
    expect(expressionsIn("    if (e == NULL) { status = -1; }")).toEqual(["e", "status"]);
    expect(expressionsIn('    printf("%d words", count); // n here')).toEqual(["count"]);
  });

  it("read Python as Python: its keywords, constants, comments, and string prefixes", () => {
    const python = (line: string) => expressionsIn(line, "app.py");
    expect(python("    while True:")).toEqual([]);
    expect(python("    if x is None and not done:")).toEqual(["x", "done"]);
    expect(python("    def take(self, name, count):")).toEqual(["self", "name", "count"]);
    expect(python("    total = f'{x}' + rb\"\\d\" + name  # count it")).toEqual(["total", "name"]);
    expect(python("    return lambda y: y + offset")).toEqual(["y", "offset"]);
    // A soft keyword is a name where it does not begin its statement.
    expect(python("    match point:")).toEqual(["point"]);
    expect(python('match = {"case": 1}')).toEqual(["match"]);
    expect(python("type Point = tuple[int, int]")).toEqual(["Point", "tuple", "int"]);
    expect(python("    kind = type")).toEqual(["kind", "type"]);
    expect(expressionAt("    while True:", 11, "app.py")).toBeNull();
    expect(expressionAt("    if done is None:", 16, "app.py")).toBeNull();
    // Other languages keep their names, though Python's keywords.
    expect(expressionsIn("    int from = pass + is;")).toEqual(["from", "pass", "is"]);
  });
});

describe("inline values", () => {
  const text = [
    "    def add(self, item):",
    "        self.items[item.name] = item",
    "",
    "    def take(self, name, count):",
    "        item = self.items[name]",
  ].join("\n");

  it("come from the lines just run, and none before the frame's function", () => {
    expect([...inlineExpressions(text, 5).keys()]).toEqual([5, 4, 2]);
    expect([...inlineExpressions(text, 5, 4).keys()]).toEqual([5, 4]);
  });

  it("leave out what the tree says better: aggregates and unavailable values", () => {
    const row = (text: string): Row => ({
      name: "item",
      text,
      detail: null,
      type: null,
      path: "item",
      children: null,
      editable: false,
      memory: null,
      memoryBytes: null,
      truncated: false,
      drawings: [],
    });
    expect(shownInline(row("3"))).toBe(true);
    expect(shownInline(row("'bolt'"))).toBe(true);
    expect(shownInline(row("{x = 1}"))).toBe(false);
    expect(shownInline(row("<unavailable: the variable is unbound: not yet assigned>"))).toBe(
      false,
    );
  });
});
