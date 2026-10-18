// Values beside the lines the program just ran: each name and member chain
// those lines use, evaluated at the frame, once, on the last line that uses
// it. Values that do not evaluate, such as types and functions, are left
// out, and so are aggregates and values that are unavailable, such as a
// variable not yet assigned, which the value tree shows better.

import { useEffect, useMemo, useState } from "react";
import { cache } from "../../data";
import { expressionsIn } from "../../expressions";
import type { At } from "../../focus";
import type { Row } from "../../protocol";
import { useConnection, useModel } from "../../store";
import { recall } from "../../tree";
import type { InlineValue } from "./SourceView";

/** How many lines before the frame's line show values. */
const BEFORE = 3;
/** The most values shown. */
const MOST = 12;

/**
 * The expressions each line near `line` shows, closest lines winning, and
 * none before `first`, where the frame's function begins, when it is known.
 */
export function inlineExpressions(
  text: string,
  line: number,
  first: number | null = null,
): Map<number, string[]> {
  const lines = text.split("\n");
  const shown = new Map<number, string[]>();
  const seen = new Set<string>();
  let count = 0;
  for (let number = line; number >= Math.max(1, line - BEFORE, first ?? 1); number -= 1) {
    const found = expressionsIn(lines[number - 1] ?? "").filter((expression) => {
      if (seen.has(expression) || count >= MOST) {
        return false;
      }
      seen.add(expression);
      count += 1;
      return true;
    });
    if (found.length > 0) {
      shown.set(number, found);
    }
  }
  return shown;
}

/**
 * Whether a value is short and sure enough for the end of a line. An
 * aggregate says too much, and why a value is unavailable is long and, for
 * a variable a line is about to assign, no news; the tree has both.
 */
export function shownInline(row: Row): boolean {
  return !row.truncated && !row.text.startsWith("{") && !row.text.startsWith("<unavailable");
}

/** The inline values at `at`'s frame, by line, once they arrive. */
export function useInlineValues(
  at: At | null,
  text: string | undefined,
  line: number | null,
  first: number | null,
): Map<number, InlineValue[]> {
  const connection = useConnection();
  // A change to a value asks again.
  const writes = useModel((model) => model.state?.writes);
  const wanted = useMemo(
    () => (at && text && line ? inlineExpressions(text, line, first) : new Map<number, string[]>()),
    [at, text, line, first],
  );
  // The values shown belong to one stop and one count of writes.
  const [values, setValues] = useState<{
    at: At | null;
    writes: number | undefined;
    rows: Map<string, Row>;
  }>({ at: null, writes: undefined, rows: new Map() });

  useEffect(() => {
    // A stop always comes with a state, which counts the writes.
    if (!at || writes === undefined) {
      return;
    }
    let live = true;
    const expressions = [...wanted.values()].flat();
    void Promise.all(
      expressions.map((expression) =>
        cache
          .get(connection, "evaluate", { ...at, expression })
          .promise.then((settled) => [expression, settled] as const),
      ),
    ).then((settled) => {
      if (!live) {
        return;
      }
      const rows = new Map<string, Row>();
      for (const [expression, outcome] of settled) {
        if (outcome.ok) {
          rows.set(expression, outcome.value as Row);
        }
      }
      setValues({ at, writes, rows });
    });
    return () => {
      live = false;
    };
  }, [at, wanted, connection, writes]);

  return useMemo(() => {
    const shown = new Map<number, InlineValue[]>();
    if (!at || values.at !== at || values.writes !== writes) {
      return shown;
    }
    for (const [number, expressions] of wanted) {
      const parts: InlineValue[] = [];
      for (const expression of expressions) {
        const row = values.rows.get(expression);
        if (!row || !shownInline(row)) {
          continue;
        }
        const key = `inline/${expression}`;
        const was = recall.before(at.stop, key);
        recall.note(at.stop, key, row.text);
        parts.push({
          text: `${expression} = ${row.text}`,
          changed: was !== undefined && was !== row.text,
        });
      }
      if (parts.length > 0) {
        shown.set(number, parts);
      }
    }
    return shown;
  }, [at, values, wanted, writes]);
}
