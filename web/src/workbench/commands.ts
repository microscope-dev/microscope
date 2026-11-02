// Commands: everything a person can do by name. The menus, the palette, the
// keys, and the key help all read one registry, so a command's label, keys,
// and whether it can run now are the same wherever it is offered.

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

/** Whether a command can run now. A blocked one says why, as a disabled
 * menu item or a key that does nothing should. An absent one has no meaning
 * here at all, such as stepping on a page with no program, and its key is
 * left to the browser. */
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
  /** The first of each kind, typing or not, is the one help shows. */
  readonly keys: readonly Chord[];
  available(context: Context): Availability;
  /** Runs only when available says it is ready. */
  run(context: Context): void;
}

export interface Registry<Id extends string, Context> {
  /** In the order they were defined. */
  readonly ids: readonly Id[];
  get(id: Id): Command<Context>;
  /** The command a key press means, given whether a text box has focus. */
  match(event: KeyLike, inTextBox: boolean): Id | null;
  /** How a command's key is written, its typing one or its other one. */
  describe(id: Id, typed?: boolean): string | null;
  /** A command's keys as `aria-keyshortcuts` lists them. */
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
