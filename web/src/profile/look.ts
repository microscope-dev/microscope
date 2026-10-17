// How a tab looks at a profile, all of it in the address bar: the view,
// what it aggregates, the time range, threads, and tasks selected, the
// transforms in order, and what is selected and zoomed, by function names
// rather than node numbers, so a link opens the same view of the same
// profile anywhere.

import type {
  ProfileGranularity,
  ProfileQuery,
  ProfileTransform,
  SyscallGroup,
  SyscallQuery,
  TransformOp,
} from "../protocol";

export type ProfileView = "flame" | "tree" | "functions" | "zones" | "syscalls";

/** The Syscalls view's tabs; the summary when absent. */
export type SyscallTab = "log" | "files" | "network" | "processes";

export interface ProfileLook {
  /** The view shown; the flame graph when absent. */
  view?: ProfileView;
  /** `START-END`, nanoseconds from the profile's start. */
  range?: string;
  /** Thread rows selected; every thread when absent. */
  th?: string[];
  /** Where task snapshots found tasks waiting, not the CPU samples. */
  src?: "waits";
  /** Task rows selected, `none` for what no snapshot named; every task
   * when absent. */
  tk?: string[];
  /** Each path rooted at its task. */
  bt?: "1";
  /** Labels a sample must carry, each `KEY=VALUE`. */
  lb?: string[];
  /** Each path rooted at its label of this key. */
  bl?: string;
  /** The zone site, or label key, the zones view finds. */
  zs?: string;
  /** Timeline lanes of tasks rather than threads. */
  lanes?: "tasks";
  /** Transforms in order, each `OP:NAME`. */
  tf?: string[];
  invert?: "1";
  /** The frames threads ran, not each async task's own. */
  native?: "1";
  /** Each stack as its Python code sees it. */
  py?: "1";
  /** How finely frames are told apart; by function when absent. */
  by?: Exclude<ProfileGranularity, "function">;
  /** The function selected, by name. */
  fn?: string;
  /** The flame graph's zoomed node, by its path of names from the root. */
  z?: string[];
  /** The profile a diff compares with, by its id; this one when absent and
   * `br` is set. */
  bp?: string;
  /** The range of the base profile a diff compares with, `START-END`. */
  br?: string;
  /** What the selected function shows: its source lines or its code,
   * rather than its callers and callees. */
  ann?: "lines" | "code";
  /** The Syscalls view's selection, in strace's `-e` syntax. */
  sf?: string;
  /** What the Syscalls summary's rows are of; calls when absent. */
  sg?: Exclude<SyscallGroup, "syscall">;
  /** The call selected in the Syscalls summary, by name. */
  sc?: string;
  /** The Syscalls view's tab. */
  st?: SyscallTab;
  /** The rate chart stacks calls rather than classes. */
  sr?: "syscall";
  /** The log line expanded, by its event. */
  ev?: string;
  /** Calls of one log2 latency bucket, as a heat map's row names it. */
  sl?: string;
}

const VIEWS: readonly ProfileView[] = ["flame", "tree", "functions", "zones", "syscalls"];
const SYSCALL_GROUPS: readonly string[] = ["class", "process", "thread", "path", "site"];
const SYSCALL_TABS: readonly string[] = ["log", "files", "network", "processes"];
const GRANULARITIES: readonly string[] = ["physical", "line", "address"];
const OPS: readonly TransformOp[] = ["focus", "merge", "drop", "collapse"];

/** The typed look, dropping anything malformed. */
export function validateProfileLook(search: Record<string, unknown>): ProfileLook {
  const look: ProfileLook = {};
  const text = (key: string) => {
    const value = search[key];
    return typeof value === "string" && value !== "" ? value : undefined;
  };
  const list = (key: string) => {
    const value = search[key];
    const items = Array.isArray(value) ? value : typeof value === "string" ? [value] : [];
    const strings = items.filter((item): item is string => typeof item === "string" && item !== "");
    return strings.length > 0 ? strings : undefined;
  };
  const view = text("view");
  if (view && (VIEWS as readonly string[]).includes(view) && view !== "flame") {
    look.view = view as ProfileView;
  }
  const range = text("range");
  if (range && parseRange(range)) {
    look.range = range;
  }
  const th = list("th")?.filter((row) => /^\d+$/.test(row));
  if (th && th.length > 0) {
    look.th = th;
  }
  if (text("src") === "waits") {
    look.src = "waits";
  }
  const tk = list("tk")?.filter((row) => row === "none" || /^\d+$/.test(row));
  if (tk && tk.length > 0) {
    look.tk = tk;
  }
  if (text("bt") === "1") {
    look.bt = "1";
  }
  const lb = list("lb")?.filter((label) => label.indexOf("=") > 0);
  if (lb && lb.length > 0) {
    look.lb = lb;
  }
  for (const key of ["bl", "zs"] as const) {
    const value = text(key);
    if (value) {
      look[key] = value;
    }
  }
  if (text("lanes") === "tasks") {
    look.lanes = "tasks";
  }
  const tf = list("tf")?.filter((entry) => parseTransform(entry) !== null);
  if (tf && tf.length > 0) {
    look.tf = tf;
  }
  if (text("invert") === "1") {
    look.invert = "1";
  }
  if (text("native") === "1") {
    look.native = "1";
  }
  if (text("py") === "1") {
    look.py = "1";
  }
  const by = text("by");
  if (by && GRANULARITIES.includes(by)) {
    look.by = by as ProfileLook["by"] & string;
  }
  const fn = text("fn");
  if (fn) {
    look.fn = fn;
  }
  const z = list("z");
  if (z) {
    look.z = z;
  }
  const bp = text("bp");
  if (bp) {
    look.bp = bp;
  }
  const br = text("br");
  if (br && parseRange(br)) {
    look.br = br;
  }
  const ann = text("ann");
  if (ann === "lines" || ann === "code") {
    look.ann = ann;
  }
  for (const key of ["sf", "sc"] as const) {
    const value = text(key);
    if (value) {
      look[key] = value;
    }
  }
  const sg = text("sg");
  if (sg && SYSCALL_GROUPS.includes(sg)) {
    look.sg = sg as ProfileLook["sg"] & string;
  }
  const st = text("st");
  if (st && SYSCALL_TABS.includes(st)) {
    look.st = st as SyscallTab;
  }
  if (text("sr") === "syscall") {
    look.sr = "syscall";
  }
  const ev = text("ev");
  if (ev && /^\d+$/.test(ev)) {
    look.ev = ev;
  }
  const sl = text("sl");
  if (sl && /^\d{1,2}$/.test(sl)) {
    look.sl = sl;
  }
  return look;
}

