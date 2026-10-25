// The URL is the focus. The path names the debugger state: session, stop,
// thread or task, frame. The query names how a tab looks at it: the file and lines
// shown, the view, watches, and the expanded rows of the variable tree.
// Every function here is pure, so links round-trip exactly.

import type { TaskKey } from "./protocol";

/** What a tab looks at, beyond the stop: everything in the query. */
export interface Look {
  /** The source shown: `PATH:LINE` or `PATH:FIRST-LAST`. */
  src?: string;
  /** Disassembly at an address. */
  asm?: string;
  /** Memory at an address, and how many bytes the value there occupies:
   *  `0xADDRESS` or `0xADDRESS:BYTES`. */
  mem?: string;
  /** Which of the three views of the place is shown; source when absent. */
  view?: View;
  /** Watch expressions, in order. */
  w?: string[];
  /** Expanded paths in the variable tree, such as `args/req`. */
  x?: string[];
  /** Values pinned to the drawings, each `PATH` or `PATH~RENDERER`. */
  d?: string[];
}

export type View = "source" | "disassembly" | "memory" | "drawings";

const VIEWS: readonly View[] = ["source", "disassembly", "memory", "drawings"];

/** The most items a look's list keeps, since each watch or row is a request. */
const LIST_LIMIT = 64;

/** The query keys that repeat: a stop's watches, expanded rows, and
 * drawings, and a profile's threads, transforms, and zoomed path. */
const LISTS = new Set(["w", "x", "d", "th", "tk", "tf", "z", "lb"]);

/**
 * Parses a query, keeping a repeated key's values in order. Lists repeat
 * their key, `w=a&w=b`, because a watch expression can hold any character.
 */
export function parseSearch(search: string): Record<string, string | string[]> {
  const parsed: Record<string, string | string[]> = {};
  for (const [key, value] of new URLSearchParams(search)) {
    const existing = parsed[key];
    if (LISTS.has(key)) {
      parsed[key] = [...(Array.isArray(existing) ? existing : []), value];
    } else if (existing === undefined) {
      parsed[key] = value;
    }
  }
  return parsed;
}

export function stringifySearch(search: Record<string, unknown>): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(search)) {
    if (Array.isArray(value)) {
      for (const item of value) {
        params.append(key, String(item));
      }
    } else if (value !== undefined && value !== null && value !== "") {
      params.append(key, String(value));
    }
  }
  const text = params.toString();
  // `/`, `:`, and `~` read better than their escapes, and are safe in a query.
  return text
    ? `?${text.replaceAll("%2F", "/").replaceAll("%3A", ":").replaceAll("%7E", "~")}`
    : "";
}

/** The typed query, dropping anything malformed. */
export function validateLook(search: Record<string, unknown>): Look {
  const look: Look = {};
  const text = (key: string) => {
    const value = search[key];
    return typeof value === "string" && value !== "" ? value : undefined;
  };
  const list = (key: string) => {
    const value = search[key];
    const items = Array.isArray(value) ? value : typeof value === "string" ? [value] : [];
    const strings = items.filter((item): item is string => typeof item === "string" && item !== "");
    return strings.length > 0 ? strings.slice(0, LIST_LIMIT) : undefined;
  };
  const src = text("src");
  if (src && parsePlace(src)) {
    look.src = src;
  }
  const asm = text("asm");
  if (asm && isAddress(asm)) {
    look.asm = asm;
  }
  const mem = text("mem");
  if (mem && /^0x[0-9a-f]{1,16}(:[1-9]\d{0,8})?$/i.test(mem)) {
    look.mem = mem;
  }
  const view = text("view");
  if (view && (VIEWS as readonly string[]).includes(view) && view !== "source") {
    look.view = view as View;
  }
  const w = list("w");
  if (w) {
    look.w = w;
  }
  const x = list("x");
  if (x) {
    look.x = x;
  }
  const d = list("d");
  if (d) {
    look.d = d;
  }
  return look;
}

function isAddress(text: string): boolean {
  return /^0x[0-9a-f]{1,16}$/i.test(text);
}

/** A place in a source file: a line, or a range of them. */
export interface SourcePlace {
  path: string;
  line: number;
  end: number;
}

/** Parses `PATH:LINE` or `PATH:FIRST-LAST`, where the path may hold colons. */
export function parsePlace(text: string): SourcePlace | null {
  const match = /^(.+):(\d+)(?:-(\d+))?$/.exec(text);
  if (!match) {
    return null;
  }
  const [, path, first, last] = match as unknown as [string, string, string, string | undefined];
  const line = Number(first);
  const end = last === undefined ? line : Number(last);
  if (!Number.isSafeInteger(end) || line < 1 || end < line) {
    return null;
  }
  return { path, line, end };
}

export function formatPlace(place: SourcePlace): string {
  return place.end > place.line
    ? `${place.path}:${place.line}-${place.end}`
    : `${place.path}:${place.line}`;
}

/**
 * A path as a link names it: relative to the directory microscope runs in, when
 * it is inside it, which keeps links short. A compiler can record relative
 * paths too, so a path stays whole when its short form names another of the
 * recorded `files`, or while they are unknown.
 */
