import { assert } from "./assert";

function segment(name: string): string {
  return name.replaceAll("%", "%25").replaceAll("/", "%2F").replaceAll("~", "%7E");
}

export function childKey(parent: string, name: string, occurrence = 1): string {
  assert(Number.isInteger(occurrence) && occurrence >= 1, `row occurrence ${occurrence}`);
  return `${parent}/${segment(name)}${occurrence > 1 ? `~${occurrence}` : ""}`;
}

export function toggled(open: readonly string[] | undefined, key: string): string[] | undefined {
  const current = open ?? [];
  const next = current.includes(key)
    ? current.filter((path) => path !== key && !path.startsWith(`${key}/`))
    : [...current, key];
  return next.length > 0 ? next : undefined;
}

const STOPS = 4;

export class Recall {
  readonly #stops = new Map<number, Map<string, string>>();

  note(stop: number, key: string, text: string): void {
    let shown = this.#stops.get(stop);
    if (!shown) {
      shown = new Map();
      this.#stops.set(stop, shown);
      const oldest = [...this.#stops.keys()].toSorted((a, b) => a - b);
      while (oldest.length > STOPS) {
        this.#stops.delete(oldest.shift() as number);
      }
    }
    shown.set(key, text);
  }

  before(stop: number, key: string): string | undefined {
    const earlier = [...this.#stops.keys()].filter((seen) => seen < stop);
    if (earlier.length === 0) {
      return undefined;
    }
    return this.#stops.get(Math.max(...earlier))?.get(key);
  }

  stops(): number {
    return this.#stops.size;
  }

  clear(): void {
    this.#stops.clear();
  }
}

export const recall = new Recall();
