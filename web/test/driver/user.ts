// What a person does with the page, in their words. A journey is written
// against this interface once and runs at two levels: in Vitest's Chromium
// against a scripted debugger (fast.ts), and in Playwright against a real
// `microscope web` (real.ts). When the page changes how something is done,
// the backends change and the journeys stay as they are.

/** A pane the page shows values in. */
export type ValuePane = "Variables" | "Watch";

export interface User {
  /** Presses keys as the Keyboard shortcuts pane writes them: `F10`, `Alt+Shift+ArrowRight`. */
  press(chord: string): Promise<void>;

  /** Adds a breakpoint by typing where, as `kvstore.c:92`. */
  addBreakpoint(location: string): Promise<void>;

  /** Waits until the program is stopped on the source line holding `text`. */
  expectCurrentLine(text: string): Promise<void>;

  /** Waits until `pane` shows the value named `name` as `text`. */
  expectValue(pane: ValuePane, name: string, text: string): Promise<void>;

  /** Opens a menu on the menu bar and chooses an item in it. */
  menu(menu: string, item: string): Promise<void>;

  /** Chooses the tab with this title. */
  chooseTab(title: string): Promise<void>;

  /** Waits until the groups show these tabs, in reading order, the shown
   * one in each marked with a star: `[["*Threads", "Tasks"], …]`. */
  expectGroups(groups: string[][]): Promise<void>;
}

export const MODIFIERS = ["Ctrl", "Alt", "Shift", "Meta"] as const;
export type Modifier = (typeof MODIFIERS)[number];

export interface Chord {
  modifiers: Modifier[];
  /** As `KeyboardEvent.key` names it: `F10`, `ArrowRight`, `k`, `[`. */
  key: string;
}

/** Reads `Alt+Shift+ArrowRight`; modifiers come in MODIFIERS order. */
export function parseChord(text: string): Chord {
  const parts = text.split("+");
  // A chord ending in `+` presses the plus key itself.
  if (parts.at(-1) === "" && parts.length > 1) {
    parts.splice(-2, 2, "+");
  }
  const key = parts.pop();
  if (!key) {
    throw new Error(`no key in the chord ${JSON.stringify(text)}`);
  }
  const modifiers = parts.map((part) => {
    if (!MODIFIERS.includes(part as Modifier)) {
      throw new Error(`${JSON.stringify(part)} in ${JSON.stringify(text)} is not a modifier`);
    }
    return part as Modifier;
  });
  const ordered = MODIFIERS.filter((modifier) => modifiers.includes(modifier));
  if (ordered.length !== modifiers.length || ordered.some((m, i) => m !== modifiers[i])) {
    throw new Error(`write the modifiers of ${JSON.stringify(text)} as ${ordered.join("+")}`);
  }
  return { modifiers, key };
}

/** The chord as testing-library's `keyboard` writes it: modifiers held
 * around the key, `{` and `[` doubled since they open its own syntax. */
export function toKeyboard(chord: Chord): string {
  const key = chord.key === "{" ? "{{" : chord.key === "[" ? "[[" : `{${chord.key}}`;
  const down = chord.modifiers.map(
    (modifier) => `{${modifier === "Ctrl" ? "Control" : modifier}>}`,
  );
  const up = chord.modifiers
    .map((modifier) => `{/${modifier === "Ctrl" ? "Control" : modifier}}`)
    .toReversed();
  return [...down, key, ...up].join("");
}

/** The chord as Playwright's `keyboard.press` writes it. */
export function toPlaywright(chord: Chord): string {
  return [...chord.modifiers.map((m) => (m === "Ctrl" ? "Control" : m)), chord.key].join("+");
}
