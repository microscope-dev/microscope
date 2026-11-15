export type ValuePane = "Variables" | "Watch";

export interface User {
  press(chord: string): Promise<void>;

  addBreakpoint(location: string): Promise<void>;

  expectCurrentLine(text: string): Promise<void>;

  expectValue(pane: ValuePane, name: string, text: string): Promise<void>;

  menu(menu: string, item: string): Promise<void>;

  chooseTab(title: string): Promise<void>;

  expectGroups(groups: string[][]): Promise<void>;
}

export const MODIFIERS = ["Ctrl", "Alt", "Shift", "Meta"] as const;
export type Modifier = (typeof MODIFIERS)[number];

export interface Chord {
  modifiers: Modifier[];

  key: string;
}

export function parseChord(text: string): Chord {
  const parts = text.split("+");

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

export function toPlaywright(chord: Chord): string {
  return [...chord.modifiers.map((m) => (m === "Ctrl" ? "Control" : m)), chord.key].join("+");
}
