// Reading the keyboard: which chord a press is, and whether the focused
// element types text, in which case letters belong to it.

export interface Chord {
  readonly key: string;
  readonly shift?: boolean;
  readonly ctrl?: boolean;
  readonly alt?: boolean;
}

export interface KeyLike {
  key: string;
  code?: string;
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}

/** Whether a chord types text, and so acts only outside text boxes: a
 * letter or Escape with neither Ctrl nor Alt. */
export function typing(chord: Chord): boolean {
  return !chord.ctrl && !chord.alt && !/^F\d+$/.test(chord.key);
}

/** Whether a press is a chord. Letters name their case, so Shift is part of
 * typing them. Presses with Cmd or the Windows key are the browser's. */
export function matches(chord: Chord, event: KeyLike, inTextBox: boolean): boolean {
  const key = keyOf(event);
  const letter = typing(chord) && key.length === 1;
  return (
    chord.key === key &&
    (letter || (chord.shift ?? false) === event.shiftKey) &&
    (chord.ctrl ?? false) === event.ctrlKey &&
    (chord.alt ?? false) === event.altKey &&
    !event.metaKey &&
    !(typing(chord) && inTextBox)
  );
}

/** Two chords no press can tell apart. */
export function sameKey(a: Chord, b: Chord): boolean {
  const letter = typing(a) && a.key.length === 1;
  return (
    a.key === b.key &&
    (letter || Boolean(a.shift) === Boolean(b.shift)) &&
    Boolean(a.ctrl) === Boolean(b.ctrl) &&
    Boolean(a.alt) === Boolean(b.alt)
  );
}

const ARROWS: Readonly<Record<string, string>> = {
  ArrowLeft: "←",
  ArrowRight: "→",
  ArrowUp: "↑",
  ArrowDown: "↓",
};

/** How a chord is written in tooltips and help, such as `⇧F11`. */
export function describeChord(chord: Chord): string {
  if (typing(chord)) {
    return chord.key === "Escape" ? "Esc" : chord.key;
  }
  const key = chord.key.length === 1 ? chord.key.toUpperCase() : (ARROWS[chord.key] ?? chord.key);
  return `${chord.ctrl ? "Ctrl+" : ""}${chord.alt ? "Alt+" : ""}${chord.shift ? "⇧" : ""}${key}`;
}

/** A chord as `aria-keyshortcuts` writes it, such as `Shift+F11`. */
export function ariaChord(chord: Chord): string {
  const key = chord.key === " " ? "Space" : chord.key;
  return [chord.ctrl && "Control", chord.alt && "Alt", chord.shift && "Shift", key]
    .filter(Boolean)
    .join("+");
}

/** Punctuation chords name by the key pressed, unshifted. */
const PUNCTUATION: Readonly<Record<string, string>> = {
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Backquote: "`",
};

/** The key a chord names: Alt changes what letters and digits type, and
 * Alt, Ctrl, and Shift change what punctuation types. */
function keyOf(event: KeyLike): string {
  if (!event.code) {
    return event.key;
  }
  if (event.altKey) {
    const digit = /^Digit(\d)$/.exec(event.code);
    if (digit) {
      return digit[1] as string;
    }
    const letter = /^Key([A-Z])$/.exec(event.code);
    if (letter) {
      return (letter[1] as string).toLowerCase();
    }
  }
  const punctuation = PUNCTUATION[event.code];
  return punctuation && (event.altKey || event.ctrlKey) ? punctuation : event.key;
}

/**
 * Whether keys typed now go into text. The source view is read-only and
 * keeps the debugger's keys, so it is no text box.
 */
export function isTextBox(element: Element | null): boolean {
  if (!element) {
    return false;
  }
  if (element.closest("[data-keys=debugger]")) {
    return false;
  }
  // A live drawing's canvas takes the letters, as a text box does.
  if (element.closest("[data-keys=renderer]")) {
    return true;
  }
  if (element instanceof HTMLTextAreaElement || element instanceof HTMLSelectElement) {
    return true;
  }
  if (element instanceof HTMLInputElement) {
    return !["checkbox", "radio", "button", "submit"].includes(element.type);
  }
  return element instanceof HTMLElement && element.isContentEditable;
}

/** Whether a menu or dialog holding `element` closes on Escape itself, so
 * Escape must not also leave the view. Its other keys stay the debugger's. */
export function keepsEscape(element: Element | null): boolean {
  return element?.closest("[data-keys=escape]") != null;
}
