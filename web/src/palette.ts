// How the palette matches what is typed: case aside, a whole name first,
// then names it begins, names that hold it, and names that hold its letters
// in order, as the server ranks functions. Shorter names come first among
// equals.

import { assert } from "./assert";

export interface Score {
  /** 0 for the whole name, up to 3 for scattered letters. */
  rank: number;
  /** The positions of the characters matched. */
  marks: number[];
}

export function score(text: string, query: string): Score | null {
  const name = text.toLowerCase();
  const wanted = query.toLowerCase();
  const run = (start: number) => Array.from({ length: wanted.length }, (_, index) => start + index);
  if (name === wanted) {
    return { rank: 0, marks: run(0) };
  }
  if (name.startsWith(wanted)) {
    return { rank: 1, marks: run(0) };
  }
  const at = name.indexOf(wanted);
  if (at >= 0) {
    return { rank: 2, marks: run(at) };
  }
  const marks: number[] = [];
  let from = 0;
  for (const letter of wanted) {
    const found = name.indexOf(letter, from);
    if (found < 0) {
      return null;
    }
    marks.push(found);
    from = found + 1;
  }
  return { rank: 3, marks };
}

/**
 * The `limit` items whose text best matches `query`, or the first `limit`
 * as they are when nothing is typed.
 */
export function best<T>(
  items: readonly T[],
  query: string,
  text: (item: T) => string,
  limit: number,
): T[] {
  assert(Number.isInteger(limit) && limit >= 0, `palette limit ${limit}`);
  if (query === "") {
    return items.slice(0, limit);
  }
  return items
    .flatMap((item) => {
      const found = score(text(item), query);
      return found ? [{ item, rank: found.rank, length: text(item).length }] : [];
    })
    .sort((a, b) => a.rank - b.rank || a.length - b.length)
    .slice(0, limit)
    .map((scored) => scored.item);
}
