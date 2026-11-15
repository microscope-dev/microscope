import { assert } from "../assert";
import {
  ariaChord,
  type Chord,
  describeChord,
  type KeyLike,
  matches,
  sameKey,
  typing,
} from "./keys";

export type Menu = "File" | "Edit" | "View" | "Go" | "Run" | "Debug" | "Window" | "Help";

export type Availability =
  | { readonly state: "ready" }
  | { readonly state: "blocked"; readonly why: string }
  | { readonly state: "absent" };

export const READY: Availability = { state: "ready" };
export const ABSENT: Availability = { state: "absent" };
export const blocked = (why: string): Availability => ({ state: "blocked", why });

export interface Command<Context> {
  readonly label: string;
  readonly menu: Menu | null;

  readonly keys: readonly Chord[];
  available(context: Context): Availability;

  run(context: Context): void;
}

export interface Registry<Id extends string, Context> {
  readonly ids: readonly Id[];
  get(id: Id): Command<Context>;

  match(event: KeyLike, inTextBox: boolean): Id | null;

  describe(id: Id, typed?: boolean): string | null;

  aria(id: Id): string | null;
}

export function registry<Id extends string, Context>(
  commands: Readonly<Record<Id, Command<Context>>>,
): Registry<Id, Context> {
  const ids = Object.keys(commands) as Id[];
  const bound: { id: Id; chord: Chord }[] = ids.flatMap((id) =>
    commands[id].keys.map((chord) => ({ id, chord })),
  );
  for (const [index, { id, chord }] of bound.entries()) {
    const twice = bound.slice(0, index).find((earlier) => sameKey(earlier.chord, chord));
    assert(!twice, `${describeChord(chord)} is bound to ${twice?.id} and to ${id}`);
  }
  return {
    ids,
    get: (id) => commands[id],
    match: (event, inTextBox) =>
      bound.find(({ chord }) => matches(chord, event, inTextBox))?.id ?? null,
    describe: (id, typed = false) => {
      const chord = commands[id].keys.find((candidate) => typing(candidate) === typed);
      return chord ? describeChord(chord) : null;
    },
    aria: (id) => commands[id].keys.map(ariaChord).join(" ") || null,
  };
}
