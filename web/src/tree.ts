// The value tree's memory. A row is named by its path: its scope, then the
// name of each row above it. The link lists the open paths (`x=`), so a
// tree opens the same rows at the next stop, and changed values compare a
// path's text with the stop seen before.

import { assert } from "./assert";

/** One step of a path: a name, with `/`, `%`, and `~` escaped so it stays one. */
function segment(name: string): string {
  return name.replaceAll("%", "%25").replaceAll("/", "%2F").replaceAll("~", "%7E");
}

/**
 * The path of a child row named `name` under `parent`. Names can repeat, as
 * shadowed variables do: `occurrence` counts this name's rows so far.
 */
export function childKey(parent: string, name: string, occurrence = 1): string {
  assert(Number.isInteger(occurrence) && occurrence >= 1, `row occurrence ${occurrence}`);
  return `${parent}/${segment(name)}${occurrence > 1 ? `~${occurrence}` : ""}`;
}

/**
 * The open paths after opening or closing `key`. Closing a row closes the
 * rows open beneath it. No open paths is none at all, so the link drops it.
 */
export function toggled(open: readonly string[] | undefined, key: string): string[] | undefined {
  const current = open ?? [];
  const next = current.includes(key)
    ? current.filter((path) => path !== key && !path.startsWith(`${key}/`))
    : [...current, key];
  return next.length > 0 ? next : undefined;
}

/** The most stops whose values are kept to compare with. */
const STOPS = 4;

/** What each path showed at the last few stops this tab saw. */
export class Recall {
  readonly #stops = new Map<number, Map<string, string>>();

  /** Records the text a path shows at a stop. */
  note(stop: number, key: string, text: string): void {
    let shown = this.#stops.get(stop);
    if (!shown) {
      shown = new Map();
      this.#stops.set(stop, shown);
      const oldest = [...this.#stops.keys()].sort((a, b) => a - b);
      while (oldest.length > STOPS) {
        this.#stops.delete(oldest.shift() as number);
      }
    }
    shown.set(key, text);
  }

  /** The text a path showed at the latest stop before `stop` that showed it. */
  before(stop: number, key: string): string | undefined {
    const earlier = [...this.#stops.keys()].filter((seen) => seen < stop);
    if (earlier.length === 0) {
      return undefined;
    }
    return this.#stops.get(Math.max(...earlier))?.get(key);
  }

  /** How many stops are kept. */
  stops(): number {
    return this.#stops.size;
  }

  clear(): void {
    this.#stops.clear();
  }
}

/** This tab's memory of values. */
export const recall = new Recall();