/** The Syscalls view's query of `profile`: the look's selection over its
 * time range. */
export function syscallQueryOf(profile: string, look: ProfileLook, revision: number): SyscallQuery {
  const range = look.range ? parseRange(look.range) : null;
  return {
    profile,
    revision,
    ...(range ? { range } : {}),
    ...(look.sl ? { latency: Number(look.sl) } : {}),
    filter: look.sf ?? "",
  };
}

/** Parses `START-END`, a nonempty range of whole nanoseconds. */
export function parseRange(text: string): [number, number] | null {
  const match = /^(\d{1,16})-(\d{1,16})$/.exec(text);
  if (!match) {
    return null;
  }
  const start = Number(match[1]);
  const end = Number(match[2]);
  return end > start ? [start, end] : null;
}

export function formatRange([start, end]: [number, number]): string {
  return `${Math.round(start)}-${Math.round(end)}`;
}

/** Parses `OP:NAME`; the name may hold colons of its own. */
export function parseTransform(entry: string): ProfileTransform | null {
  const at = entry.indexOf(":");
  const op = entry.slice(0, at);
  const name = entry.slice(at + 1);
  return at > 0 && name !== "" && (OPS as readonly string[]).includes(op)
    ? { op: op as TransformOp, name }
    : null;
}

export function formatTransform(transform: ProfileTransform): string {
  return `${transform.op}:${transform.name}`;
}

/** The query a look asks of `profile`. */
export function queryOf(profile: string, look: ProfileLook): ProfileQuery {
  const range = look.range ? parseRange(look.range) : null;
  const baseRange = look.br ? parseRange(look.br) : null;
  const base =
    look.bp || baseRange
      ? { profile: look.bp ?? profile, ...(baseRange ? { range: baseRange } : {}) }
      : null;
  return {
    profile,
    ...(range ? { range } : {}),
    ...(look.th ? { threads: look.th.map(Number) } : {}),
    source: look.src === "waits" ? "waits" : "samples",
    ...(look.tk ? { tasks: look.tk.map((row) => (row === "none" ? null : Number(row))) } : {}),
    byTask: look.bt === "1",
    labels: look.lb ?? [],
    ...(look.bl ? { byLabel: look.bl } : {}),
    transforms: (look.tf ?? []).flatMap((entry) => {
      const transform = parseTransform(entry);
      return transform ? [transform] : [];
    }),
    invert: look.invert === "1",
    look: look.native === "1" ? "native" : look.py === "1" ? "python" : "logical",
    granularity: look.by ?? "function",
    ...(base ? { base } : {}),
  };
}

/** The same look with `transform` applied after the others. What was
 * selected or zoomed may no longer be where it was, so neither is kept. */
export function transformed(look: ProfileLook, transform: ProfileTransform): ProfileLook {
  const { z: _z, ...rest } = look;
  return { ...rest, tf: [...(look.tf ?? []), formatTransform(transform)] };
}

/** The same look without its transform at `index`, and those after it. */
export function untransformed(look: ProfileLook, index: number): ProfileLook {
  const { tf, z: _z, ...rest } = look;
  const kept = (tf ?? []).slice(0, index);
  return kept.length > 0 ? { ...rest, tf: kept } : rest;
}

/** The same look with only `key` changed, or removed when undefined. */
export function withLook<K extends keyof ProfileLook>(
  look: ProfileLook,
  key: K,
  value: ProfileLook[K] | undefined,
): ProfileLook {
  const { [key]: _old, ...rest } = look;
  return value === undefined ? (rest as ProfileLook) : ({ ...rest, [key]: value } as ProfileLook);
}