export function linkPath(
  path: string,
  cwd: string | undefined,
  files: readonly string[] | undefined,
): string {
  if (cwd && files && path.startsWith(`${cwd}/`)) {
    const short = path.slice(cwd.length + 1);
    if (!files.includes(short)) {
      return short;
    }
  }
  return path;
}

/**
 * The path a link names, as the debug information records it, or null while
 * the recorded `files` that tell a relative path's meaning are unknown.
 */
export function recordedPath(
  path: string,
  cwd: string | undefined,
  files: readonly string[] | undefined,
): string | null {
  if (path.startsWith("/")) {
    return path;
  }
  if (!files) {
    return null;
  }
  if (files.includes(path) || !cwd) {
    return path;
  }
  return `${cwd}/${path}`;
}

/**
 * Whether a link someone else made names a path on this page, never another
 * host: browsers read `//host` and `/\\host` as one.
 */
export function isPagePath(text: string | undefined): text is string {
  const control = (char: string) => char < " " || char === "\u007f";
  return text !== undefined && /^\/(?![/\\])/.test(text) && ![...text].some(control);
}

/**
 * The part of the path after the session: the stop, the thread or task,
 * and the frame. A task's focus names no thread, and says 0.
 */
export interface At {
  stop: number;
  thread: number;
  task?: TaskKey | null;
  frame: number;
}

/** A task as its path segment names it: `RUNTIME.NUMBER`. */
export function taskSegment(task: TaskKey): string {
  return `${task.runtime}.${task.number}`;
}

export function stopPath(session: string, at: At): string {
  const who = at.task ? `task/${taskSegment(at.task)}` : `t/${at.thread}`;
  return `/s/${session}/stop/${at.stop}/${who}/f/${at.frame}`;
}

/** Whether `text` is a 64-bit unsigned number's decimal digits, as the
 * server writes a task's number. */
export function isU64(text: string): boolean {
  return /^(0|[1-9]\d{0,19})$/.test(text) && BigInt(text) < 2n ** 64n;
}

/** Parses a route's path parameters, or null when they are not numbers. */
export function parseAt(params: {
  stop: string;
  thread?: string | undefined;
  task?: string | undefined;
  frame: string;
}): At | null {
  const number = (text: string) =>
    /^\d+$/.test(text) && Number.isSafeInteger(Number(text)) ? Number(text) : Number.NaN;
  const [stop, frame] = [number(params.stop), number(params.frame)];
  if (Number.isNaN(stop) || Number.isNaN(frame)) {
    return null;
  }
  if (params.task !== undefined) {
    // A task's number uses all 64 bits, so it stays the digits it is.
    const parts = params.task.split(".");
    const [runtime, task] = [number(parts[0] ?? ""), parts[1] ?? ""];
    if (parts.length !== 2 || Number.isNaN(runtime) || !isU64(task)) {
      return null;
    }
    return { stop, thread: 0, task: { runtime, number: task }, frame };
  }
  const thread = params.thread === undefined ? Number.NaN : number(params.thread);
  if (Number.isNaN(thread)) {
    return null;
  }
  return { stop, thread, frame };
}

/**
 * What a followed link keeps at a new stop: how the tab looks, but not where
 * it looked, since source and disassembly follow the program counter.
 * Memory stays where it is.
 */
export function followedLook(look: Look): Look {
  const { src: _src, asm: _asm, ...kept } = look;
  return kept;
}

/** The same look, showing the source at `src`. */
export function showingSource(look: Look, src: string): Look {
  const { view: _view, ...rest } = look;
  return { ...rest, src };
}

/** A value pinned to the drawings, and the renderer it names, if any. */
export interface Pinned {
  path: string;
  renderer: string | null;
}

const RENDERER = /^[A-Za-z0-9_-]{1,64}$/;

/** Parses a `d` entry, `PATH`, `PATH~RENDERER`, or `PATH~`. A path may
 * hold `~`, C's complement, so only what follows the last one names a
 * renderer, and a trailing `~` says that none does. */
export function parsePinned(entry: string): Pinned {
  if (entry.length > 1 && entry.endsWith("~")) {
    return { path: entry.slice(0, -1), renderer: null };
  }
  const at = entry.lastIndexOf("~");
  const renderer = at > 0 ? entry.slice(at + 1) : "";
  return RENDERER.test(renderer)
    ? { path: entry.slice(0, at), renderer }
    : { path: entry, renderer: null };
}

export function formatPinned(pinned: Pinned): string {
  if (pinned.renderer !== null) {
    return `${pinned.path}~${pinned.renderer}`;
  }
  // `flags&~mask` alone would read as `flags&` drawn by `mask`.
  return parsePinned(pinned.path).path === pinned.path ? pinned.path : `${pinned.path}~`;
}

/** The same look with `pinned` drawn, once, and the drawings shown. */
export function drawing(look: Look, pinned: Pinned): Look {
  const entry = formatPinned(pinned);
  const d = (look.d ?? []).filter((each) => each !== entry);
  return { ...look, d: [...d, entry], view: "drawings" };
}

/** The same look, in `view`: source is the view a look names by none. */
export function inView(look: Look, view: View): Look {
  const { view: _view, ...rest } = look;
  return view === "source" ? rest : { ...rest, view };
}
