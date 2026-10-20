// The keyboard map: VS Code's debugger keys, which a page can claim (F5
// does not reload, F11 does not go fullscreen), and letter keys that work
// whenever no text box has focus, for keyboards without function keys.

import type { ActionName } from "./actions";

export type Command =
  | ActionName
  | "toggleBreakpoint"
  | "editBreakpoint"
  | "frameUp"
  | "frameDown"
  | "copyLink"
  | "pin"
  | "watchSelection"
  | "palette"
  | "files"
  | "line"
  | "help"
  | "back"
  | `pane${1 | 2 | 3 | 4 | 5 | 6 | 7}`
  | "viewSource"
  | "viewDisassembly"
  | "viewMemory"
  | "viewDrawings"
  | "reloadViews";

/** What each command is called in the palette and the key help. */
export const LABELS: Record<Command, string> = {
  continue: "Continue",
  pause: "Pause",
  kill: "Kill",
  restart: "Restart",
  over: "Step over",
  into: "Step into",
  out: "Step out",
  instruction: "Step one instruction",
  overInstruction: "Step over one instruction",
  toggleBreakpoint: "Toggle breakpoint",
  editBreakpoint: "Edit breakpoint condition",
  jumpToCursor: "Jump to cursor",
  stepIntoCall: "Step into a call…",
  frameUp: "Frame up",
  frameDown: "Frame down",
  copyLink: "Copy link",
  pin: "Pin this tab",
  watchSelection: "Watch selection",
  palette: "Command palette",
  files: "Open file",
  line: "Go to line",
  help: "Every key",
  back: "Back to source",
  pane1: "Focus threads",
  pane2: "Focus the call stack",
  pane3: "Focus breakpoints",
  pane4: "Focus watches",
  pane5: "Focus variables",
  pane6: "Focus the console and output",
  pane7: "Focus registers",
  viewSource: "Show source",
  viewDisassembly: "Show disassembly",
  viewMemory: "Show memory",
  viewDrawings: "Show drawings",
  reloadViews: "Reload views",
};

export interface Binding {
  key: string;
  shift?: boolean;
  ctrl?: boolean;
  alt?: boolean;
  /** Letter keys type text, so they act only outside text boxes. */
  typing: boolean;
  command: Command;
}

const letters = (pairs: [string, Command, boolean?][]): Binding[] =>
  pairs.map(([key, command, shift]) => ({ key, typing: true, command, shift: shift ?? false }));

export const BINDINGS: readonly Binding[] = [
  { key: "F5", typing: false, command: "continue" },
  { key: "F6", typing: false, command: "pause" },
  { key: "F5", shift: true, typing: false, command: "kill" },
  { key: "F5", ctrl: true, shift: true, typing: false, command: "restart" },
  { key: "F10", typing: false, command: "over" },
  { key: "F11", typing: false, command: "into" },
  { key: "F11", shift: true, typing: false, command: "out" },
  { key: "F11", ctrl: true, typing: false, command: "stepIntoCall" },
  { key: "F9", typing: false, command: "toggleBreakpoint" },
  { key: "F9", shift: true, typing: false, command: "editBreakpoint" },
  { key: "k", ctrl: true, typing: false, command: "palette" },
  { key: "p", ctrl: true, typing: false, command: "files" },
  { key: "g", ctrl: true, typing: false, command: "line" },
  ...letters([
    ["c", "continue"],
    ["n", "over"],
    ["s", "into"],
    ["f", "out"],
    ["N", "overInstruction", true],
    ["S", "instruction", true],
    ["b", "toggleBreakpoint"],
    ["J", "jumpToCursor", true],
    ["i", "stepIntoCall"],
    ["u", "frameUp"],
    ["d", "frameDown"],
    ["w", "watchSelection"],
    ["y", "copyLink"],
    ["p", "pin"],
    ["?", "help", true],
    ["Escape", "back"],
  ]),
  ...([1, 2, 3, 4, 5, 6, 7] as const).map(
    (number): Binding => ({
      key: String(number),
      alt: true,
      typing: false,
      command: `pane${number}`,
    }),
  ),
  { key: "s", alt: true, typing: false, command: "viewSource" },
  { key: "d", alt: true, typing: false, command: "viewDisassembly" },
  { key: "m", alt: true, typing: false, command: "viewMemory" },
  { key: "v", alt: true, typing: false, command: "viewDrawings" },
];

export interface KeyLike {
  key: string;
  code?: string;
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}

/** The key a binding names: Alt changes what letters and digits type. */
function keyOf(event: KeyLike): string {
  if (event.altKey && event.code) {
    const digit = /^Digit(\d)$/.exec(event.code);
    if (digit) {
      return digit[1] as string;
    }
    const letter = /^Key([A-Z])$/.exec(event.code);
    if (letter) {
      return (letter[1] as string).toLowerCase();
    }
  }
  return event.key;
}

/** The command a key press means, given whether a text box has focus. */
export function commandFor(event: KeyLike, inTextBox: boolean): Command | null {
  const key = keyOf(event);
  for (const binding of BINDINGS) {
    // Letters name their case; Shift is part of typing them.
    const letter = binding.typing && key.length === 1;
    if (
      binding.key === key &&
      (letter || (binding.shift ?? false) === event.shiftKey) &&
      (binding.ctrl ?? false) === event.ctrlKey &&
      (binding.alt ?? false) === event.altKey &&
      !event.metaKey &&
      !(binding.typing && inTextBox)
    ) {
      return binding.command;
    }
  }
  return null;
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

/** How a command's key is written in tooltips and help, such as `⇧F11`. */
export function describe(command: Command, typing = false): string | null {
  const binding = BINDINGS.find(
    (candidate) => candidate.command === command && candidate.typing === typing,
  );
  if (!binding) {
    return null;
  }
  if (binding.typing) {
    return binding.key === "Escape" ? "Esc" : binding.key;
  }
  return `${binding.ctrl ? "Ctrl+" : ""}${binding.alt ? "Alt+" : ""}${binding.shift ? "⇧" : ""}${binding.key.length === 1 ? binding.key.toUpperCase() : binding.key}`;
}
