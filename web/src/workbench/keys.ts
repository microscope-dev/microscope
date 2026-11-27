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

export function typing(chord: Chord): boolean {
  return !chord.ctrl && !chord.alt && !/^F\d+$/u.test(chord.key);
}

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

export function describeChord(chord: Chord): string {
  if (typing(chord)) {
    return chord.key === "Escape" ? "Esc" : chord.key;
  }
  const key = chord.key.length === 1 ? chord.key.toUpperCase() : (ARROWS[chord.key] ?? chord.key);
  return `${chord.ctrl ? "Ctrl+" : ""}${chord.alt ? "Alt+" : ""}${chord.shift ? "⇧" : ""}${key}`;
}

export function ariaChord(chord: Chord): string {
  const key = chord.key === " " ? "Space" : chord.key;
  return [chord.ctrl && "Control", chord.alt && "Alt", chord.shift && "Shift", key]
    .filter(Boolean)
    .join("+");
}

const PUNCTUATION: Readonly<Record<string, string>> = {
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Backquote: "`",
};

/** Prefers physical `event.code` under Alt/Ctrl so macOS Option dead keys and accented characters still match shortcuts. */
function keyOf(event: KeyLike): string {
  if (!event.code) {
    return event.key;
  }
  if (event.altKey) {
    const digit = /^Digit(\d)$/u.exec(event.code);
    if (digit) {
      return digit[1] as string;
    }
    const letter = /^Key([A-Z])$/u.exec(event.code);
    if (letter) {
      return (letter[1] as string).toLowerCase();
    }
  }
  const punctuation = PUNCTUATION[event.code];
  return punctuation && (event.altKey || event.ctrlKey) ? punctuation : event.key;
}

/**
 * Reports whether `element` should consume plain-key shortcuts as text input.
 * Containers can override via `data-keys="debugger"` (always allow shortcuts)
 * or `data-keys="renderer"` (always swallow plain keys for custom canvases).
 */
export function isTextBox(element: Element | null): boolean {
  if (!element) {
    return false;
  }
  if (element.closest("[data-keys=debugger]")) {
    return false;
  }

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

export function keepsEscape(element: Element | null): boolean {
  return element !== null && element.closest("[data-keys=escape]") !== null;
}
