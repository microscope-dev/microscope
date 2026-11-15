import { useEffect, useMemo, useState } from "react";

import { assert } from "../../assert";
import { cache } from "../../data";
import { expressionsIn } from "../../expressions";
import type { At } from "../../focus";
import type { Row } from "../../protocol";
import { useConnection, useModel } from "../../store";
import { recall } from "../../tree";
import type { InlineValue } from "./SourceView";

const BEFORE = 3;

const MOST = 12;

export function inlineExpressions(
  text: string,
  line: number,
  first: number | null = null,
  path?: string,
): Map<number, string[]> {
  const lines = text.split("\n");
  const shown = new Map<number, string[]>();
  const seen = new Set<string>();
  let count = 0;
  for (let number = line; number >= Math.max(1, line - BEFORE, first ?? 1); number -= 1) {
    const found: string[] = [];
    for (const expression of expressionsIn(lines[number - 1] ?? "", path)) {
      if (seen.has(expression) || count >= MOST) {
        continue;
      }
      seen.add(expression);
      count += 1;
      found.push(expression);
    }
    if (found.length > 0) {
      shown.set(number, found);
    }
  }
  assert(count <= MOST, `${count} inline expressions, over the most of ${MOST}`);
  assert(seen.size === count, `${seen.size} expressions seen but ${count} shown`);
  return shown;
}

export function shownInline(row: Row): boolean {
  return !row.truncated && !row.text.startsWith("{") && !row.text.startsWith("<unavailable");
}

export function useInlineValues(
  at: At | null,
  path: string,
  text: string | undefined,
  line: number | null,
  first: number | null,
): Map<number, InlineValue[]> {
  const connection = useConnection();

  const writes = useModel((model) => model.state?.writes);
  const wanted = useMemo(
    () =>
      at && text && line ? inlineExpressions(text, line, first, path) : new Map<number, string[]>(),
    [at, path, text, line, first],
  );

  const [values, setValues] = useState<{
    at: At | null;
    writes: number | undefined;
    rows: Map<string, Row>;
  }>({ at: null, writes: undefined, rows: new Map() });

  useEffect(() => {
    if (!at || writes === undefined) {
      return;
    }
    let live = true;
    const expressions = [...wanted.values()].flat();
    const load = async () => {
      const settled = await Promise.all(
        expressions.map((expression) =>
          cache
            .get(connection, "evaluate", { ...at, expression })
            .promise.then((outcome) => [expression, outcome] as const),
        ),
      );
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
    };
    void load();
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
