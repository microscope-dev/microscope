import { assert } from "./assert";
import { target } from "./memory";
import type { TaskKey } from "./protocol";

export interface Look {
  src?: string;

  asm?: string;

  mem?: string;

  view?: View;

  w?: string[];

  x?: string[];

  d?: string[];
}

export type View = "source" | "disassembly" | "memory" | "drawings";

const VIEWS: readonly View[] = ["source", "disassembly", "memory", "drawings"];

const LIST_LIMIT = 64;

const LISTS = new Set(["w", "x", "d", "th", "tk", "tf", "z", "lb"]);

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

  return text
    ? `?${text.replaceAll("%2F", "/").replaceAll("%3A", ":").replaceAll("%7E", "~")}`
    : "";
}

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
  if (mem && target(mem)) {
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
  return /^0x[0-9a-f]{1,16}$/iu.test(text);
}

export interface SourcePlace {
  path: string;
  line: number;
  end: number;
}

export function parsePlace(text: string): SourcePlace | null {
  const match = /^(.+):(\d+)(?:-(\d+))?$/u.exec(text);
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

export function isPagePath(text: string | undefined): text is string {
  return (
    text !== undefined && /^\/(?![/\\])/u.test(text) && ![...text].some((char) => isControl(char))
  );
}

function isControl(char: string): boolean {
  return char < " " || char === "\u007F";
}

export interface At {
  stop: number;
  thread: number;
  task?: TaskKey | null;
  frame: number;
}

export function taskSegment(task: TaskKey): string {
  return `${task.runtime}.${task.number}`;
}

export function stopPath(session: string, at: At): string {
  assert(
    !at.task || at.thread === 0,
    `a task's focus at stop ${at.stop} names thread ${at.thread}`,
  );
  const who = at.task ? `task/${taskSegment(at.task)}` : `t/${at.thread}`;
  return `/s/${session}/stop/${at.stop}/${who}/f/${at.frame}`;
}

export function isU64(text: string): boolean {
  return /^(0|[1-9]\d{0,19})$/u.test(text) && BigInt(text) < 2n ** 64n;
}

function pathNumber(text: string): number {
  return /^\d+$/u.test(text) && Number.isSafeInteger(Number(text)) ? Number(text) : Number.NaN;
}

export function parseAt(params: {
  stop: string;
  thread?: string | undefined;
  task?: string | undefined;
  frame: string;
}): At | null {
  const [stop, frame] = [pathNumber(params.stop), pathNumber(params.frame)];
  if (Number.isNaN(stop) || Number.isNaN(frame)) {
    return null;
  }
  if (params.task !== undefined) {
    const parts = params.task.split(".");
    const [runtime, task] = [pathNumber(parts[0] ?? ""), parts[1] ?? ""];
    if (parts.length !== 2 || Number.isNaN(runtime) || !isU64(task)) {
      return null;
    }
    return { stop, thread: 0, task: { runtime, number: task }, frame };
  }
  const thread = params.thread === undefined ? Number.NaN : pathNumber(params.thread);
  if (Number.isNaN(thread)) {
    return null;
  }
  return { stop, thread, frame };
}

export function followedLook(look: Look): Look {
  const { src: _src, asm: _asm, ...kept } = look;
  return kept;
}

export function showingSource(look: Look, src: string): Look {
  const { view: _view, ...rest } = look;
  return { ...rest, src };
}

export interface Pinned {
  path: string;
  renderer: string | null;
}

const RENDERER = /^[A-Za-z0-9_-]{1,64}$/u;

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

  return parsePinned(pinned.path).path === pinned.path ? pinned.path : `${pinned.path}~`;
}

export function drawing(look: Look, pinned: Pinned): Look {
  const entry = formatPinned(pinned);
  const d = (look.d ?? []).filter((each) => each !== entry);
  return { ...look, d: [...d, entry], view: "drawings" };
}

export function watching(look: Look, expression: string): Look {
  const w = look.w ?? [];
  return w.includes(expression) ? look : { ...look, w: [...w, expression] };
}

export function inView(look: Look, view: View): Look {
  const { view: _view, ...rest } = look;
  return view === "source" ? rest : { ...rest, view };
}
